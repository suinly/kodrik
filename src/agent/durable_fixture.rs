//! Process-only crash gates. This module and every call site are absent in production.
use super::{
    Agent, AgentEvent, AgentRunner, AssistantMessage, ContextBuilder, LlmContext, UserMessage,
    inbox::{BusyMode, SubmissionRequest},
    llm::{GenerationRetry, LlmProvider},
    message::{Message, ToolCall},
    recovery::RecoveryTarget,
    run_state::{RunPhase, RunRecord, ToolExecution},
    session::{Session, SessionId, SessionStore, SqliteSessionStore},
    tools::{ReplayPolicy, Tool, ToolOutput, ToolRegistry},
};
use crate::{
    Interface, MessageId,
    gateway::{IncomingMessage, InterfaceId},
};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

pub(crate) fn boundary(point: &str) -> anyhow::Result<()> {
    if std::env::var("KODRIK_DURABLE_POINT").ok().as_deref() != Some(point) {
        return Ok(());
    }
    let dir = PathBuf::from(std::env::var("KODRIK_DURABLE_FIXTURE")?);
    std::fs::write(dir.join("ready"), point)?;
    if std::env::var("KODRIK_DURABLE_ACTION").ok().as_deref() == Some("fail") {
        anyhow::bail!("injected storage acknowledgement failure at {point}");
    }
    loop {
        std::thread::sleep(Duration::from_millis(20));
    }
}
pub(crate) fn checkpoint(
    when: &str,
    run: &RunRecord,
    tool: Option<&ToolExecution>,
) -> anyhow::Result<()> {
    let point = if run.next_retry_at.is_some() {
        "retry"
    } else if tool.is_some_and(|t| t.result.is_some()) {
        "result"
    } else if run.revision == 1 {
        "binding"
    } else {
        match run.phase {
            RunPhase::NeedTool { .. } => "decision",
            RunPhase::ExecutingTool { .. } => "intent",
            RunPhase::Completed => "final",
            _ => "model",
        }
    };
    boundary(&format!("{when}_{point}"))
}
async fn gate(dir: &Path, point: &str) -> ! {
    std::fs::write(dir.join("ready"), point).unwrap();
    std::future::pending().await
}
async fn event(url: &str, kind: &str) {
    reqwest::Client::new()
        .post(format!("{url}/{kind}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
}
struct Model {
    url: String,
    dir: PathBuf,
    scenario: String,
    store: SqliteSessionStore,
}
#[async_trait::async_trait]
impl LlmProvider for Model {
    async fn generate(
        &self,
        context: &LlmContext<'_>,
        _: &[&dyn Tool],
        progress: Option<&tokio::sync::mpsc::Sender<AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        if context
            .instructions
            .starts_with("You are a conversation summarizer")
        {
            event(&self.url, "summary").await;
            return Ok((
                vec![Message::Assistant(AssistantMessage {
                    content: "summary".into(),
                })],
                Some(20),
            ));
        }
        event(&self.url, "model").await;
        if context
            .messages
            .iter()
            .any(|m| matches!(m,Message::User(u) if u.content.starts_with("INTERNAL WAKEUP EVENT")))
        {
            return Ok((
                vec![Message::Assistant(AssistantMessage {
                    content: r#"{"message":"wakeup saved"}"#.into(),
                })],
                Some(10),
            ));
        }
        if self.scenario == "stream" {
            progress
                .unwrap()
                .send(AgentEvent::TextDelta("old partial".into()))
                .await
                .unwrap();
            let id = self.store.unfinished_runs().await.unwrap()[0].id.clone();
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if self
                        .store
                        .run_view(&id)
                        .await
                        .unwrap()
                        .live
                        .current
                        .as_ref()
                        .is_some_and(|p| p.text == "old partial")
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            gate(&self.dir, "stream").await;
        }
        if self.scenario == "cancel" {
            boundary("cancel")?;
        }
        if self.scenario == "retry" {
            return Err(GenerationRetry {
                limit: 3,
                retry_after: Some(Duration::from_secs(2)),
                error: anyhow::anyhow!("temporary failure"),
            }
            .into());
        }
        let result = if context
            .messages
            .iter()
            .any(|m| matches!(m, Message::ToolResult(_)))
        {
            Message::Assistant(AssistantMessage {
                content: "done".into(),
            })
        } else {
            Message::ToolCall(ToolCall {
                call_id: "fixture-call".into(),
                name: "effect".into(),
                arguments: serde_json::json!({}),
            })
        };
        Ok((vec![result], Some(10)))
    }
}
struct Effect {
    url: String,
    dir: PathBuf,
    block: bool,
    policy: ReplayPolicy,
}
#[async_trait::async_trait]
impl Tool for Effect {
    fn name(&self) -> String {
        "effect".into()
    }
    fn description(&self) -> String {
        "external fixture effect".into()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    fn replay_policy(&self) -> ReplayPolicy {
        self.policy
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolOutput> {
        event(&self.url, "effect").await;
        if self.block {
            gate(&self.dir, "effect").await;
        }
        Ok(serde_json::json!({"external_effect":true}).into())
    }
}
struct Chat {
    url: String,
    dir: PathBuf,
    block: bool,
}
#[async_trait::async_trait]
impl Interface for Chat {
    fn id(&self) -> InterfaceId {
        InterfaceId("telegram:fixture".into())
    }
    fn allows_recipient(&self, chat: &str) -> bool {
        chat == "42"
    }
    async fn run(&self, _: tokio::sync::mpsc::Sender<crate::gateway::Event>) -> anyhow::Result<()> {
        Ok(())
    }
    async fn send(&self, _: &str, _: &str) -> anyhow::Result<MessageId> {
        event(&self.url, "send").await;
        if self.block {
            gate(&self.dir, "delivery").await;
        }
        Ok(MessageId("fixture-delivery".into()))
    }
}
fn conversation() -> SessionId {
    SessionId::new("telegram:fixture:42")
}
fn input() -> IncomingMessage {
    let mut message = IncomingMessage::new(InterfaceId("telegram:fixture".into()), "42", "work");
    message.request_id = "stable-fixture-request".into();
    message
}
struct Routing(String);
#[async_trait::async_trait]
impl LlmProvider for Routing {
    async fn generate(
        &self,
        _: &LlmContext<'_>,
        _: &[&dyn Tool],
        _: Option<&tokio::sync::mpsc::Sender<AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        event(&self.0, "routing").await;
        Ok((
            vec![Message::Assistant(AssistantMessage {
                content: r#"{"choice":"new"}"#.into(),
            })],
            None,
        ))
    }
}
fn runner(
    store: SqliteSessionStore,
    url: &str,
    dir: &Path,
    scenario: &str,
    policy: ReplayPolicy,
) -> AgentRunner {
    let runner = AgentRunner::new(
        Arc::new(Agent::new(
            Arc::new(Model {
                url: url.into(),
                dir: dir.into(),
                scenario: scenario.into(),
                store: store.clone(),
            }),
            ToolRegistry::default().register(Arc::new(Effect {
                url: url.into(),
                dir: dir.into(),
                block: scenario == "effect",
                policy,
            })),
            ContextBuilder::new("fixture rules"),
        )),
        Arc::new(store),
    );
    if scenario == "routing" {
        runner.with_session_router(super::session_router::SessionRouter::new(
            super::system_one::SystemOne::new(Arc::new(Routing(url.into()))),
            0,
        ))
    } else {
        runner
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_process_worker() {
    let Ok(dir) = std::env::var("KODRIK_DURABLE_FIXTURE") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let url = std::env::var("KODRIK_DURABLE_URL").unwrap();
    let scenario = std::env::var("KODRIK_DURABLE_SCENARIO").unwrap();
    let store = SqliteSessionStore::open_async(dir.join("sessions"))
        .await
        .unwrap();
    let policy = if matches!(scenario.as_str(), "safe" | "revoked") {
        ReplayPolicy::Safe
    } else {
        ReplayPolicy::Never
    };
    let runner = runner(store.clone(), &url, &dir, &scenario, policy);
    if scenario == "claim" {
        let report = runner
            .recover(RecoveryTarget::Conversation(conversation()))
            .await
            .unwrap();
        assert!(report.completed.is_empty());
        assert!(!report.blocked.is_empty());
        return;
    }
    if scenario == "routing" {
        let old:Session=serde_json::from_value(serde_json::json!({"id":"old-session","last_message_at":"2000-01-01T00:00:00Z","messages":[{"User":{"content":"old topic"}}]})).unwrap();
        store.save(&old).await.unwrap();
        store.select(&conversation(), old.id()).await.unwrap();
    }
    if scenario == "wakeup" {
        use super::reminders::{Recipient, ReminderStore};
        let source = runner.new_session(&conversation()).await.unwrap();
        let journal = ReminderStore::wakeups(dir.join("journal"));
        journal.execute_wakeup(&Recipient{interface:"telegram:fixture".into(),chat:"42".into()},source.id().as_str(),serde_json::from_value(serde_json::json!({"action":"create","text":"fixture wakeup","due_at":(chrono::Utc::now()-chrono::Duration::minutes(1)).to_rfc3339()})).unwrap(),chrono::Utc::now()-chrono::Duration::minutes(2)).unwrap();
        let task = journal.due(chrono::Utc::now()).unwrap().remove(0);
        runner
            .execute_wakeup(
                &Chat {
                    url,
                    dir,
                    block: false,
                },
                &journal,
                &task,
                chrono::Utc::now(),
            )
            .await
            .unwrap();
        return;
    }
    if scenario == "compaction" {
        let mut session = Session::new(conversation());
        session.push(Message::User(UserMessage {
            content: "old ".repeat(1000),
            files: vec![],
            images: vec![],
        }));
        session.push(Message::User(UserMessage {
            content: "latest".into(),
            files: vec![],
            images: vec![],
        }));
        let mut cp = super::run_state::Checkpoint::new(&store, &conversation(), &session);
        cp.record.isolated = Some(false);
        cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
        let compactor = super::compact::Compactor {
            llm: Arc::new(Model {
                url,
                dir,
                scenario,
                store,
            }),
            threshold: 80000,
        };
        compactor
            .force_durable(
                &mut session,
                &ContextBuilder::new("fixture rules"),
                &[],
                &mut cp,
            )
            .await
            .unwrap();
        return;
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel(100);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = runner.run(conversation(), input(), Some(tx)).await;
    if std::env::var("KODRIK_DURABLE_ACTION").ok().as_deref() == Some("fail") {
        assert!(
            result.is_err(),
            "acknowledgement failure must stop execution"
        );
        return;
    }
    result.unwrap();
    if scenario == "delivery" {
        let id = store
            .pending_deliveries(conversation().as_str())
            .await
            .unwrap()
            .remove(0);
        store
            .deliver_receipt(
                &Chat {
                    url,
                    dir,
                    block: true,
                },
                &id,
                chrono::Utc::now(),
                None,
            )
            .await
            .unwrap();
    }
}
struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn child(dir: &Path, url: &str, scenario: &str, point: &str, fail: bool) -> Child {
    Child(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "agent::durable_fixture::durable_process_worker",
                "--nocapture",
            ])
            .env("KODRIK_DURABLE_FIXTURE", dir)
            .env("KODRIK_DURABLE_URL", url)
            .env("KODRIK_DURABLE_SCENARIO", scenario)
            .env("KODRIK_DURABLE_POINT", point)
            .env("KODRIK_DURABLE_ACTION", if fail { "fail" } else { "kill" })
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    )
}
async fn wait_ready(child: &mut Child, dir: &Path) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !dir.join("ready").exists() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "worker exited before crash boundary"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("worker boundary deadline");
}
struct Service {
    url: String,
    events: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Service {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Service {
    async fn start(dir: &Path) -> Self {
        use axum::{
            Router,
            extract::{Path as RoutePath, State},
            routing::post,
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        let log = dir.join("external-service.log");
        let app = Router::new()
            .route(
                "/{kind}",
                post(
                    |State((events, path)): State<(Arc<Mutex<Vec<String>>>, PathBuf)>,
                     RoutePath(kind): RoutePath<String>| async move {
                        use std::io::Write;
                        let mut log = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(path)
                            .unwrap();
                        writeln!(log, "{kind}").unwrap();
                        log.sync_all().unwrap();
                        events
                            .lock()
                            .unwrap()
                            .push(format!("{kind}|{}", chrono::Utc::now().timestamp_millis()));
                        "ok"
                    },
                ),
            )
            .with_state((events.clone(), log));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { url, events, task }
    }
    fn count(&self, kind: &str) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.split_once('|').unwrap().0 == kind)
            .count()
    }
}
#[tokio::test]
async fn durable_process_crash_matrix() {
    for (scenario, point, expected_effects) in [
        ("admission-before", "before_admission", 1),
        ("admission-after", "after_admission", 1),
        ("binding", "before_binding", 1),
        ("routing", "before_binding", 1),
        ("stream", "", 1),
        ("decision", "after_decision", 1),
        ("unsafe", "after_intent", 0),
        ("safe", "after_intent", 1),
        ("revoked", "after_intent", 0),
        ("effect", "", 1),
        ("result", "after_result", 1),
        ("final", "after_final", 1),
        ("delivery", "", 1),
        ("cancel", "cancel", 0),
        ("retry", "after_retry", 1),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::start(dir.path()).await;
        let mut process = child(dir.path(), &service.url, scenario, point, false);
        wait_ready(&mut process, dir.path()).await;
        if scenario == "cancel" {
            // Persist Stop while the model poll is blocked, before it can finish cancellation.
            let store = SqliteSessionStore::open_async(dir.path().join("sessions"))
                .await
                .unwrap();
            let id = store.unfinished_runs().await.unwrap()[0].id.clone();
            store.request_cancel(&id).await.unwrap();
        }
        if scenario == "decision" {
            assert!(
                child(dir.path(), &service.url, "claim", "", false)
                    .0
                    .wait()
                    .unwrap()
                    .success()
            );
        }
        process.0.kill().unwrap();
        process.0.wait().unwrap();
        let store = SqliteSessionStore::open_async(dir.path().join("sessions"))
            .await
            .unwrap();
        let before = store.unfinished_runs().await.unwrap();
        let retry_at = before.first().and_then(|r| r.next_retry_at);
        if scenario.starts_with("admission") || scenario == "binding" {
            let first = store
                .submit(SubmissionRequest::new(
                    &conversation(),
                    input(),
                    BusyMode::FollowUp,
                ))
                .await
                .unwrap();
            let again = store
                .submit(SubmissionRequest::new(
                    &conversation(),
                    input(),
                    BusyMode::FollowUp,
                ))
                .await
                .unwrap();
            assert_eq!(first.id, again.id);
            assert!(again.duplicate);
        }
        if scenario == "routing" {
            assert_eq!(
                store.active_id(&conversation()).await.unwrap().as_str(),
                "old-session"
            );
            assert_eq!(service.count("routing"), 1);
        }
        let models_before = service.count("model");
        let policy = if scenario == "safe" {
            ReplayPolicy::Safe
        } else {
            ReplayPolicy::Never
        };
        let report = runner(
            store.clone(),
            &service.url,
            dir.path(),
            if scenario == "routing" { "routing" } else { "" },
            policy,
        )
        .recover(RecoveryTarget::Conversation(conversation()))
        .await
        .unwrap();
        assert!(
            report.blocked.is_empty(),
            "{scenario}: {:?}",
            report.blocked
        );
        assert!(
            store.unfinished_runs().await.unwrap().is_empty(),
            "{scenario}"
        );
        assert_eq!(service.count("effect"), expected_effects, "{scenario}");
        if matches!(scenario, "final" | "delivery" | "cancel") {
            assert_eq!(service.count("model"), models_before, "{scenario}");
        }
        if let Some(at) = retry_at {
            let events = service.events.lock().unwrap();
            let first = events
                .iter()
                .filter(|s| s.starts_with("model|"))
                .nth(models_before)
                .unwrap()
                .split_once('|')
                .unwrap()
                .1
                .parse::<i64>()
                .unwrap();
            assert!(
                first >= at.timestamp_millis(),
                "request issued before durable retry deadline"
            );
        }
        if scenario == "routing" {
            assert_ne!(
                store.active_id(&conversation()).await.unwrap().as_str(),
                "old-session"
            );
            assert_eq!(service.count("routing"), 2);
        }
        if scenario == "stream" {
            let view = store.run_view(&before[0].id).await.unwrap();
            assert_eq!(view.live.interrupted.as_ref().unwrap().text, "old partial");
            assert!(matches!(
                view.live.interrupted.as_ref().unwrap().status,
                super::run_view::PartialStatus::Interrupted
            ));
            assert!(
                !view.session.messages().iter().any(
                    |m| matches!(m,Message::Assistant(a) if a.content.contains("old partial"))
                )
            );
        }
        if scenario == "cancel" {
            assert_eq!(report.cancelled.len(), 1);
        }
        if matches!(scenario, "final" | "delivery") {
            for id in store
                .pending_deliveries(conversation().as_str())
                .await
                .unwrap()
            {
                store
                    .deliver_receipt(
                        &Chat {
                            url: service.url.clone(),
                            dir: dir.path().into(),
                            block: false,
                        },
                        &id,
                        chrono::Utc::now(),
                        None,
                    )
                    .await
                    .unwrap();
            }
            assert_eq!(service.count("send"), 1, "{scenario}");
            let submission = store
                .submission(conversation().as_str(), "stable-fixture-request")
                .await
                .unwrap()
                .unwrap();
            let view = store
                .run_view(submission.run_id.as_deref().unwrap())
                .await
                .unwrap();
            assert_eq!(
                view.deliveries[0]["status"],
                if scenario == "delivery" {
                    "Uncertain"
                } else {
                    "Sent"
                }
            );
        }
        if matches!(scenario, "unsafe" | "revoked" | "effect") {
            let saved = store.load(&conversation()).await.unwrap().unwrap();
            assert!(
                saved.messages().iter().any(
                    |m| matches!(m,Message::ToolResult(r) if r.content.output["interrupted"]==true)
                ),
                "{scenario}"
            );
        }
    }
}
#[tokio::test]
async fn durable_process_unknown_commit_stops_then_rereads() {
    for point in ["before_result", "after_result"] {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::start(dir.path()).await;
        let mut process = child(dir.path(), &service.url, "fault", point, true);
        wait_ready(&mut process, dir.path()).await;
        assert!(process.0.wait().unwrap().success());
        assert_eq!(service.count("effect"), 1);
        assert_eq!(service.count("model"), 1);
        let store = SqliteSessionStore::open_async(dir.path().join("sessions"))
            .await
            .unwrap();
        let report = runner(
            store.clone(),
            &service.url,
            dir.path(),
            "",
            ReplayPolicy::Never,
        )
        .recover(RecoveryTarget::Conversation(conversation()))
        .await
        .unwrap();
        assert_eq!(report.completed.len(), 1, "{:?}", report.blocked);
        assert_eq!(service.count("effect"), 1);
        assert_eq!(service.count("model"), 2);
        let saved = store.load(&conversation()).await.unwrap().unwrap();
        let result = saved
            .messages()
            .iter()
            .find_map(|m| {
                if let Message::ToolResult(r) = m {
                    Some(&r.content.output)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(result["interrupted"] == true, point == "before_result");
        assert_eq!(result["external_effect"] == true, point == "after_result");
    }
}

#[tokio::test]
async fn durable_storage_readonly_and_full_leave_confirmed_checkpoint() {
    for full in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap();
        let mut session = Session::new(conversation());
        let mut cp = super::run_state::Checkpoint::new(&store, &conversation(), &session);
        cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
        store
            .with_connection(move |db| {
                if full {
                    let pages: i64 = db.pragma_query_value(None, "page_count", |r| r.get(0))?;
                    db.pragma_update(None, "max_page_count", pages)?;
                } else {
                    db.pragma_update(None, "query_only", true)?;
                }
                Ok(())
            })
            .await
            .unwrap();
        session.push(Message::User(UserMessage {
            content: "x".repeat(1_000_000),
            files: vec![],
            images: vec![],
        }));
        let error = cp
            .save(&session, RunPhase::NeedModel, None)
            .await
            .unwrap_err();
        let sqlite = error.downcast_ref::<rusqlite::Error>().unwrap();
        assert_eq!(
            sqlite.sqlite_error_code().unwrap(),
            if full {
                rusqlite::ErrorCode::DiskFull
            } else {
                rusqlite::ErrorCode::ReadOnly
            }
        );
        assert!(cp.storage_uncertain);
        assert!(cp.save(&session, RunPhase::NeedModel, None).await.is_err());
        let (saved, history) = store.load_run(&cp.record.id).await.unwrap().unwrap();
        assert_eq!(saved.revision, 1);
        assert!(history.messages().is_empty());
    }
}

#[tokio::test]
async fn durable_process_ready_compaction_preserves_new_suffix() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::start(dir.path()).await;
    let mut process = child(
        dir.path(),
        &service.url,
        "compaction",
        "compaction_ready",
        false,
    );
    wait_ready(&mut process, dir.path()).await;
    process.0.kill().unwrap();
    process.0.wait().unwrap();
    let store = SqliteSessionStore::open_async(dir.path().join("sessions"))
        .await
        .unwrap();
    let record = store.unfinished_runs().await.unwrap().remove(0);
    let (_, mut session) = store.load_run(&record.id).await.unwrap().unwrap();
    session.push(Message::User(UserMessage {
        content: "new suffix".into(),
        files: vec![],
        images: vec![],
    }));
    let mut cp = super::run_state::Checkpoint::new(&store, &conversation(), &session);
    cp.record = record;
    cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
    let compactor = super::compact::Compactor {
        llm: Arc::new(Model {
            url: service.url.clone(),
            dir: dir.path().into(),
            scenario: "".into(),
            store,
        }),
        threshold: 80000,
    };
    compactor
        .force_durable(
            &mut session,
            &ContextBuilder::new("fixture rules"),
            &[],
            &mut cp,
        )
        .await
        .unwrap();
    assert_eq!(service.count("summary"), 1);
    assert!(session.compaction().is_some());
    assert!(matches!(session.messages().last(),Some(Message::User(u)) if u.content=="new suffix"));
}

#[tokio::test]
#[ignore = "manual checkpoint latency/RSS/cancellation measurement, not a timing assertion"]
async fn durable_checkpoint_benchmark() {
    struct Pending(tokio::sync::Notify);
    #[async_trait::async_trait]
    impl LlmProvider for Pending {
        async fn generate(
            &self,
            _: &LlmContext<'_>,
            _: &[&dyn Tool],
            _: Option<&tokio::sync::mpsc::Sender<AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            self.0.notify_one();
            std::future::pending().await
        }
    }
    fn peak_rss() -> u64 {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        unsafe {
            assert_eq!(libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()), 0);
            let rss = usage.assume_init().ru_maxrss as u64;
            if cfg!(target_os = "macos") {
                rss
            } else {
                rss * 1024
            }
        }
    }
    for (name, messages, image_bytes, iterations) in
        [("small", 8, 0, 100), ("images", 16, 262144, 40)]
    {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap();
        let mut session = Session::new(conversation());
        for _ in 0..messages {
            session.push(Message::User(UserMessage {
                content: "x".repeat(512),
                files: vec![],
                images: if image_bytes == 0 {
                    vec![]
                } else {
                    vec![super::Image {
                        data_url: format!("data:image/png;base64,{}", "A".repeat(image_bytes)),
                        width: 256,
                        height: 256,
                    }]
                },
            }));
        }
        let payload_bytes = serde_json::to_vec(&session).unwrap().len();
        let mut cp = super::run_state::Checkpoint::new(&store, &conversation(), &session);
        let mut samples = vec![];
        let mut serialization = vec![];
        let mut max_wal = 0;
        for _ in 0..iterations {
            let start = std::time::Instant::now();
            let bytes = serde_json::to_vec(&session).unwrap();
            serialization.push(start.elapsed().as_secs_f64() * 1000.0);
            std::hint::black_box(bytes);
            let start = std::time::Instant::now();
            cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
            max_wal = max_wal.max(
                std::fs::metadata(dir.path().join("state.sqlite3-wal")).map_or(0, |m| m.len()),
            );
        }
        cp.save(&session, RunPhase::Completed, None).await.unwrap();
        let db_bytes = std::fs::metadata(dir.path().join("state.sqlite3"))
            .unwrap()
            .len();
        samples.sort_by(f64::total_cmp);
        serialization.sort_by(f64::total_cmp);
        let model = Arc::new(Pending(Default::default()));
        let runner = Arc::new(AgentRunner::new(
            Arc::new(Agent::new(
                model.clone(),
                ToolRegistry::default(),
                ContextBuilder::new("rules"),
            )),
            Arc::new(store.clone()),
        ));
        let (cancel, rx) = tokio::sync::oneshot::channel();
        let worker = runner.clone();
        let task = tokio::spawn(async move {
            worker
                .run_with_cancel(conversation(), input(), None, Some(rx), None)
                .await
        });
        model.0.notified().await;
        let start = std::time::Instant::now();
        cancel.send(()).unwrap();
        assert!(task.await.unwrap().is_err());
        let cancel_ms = start.elapsed().as_secs_f64() * 1000.0;
        println!(
            "CHECKPOINT_BENCH {}",
            serde_json::json!({"workload":name,"iterations":iterations,"messages":messages,"synthetic_image_base64_bytes_each":image_bytes,"payload_bytes":payload_bytes,"commit_p50_ms":samples[iterations/2],"commit_p95_ms":samples[(iterations*95/100).min(iterations-1)],"serialization_p50_ms":serialization[iterations/2],"db_bytes":db_bytes,"max_wal_bytes":max_wal,"process_peak_rss_bytes":peak_rss(),"cancel_to_terminal_ms":cancel_ms})
        );
    }
}

#[tokio::test]
async fn durable_process_wakeup_terminal_before_ready_reuses_saved_answer() {
    use super::reminders::{Recipient, ReminderStore};
    let dir = tempfile::tempdir().unwrap();
    let service = Service::start(dir.path()).await;
    let mut process = child(dir.path(), &service.url, "wakeup", "after_final", false);
    wait_ready(&mut process, dir.path()).await;
    process.0.kill().unwrap();
    process.0.wait().unwrap();
    assert_eq!(service.count("model"), 1);
    let journal = ReminderStore::wakeups(dir.path().join("journal"));
    journal.recover().unwrap();
    let task = journal.due(chrono::Utc::now()).unwrap().remove(0);
    let store = SqliteSessionStore::open_async(dir.path().join("sessions"))
        .await
        .unwrap();
    let run_id = store
        .submission(
            &format!("companion:wakeup:{}", task.id),
            &format!("wakeup:{}", task.id),
        )
        .await
        .unwrap()
        .unwrap()
        .run_id
        .unwrap();
    let runner = runner(
        store.clone(),
        &service.url,
        dir.path(),
        "",
        ReplayPolicy::Never,
    );
    runner
        .execute_wakeup(
            &Chat {
                url: service.url.clone(),
                dir: dir.path().into(),
                block: false,
            },
            &journal,
            &task,
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    assert_eq!(service.count("model"), 1);
    let same = store
        .submission(
            &format!("companion:wakeup:{}", task.id),
            &format!("wakeup:{}", task.id),
        )
        .await
        .unwrap()
        .unwrap()
        .run_id
        .unwrap();
    assert_eq!(same, run_id);
    let records = journal
        .execute_wakeup(
            &Recipient {
                interface: "telegram:fixture".into(),
                chat: "42".into(),
            },
            "source",
            serde_json::from_value(serde_json::json!({"action":"list"})).unwrap(),
            chrono::Utc::now(),
        )
        .unwrap();
    assert_eq!(records["wakeups"][0]["status"], "ready");
    assert_eq!(records["wakeups"][0]["message"], "wakeup saved");
}
