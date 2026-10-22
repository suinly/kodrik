use super::{
    message::{Message, ToolCall},
    session::{SessionId, SqliteSessionStore},
    tools::{Tool, ToolOutput, ToolRegistry},
    *,
};
use crate::gateway::{IncomingMessage, InterfaceId};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::mpsc;

fn read(db: &PathBuf, table: &str) -> Vec<serde_json::Value> {
    let db = rusqlite::Connection::open(db).unwrap();
    let mut stmt = db.prepare(&format!("SELECT payload FROM {table}")).unwrap();
    stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))
        .unwrap()
        .map(|r| serde_json::from_slice(&r.unwrap()).unwrap())
        .collect()
}
struct Model {
    db: PathBuf,
    check: &'static str,
    calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl llm::LlmProvider for Model {
    async fn generate(
        &self,
        context: &LlmContext<'_>,
        _: &[&dyn Tool],
        _: Option<&mpsc::Sender<AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.check == "input" {
            let sessions = read(&self.db, "sessions");
            assert_eq!(sessions.len(), 1, "input must be committed before model");
            assert_eq!(sessions[0]["messages"][0]["User"]["content"], "hello");
            assert_eq!(read(&self.db, "runs")[0]["phase"], "NeedModel");
        }
        if self.check == "result" && n == 1 {
            let executions = read(&self.db, "tool_executions");
            assert_eq!(
                executions.len(),
                1,
                "result must be committed before next model"
            );
            assert_eq!(executions[0]["result"]["output"]["ok"], true);
            let sessions = read(&self.db, "sessions");
            assert!(
                sessions[0]["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["ToolResult"]["output"]["ok"] == true)
            );
            assert!(matches!(
                context.messages.last(),
                Some(Message::ToolResult(_))
            ));
        }
        if self.check == "audit_start" || self.check == "audit_final" {
            break_journal(&self.db);
        }
        if n == 0 && self.check != "input" && self.check != "audit_final" {
            let mut calls = vec![Message::ToolCall(ToolCall {
                call_id: "call-1".into(),
                name: "effect".into(),
                arguments: serde_json::json!({"value":42}),
            })];
            if self.check == "multi" {
                calls.push(Message::ToolCall(ToolCall {
                    call_id: "call-2".into(),
                    name: "effect".into(),
                    arguments: serde_json::json!({"value":43}),
                }));
            }
            Ok((calls, None))
        } else {
            Ok((
                vec![Message::Assistant(AssistantMessage {
                    content: "done".into(),
                })],
                None,
            ))
        }
    }
}
struct Effect {
    db: PathBuf,
    check: bool,
    safe: bool,
    break_audit: bool,
    calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl Tool for Effect {
    fn replay_policy(&self) -> tools::ReplayPolicy {
        if self.safe {
            tools::ReplayPolicy::Safe
        } else {
            tools::ReplayPolicy::Never
        }
    }
    fn name(&self) -> String {
        "effect".into()
    }
    fn description(&self) -> String {
        "test effect".into()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        if self.check {
            let runs = read(&self.db, "runs");
            assert_eq!(runs.len(), 1, "intent must be committed before effect");
            assert_eq!(runs[0]["phase"]["ExecutingTool"]["call_id"], "call-1");
            let executions = read(&self.db, "tool_executions");
            assert_eq!(executions[0]["call"]["arguments"], args);
            assert_eq!(executions[0]["replay"], "Never");
        }
        if self.break_audit {
            break_journal(&self.db);
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(serde_json::json!({"ok":true}).into())
    }
}
async fn setup(
    check: &'static str,
) -> (
    tempfile::TempDir,
    AgentRunner,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteSessionStore::open_async(dir.path().to_owned())
            .await
            .unwrap(),
    );
    let db = dir.path().join("state.sqlite3");
    let models = Arc::new(AtomicUsize::new(0));
    let effects = Arc::new(AtomicUsize::new(0));
    let agent = Agent::new(
        Arc::new(Model {
            db: db.clone(),
            check,
            calls: models.clone(),
        }),
        ToolRegistry::default().register(Arc::new(Effect {
            db,
            check: check == "intent",
            safe: check == "safe",
            break_audit: check == "audit_finish",
            calls: effects.clone(),
        })),
        ContextBuilder::new("test"),
    );
    let agent = if check.starts_with("audit") {
        agent.with_journal(crate::audit::Journal::new(dir.path().join("actions")))
    } else {
        agent
    };
    (
        dir,
        AgentRunner::new(Arc::new(agent), store),
        models,
        effects,
    )
}
async fn run(runner: &AgentRunner) -> anyhow::Result<AssistantMessage> {
    runner
        .run(
            SessionId::new("test:chat"),
            IncomingMessage::new(InterfaceId("test".into()), "chat", "hello"),
            None,
        )
        .await
}
#[tokio::test]
async fn durable_input_saved_before_model() {
    let (_dir, r, _, _) = setup("input").await;
    run(&r).await.unwrap();
}
#[tokio::test]
async fn durable_intent_saved_before_tool() {
    let (_dir, r, _, e) = setup("intent").await;
    run(&r).await.unwrap();
    assert_eq!(e.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn durable_result_saved_before_next_model() {
    let (_dir, r, _, _) = setup("result").await;
    run(&r).await.unwrap();
}
#[tokio::test]
async fn durable_commit_failure_stops_effects() {
    let (dir, r, m, e) = setup("failure").await;
    let db = rusqlite::Connection::open(dir.path().join("state.sqlite3")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_run BEFORE INSERT ON runs BEGIN SELECT RAISE(FAIL,'disk full'); END;",
    )
    .unwrap();
    assert!(run(&r).await.is_err());
    assert_eq!(m.load(Ordering::SeqCst), 0);
    assert_eq!(e.load(Ordering::SeqCst), 0);
    assert!(read(&dir.path().join("state.sqlite3"), "sessions").is_empty());
}

#[tokio::test]
async fn durable_failed_boundaries_never_advance_effects_or_publish_completion() {
    for (table, condition, models, effects, phase) in [
        ("tool_executions", "1", 1, 0, "NeedTool"),
        (
            "tool_executions",
            "json_extract(CAST(NEW.payload AS TEXT),'$.result') IS NOT NULL",
            1,
            1,
            "ExecutingTool",
        ),
        ("runs", "NEW.terminal=1", 2, 1, "NeedModel"),
    ] {
        let (dir, r, m, e) = setup("failure").await;
        let path = dir.path().join("state.sqlite3");
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(&format!("CREATE TRIGGER fail_boundary BEFORE INSERT ON {table} WHEN {condition} BEGIN SELECT RAISE(FAIL,'disk full'); END;")).unwrap();
        let (tx, mut rx) = mpsc::channel(32);
        assert!(
            r.run(
                SessionId::new("test:chat"),
                IncomingMessage::new(InterfaceId("test".into()), "chat", "hello"),
                Some(tx)
            )
            .await
            .is_err()
        );
        assert_eq!(m.load(Ordering::SeqCst), models);
        assert_eq!(e.load(Ordering::SeqCst), effects);
        let saved = read(&path, "runs");
        assert!(saved[0]["phase"] == phase || saved[0]["phase"].get(phase).is_some());
        assert!(
            !read(&path, "sessions")[0]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["Assistant"]["content"] == "done")
        );
        while let Some(event) = rx.recv().await {
            assert!(!matches!(event, AgentEvent::Message(_)));
        }
    }
}

#[tokio::test]
async fn durable_multiple_calls_preserve_order_and_duplicate_results_are_rejected() {
    use super::run_state::{RunRecord, ToolExecution};
    use super::session::Session;
    let (dir, r, m, e) = setup("multi").await;
    let path = dir.path().join("state.sqlite3");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TRIGGER stop_second BEFORE INSERT ON tool_executions WHEN NEW.call_id='call-2' BEGIN SELECT RAISE(FAIL,'crash boundary'); END;").unwrap();
    assert!(run(&r).await.is_err());
    assert_eq!(m.load(Ordering::SeqCst), 1);
    assert_eq!(e.load(Ordering::SeqCst), 1);
    let mut record: RunRecord = serde_json::from_value(read(&path, "runs").remove(0)).unwrap();
    assert_eq!(record.pending_calls[0].call_id, "call-2");
    let mut session: Session = serde_json::from_value(read(&path, "sessions").remove(0)).unwrap();
    assert!(matches!(&session.messages()[3],Message::ToolResult(r) if r.call_id=="call-1"));
    let execution: ToolExecution =
        serde_json::from_value(read(&path, "tool_executions").remove(0)).unwrap();
    let store = SqliteSessionStore::open_async(dir.path().to_owned())
        .await
        .unwrap();
    store
        .commit_step(record.revision - 1, &session, &record, Some(&execution))
        .await
        .unwrap();
    assert_eq!(
        read(&path, "sessions")[0]["messages"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    let old = record.revision;
    record.revision += 1;
    session.push(session.messages()[3].clone());
    assert!(
        store
            .commit_step(old, &session, &record, Some(&execution))
            .await
            .is_err()
    );
    assert_eq!(
        read(&path, "sessions")[0]["messages"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
}

#[tokio::test]
async fn durable_read_policy_is_saved_in_intent() {
    let (dir, r, _, _) = setup("safe").await;
    run(&r).await.unwrap();
    assert_eq!(
        read(&dir.path().join("state.sqlite3"), "tool_executions")[0]["replay"],
        "Safe"
    );
}

fn break_journal(db: &std::path::Path) {
    let file = db
        .parent()
        .unwrap()
        .join("actions")
        .join(format!("{}.jsonl", chrono::Utc::now().format("%Y-%m-%d")));
    std::fs::rename(&file, file.with_extension("saved")).unwrap();
    std::fs::create_dir(file).unwrap();
}
#[tokio::test]
async fn durable_audit_failure_preserves_known_outcome_and_blocks_next_action() {
    for (check, count) in [("audit_start", 0), ("audit_finish", 1)] {
        let (dir, r, m, e) = setup(check).await;
        assert!(run(&r).await.is_err());
        assert_eq!(m.load(Ordering::SeqCst), 1);
        assert_eq!(e.load(Ordering::SeqCst), count);
        let path = dir.path().join("state.sqlite3");
        let result = read(&path, "tool_executions").remove(0);
        assert_eq!(result["result"].is_null(), count == 0);
        if count == 1 {
            assert_eq!(result["result"]["output"]["ok"], true);
        }
        assert!(read(&path, "runs")[0]["phase"].get("Failed").is_some());
    }
}
#[tokio::test]
async fn durable_audit_records_correlate_with_saved_run() {
    let (dir, r, _, _) = setup("audit_ok").await;
    run(&r).await.unwrap();
    let id = read(&dir.path().join("state.sqlite3"), "runs")[0]["id"].clone();
    let file = dir
        .path()
        .join("actions")
        .join(format!("{}.jsonl", chrono::Utc::now().format("%Y-%m-%d")));
    for line in std::fs::read_to_string(file).unwrap().lines() {
        let row: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(row["run_id"], id);
    }
}
#[test]
fn durable_known_readers_opt_into_safe_replay() {
    use tools::*;
    let dir = tempfile::tempdir().unwrap();
    let readers: Vec<Box<dyn Tool>> = vec![
        Box::new(SessionSearchTool::new(dir.path().to_owned())),
        Box::new(EncyclopediaTool),
        Box::new(ViewImageTool),
    ];
    for tool in readers {
        assert_eq!(tool.replay_policy(), ReplayPolicy::Safe, "{}", tool.name());
    }
    assert_eq!(ExecCommandTool.replay_policy(), ReplayPolicy::Never);
}

#[tokio::test]
async fn durable_completed_session_can_be_deleted_but_live_run_cannot() {
    use session::SessionStore;
    let (dir, r, _, _) = setup("input").await;
    run(&r).await.unwrap();
    let store = SqliteSessionStore::open_async(dir.path().to_owned())
        .await
        .unwrap();
    store.delete(&SessionId::new("test:chat")).await.unwrap();
    assert!(
        store
            .load(&SessionId::new("test:chat"))
            .await
            .unwrap()
            .is_none()
    );
    assert!(read(&dir.path().join("state.sqlite3"), "runs").is_empty());

    let (dir, r, _, _) = setup("failure").await;
    let path = dir.path().join("state.sqlite3");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TRIGGER stop_intent BEFORE INSERT ON tool_executions BEGIN SELECT RAISE(FAIL,'stop'); END;").unwrap();
    assert!(run(&r).await.is_err());
    let store = SqliteSessionStore::open_async(dir.path().to_owned())
        .await
        .unwrap();
    assert!(store.delete(&SessionId::new("test:chat")).await.is_err());
    assert_eq!(read(&path, "runs").len(), 1);
}

#[tokio::test]
async fn durable_final_audit_failure_does_not_claim_completed_run() {
    let (dir, r, _, _) = setup("audit_final").await;
    assert!(run(&r).await.is_err());
    let path = dir.path().join("state.sqlite3");
    assert!(read(&path, "runs")[0]["phase"].get("Failed").is_some());
    assert_eq!(
        read(&path, "sessions")[0]["messages"][1]["Assistant"]["content"],
        "done"
    );
}

struct CompactionCheck {
    db: PathBuf,
    calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl llm::LlmProvider for CompactionCheck {
    async fn generate(
        &self,
        _: &LlmContext<'_>,
        _: &[&dyn Tool],
        _: Option<&mpsc::Sender<AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(
            read(&self.db, "sessions")[0]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["User"]["content"] == "steer"),
            "steering must be durable before compaction model call"
        );
        Ok((
            vec![Message::Assistant(AssistantMessage {
                content: "short summary".into(),
            })],
            None,
        ))
    }
}
#[tokio::test]
async fn durable_steering_saved_before_compaction_model_request() {
    use session::{Session, SessionStore};
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteSessionStore::open_async(dir.path().to_owned())
            .await
            .unwrap(),
    );
    let mut session = Session::new(SessionId::new("test:chat"));
    for _ in 0..10 {
        session.push(Message::User(UserMessage {
            content: "history ".repeat(15),
            images: vec![],
            files: vec![],
        }));
    }
    session.input_tokens = Some(1000);
    store.save(&session).await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(CompactionCheck {
        db: dir.path().join("state.sqlite3"),
        calls: calls.clone(),
    });
    let agent = Agent::new(
        provider.clone(),
        ToolRegistry::default(),
        ContextBuilder::new("test"),
    )
    .with_compaction(provider, 500);
    let r = AgentRunner::new(Arc::new(agent), store);
    let (tx, mut rx) = mpsc::unbounded_channel();
    tx.send(UserMessage {
        content: "steer".into(),
        images: vec![],
        files: vec![],
    })
    .unwrap();
    r.run_with_cancel(
        SessionId::new("test:chat"),
        IncomingMessage::new(InterfaceId("test".into()), "chat", "hello"),
        None,
        None,
        Some(&mut rx),
    )
    .await
    .unwrap();
    assert!(calls.load(Ordering::SeqCst) > 1);
}

#[tokio::test]
async fn durable_preflight_cancel_cannot_overwrite_an_existing_live_run() {
    let (dir, runner, _, effects) = setup("failure").await;
    let path = dir.path().join("state.sqlite3");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TRIGGER stop_result BEFORE INSERT ON tool_executions WHEN json_extract(CAST(NEW.payload AS TEXT),'$.result') IS NOT NULL BEGIN SELECT RAISE(FAIL,'disk full'); END;").unwrap();
    assert!(run(&runner).await.is_err());
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    let before = read(&path, "sessions");
    let runs_before = read(&path, "runs");
    let tools_before = read(&path, "tool_executions");
    let (cancel, cancelled) = tokio::sync::oneshot::channel();
    cancel.send(()).unwrap();
    assert!(
        runner
            .run_with_cancel(
                SessionId::new("test:chat"),
                IncomingMessage::new(InterfaceId("test".into()), "chat", "another input"),
                None,
                Some(cancelled),
                None
            )
            .await
            .is_err()
    );
    assert_eq!(
        read(&path, "sessions"),
        before,
        "early cancellation must not overwrite the previous live run"
    );
    assert_eq!(read(&path, "runs"), runs_before);
    assert_eq!(read(&path, "tool_executions"), tools_before);
}

#[tokio::test]
async fn durable_another_conversation_cannot_overwrite_a_live_session() {
    use session::SessionStore;
    let (dir, runner, models, _) = setup("failure").await;
    let path = dir.path().join("state.sqlite3");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TRIGGER stop_result BEFORE INSERT ON tool_executions WHEN json_extract(CAST(NEW.payload AS TEXT),'$.result') IS NOT NULL BEGIN SELECT RAISE(FAIL,'disk full'); END;").unwrap();
    assert!(run(&runner).await.is_err());
    let before = read(&path, "sessions");
    let runs_before = read(&path, "runs");
    let tools_before = read(&path, "tool_executions");
    let store = SqliteSessionStore::open_async(dir.path().to_owned())
        .await
        .unwrap();
    store
        .select(
            &SessionId::new("another:chat"),
            &SessionId::new("test:chat"),
        )
        .await
        .unwrap();
    assert!(
        runner
            .run(
                SessionId::new("another:chat"),
                IncomingMessage::new(InterfaceId("another".into()), "chat", "another input"),
                None
            )
            .await
            .is_err()
    );
    assert_eq!(models.load(Ordering::SeqCst), 1);
    assert_eq!(read(&path, "sessions"), before);
    assert_eq!(read(&path, "runs"), runs_before);
    assert_eq!(read(&path, "tool_executions"), tools_before);
}
