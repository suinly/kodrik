use super::*;
use crate::agent::{
    Agent, AssistantMessage, ContextBuilder, LlmContext, UserMessage,
    llm::LlmProvider,
    message::Message,
    tools::{ExecCommandTool, Tool, ToolRegistry},
};
use crate::agent::{
    message::ToolCall,
    run_state::{Checkpoint, RunPhase},
    session::{Session, SessionId, SqliteSessionStore},
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

// Provider fixtures for unrelated isolation tests still exercise the audit call.
pub(crate) fn safe_audit_response(request: &Value) -> Option<Value> {
    if !request["instructions"]
        .as_str()
        .is_some_and(|text| text.starts_with("You are an independent decision component."))
    {
        return None;
    }
    assert!(request["tools"].as_array().is_none_or(Vec::is_empty));
    let input: Value =
        serde_json::from_str(request["input"][0]["content"].as_str().unwrap()).unwrap();
    let choices: Vec<_> = input["ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| json!({"id":id,"choice":"safe"}))
        .collect();
    Some(
        json!({"output":[{"type":"message","content":[{"type":"output_text","text":json!({"choices":choices}).to_string()}]}]}),
    )
}

struct ModelBatch {
    workdir: String,
}
#[async_trait::async_trait]
impl LlmProvider for ModelBatch {
    async fn generate(
        &self,
        context: &LlmContext<'_>,
        _: &[&dyn Tool],
        _: Option<&tokio::sync::mpsc::Sender<crate::agent::AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        if context
            .messages
            .iter()
            .any(|m| matches!(m, Message::ToolResult(_)))
        {
            return Ok((
                vec![Message::Assistant(AssistantMessage {
                    content: "done".into(),
                })],
                None,
            ));
        }
        let mut calls: Vec<_> = [
            ("first", "printf safe > first"),
            ("risk", "printf risky > risk"),
            ("last", "printf safe > last"),
        ]
        .into_iter()
        .map(|(id, cmd)| {
            Message::ToolCall(ToolCall {
                call_id: id.into(),
                name: "exec_command".into(),
                arguments: json!({"cmd":cmd,"workdir":self.workdir}),
            })
        })
        .collect();
        calls.insert(
            1,
            Message::ToolCall(ToolCall {
                call_id: "other-tool".into(),
                name: "unavailable_other_tool".into(),
                arguments: json!({}),
            }),
        );
        Ok((calls, None))
    }
}
struct Auditor {
    calls: AtomicUsize,
    malformed: bool,
}
#[async_trait::async_trait]
impl LlmProvider for Auditor {
    async fn generate(
        &self,
        _: &LlmContext<'_>,
        tools: &[&dyn Tool],
        _: Option<&tokio::sync::mpsc::Sender<crate::agent::AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        assert!(tools.is_empty());
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok((
            vec![Message::Assistant(AssistantMessage {
                content: if self.malformed {
                    "bad response".into()
                } else {
                    r#"{"choices":[{"id":"first","choice":"safe"},{"id":"risk","choice":"destructive"},{"id":"last","choice":"safe"},{"id":"other-tool","choice":"safe"}]}"#.into()
                },
            })],
            None,
        ))
    }
}

#[tokio::test]
async fn batched_shells_wait_for_their_own_decision_and_execute_in_order() {
    for (approve, restart) in [(true, false), (false, false), (true, true)] {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
        store
            .set_default_command_audit_mode(CommandAuditMode::Review)
            .await
            .unwrap();
        let auditor = Arc::new(Auditor {
            calls: AtomicUsize::new(0),
            malformed: false,
        });
        let agent = Arc::new(
            Agent::new(
                Arc::new(ModelBatch {
                    workdir: dir.path().display().to_string(),
                }),
                ToolRegistry::default().register(Arc::new(ExecCommandTool)),
                ContextBuilder::new("test"),
            )
            .with_command_auditor(SystemOne::new(auditor.clone())),
        );
        let session = Session::new(SessionId::new("audit-live"));
        let mut cp = Checkpoint::new(&store, session.id(), &session);
        let worker = agent.clone();
        let mut task = tokio::spawn(async move {
            let mut session = session;
            worker
                .run_durable(
                    &mut session,
                    UserMessage {
                        content: "work".into(),
                        files: vec![],
                        images: vec![],
                    },
                    None,
                    None,
                    Some(&mut cp),
                )
                .await
        });
        let pending = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let pending = store.pending_command_approvals("audit-live").await.unwrap();
                if pending.len() == 1
                    && dir.path().join("first").exists()
                    && store
                        .load_run(&pending[0].run_id)
                        .await
                        .unwrap()
                        .is_some_and(|(record, _)| !record.attempt_active)
                {
                    break pending;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!dir.path().join("risk").exists());
        assert!(!dir.path().join("last").exists());
        if restart {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            drop(store);
            store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
            let (record, mut saved) = store.load_run(&pending[0].run_id).await.unwrap().unwrap();
            let ownership = store
                .claim_execution(&record.conversation_id, &record.session_id)
                .await
                .unwrap();
            let mut cp = Checkpoint::resume(&store, record, ownership);
            assert!(cp.prepare_recovery(&saved, 3).await.unwrap());
            assert_eq!(cp.record.recovery_attempts, 0);
            task = tokio::spawn(async move { agent.resume_durable(&mut saved, &mut cp).await });
            assert_eq!(
                store.pending_command_approvals("audit-live").await.unwrap()[0].id,
                pending[0].id
            );
        }
        store
            .decide_command_approval("audit-live", &pending[0].id, approve)
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(dir.path().join("risk").exists(), approve);
        assert!(dir.path().join("last").exists());
        assert_eq!(auditor.calls.load(Ordering::SeqCst), 1);
        assert!(
            store
                .decide_command_approval("audit-live", &pending[0].id, true)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn malformed_batch_requires_separate_permission_for_every_command() {
    for mode in [CommandAuditMode::Review, CommandAuditMode::Ask] {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
        store.set_default_command_audit_mode(mode).await.unwrap();
        let auditor = Arc::new(Auditor {
            calls: AtomicUsize::new(0),
            malformed: true,
        });
        let agent = Agent::new(
            Arc::new(ModelBatch {
                workdir: dir.path().display().to_string(),
            }),
            ToolRegistry::default().register(Arc::new(ExecCommandTool)),
            ContextBuilder::new("test"),
        )
        .with_command_auditor(SystemOne::new(auditor.clone()));
        let mut session = Session::new(SessionId::new("audit-failure"));
        let mut cp = Checkpoint::new(&store, session.id(), &session);
        let task = tokio::spawn(async move {
            agent
                .run_durable(
                    &mut session,
                    UserMessage {
                        content: "work".into(),
                        files: vec![],
                        images: vec![],
                    },
                    None,
                    None,
                    Some(&mut cp),
                )
                .await
        });
        let pending = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let pending = store
                    .pending_command_approvals("audit-failure")
                    .await
                    .unwrap();
                if pending.len() == 4 {
                    break pending;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        for marker in ["first", "risk", "last"] {
            assert!(!dir.path().join(marker).exists());
        }
        for request in pending {
            store
                .decide_command_approval("audit-failure", &request.id, request.call_id == "risk")
                .await
                .unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!dir.path().join("first").exists());
        assert!(dir.path().join("risk").exists());
        assert!(!dir.path().join("last").exists());
        assert_eq!(
            auditor.calls.load(Ordering::SeqCst),
            usize::from(mode == CommandAuditMode::Review)
        );
    }
}

struct Unavailable(bool);
#[async_trait::async_trait]
impl LlmProvider for Unavailable {
    async fn generate(
        &self,
        _: &LlmContext<'_>,
        _: &[&dyn Tool],
        _: Option<&tokio::sync::mpsc::Sender<crate::agent::AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        if self.0 {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
        anyhow::bail!("provider unavailable")
    }
}

#[tokio::test(start_paused = true)]
async fn command_review_errors_and_timeouts_require_confirmation() {
    for timeout in [false, true] {
        let service = SystemOne::new(Arc::new(Unavailable(timeout)));
        let session = Session::new(SessionId::new("test"));
        let calls = [ToolCall {
            call_id: "test".into(),
            name: "exec_command".into(),
            arguments: json!({"cmd":"pwd"}),
        }];
        let started = tokio::time::Instant::now();
        let result = review(&service, &session, &calls, false, &[]).await;
        assert_eq!(result[0].1, "unavailable");
        if timeout {
            assert_eq!(started.elapsed(), std::time::Duration::from_secs(15));
        }
    }
}

#[tokio::test]
async fn isolated_child_agent_inherits_the_command_gate() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
    store
        .set_default_command_audit_mode(CommandAuditMode::Review)
        .await
        .unwrap();
    let sandbox = Arc::new(crate::agent::sandbox::Sandbox::new(
        dir.path().to_path_buf(),
    ));
    let auditor = Arc::new(Auditor {
        calls: AtomicUsize::new(0),
        malformed: false,
    });
    let agent = Agent::new(
        Arc::new(ModelBatch {
            workdir: "/workspace".into(),
        }),
        ToolRegistry::default().register(sandbox.tool("exec_command")),
        ContextBuilder::new("test"),
    )
    .with_sandbox(sandbox.clone())
    .with_command_auditor(SystemOne::new(auditor.clone()))
    .child_agent();
    let mut session = Session::new(SessionId::new("isolated-review"));
    let mut cp = Checkpoint::new(&store, session.id(), &session);
    let task = tokio::spawn(async move {
        agent
            .run_durable(
                &mut session,
                UserMessage {
                    content: "work".into(),
                    files: vec![],
                    images: vec![],
                },
                None,
                None,
                Some(&mut cp),
            )
            .await
    });
    let pending = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let pending = store
                .pending_command_approvals("isolated-review")
                .await
                .unwrap();
            if pending.len() == 1 {
                break pending;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(pending[0].workdir.as_deref(), Some("/workspace"));
    let output = sandbox
        .tool("exec_command")
        .execute(json!({"cmd":"test ! -e risk"}))
        .await
        .unwrap();
    assert_eq!(output.output["exit_code"], 0);
    store
        .decide_command_approval("isolated-review", &pending[0].id, true)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let output = sandbox
        .tool("exec_command")
        .execute(json!({"cmd":"cat risk"}))
        .await
        .unwrap();
    assert_eq!(output.output["stdout"], "risky");
    assert_eq!(auditor.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn noninteractive_run_saves_pending_without_running_the_command() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteSessionStore::open(dir.path().join("sessions")).unwrap());
    store
        .set_default_command_audit_mode(CommandAuditMode::Review)
        .await
        .unwrap();
    let auditor = Arc::new(Auditor {
        calls: AtomicUsize::new(0),
        malformed: true,
    });
    let agent = Agent::new(
        Arc::new(ModelBatch {
            workdir: dir.path().display().to_string(),
        }),
        ToolRegistry::default().register(Arc::new(ExecCommandTool)),
        ContextBuilder::new("test"),
    )
    .with_command_auditor(SystemOne::new(auditor));
    let runner = crate::agent::AgentRunner::new(Arc::new(agent), store.clone());
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), runner.run_once("work"))
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.is::<ApprovalRequired>());
    let runs = store.unfinished_runs().await.unwrap();
    assert_eq!(runs.len(), 1);
    assert!(!runs[0].attempt_active);
    assert_eq!(
        store
            .pending_command_approvals(&runs[0].session_id)
            .await
            .unwrap()
            .len(),
        4
    );
    for marker in ["first", "risk", "last"] {
        assert!(!dir.path().join(marker).exists());
    }
}

async fn pending_run(store: &SqliteSessionStore) -> (Checkpoint, Session, ToolCall) {
    let mut session = Session::new(SessionId::new("audit-session"));
    let call = ToolCall {
        call_id: "shell-1".into(),
        name: "exec_command".into(),
        arguments: json!({"cmd":"rm important.txt", "workdir":"/workspace", "timeout_ms":null}),
    };
    session.push(crate::agent::message::Message::ToolCall(call.clone()));
    let mut cp = Checkpoint::new(store, session.id(), &session);
    cp.record.pending_calls = vec![call.clone()];
    cp.save(
        &session,
        RunPhase::NeedTool {
            call_id: call.call_id.clone(),
        },
        None,
    )
    .await
    .unwrap();
    (cp, session, call)
}

#[tokio::test]
async fn trusted_default_executes_without_auditor_or_approvals() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
    store
        .set_default_command_audit_mode(CommandAuditMode::Trusted)
        .await
        .unwrap();
    assert_eq!(
        store.default_command_audit_mode().await.unwrap(),
        CommandAuditMode::Trusted
    );
    let auditor = Arc::new(Auditor {
        calls: AtomicUsize::new(0),
        malformed: true,
    });
    let agent = Agent::new(
        Arc::new(ModelBatch {
            workdir: dir.path().display().to_string(),
        }),
        ToolRegistry::default().register(Arc::new(ExecCommandTool)),
        ContextBuilder::new("test"),
    )
    .with_command_auditor(SystemOne::new(auditor.clone()));
    let mut session = Session::new(SessionId::new("trusted"));
    let mut cp = Checkpoint::new(&store, session.id(), &session);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        agent.run_durable(
            &mut session,
            UserMessage {
                content: "work".into(),
                files: vec![],
                images: vec![],
            },
            None,
            None,
            Some(&mut cp),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    for marker in ["first", "risk", "last"] {
        assert!(dir.path().join(marker).exists());
    }
    assert_eq!(auditor.calls.load(Ordering::SeqCst), 0);
    assert!(
        store
            .pending_command_approvals("trusted")
            .await
            .unwrap()
            .is_empty()
    );
}

pub(crate) struct SingleToolCall(pub(crate) ToolCall);

#[async_trait::async_trait]
impl LlmProvider for SingleToolCall {
    async fn generate(
        &self,
        context: &LlmContext<'_>,
        _: &[&dyn Tool],
        _: Option<&tokio::sync::mpsc::Sender<crate::agent::AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        Ok((
            vec![if context
                .messages
                .iter()
                .any(|m| matches!(m, Message::ToolResult(_)))
            {
                Message::Assistant(AssistantMessage {
                    content: "done".into(),
                })
            } else {
                Message::ToolCall(self.0.clone())
            }],
            None,
        ))
    }
}

struct McpDeleteFile(std::path::PathBuf);

#[async_trait::async_trait]
impl Tool for McpDeleteFile {
    fn name(&self) -> String {
        "mcp__files__delete".into()
    }
    fn description(&self) -> String {
        "Delete a file permanently".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"}}})
    }
    async fn execute(&self, _: Value) -> anyhow::Result<crate::agent::tools::ToolOutput> {
        std::fs::remove_file(&self.0)?;
        Ok(json!({"deleted":true}).into())
    }
}

struct SafeToolReviewer;

#[async_trait::async_trait]
impl LlmProvider for SafeToolReviewer {
    async fn generate(
        &self,
        context: &LlmContext<'_>,
        tools: &[&dyn Tool],
        _: Option<&tokio::sync::mpsc::Sender<crate::agent::AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        assert!(tools.is_empty());
        let Message::User(input) = &context.messages[0] else {
            panic!()
        };
        let input: Value = serde_json::from_str(&input.content)?;
        assert_eq!(input["state"]["tools"][0]["name"], "apply_patch");
        assert!(
            input["state"]["tools"][0]["description"]
                .as_str()
                .unwrap()
                .contains("Edit files")
        );
        assert_eq!(input["state"]["tools"][0]["parameters"]["type"], "object");
        Ok((
            vec![Message::Assistant(AssistantMessage {
                content: r#"{"choices":[{"id":"create","choice":"safe"}]}"#.into(),
            })],
            None,
        ))
    }
}

#[tokio::test]
async fn default_review_allows_safe_tool_calls_and_records_the_review() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
    let call = ToolCall {
        call_id: "create".into(),
        name: "apply_patch".into(),
        arguments: json!({"patch":"*** Begin Patch\n*** Add File: note.txt\n+hello\n*** End Patch","workdir":dir.path()}),
    };
    let agent = Agent::new(
        Arc::new(SingleToolCall(call.clone())),
        ToolRegistry::default().register(Arc::new(crate::agent::tools::ApplyPatchTool)),
        ContextBuilder::new("test"),
    )
    .with_command_auditor(SystemOne::new(Arc::new(SafeToolReviewer)));
    let mut session = Session::new(SessionId::new("safe-tool"));
    let mut cp = Checkpoint::new(&store, session.id(), &session);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        agent.run_durable(
            &mut session,
            UserMessage {
                content: "create note".into(),
                files: vec![],
                images: vec![],
            },
            None,
            None,
            Some(&mut cp),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("note.txt")).unwrap(),
        "hello\n"
    );
    assert_eq!(
        store.command_decision(&cp.record.id, &call).await.unwrap(),
        Some(Decision::Safe)
    );
    assert!(
        store
            .pending_command_approvals("safe-tool")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn recovered_safe_tool_can_be_approved_after_switching_to_ask() {
    use crate::agent::{
        run_state::ToolExecution,
        tools::{ReplayPolicy, ViewImageTool},
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("image.png");
    image::RgbImage::new(1, 1).save(&path).unwrap();
    let store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
    store
        .with_connection(|db| {
            db.execute(
                "INSERT INTO web_push_subscriptions VALUES('device','hash','{}','ru')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let call = ToolCall {
        call_id: "image".into(),
        name: "view_image".into(),
        arguments: json!({"path":path}),
    };
    let mut session = Session::new(SessionId::new("replay-approval"));
    session.push(Message::ToolCall(call.clone()));
    let mut cp = Checkpoint::new(&store, session.id(), &session);
    cp.record.command_audit = true;
    cp.record.isolated = Some(false);
    cp.record.pending_calls = vec![call.clone()];
    cp.save(
        &session,
        RunPhase::NeedTool {
            call_id: call.call_id.clone(),
        },
        None,
    )
    .await
    .unwrap();
    store
        .record_command_audits(&cp.record, &[(call.clone(), "safe".into())])
        .await
        .unwrap();
    cp.save(
        &session,
        RunPhase::ExecutingTool {
            call_id: call.call_id.clone(),
            replay: ReplayPolicy::Safe,
        },
        Some(ToolExecution {
            call: call.clone(),
            replay: ReplayPolicy::Safe,
            result: None,
        }),
    )
    .await
    .unwrap();
    store
        .set_command_audit_mode("replay-approval", CommandAuditMode::Ask)
        .await
        .unwrap();
    assert!(
        store
            .pending_command_approvals("replay-approval")
            .await
            .unwrap()
            .is_empty(),
        "active execution must not be re-approved"
    );
    let agent = Agent::new(
        Arc::new(SingleToolCall(call)),
        ToolRegistry::default().register(Arc::new(ViewImageTool)),
        ContextBuilder::new("test"),
    )
    .with_command_auditor(SystemOne::new(Arc::new(Unavailable(false))));
    let resumed_agent = agent.clone();
    let mut task = tokio::spawn(async move {
        agent.resume_durable(&mut session, &mut cp).await.unwrap();
        session
    });
    let request = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Some(request) = store
                .pending_command_approvals("replay-approval")
                .await
                .unwrap()
                .into_iter()
                .next()
            {
                break request;
            }
            assert!(
                !task.is_finished(),
                "recovered tool bypassed current Ask mode"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("recovered invocation has no approval controls");
    let queued = store
        .with_connection(|db| {
            Ok(
                db.query_row("SELECT event_id,session_id FROM web_push_queue", [], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(
        queued,
        (format!("approval:{}", request.id), "replay-approval".into())
    );
    store
        .with_connection(|db| {
            db.execute("DELETE FROM web_push_queue", [])?;
            Ok(())
        })
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
    let (record, mut saved) = store.load_run(&request.run_id).await.unwrap().unwrap();
    let ownership = store
        .claim_execution(&record.conversation_id, &record.session_id)
        .await
        .unwrap();
    let mut checkpoint = Checkpoint::resume(&store, record, ownership);
    assert!(checkpoint.prepare_recovery(&saved, 3).await.unwrap());
    task = tokio::spawn(async move {
        resumed_agent
            .resume_durable(&mut saved, &mut checkpoint)
            .await
            .unwrap();
        saved
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while store
            .pending_command_approvals("replay-approval")
            .await
            .unwrap()
            .is_empty()
        {
            assert!(!task.is_finished());
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        store
            .with_connection(|db| Ok(db.query_row(
                "SELECT count(*) FROM web_push_queue",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
        0,
        "recovery must not repeat an already delivered approval push"
    );
    store
        .decide_command_approval("replay-approval", &request.id, true)
        .await
        .unwrap();
    let session = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert!(session.messages().iter().any(|m| matches!(m,Message::ToolResult(result) if result.call_id == "image" && !result.content.images.is_empty())));
    assert!(
        store
            .pending_command_approvals("replay-approval")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .decide_command_approval("replay-approval", &request.id, true)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn tool_execution_checkpoint_rejects_unapproved_calls_after_mode_change() {
    use crate::agent::{run_state::ToolExecution, tools::ReplayPolicy};
    for name in ["apply_patch", "mcp__files__delete"] {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().into()).unwrap();
        let call = ToolCall {
            call_id: "delete".into(),
            name: name.into(),
            arguments: json!({"path":"important.txt"}),
        };
        let mut session = Session::new(SessionId::new("guard"));
        session.push(Message::ToolCall(call.clone()));
        let mut cp = Checkpoint::new(&store, session.id(), &session);
        cp.record.command_audit = true;
        cp.record.pending_calls = vec![call.clone()];
        cp.save(
            &session,
            RunPhase::NeedTool {
                call_id: call.call_id.clone(),
            },
            None,
        )
        .await
        .unwrap();
        store
            .record_command_audits(&cp.record, &[(call.clone(), "safe".into())])
            .await
            .unwrap();
        store
            .set_command_audit_mode("guard", CommandAuditMode::Ask)
            .await
            .unwrap();
        let result = cp
            .save(
                &session,
                RunPhase::ExecutingTool {
                    call_id: call.call_id.clone(),
                    replay: ReplayPolicy::Never,
                },
                Some(ToolExecution {
                    call: call.clone(),
                    replay: ReplayPolicy::Never,
                    result: None,
                }),
            )
            .await;
        assert!(result.unwrap_err().is::<AuditChanged>());
        let request = store
            .pending_command_approvals("guard")
            .await
            .unwrap()
            .remove(0);
        store
            .decide_command_approval("guard", &request.id, true)
            .await
            .unwrap();
        cp.save(
            &session,
            RunPhase::ExecutingTool {
                call_id: call.call_id.clone(),
                replay: ReplayPolicy::Never,
            },
            Some(ToolExecution {
                call,
                replay: ReplayPolicy::Never,
                result: None,
            }),
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn tool_approvals_block_patch_and_mcp_effects_until_exact_permission() {
    use crate::agent::tools::ApplyPatchTool;
    for mcp in [false, true] {
        for (mode, approve) in [
            (CommandAuditMode::Review, false),
            (CommandAuditMode::Review, true),
            (CommandAuditMode::Ask, false),
            (CommandAuditMode::Ask, true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("important.txt");
            std::fs::write(&file, "keep").unwrap();
            let store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
            if mode == CommandAuditMode::Ask {
                store.set_default_command_audit_mode(mode).await.unwrap();
            }
            let call = ToolCall {
                call_id: "delete".into(),
                name: if mcp {
                    "mcp__files__delete"
                } else {
                    "apply_patch"
                }
                .into(),
                arguments: if mcp {
                    json!({"path":file})
                } else {
                    json!({"patch":"*** Begin Patch\n*** Delete File: important.txt\n*** End Patch", "workdir":dir.path()})
                },
            };
            let agent = Agent::new(
                Arc::new(SingleToolCall(call.clone())),
                ToolRegistry::default()
                    .register(Arc::new(ApplyPatchTool))
                    .register(Arc::new(McpDeleteFile(file.clone()))),
                ContextBuilder::new("test"),
            )
            .with_command_auditor(SystemOne::new(Arc::new(Unavailable(false))));
            let mut session = Session::new(SessionId::new("tool-approval"));
            let mut cp = Checkpoint::new(&store, session.id(), &session);
            let task = tokio::spawn(async move {
                agent
                    .run_durable(
                        &mut session,
                        UserMessage {
                            content: "work".into(),
                            files: vec![],
                            images: vec![],
                        },
                        None,
                        None,
                        Some(&mut cp),
                    )
                    .await
            });
            let request = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if let Some(request) = store
                        .pending_command_approvals("tool-approval")
                        .await
                        .unwrap()
                        .into_iter()
                        .next()
                    {
                        break request;
                    }
                    assert!(file.exists(), "tool ran without permission");
                    assert!(!task.is_finished(), "tool bypassed approval");
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert!(file.exists());
            assert_eq!(request.tool_name, call.name);
            assert!(
                request.cmd.contains(&call.name),
                "older clients must show the tool identity"
            );
            assert!(
                request.cmd.contains("important.txt"),
                "older clients must show the full invocation"
            );
            assert_eq!(
                serde_json::from_str::<Value>(request.arguments_json.as_deref().unwrap()).unwrap(),
                call.arguments
            );
            assert!(
                store
                    .decide_command_approval("foreign", &request.id, true)
                    .await
                    .is_err()
            );
            let reopened = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
            assert_eq!(
                reopened
                    .pending_command_approvals("tool-approval")
                    .await
                    .unwrap()[0]
                    .id,
                request.id
            );
            reopened
                .decide_command_approval("tool-approval", &request.id, approve)
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(file.exists(), !approve);
            let mut changed = call.clone();
            changed.name = "exec_command".into();
            assert!(
                store
                    .command_decision(&request.run_id, &changed)
                    .await
                    .is_err()
            );
            changed = call;
            changed.arguments["extra"] = json!(true);
            assert!(
                store
                    .command_decision(&request.run_id, &changed)
                    .await
                    .is_err()
            );
            assert!(
                store
                    .decide_command_approval("tool-approval", &request.id, true)
                    .await
                    .is_err()
            );
        }
    }
}

#[tokio::test]
async fn profile_default_is_captured_per_generation_and_survives_reopen() {
    use crate::agent::session::SessionStore;
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().into()).unwrap();
    let original = Session::new(SessionId::new("original"));
    store.save(&original).await.unwrap();
    store
        .set_default_command_audit_mode(CommandAuditMode::Ask)
        .await
        .unwrap();
    store.save(&original).await.unwrap();
    assert_eq!(
        store.command_audit_mode("original").await.unwrap(),
        CommandAuditMode::Review
    );
    let new = store
        .new_session(&SessionId::new("cli:main"))
        .await
        .unwrap();
    assert_eq!(
        store.command_audit_mode(new.id().as_str()).await.unwrap(),
        CommandAuditMode::Ask
    );
    store
        .set_command_audit_mode("original", CommandAuditMode::Review)
        .await
        .unwrap();
    store.save(&original).await.unwrap();
    assert_eq!(
        store.command_audit_mode("original").await.unwrap(),
        CommandAuditMode::Review
    );
    drop(store);
    let store = SqliteSessionStore::open(dir.path().into()).unwrap();
    assert_eq!(
        store.default_command_audit_mode().await.unwrap(),
        CommandAuditMode::Ask
    );
    assert_eq!(
        store.command_audit_mode("original").await.unwrap(),
        CommandAuditMode::Review
    );
    store
        .save(&Session::new(SessionId::new("original")))
        .await
        .unwrap();
    assert_eq!(
        store.command_audit_mode("original").await.unwrap(),
        CommandAuditMode::Ask
    );
}

#[tokio::test]
async fn always_ask_invalidates_safe_verdict_before_execution_intent() {
    use crate::agent::{run_state::ToolExecution, tools::ReplayPolicy};
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().into()).unwrap();
    store
        .set_default_command_audit_mode(CommandAuditMode::Review)
        .await
        .unwrap();
    let (mut cp, session, call) = pending_run(&store).await;
    cp.record.command_audit = true;
    store
        .record_command_audits(&cp.record, &[(call.clone(), "safe".into())])
        .await
        .unwrap();
    store
        .set_command_audit_mode("audit-session", CommandAuditMode::Ask)
        .await
        .unwrap();
    let intent = ToolExecution {
        call: call.clone(),
        replay: ReplayPolicy::Never,
        result: None,
    };
    let phase = RunPhase::ExecutingTool {
        call_id: call.call_id.clone(),
        replay: ReplayPolicy::Never,
    };
    let revision = cp.record.revision;
    assert!(
        cp.save(&session, phase.clone(), Some(intent.clone()))
            .await
            .unwrap_err()
            .is::<AuditChanged>()
    );
    assert_eq!(cp.record.revision, revision);
    let pending = store
        .pending_command_approvals("audit-session")
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].reason_code, "ask");
    store
        .decide_command_approval("audit-session", &pending[0].id, true)
        .await
        .unwrap();
    cp.save(&session, phase, Some(intent)).await.unwrap();
    assert!(
        store
            .pending_command_approvals("audit-session")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn ask_to_review_rechecks_unreviewed_commands_without_losing_human_decisions() {
    for human_decision in [None, Some(true), Some(false)] {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().into()).unwrap();
        store
            .set_default_command_audit_mode(CommandAuditMode::Ask)
            .await
            .unwrap();
        let (cp, session, call) = pending_run(&store).await;
        let provider = Arc::new(Auditor {
            calls: AtomicUsize::new(0),
            malformed: true,
        });
        let auditor = SystemOne::new(provider.clone());
        let messages = [Message::ToolCall(call.clone())];
        prepare(&auditor, &session, &messages, Some(&cp), false, &[])
            .await
            .unwrap();
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        let pending = store
            .pending_command_approvals("audit-session")
            .await
            .unwrap();
        if let Some(approve) = human_decision {
            store
                .decide_command_approval("audit-session", &pending[0].id, approve)
                .await
                .unwrap();
        }
        store
            .set_command_audit_mode("audit-session", CommandAuditMode::Review)
            .await
            .unwrap();
        prepare(&auditor, &session, &messages, Some(&cp), false, &[])
            .await
            .unwrap();
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            usize::from(human_decision.is_none())
        );
        // A review response already in flight must also preserve the human's decision.
        store
            .record_command_audits(&cp.record, &[(call.clone(), "safe".into())])
            .await
            .unwrap();
        assert_eq!(
            store.command_decision(&cp.record.id, &call).await.unwrap(),
            Some(match human_decision {
                None => Decision::Pending,
                Some(true) => Decision::Approved,
                Some(false) => Decision::Denied,
            })
        );
        if human_decision.is_none() {
            assert_eq!(
                store
                    .pending_command_approvals("audit-session")
                    .await
                    .unwrap()[0]
                    .id,
                pending[0].id
            );
        }
    }
}

#[test]
fn approval_reasons_support_both_languages_and_legacy_records() {
    use crate::i18n::Language;
    for code in [
        "safe",
        "destructive",
        "disclosure",
        "security",
        "uncertain",
        "unavailable",
        "ask",
    ] {
        let mut approval: CommandApproval = serde_json::from_value(json!({
            "id":"id", "run_id":"run", "call_id":"call", "cmd":"ls", "workdir":null, "reason":reason(code).unwrap()
        })).unwrap();
        assert_eq!(
            approval.reason_text(Language::En),
            match code {
                "safe" => "Tool call assessed as safe",
                "uncertain" => "Tool effects could not be established safely",
                "unavailable" => "Independent tool review was unavailable",
                "ask" => "This session requires permission for every tool call",
                _ => reason(code).unwrap(),
            }
        );
        assert_ne!(
            approval.reason_text(Language::Ru),
            approval.reason_text(Language::En)
        );
        let translated = approval.reason_text(Language::Ru);
        approval.reason_code = code.into();
        assert_eq!(approval.reason_text(Language::Ru), translated);
    }
}

#[tokio::test]
async fn schema_upgrade_preserves_existing_modes_and_defaults_new_sessions_to_review() {
    use crate::agent::session::SessionStore;
    for version in [8, 9, 10] {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().into()).unwrap();
        let first = Session::new(SessionId::new("first"));
        let second = Session::new(SessionId::new("second"));
        store.save(&first).await.unwrap();
        store.save(&second).await.unwrap();
        drop(store);
        let db = Connection::open(dir.path().join("state.sqlite3")).unwrap();
        db.execute_batch("DROP TABLE remote_operations; DROP TABLE remote_bindings; DROP TABLE command_audit_modes; DROP TABLE command_audit_profile;")
            .unwrap();
        if version == 8 {
            db.execute_batch("DROP TABLE command_audits;").unwrap();
        }
        if version == 10 {
            migrate_settings(&db).unwrap();
            db.execute(
                "INSERT INTO command_audit_disabled VALUES(?1,?2)",
                params![first.id().as_str(), first.generation()],
            )
            .unwrap();
            db.execute(
                "INSERT INTO command_audit_disabled VALUES(?1,'old-generation')",
                [second.id().as_str()],
            )
            .unwrap();
        }
        db.pragma_update(None, "user_version", version).unwrap();
        drop(db);
        let store = SqliteSessionStore::open(dir.path().into()).unwrap();
        assert_eq!(
            store.default_command_audit_mode().await.unwrap(),
            CommandAuditMode::Review
        );
        assert_eq!(
            store.command_audit_mode("first").await.unwrap(),
            if version == 9 {
                CommandAuditMode::Review
            } else {
                CommandAuditMode::Trusted
            }
        );
        assert_eq!(
            store.command_audit_mode("second").await.unwrap(),
            if version == 8 {
                CommandAuditMode::Trusted
            } else {
                CommandAuditMode::Review
            }
        );
        store
            .set_default_command_audit_mode(CommandAuditMode::Ask)
            .await
            .unwrap();
        assert_ne!(
            store.command_audit_mode("first").await.unwrap(),
            CommandAuditMode::Ask
        );
        assert_ne!(
            store.command_audit_mode("second").await.unwrap(),
            CommandAuditMode::Ask
        );
    }
}

#[tokio::test]
async fn imported_legacy_session_keeps_trusted_when_profile_changes() {
    let dir = tempfile::tempdir().unwrap();
    let session = Session::new(SessionId::new("legacy"));
    std::fs::write(
        dir.path().join("legacy.json"),
        serde_json::to_vec(&session).unwrap(),
    )
    .unwrap();
    let store = SqliteSessionStore::open(dir.path().into()).unwrap();
    store
        .set_default_command_audit_mode(CommandAuditMode::Ask)
        .await
        .unwrap();
    assert_eq!(
        store.command_audit_mode("legacy").await.unwrap(),
        CommandAuditMode::Trusted
    );
}

#[tokio::test]
async fn command_audit_switch_is_persistent_session_scoped_and_generation_bound() {
    use crate::agent::session::SessionStore;
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().into()).unwrap();
    store
        .set_default_command_audit_mode(CommandAuditMode::Review)
        .await
        .unwrap();
    let (mut cp, session, call) = pending_run(&store).await;
    store
        .record_command_audits(&cp.record, &[(call, "uncertain".into())])
        .await
        .unwrap();
    assert!(store.command_audit_enabled("audit-session").await.unwrap());
    let revision = store.view_revision().await.unwrap();
    store
        .set_command_audit_enabled("audit-session", false)
        .await
        .unwrap();
    assert!(store.view_revision().await.unwrap() > revision);
    assert!(
        store
            .pending_command_approvals("audit-session")
            .await
            .unwrap()
            .is_empty()
    );
    cp.persist(&session, None).await.unwrap();
    assert!(
        !store.command_audit_enabled("audit-session").await.unwrap(),
        "checkpoints must not overwrite the human setting"
    );
    assert!(
        store
            .set_command_audit_enabled("missing", false)
            .await
            .is_err()
    );
    for id in [
        "companion:job",
        "telegram:bot:123:guest",
        "telegram:bot:-123",
    ] {
        store.save(&Session::new(SessionId::new(id))).await.unwrap();
        assert!(store.set_command_audit_enabled(id, false).await.is_err());
    }
    drop(cp);
    drop(store);
    let store = SqliteSessionStore::open(dir.path().into()).unwrap();
    store
        .set_default_command_audit_mode(CommandAuditMode::Review)
        .await
        .unwrap();
    assert!(!store.command_audit_enabled("audit-session").await.unwrap());
    let new = store
        .new_session(&SessionId::new("cli:main"))
        .await
        .unwrap();
    assert!(
        store
            .command_audit_enabled(new.id().as_str())
            .await
            .unwrap()
    );
    store
        .set_command_audit_enabled("audit-session", true)
        .await
        .unwrap();
    assert_eq!(
        store
            .pending_command_approvals("audit-session")
            .await
            .unwrap()
            .len(),
        1
    );
    store
        .set_command_audit_enabled("audit-session", false)
        .await
        .unwrap();
    store
        .save(&Session::new(SessionId::new("audit-session")))
        .await
        .unwrap();
    assert!(
        store.command_audit_enabled("audit-session").await.unwrap(),
        "replacement session must not inherit an old bypass"
    );
}

#[tokio::test]
async fn disabling_command_audit_releases_pending_but_preserves_denial() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
    store
        .set_default_command_audit_mode(CommandAuditMode::Review)
        .await
        .unwrap();
    let auditor = Arc::new(Auditor {
        calls: AtomicUsize::new(0),
        malformed: true,
    });
    let agent = Agent::new(
        Arc::new(ModelBatch {
            workdir: dir.path().display().to_string(),
        }),
        ToolRegistry::default().register(Arc::new(ExecCommandTool)),
        ContextBuilder::new("test"),
    )
    .with_command_auditor(SystemOne::new(auditor.clone()));
    let mut session = Session::new(SessionId::new("audit-toggle"));
    let mut cp = Checkpoint::new(&store, session.id(), &session);
    let task = tokio::spawn(async move {
        agent
            .run_durable(
                &mut session,
                UserMessage {
                    content: "work".into(),
                    files: vec![],
                    images: vec![],
                },
                None,
                None,
                Some(&mut cp),
            )
            .await
    });
    let pending = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let pending = store
                .pending_command_approvals("audit-toggle")
                .await
                .unwrap();
            if pending.len() == 4 {
                break pending;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let denied = pending
        .iter()
        .find(|request| request.call_id == "risk")
        .unwrap();
    store
        .decide_command_approval("audit-toggle", &denied.id, false)
        .await
        .unwrap();
    store
        .set_command_audit_enabled("audit-toggle", false)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(dir.path().join("first").exists());
    assert!(!dir.path().join("risk").exists());
    assert!(dir.path().join("last").exists());
    assert_eq!(auditor.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn disabled_session_skips_auditor_and_reenabling_checks_unreviewed_calls() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().into()).unwrap();
    store
        .set_default_command_audit_mode(CommandAuditMode::Review)
        .await
        .unwrap();
    let (mut cp, session, call) = pending_run(&store).await;
    let provider = Arc::new(Auditor {
        calls: AtomicUsize::new(0),
        malformed: true,
    });
    let auditor = Arc::new(SystemOne::new(provider.clone()));
    store
        .set_command_audit_enabled("audit-session", false)
        .await
        .unwrap();
    let messages = [Message::ToolCall(call.clone())];
    assert!(
        prepare(&auditor, &session, &messages, Some(&cp), false, &[])
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        wait_for_decision(&auditor, &mut cp, &session, &call, false, &[])
            .await
            .unwrap(),
        Permission::Allowed
    ));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    store
        .set_command_audit_enabled("audit-session", true)
        .await
        .unwrap();
    let task = tokio::spawn(async move {
        wait_for_decision(&auditor, &mut cp, &session, &call, false, &[]).await
    });
    let request = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(request) = store
                .pending_command_approvals("audit-session")
                .await
                .unwrap()
                .into_iter()
                .next()
            {
                break request;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        !task.is_finished(),
        "re-enabling must gate a previously unreviewed call"
    );
    store
        .decide_command_approval("audit-session", &request.id, false)
        .await
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        Permission::Denied
    ));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn revoked_environment_approval_stops_waiting_even_in_trusted_mode() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().into()).unwrap();
    let (mut cp, session, call) = pending_run(&store).await;
    store
        .record_command_audits(&cp.record, &[(call.clone(), "ask".into())])
        .await
        .unwrap();
    let key = crate::remote::protocol::EnvironmentKey {
        connection_id: uuid::Uuid::new_v4(),
        generation: uuid::Uuid::new_v4(),
    };
    let target = crate::remote::tools::AuditTarget {
        tool_kind: crate::remote::protocol::ToolKind::Exec,
        environment: key.clone(),
        project_root: "/laptop".into(),
    };
    store.with_connection(move |db| {
        db.execute("UPDATE command_audits SET payload=CAST(json_set(CAST(payload AS TEXT),'$.approval.target',json(?1)) AS BLOB)",[serde_json::to_string(&target)?])?;
        Ok(())
    }).await.unwrap();
    store.revoke_remote_approvals(key).await.unwrap();
    store
        .set_command_audit_enabled(session.id().as_str(), false)
        .await
        .unwrap();
    let auditor = SystemOne::new(Arc::new(Auditor {
        calls: AtomicUsize::new(0),
        malformed: true,
    }));
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        wait_for_decision(&auditor, &mut cp, &session, &call, false, &[]),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(result, Permission::Unavailable));
}

#[tokio::test]
async fn unavailable_local_tool_does_not_create_a_late_approval() {
    struct RevokedTool;
    #[async_trait::async_trait]
    impl Tool for RevokedTool {
        fn name(&self) -> String {
            "exec_command".into()
        }
        fn description(&self) -> String {
            "fixture".into()
        }
        fn parameters(&self) -> Value {
            json!({"type":"object"})
        }
        fn available(&self) -> bool {
            false
        }
        fn audit_target(&self) -> Option<crate::remote::tools::AuditTarget> {
            Some(crate::remote::tools::AuditTarget {
                tool_kind: crate::remote::protocol::ToolKind::Exec,
                environment: crate::remote::protocol::EnvironmentKey {
                    connection_id: uuid::Uuid::new_v4(),
                    generation: uuid::Uuid::new_v4(),
                },
                project_root: "/laptop".into(),
            })
        }
        async fn execute(&self, _: Value) -> anyhow::Result<crate::agent::tools::ToolOutput> {
            panic!("revoked tool executed")
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().into()).unwrap();
    let (cp, session, call) = pending_run(&store).await;
    store
        .set_command_audit_mode(session.id().as_str(), CommandAuditMode::Ask)
        .await
        .unwrap();
    let auditor = SystemOne::new(Arc::new(Auditor {
        calls: AtomicUsize::new(0),
        malformed: true,
    }));
    prepare(
        &auditor,
        &session,
        &[Message::ToolCall(call)],
        Some(&cp),
        false,
        &[&RevokedTool],
    )
    .await
    .unwrap();
    assert!(
        store
            .pending_command_approvals(session.id().as_str())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn bypassed_execution_cannot_receive_a_stale_rejection_after_reenable() {
    use crate::agent::run_state::ToolExecution;
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().into()).unwrap();
    store
        .set_default_command_audit_mode(CommandAuditMode::Review)
        .await
        .unwrap();
    let (mut cp, session, call) = pending_run(&store).await;
    cp.record.command_audit = true;
    store
        .record_command_audits(&cp.record, &[(call.clone(), "uncertain".into())])
        .await
        .unwrap();
    let request = store
        .pending_command_approvals("audit-session")
        .await
        .unwrap()
        .remove(0);
    store
        .set_command_audit_enabled("audit-session", false)
        .await
        .unwrap();
    let replay = crate::agent::tools::ReplayPolicy::Never;
    cp.save(
        &session,
        RunPhase::ExecutingTool {
            call_id: call.call_id.clone(),
            replay,
        },
        Some(ToolExecution {
            call,
            replay,
            result: None,
        }),
    )
    .await
    .unwrap();
    store
        .set_command_audit_enabled("audit-session", true)
        .await
        .unwrap();
    assert!(
        store
            .pending_command_approvals("audit-session")
            .await
            .unwrap()
            .is_empty(),
        "an executing command is no longer awaiting a decision"
    );
    assert!(
        store
            .decide_command_approval("audit-session", &request.id, false)
            .await
            .is_err()
    );
    assert!(
        store
            .disable_command_audit_for_approval("audit-session", &request.id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn execution_intent_rechecks_permission_and_rolls_back_when_it_changed() {
    use crate::agent::run_state::ToolExecution;
    for rejected in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().into()).unwrap();
        store
            .set_default_command_audit_mode(CommandAuditMode::Review)
            .await
            .unwrap();
        let (mut cp, session, call) = pending_run(&store).await;
        cp.record.command_audit = true;
        if rejected {
            store
                .record_command_audits(&cp.record, &[(call.clone(), "uncertain".into())])
                .await
                .unwrap();
        }
        let auditor = SystemOne::new(Arc::new(Auditor {
            calls: AtomicUsize::new(0),
            malformed: true,
        }));
        store
            .set_command_audit_enabled("audit-session", false)
            .await
            .unwrap();
        assert!(matches!(
            wait_for_decision(&auditor, &mut cp, &session, &call, false, &[])
                .await
                .unwrap(),
            Permission::Allowed
        ));
        store
            .set_command_audit_enabled("audit-session", true)
            .await
            .unwrap();
        if rejected {
            let request = store
                .pending_command_approvals("audit-session")
                .await
                .unwrap()
                .remove(0);
            store
                .decide_command_approval("audit-session", &request.id, false)
                .await
                .unwrap();
            store
                .set_command_audit_enabled("audit-session", false)
                .await
                .unwrap();
        }
        let revision = cp.record.revision;
        let replay = crate::agent::tools::ReplayPolicy::Never;
        let error = cp
            .save(
                &session,
                RunPhase::ExecutingTool {
                    call_id: call.call_id.clone(),
                    replay,
                },
                Some(ToolExecution {
                    call: call.clone(),
                    replay,
                    result: None,
                }),
            )
            .await
            .unwrap_err();
        assert!(error.is::<AuditChanged>());
        assert_eq!(cp.record.revision, revision);
        assert!(matches!(cp.record.phase, RunPhase::NeedTool { .. }));
        assert!(!cp.storage_uncertain);
        assert!(
            store
                .tool_execution(&cp.record.id, &call.call_id)
                .await
                .unwrap()
                .is_none()
        );
        cp.persist(&session, None).await.unwrap();
    }
}

#[tokio::test]
async fn pending_decisions_survive_reopen_and_bind_session_call_and_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().to_path_buf()).unwrap();
    store
        .set_default_command_audit_mode(CommandAuditMode::Review)
        .await
        .unwrap();
    let (cp, _, call) = pending_run(&store).await;
    store
        .record_command_audits(&cp.record, &[(call.clone(), "destructive".into())])
        .await
        .unwrap();
    // Exercise a persisted shell approval from before generic tool approvals.
    store.with_connection(|db| {
        db.execute("UPDATE command_audits SET payload=CAST(json_remove(CAST(payload AS TEXT),'$.approval.tool_name','$.approval.arguments_json') AS BLOB)",[])?;
        Ok(())
    }).await.unwrap();
    let pending = store
        .pending_command_approvals("audit-session")
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].cmd, "rm important.txt");
    assert!(
        store
            .pending_command_approvals("foreign-session")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .decide_command_approval("foreign-session", &pending[0].id, true)
            .await
            .is_err()
    );
    let id = cp.record.id.clone();
    drop(cp);
    drop(store);
    let store = SqliteSessionStore::open(dir.path().to_path_buf()).unwrap();
    store
        .set_default_command_audit_mode(CommandAuditMode::Review)
        .await
        .unwrap();
    assert_eq!(
        store
            .pending_command_approvals("audit-session")
            .await
            .unwrap()[0]
            .id,
        pending[0].id
    );
    store
        .decide_command_approval("audit-session", &pending[0].id, true)
        .await
        .unwrap();
    assert!(
        store
            .decide_command_approval("audit-session", &pending[0].id, true)
            .await
            .is_err()
    );
    assert_eq!(
        store.command_decision(&id, &call).await.unwrap(),
        Some(Decision::Approved)
    );
    let mut changed = call;
    changed.arguments["cmd"] = json!("rm something-else.txt");
    assert!(store.command_decision(&id, &changed).await.is_err());
}

#[tokio::test]
async fn cancelled_and_completed_runs_cannot_receive_approval() {
    for cancel in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().to_path_buf()).unwrap();
        store
            .set_default_command_audit_mode(CommandAuditMode::Review)
            .await
            .unwrap();
        let (mut cp, session, call) = pending_run(&store).await;
        store
            .record_command_audits(&cp.record, &[(call, "uncertain".into())])
            .await
            .unwrap();
        let pending = store
            .pending_command_approvals("audit-session")
            .await
            .unwrap();
        if cancel {
            store.request_cancel(&cp.record.id).await.unwrap();
        } else {
            cp.save(&session, RunPhase::Completed, None).await.unwrap();
        }
        assert!(
            store
                .decide_command_approval("audit-session", &pending[0].id, true)
                .await
                .is_err()
        );
        assert!(
            store
                .pending_command_approvals("audit-session")
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn personal_store_migration_preserves_pending_approved_and_denied_commands() {
    for decision in [None, Some(true), Some(false)] {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(source.path().to_path_buf()).unwrap();
        store
            .set_default_command_audit_mode(CommandAuditMode::Review)
            .await
            .unwrap();
        let (cp, _, call) = pending_run(&store).await;
        store
            .record_command_audits(&cp.record, &[(call.clone(), "destructive".into())])
            .await
            .unwrap();
        let request = store
            .pending_command_approvals("audit-session")
            .await
            .unwrap()
            .remove(0);
        if let Some(approve) = decision {
            store
                .decide_command_approval("audit-session", &request.id, approve)
                .await
                .unwrap();
        }
        store
            .set_command_audit_enabled("audit-session", false)
            .await
            .unwrap();
        let run_id = cp.record.id.clone();
        drop(cp);
        drop(store);
        crate::agent::session::stage_personal_sessions(
            &[source.path().to_path_buf()],
            destination.path(),
        )
        .unwrap();
        let migrated = SqliteSessionStore::open(destination.path().to_path_buf()).unwrap();
        assert!(
            !migrated
                .command_audit_enabled("audit-session")
                .await
                .unwrap()
        );
        assert_eq!(
            migrated.command_decision(&run_id, &call).await.unwrap(),
            Some(match decision {
                None => Decision::Pending,
                Some(true) => Decision::Approved,
                Some(false) => Decision::Denied,
            })
        );
    }
}

#[tokio::test]
async fn unsupported_approval_channels_return_blocked_tool_results_without_effects() {
    for id in ["telegram:test:42:77:guest", "companion:wakeup:review"] {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
        store
            .set_default_command_audit_mode(CommandAuditMode::Review)
            .await
            .unwrap();
        let agent = Agent::new(
            Arc::new(ModelBatch {
                workdir: dir.path().display().to_string(),
            }),
            ToolRegistry::default().register(Arc::new(ExecCommandTool)),
            ContextBuilder::new("test"),
        )
        .with_command_auditor(SystemOne::new(Arc::new(Auditor {
            calls: AtomicUsize::new(0),
            malformed: true,
        })));
        let mut session = Session::new(SessionId::new(id));
        let mut cp = Checkpoint::new(&store, session.id(), &session);
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            agent.run_durable(
                &mut session,
                UserMessage {
                    content: "work".into(),
                    files: vec![],
                    images: vec![],
                },
                None,
                None,
                Some(&mut cp),
            ),
        )
        .await
        .expect("unsupported channel must not wait for an impossible approval")
        .unwrap();
        let errors: Vec<_> = session
            .messages()
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult(result) if result.call_id != "other-tool" => {
                    result.content.output["error"].as_str()
                }
                _ => None,
            })
            .collect();
        assert_eq!(errors.len(), 3);
        assert!(
            errors
                .iter()
                .all(|error| error.contains("approval controls"))
        );
        for marker in ["first", "risk", "last"] {
            assert!(!dir.path().join(marker).exists());
        }
    }
}

#[tokio::test]
async fn real_children_use_the_parents_approval_channel() {
    use crate::agent::subagents::{ParentContext, Status, Subagents};
    for (interactive, disabled) in [(true, false), (false, false), (true, true)] {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
        store
            .set_default_command_audit_mode(CommandAuditMode::Review)
            .await
            .unwrap();
        let auditor = Arc::new(Auditor {
            calls: AtomicUsize::new(0),
            malformed: true,
        });
        let agent = Agent::new(
            Arc::new(ModelBatch {
                workdir: dir.path().display().to_string(),
            }),
            ToolRegistry::default().register(Arc::new(ExecCommandTool)),
            ContextBuilder::new("test"),
        )
        .with_command_auditor(SystemOne::new(auditor.clone()));
        let manager = Subagents::new(Arc::new(agent.child_agent()), store.clone())
            .await
            .unwrap();
        let session = Session::new(SessionId::new(if interactive {
            "interactive-parent"
        } else {
            "cli:once:parent"
        }));
        let mut cp = Checkpoint::new(&store, session.id(), &session);
        cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
        if disabled {
            store
                .set_command_audit_enabled(session.id().as_str(), false)
                .await
                .unwrap();
        }
        let (_lifetime, finished) = tokio::sync::watch::channel(false);
        let parent = ParentContext { session, finished };
        let child = manager.spawn(&parent, "do the work".into()).await.unwrap();
        if interactive && !disabled {
            let pending = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let pending = store
                        .pending_command_approvals(parent.session.id().as_str())
                        .await
                        .unwrap();
                    if pending.len() == 4 {
                        break pending;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(
                store
                    .pending_command_approvals(&child.id)
                    .await
                    .unwrap()
                    .is_empty()
            );
            for request in pending {
                store
                    .decide_command_approval(
                        parent.session.id().as_str(),
                        &request.id,
                        request.call_id == "risk",
                    )
                    .await
                    .unwrap();
            }
        }
        let child = manager
            .wait(&parent.session, &child.id, 5000)
            .await
            .unwrap();
        assert_eq!(child.status, Status::Completed, "{:?}", child.error);
        assert_eq!(dir.path().join("risk").exists(), interactive);
        assert_eq!(dir.path().join("first").exists(), disabled);
        assert_eq!(dir.path().join("last").exists(), disabled);
        assert_eq!(auditor.calls.load(Ordering::SeqCst), usize::from(!disabled));
    }
}
