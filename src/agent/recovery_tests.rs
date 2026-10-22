use super::recovery::RecoveryTarget;
use super::{
    message::{Message, ToolCall},
    run_state::{Checkpoint, RunPhase, ToolExecution},
    session::{Session, SessionId, SessionStore, SqliteSessionStore},
    tools::{ReplayPolicy, Tool, ToolOutput, ToolRegistry},
    *,
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

struct Effect {
    policy: ReplayPolicy,
    available: bool,
    calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl Tool for Effect {
    fn name(&self) -> String {
        "effect".into()
    }
    fn description(&self) -> String {
        "effect".into()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    fn replay_policy(&self) -> ReplayPolicy {
        self.policy
    }
    fn available(&self) -> bool {
        self.available
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(serde_json::json!({"effect":true}).into())
    }
}
struct Model(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl llm::LlmProvider for Model {
    async fn generate(
        &self,
        _: &LlmContext<'_>,
        _: &[&dyn Tool],
        _: Option<&tokio::sync::mpsc::Sender<AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok((
            vec![Message::Assistant(AssistantMessage {
                content: "recovered".into(),
            })],
            None,
        ))
    }
}
async fn seed(dir: PathBuf, phase: RunPhase, policy: ReplayPolicy) -> String {
    let store = SqliteSessionStore::open_async(dir).await.unwrap();
    let mut session = Session::new(SessionId::new("cli:main"));
    session.initialize_system_prompt("test");
    session.push(Message::User(UserMessage {
        content: "original".into(),
        files: vec![],
        images: vec![],
    }));
    let mut cp = Checkpoint::new(&store, session.id(), &session);
    cp.record.isolated = Some(false);
    cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
    if matches!(
        phase,
        RunPhase::NeedTool { .. } | RunPhase::ExecutingTool { .. }
    ) {
        let call = ToolCall {
            call_id: "call".into(),
            name: "effect".into(),
            arguments: serde_json::json!({"n":1}),
        };
        session.push(Message::ToolCall(call.clone()));
        cp.record.pending_calls = vec![call.clone()];
        cp.save(
            &session,
            phase.clone(),
            if matches!(phase, RunPhase::ExecutingTool { .. }) {
                Some(ToolExecution {
                    call,
                    replay: policy,
                    result: None,
                })
            } else {
                None
            },
        )
        .await
        .unwrap();
    } else if phase.terminal() {
        cp.save(&session, phase, None).await.unwrap();
    }
    cp.record.id
}
async fn runner(
    dir: PathBuf,
    policy: Option<ReplayPolicy>,
    available: bool,
) -> (AgentRunner, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let effects = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::default();
    if let Some(policy) = policy {
        tools = tools.register(Arc::new(Effect {
            policy,
            available,
            calls: effects.clone(),
        }));
    }
    let store = Arc::new(SqliteSessionStore::open_async(dir).await.unwrap());
    (
        AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(Model(models.clone())),
                tools,
                ContextBuilder::new("test"),
            )),
            store,
        ),
        effects,
        models,
    )
}
#[tokio::test]
async fn recovery_reopen_checks_both_policies_and_availability() {
    for (saved, current, available, replays) in [
        (ReplayPolicy::Safe, Some(ReplayPolicy::Safe), true, 1),
        (ReplayPolicy::Safe, Some(ReplayPolicy::Never), true, 0),
        (ReplayPolicy::Never, Some(ReplayPolicy::Safe), true, 0),
        (ReplayPolicy::Safe, None, true, 0),
        (ReplayPolicy::Safe, Some(ReplayPolicy::Safe), false, 0),
    ] {
        let dir = tempfile::tempdir().unwrap();
        seed(
            dir.path().into(),
            RunPhase::ExecutingTool {
                call_id: "call".into(),
                replay: saved,
            },
            saved,
        )
        .await;
        let (r, e, m) = runner(dir.path().into(), current, available).await;
        let report = r
            .recover(RecoveryTarget::Conversation(SessionId::new("cli:main")))
            .await
            .unwrap();
        assert_eq!(report.completed.len(), 1);
        assert_eq!(e.load(Ordering::SeqCst), replays);
        assert_eq!(m.load(Ordering::SeqCst), 1);
        let store = SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap();
        let s = store
            .load(&SessionId::new("cli:main"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            s.messages()
                .iter()
                .filter(|m| matches!(m, Message::User(_)))
                .count(),
            1
        );
        let Message::ToolResult(result) = &s.messages()[2] else {
            panic!("missing result")
        };
        if replays == 0 {
            assert_eq!(result.content.output["interrupted"], true);
        } else {
            assert_eq!(result.content.output["effect"], true);
        }
    }
}
#[tokio::test]
async fn recovery_need_model_need_tool_and_terminal_phases() {
    for (phase, effects, models) in [
        (RunPhase::NeedModel, 0, 1),
        (
            RunPhase::NeedTool {
                call_id: "call".into(),
            },
            1,
            1,
        ),
        (RunPhase::Completed, 0, 0),
        (RunPhase::Cancelled, 0, 0),
    ] {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path().into(), phase, ReplayPolicy::Never).await;
        let (r, e, m) = runner(dir.path().into(), Some(ReplayPolicy::Never), true).await;
        r.recover(RecoveryTarget::Conversation(SessionId::new("cli:main")))
            .await
            .unwrap();
        assert_eq!(e.load(Ordering::SeqCst), effects);
        assert_eq!(m.load(Ordering::SeqCst), models);
    }
}
#[tokio::test]
async fn recovery_cancellation_survives_reopen_before_any_effect() {
    let dir = tempfile::tempdir().unwrap();
    let id = seed(
        dir.path().into(),
        RunPhase::ExecutingTool {
            call_id: "call".into(),
            replay: ReplayPolicy::Safe,
        },
        ReplayPolicy::Safe,
    )
    .await;
    let (r, _, _) = runner(dir.path().into(), Some(ReplayPolicy::Safe), true).await;
    r.cancel_run(&id).await.unwrap();
    drop(r);
    let (r, e, m) = runner(dir.path().into(), Some(ReplayPolicy::Safe), true).await;
    let report = r
        .recover(RecoveryTarget::Conversation(SessionId::new("cli:main")))
        .await
        .unwrap();
    assert!(report.cancelled.is_empty()); // Idle cancellation now commits immediately under ownership.
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    assert!(matches!(
        store.load_run(&id).await.unwrap().unwrap().0.phase,
        RunPhase::Cancelled
    ));
    assert_eq!(e.load(Ordering::SeqCst), 0);
    assert_eq!(m.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn recovery_respects_client_target_provenance_and_bound_session() {
    let dir = tempfile::tempdir().unwrap();
    let id = seed(dir.path().into(), RunPhase::NeedModel, ReplayPolicy::Never).await;
    let (r, _, m) = runner(dir.path().into(), None, true).await;
    let report = r.recover(RecoveryTarget::OneShots).await.unwrap();
    assert!(report.completed.is_empty());
    assert_eq!(m.load(Ordering::SeqCst), 0);
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    let new = store
        .new_session(&SessionId::new("cli:main"))
        .await
        .unwrap();
    let report = r
        .recover(RecoveryTarget::Conversation(SessionId::new("cli:main")))
        .await
        .unwrap();
    assert_eq!(report.completed[0].0, id);
    assert!(
        store
            .load(new.id())
            .await
            .unwrap()
            .unwrap()
            .messages()
            .is_empty()
    );
    assert_eq!(
        store.active_id(&SessionId::new("cli:main")).await.unwrap(),
        *new.id()
    );
}

struct CrashTool {
    dir: PathBuf,
    block: bool,
}
#[async_trait::async_trait]
impl Tool for CrashTool {
    fn name(&self) -> String {
        "effect".into()
    }
    fn description(&self) -> String {
        "effect".into()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolOutput> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("effects"))
            .unwrap();
        writeln!(f, "effect").unwrap();
        f.sync_all().unwrap();
        if self.block {
            std::fs::write(self.dir.join("ready"), "intent").unwrap();
            std::future::pending::<()>().await;
        }
        Ok(serde_json::json!({"effect":true}).into())
    }
}
struct CrashModel {
    dir: PathBuf,
    result_gate: bool,
}
#[async_trait::async_trait]
impl llm::LlmProvider for CrashModel {
    async fn generate(
        &self,
        ctx: &LlmContext<'_>,
        _: &[&dyn Tool],
        _: Option<&tokio::sync::mpsc::Sender<AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        if ctx.messages.len() == 1 {
            Ok((
                vec![Message::ToolCall(ToolCall {
                    call_id: "call".into(),
                    name: "effect".into(),
                    arguments: serde_json::json!({}),
                })],
                None,
            ))
        } else {
            if self.result_gate {
                std::fs::write(self.dir.join("ready"), "result").unwrap();
                std::future::pending::<()>().await;
            }
            Ok((
                vec![Message::Assistant(AssistantMessage {
                    content: "done".into(),
                })],
                None,
            ))
        }
    }
}
struct SandboxCrash {
    inner: Arc<dyn Tool>,
    dir: PathBuf,
    block: bool,
}
#[async_trait::async_trait]
impl Tool for SandboxCrash {
    fn name(&self) -> String {
        "effect".into()
    }
    fn description(&self) -> String {
        "sandbox mutation".into()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let output=self.inner.execute(serde_json::json!({"cmd":"echo effect >> /workspace/count", "workdir":null,"timeout_ms":30000})).await?;
        if self.block {
            std::fs::write(self.dir.join("ready"), "sandbox saved")?;
            std::future::pending::<()>().await;
        }
        Ok(output)
    }
}
#[tokio::test]
async fn recovery_child_worker() {
    let Ok(dir) = std::env::var("KODRIK_RECOVERY_FIXTURE") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let mode = std::env::var("KODRIK_RECOVERY_MODE").unwrap();
    let store = Arc::new(
        SqliteSessionStore::open_async(dir.join("sessions"))
            .await
            .unwrap(),
    );
    if mode == "crash-loop" {
        struct Crash(PathBuf);
        #[async_trait::async_trait]
        impl llm::LlmProvider for Crash {
            async fn generate(
                &self,
                _: &LlmContext<'_>,
                _: &[&dyn Tool],
                _: Option<&tokio::sync::mpsc::Sender<AgentEvent>>,
            ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
                std::fs::write(self.0.join("ready"), "active").unwrap();
                std::future::pending().await
            }
        }
        let r = AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(Crash(dir.clone())),
                ToolRegistry::default(),
                ContextBuilder::new("test"),
            )),
            store.clone(),
        );
        if store.unfinished_runs().await.unwrap().is_empty() {
            r.run(
                SessionId::new("cli:main"),
                crate::gateway::IncomingMessage::new(
                    crate::gateway::InterfaceId("cli".into()),
                    "main",
                    "work",
                ),
                None,
            )
            .await
            .unwrap();
        } else {
            r.recover(RecoveryTarget::Conversation(SessionId::new("cli:main")))
                .await
                .unwrap();
        }
        return;
    }
    let sandbox = Arc::new(super::sandbox::Sandbox::new(dir.clone()));
    let tool: Arc<dyn Tool> = if mode == "sandbox" {
        Arc::new(SandboxCrash {
            inner: sandbox.tool("exec_command"),
            dir: dir.clone(),
            block: true,
        })
    } else {
        Arc::new(CrashTool {
            dir: dir.clone(),
            block: mode == "intent",
        })
    };
    let mut agent = Agent::new(
        Arc::new(CrashModel {
            dir: dir.clone(),
            result_gate: mode == "result",
        }),
        ToolRegistry::default().register(tool),
        ContextBuilder::new("test"),
    );
    if mode == "sandbox" {
        agent = agent.with_sandbox(sandbox);
    }
    let r = AgentRunner::new(Arc::new(agent), store);
    if mode == "claim" {
        let report = r
            .recover(RecoveryTarget::Conversation(SessionId::new("cli:main")))
            .await
            .unwrap();
        assert!(report.completed.is_empty());
        assert!(!report.blocked.is_empty());
        return;
    }
    r.run(
        SessionId::new("cli:main"),
        crate::gateway::IncomingMessage::new(
            crate::gateway::InterfaceId("cli".into()),
            "main",
            "work",
        ),
        None,
    )
    .await
    .unwrap();
}
fn child(dir: &std::path::Path, mode: &str) -> std::process::Child {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "agent::recovery_tests::recovery_child_worker",
            "--nocapture",
        ])
        .env("KODRIK_RECOVERY_FIXTURE", dir)
        .env("KODRIK_RECOVERY_MODE", mode)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap()
}
#[tokio::test]
async fn recovery_real_process_kill_preserves_intent_result_and_excludes_second_worker() {
    for mode in ["intent", "result"] {
        let dir = tempfile::tempdir().unwrap();
        let mut process = child(dir.path(), mode);
        let ready = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while !dir.path().join("ready").exists() {
                assert!(
                    process.try_wait().unwrap().is_none(),
                    "child exited before boundary"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;
        if ready.is_err() {
            process.kill().unwrap();
            process.wait().unwrap();
            panic!("child did not reach boundary");
        }
        assert!(child(dir.path(), "claim").wait().unwrap().success());
        process.kill().unwrap();
        process.wait().unwrap();
        let store = Arc::new(
            SqliteSessionStore::open_async(dir.path().join("sessions"))
                .await
                .unwrap(),
        );
        let r = AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(CrashModel {
                    dir: dir.path().into(),
                    result_gate: false,
                }),
                ToolRegistry::default().register(Arc::new(CrashTool {
                    dir: dir.path().into(),
                    block: false,
                })),
                ContextBuilder::new("test"),
            )),
            store,
        );
        let report = r
            .recover(RecoveryTarget::Conversation(SessionId::new("cli:main")))
            .await
            .unwrap();
        assert_eq!(report.completed.len(), 1, "{:?}", report.blocked);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("effects")).unwrap(),
            "effect\n"
        );
    }
}

struct WaitingModel(tokio::sync::mpsc::UnboundedSender<()>);
#[async_trait::async_trait]
impl llm::LlmProvider for WaitingModel {
    async fn generate(
        &self,
        _: &LlmContext<'_>,
        _: &[&dyn Tool],
        _: Option<&tokio::sync::mpsc::Sender<AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        let _ = self.0.send(());
        std::future::pending().await
    }
}
#[tokio::test]
async fn cancellation_and_shutdown_have_distinct_durable_outcomes() {
    for user_stop in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SqliteSessionStore::open_async(dir.path().into())
                .await
                .unwrap(),
        );
        let (tx, mut started) = tokio::sync::mpsc::unbounded_channel();
        let r = Arc::new(AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(WaitingModel(tx)),
                ToolRegistry::default(),
                ContextBuilder::new("test"),
            )),
            store.clone(),
        ));
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        let worker = r.clone();
        let task = tokio::spawn(async move {
            worker
                .run_with_cancel(
                    SessionId::new("cli:main"),
                    crate::gateway::IncomingMessage::new(
                        crate::gateway::InterfaceId("cli".into()),
                        "main",
                        "input",
                    ),
                    None,
                    Some(cancelled),
                    None,
                )
                .await
        });
        started.recv().await.unwrap();
        let id = store.unfinished_runs().await.unwrap()[0].id.clone();
        if user_stop {
            cancel.send(()).unwrap();
        } else {
            r.suspend();
        }
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        let (record, _) = store.load_run(&id).await.unwrap().unwrap();
        assert_eq!(record.cancel_requested, user_stop);
        assert_eq!(matches!(record.phase, RunPhase::Cancelled), user_stop);
        drop(r);
        drop(store);
        let (r, _, models) = runner(dir.path().into(), None, true).await;
        r.recover(RecoveryTarget::Conversation(SessionId::new("cli:main")))
            .await
            .unwrap();
        assert_eq!(models.load(Ordering::SeqCst), usize::from(!user_stop));
    }
}
#[tokio::test]
async fn recovery_blocks_missing_provenance_and_changed_permissions() {
    for isolated in [None, Some(true)] {
        let dir = tempfile::tempdir().unwrap();
        let id = seed(dir.path().into(), RunPhase::NeedModel, ReplayPolicy::Never).await;
        let store = SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap();
        let (mut record, session) = store.load_run(&id).await.unwrap().unwrap();
        let old = record.revision;
        record.revision += 1;
        record.isolated = isolated;
        store
            .commit_step(old, &session, &record, None)
            .await
            .unwrap();
        drop(store);
        let (r, e, m) = runner(dir.path().into(), Some(ReplayPolicy::Never), true).await;
        let report = r
            .recover(RecoveryTarget::Conversation(SessionId::new("cli:main")))
            .await
            .unwrap();
        assert_eq!(report.blocked.len(), 1);
        assert_eq!(e.load(Ordering::SeqCst), 0);
        assert_eq!(m.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn recovery_cancellation_can_finalize_legacy_scope_without_execution() {
    let dir = tempfile::tempdir().unwrap();
    let id = seed(dir.path().into(), RunPhase::NeedModel, ReplayPolicy::Never).await;
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    let (mut record, session) = store.load_run(&id).await.unwrap().unwrap();
    let old = record.revision;
    record.revision += 1;
    record.isolated = None;
    store
        .commit_step(old, &session, &record, None)
        .await
        .unwrap();
    store.request_cancel(&id).await.unwrap();
    drop(store);
    let (r, _, m) = runner(dir.path().into(), None, true).await;
    let report = r
        .recover(RecoveryTarget::Conversation(SessionId::new("cli:main")))
        .await
        .unwrap();
    assert_eq!(report.cancelled, vec![id]);
    assert_eq!(m.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn recovery_killed_sandbox_mutation_is_never_replayed() {
    let dir = tempfile::tempdir().unwrap();
    let mut process = child(dir.path(), "sandbox");
    let ready = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while !dir.path().join("ready").exists() {
            assert!(process.try_wait().unwrap().is_none());
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    if ready.is_err() {
        process.kill().unwrap();
        process.wait().unwrap();
        panic!("sandbox child timeout");
    }
    process.kill().unwrap();
    process.wait().unwrap();
    let sandbox = Arc::new(super::sandbox::Sandbox::new(dir.path().into()));
    let store = Arc::new(
        SqliteSessionStore::open_async(dir.path().join("sessions"))
            .await
            .unwrap(),
    );
    let r = AgentRunner::new(
        Arc::new(
            Agent::new(
                Arc::new(Model(Arc::new(AtomicUsize::new(0)))),
                ToolRegistry::default().register(Arc::new(SandboxCrash {
                    inner: sandbox.tool("exec_command"),
                    dir: dir.path().into(),
                    block: false,
                })),
                ContextBuilder::new("test"),
            )
            .with_sandbox(sandbox.clone()),
        ),
        store,
    );
    let report = r
        .recover(RecoveryTarget::Conversation(SessionId::new("cli:main")))
        .await
        .unwrap();
    assert_eq!(report.completed.len(), 1, "{:?}", report.blocked);
    let output = sandbox
        .tool("exec_command")
        .execute(
            serde_json::json!({"cmd":"cat /workspace/count","workdir":null,"timeout_ms":30000}),
        )
        .await
        .unwrap();
    assert_eq!(output.output["stdout"], "effect\n");
}

struct LockSandboxModel {
    path: PathBuf,
    lock: Arc<std::sync::Mutex<Option<std::fs::File>>>,
}
#[async_trait::async_trait]
impl llm::LlmProvider for LockSandboxModel {
    async fn generate(
        &self,
        _: &LlmContext<'_>,
        _: &[&dyn Tool],
        _: Option<&tokio::sync::mpsc::Sender<AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)?;
        file.lock()?;
        *self.lock.lock().unwrap() = Some(file);
        Ok((
            vec![Message::ToolCall(ToolCall {
                call_id: "blocked".into(),
                name: "exec_command".into(),
                arguments: serde_json::json!({"cmd":"echo effect", "workdir":null,"timeout_ms":30000}),
            })],
            None,
        ))
    }
}
#[tokio::test]
async fn recovery_suspended_sandbox_worker_keeps_execution_ownership() {
    let dir = tempfile::tempdir().unwrap();
    let sandbox = Arc::new(super::sandbox::Sandbox::new(dir.path().into()));
    let store = Arc::new(
        SqliteSessionStore::open_async(dir.path().join("sessions"))
            .await
            .unwrap(),
    );
    let lock = Arc::new(std::sync::Mutex::new(None));
    let r = Arc::new(AgentRunner::new(
        Arc::new(
            Agent::new(
                Arc::new(LockSandboxModel {
                    path: dir.path().join("sandbox.lock"),
                    lock: lock.clone(),
                }),
                ToolRegistry::default().register(sandbox.tool("exec_command")),
                ContextBuilder::new("test"),
            )
            .with_sandbox(sandbox),
        ),
        store.clone(),
    ));
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let worker = r.clone();
    let task = tokio::spawn(async move {
        worker
            .run(
                SessionId::new("cli:main"),
                crate::gateway::IncomingMessage::new(
                    crate::gateway::InterfaceId("cli".into()),
                    "main",
                    "work",
                ),
                Some(tx),
            )
            .await
    });
    while let Some(event) = rx.recv().await {
        if matches!(event, AgentEvent::ToolStarted { .. }) {
            break;
        }
    }
    r.suspend();
    let result = task.await.unwrap();
    assert!(result.is_err());
    let held = store.claim_execution("cli:main", "cli:main").await.is_err();
    // Always unblock the detached blocking worker, including when the assertion fails.
    lock.lock().unwrap().take();
    assert!(
        held,
        "execution lock was released while sandbox blocking worker still ran"
    );
}

#[tokio::test]
async fn recovery_conversation_writers_respect_execution_ownership() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path().into(), RunPhase::NeedModel, ReplayPolicy::Never).await;
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    let _owner = store.claim_execution("cli:main", "cli:main").await.unwrap();
    let (r, _, _) = runner(dir.path().into(), None, true).await;
    assert!(
        r.observe_group_messages(
            &SessionId::new("cli:main"),
            vec![crate::gateway::IncomingMessage::new(
                crate::gateway::InterfaceId("cli".into()),
                "main",
                "another writer"
            )]
        )
        .await
        .is_err()
    );
    assert_eq!(
        store
            .load(&SessionId::new("cli:main"))
            .await
            .unwrap()
            .unwrap()
            .messages()
            .len(),
        1
    );
}

#[tokio::test]
async fn recovery_scan_waits_for_short_store_lock() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    let scan = store.execution_lock("recovery", "scan").await.unwrap();
    let other = store.clone();
    let mut waiting = tokio::spawn(async move { other.execution_lock("recovery", "scan").await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut waiting)
            .await
            .is_err()
    );
    drop(scan);
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap()
            .is_ok()
    );
}

#[tokio::test]
async fn recovery_detached_memory_worker_keeps_ownership() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open_async(dir.path().join("sessions"))
        .await
        .unwrap();
    let ownership = store.claim_execution("cli:main", "cli:main").await.unwrap();
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.path().join("USER.md.lock"))
        .unwrap();
    lock.lock().unwrap();
    let tool = super::tools::MemoryTool::new(dir.path());
    let task = tokio::spawn(super::run_state::CURRENT_EXECUTION.scope(
        Some(ownership),
        async move {
            tool.execute(serde_json::json!({"action":"add","target":"user","content":"fact"}))
                .await
        },
    ));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    task.abort();
    let _ = task.await;
    let held = store.claim_execution("cli:main", "cli:main").await.is_err();
    drop(lock);
    assert!(
        held,
        "memory worker released execution ownership before mutation ended"
    );
}

#[tokio::test(start_paused = true)]
async fn durable_retry_wait_and_budget_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap(),
    );
    let mut session = Session::new(SessionId::new("cli:main"));
    session.initialize_system_prompt("test");
    let mut cp = Checkpoint::new(&store, session.id(), &session);
    cp.record.isolated = Some(false);
    cp.record.model_attempts = 15;
    cp.record.model_limit = Some(16);
    cp.record.next_retry_at = Some(chrono::Utc::now() + chrono::Duration::seconds(30));
    cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
    let id = cp.record.id.clone();
    drop(cp);
    let calls = Arc::new(AtomicUsize::new(0));
    let runner = AgentRunner::new(
        Arc::new(Agent::new(
            Arc::new(Model(calls.clone())),
            ToolRegistry::default(),
            ContextBuilder::new("test"),
        )),
        store.clone(),
    );
    let started = tokio::time::Instant::now();
    runner
        .recover(RecoveryTarget::Conversation(SessionId::new("cli:main")))
        .await
        .unwrap();
    assert!(started.elapsed() >= std::time::Duration::from_secs(29));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        store.load_run(&id).await.unwrap().unwrap().0.model_attempts,
        16
    );
}
#[tokio::test]
async fn durable_expired_deadline_and_exhausted_budget_do_not_generate() {
    for expired in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SqliteSessionStore::open_async(dir.path().into())
                .await
                .unwrap(),
        );
        let session = Session::new(SessionId::new("cli:main"));
        let mut cp = Checkpoint::new(&store, session.id(), &session);
        cp.record.isolated = Some(false);
        cp.record.model_limit = Some(16);
        if expired {
            cp.record.deadline_at = Some(chrono::Utc::now() - chrono::Duration::seconds(1));
        } else {
            cp.record.model_attempts = 16;
        }
        cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
        let id = cp.record.id.clone();
        drop(cp);
        let calls = Arc::new(AtomicUsize::new(0));
        let runner = AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(Model(calls.clone())),
                ToolRegistry::default(),
                ContextBuilder::new("test"),
            )),
            store.clone(),
        );
        runner
            .recover(RecoveryTarget::Conversation(SessionId::new("cli:main")))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(
            store
                .load_run(&id)
                .await
                .unwrap()
                .unwrap()
                .0
                .phase
                .terminal()
        );
    }
}

struct RetryingModel(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl llm::LlmProvider for RetryingModel {
    async fn generate(
        &self,
        _: &LlmContext<'_>,
        _: &[&dyn Tool],
        _: Option<&tokio::sync::mpsc::Sender<AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(llm::GenerationRetry {
            limit: 3,
            retry_after: Some(std::time::Duration::from_secs(10)),
            error: anyhow::anyhow!("rate limited"),
        }
        .into())
    }
}
#[tokio::test(start_paused = true)]
async fn durable_retry_counts_each_attempt_and_persists_backoff() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap(),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let runner = AgentRunner::new(
        Arc::new(Agent::new(
            Arc::new(RetryingModel(calls.clone())),
            ToolRegistry::default(),
            ContextBuilder::new("test"),
        )),
        store.clone(),
    );
    let started = tokio::time::Instant::now();
    let result = runner
        .run(
            SessionId::new("cli:main"),
            crate::gateway::IncomingMessage::new(
                crate::gateway::InterfaceId("cli".into()),
                "main",
                "test",
            ),
            None,
        )
        .await;
    assert!(result.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert!(started.elapsed() >= std::time::Duration::from_secs(29));
    let records = store
        .with_connection(|db| {
            let bytes: Vec<u8> = db.query_row("SELECT payload FROM runs", [], |r| r.get(0))?;
            Ok(serde_json::from_slice::<super::run_state::RunRecord>(
                &bytes,
            )?)
        })
        .await
        .unwrap();
    assert_eq!(records.model_attempts, 4);
    assert_eq!(records.model_retries, 3);
    assert!(records.phase.terminal());
}

#[tokio::test]
async fn recovery_parks_five_crashes_and_manual_retry_preserves_phase() {
    let dir = tempfile::tempdir().unwrap();
    let id = seed(dir.path().into(), RunPhase::NeedModel, ReplayPolicy::Never).await;
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    for n in 1..=5 {
        let (record, session) = store.load_run(&id).await.unwrap().unwrap();
        let lock = store
            .claim_execution(&record.conversation_id, &record.session_id)
            .await
            .unwrap();
        let mut cp = Checkpoint::resume(&store, record, lock);
        let allowed = cp.prepare_recovery(&session, 5).await.unwrap();
        assert_eq!(allowed, n < 5);
        assert_eq!(cp.record.recovery_attempts, n);
        // Drop without clean suspension simulates an exited worker's persisted marker.
    }
    let (r, _, models) = runner(dir.path().into(), None, false).await;
    r.recover(RecoveryTarget::Conversation(SessionId::new("cli:main")))
        .await
        .unwrap();
    assert_eq!(models.load(Ordering::SeqCst), 0);
    store.retry_parked(&id).await.unwrap();
    r.recover(RecoveryTarget::Conversation(SessionId::new("cli:main")))
        .await
        .unwrap();
    assert_eq!(models.load(Ordering::SeqCst), 1);
    assert!(store.retry_parked(&id).await.is_err());
}

#[tokio::test]
async fn recovery_clean_suspend_progress_and_zero_limit() {
    let dir = tempfile::tempdir().unwrap();
    let id = seed(dir.path().into(), RunPhase::NeedModel, ReplayPolicy::Never).await;
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    let (record, session) = store.load_run(&id).await.unwrap().unwrap();
    let lock = store
        .claim_execution(&record.conversation_id, &record.session_id)
        .await
        .unwrap();
    let mut cp = Checkpoint::resume(&store, record, lock);
    cp.clean_suspend().await.unwrap();
    assert!(cp.prepare_recovery(&session, 5).await.unwrap());
    cp.clean_suspend().await.unwrap();
    assert!(cp.prepare_recovery(&session, 5).await.unwrap());
    assert_eq!(cp.record.recovery_attempts, 0);
    assert!(cp.prepare_recovery(&session, 5).await.unwrap());
    assert_eq!(cp.record.recovery_attempts, 1);
    cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
    assert_eq!(cp.record.recovery_attempts, 1);
    cp.save(
        &session,
        RunPhase::NeedTool {
            call_id: "next".into(),
        },
        None,
    )
    .await
    .unwrap();
    assert_eq!(cp.record.recovery_attempts, 0);
    assert!(!cp.prepare_recovery(&session, 0).await.unwrap());
    assert!(matches!(cp.record.phase, RunPhase::Parked { .. }));
    drop(cp);
    store.retry_parked(&id).await.unwrap();
    let (record, session) = store.load_run(&id).await.unwrap().unwrap();
    let lock = store
        .claim_execution(&record.conversation_id, &record.session_id)
        .await
        .unwrap();
    let mut cp = Checkpoint::resume(&store, record, lock);
    assert!(cp.prepare_recovery(&session, 0).await.unwrap());
    assert!(matches!(cp.record.phase, RunPhase::NeedTool { .. }));
}

#[tokio::test]
async fn recovery_parked_cancel_is_terminal_and_busy_scan_does_not_count() {
    let dir = tempfile::tempdir().unwrap();
    let id = seed(dir.path().into(), RunPhase::NeedModel, ReplayPolicy::Never).await;
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    let lock = store.claim_execution("cli:main", "cli:main").await.unwrap();
    let (r, _, models) = runner(dir.path().into(), None, false).await;
    r.recover(RecoveryTarget::Conversation(SessionId::new("cli:main")))
        .await
        .unwrap();
    assert_eq!(
        store
            .load_run(&id)
            .await
            .unwrap()
            .unwrap()
            .0
            .recovery_attempts,
        0
    );
    drop(lock);
    let r = r.with_restart_limit(0);
    r.recover(RecoveryTarget::Conversation(SessionId::new("cli:main")))
        .await
        .unwrap();
    assert_eq!(models.load(Ordering::SeqCst), 0);
    let before = store.load_run(&id).await.unwrap().unwrap().0.revision;
    r.recover(RecoveryTarget::Conversation(SessionId::new("cli:main")))
        .await
        .unwrap();
    assert_eq!(
        store.load_run(&id).await.unwrap().unwrap().0.revision,
        before
    );
    store.cancel_run(&id).await.unwrap();
    assert!(matches!(
        store.load_run(&id).await.unwrap().unwrap().0.phase,
        RunPhase::Cancelled
    ));
    assert!(store.retry_parked(&id).await.is_err());
}

#[tokio::test]
async fn runs_diagnostics_redact_payload_and_block_future_schema() {
    let dir = tempfile::tempdir().unwrap();
    let id = seed(dir.path().into(), RunPhase::NeedModel, ReplayPolicy::Never).await;
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    let rows = store.run_diagnostics(Some(&id)).await.unwrap();
    let output = serde_json::to_string(&rows).unwrap();
    assert!(!output.contains("original"));
    assert!(output.contains("NeedModel"));
    store.with_connection({let id=id.clone();move|db|{db.execute("UPDATE runs SET payload=CAST(json_set(payload,'$.schema_version',999) AS BLOB) WHERE id=?1",[id])?;Ok(())}}).await.unwrap();
    let rows = store.run_diagnostics(Some(&id)).await.unwrap();
    assert_eq!(rows[0]["reason"], "unsupported_run_version");
    assert!(store.load_run(&id).await.is_err());
    assert!(store.retry_parked(&id).await.is_err());
}

#[tokio::test]
async fn recovery_five_real_process_crashes_park_without_sixth_attempt() {
    let dir = tempfile::tempdir().unwrap();
    for _ in 0..5 {
        let mut process = child(dir.path(), "crash-loop");
        let ready = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while !dir.path().join("ready").exists() {
                assert!(process.try_wait().unwrap().is_none());
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        process.kill().unwrap();
        process.wait().unwrap();
        assert!(ready.is_ok());
        std::fs::remove_file(dir.path().join("ready")).unwrap();
    }
    let (r, _, models) = runner(dir.path().join("sessions"), None, true).await;
    r.recover(RecoveryTarget::Conversation(SessionId::new("cli:main")))
        .await
        .unwrap();
    assert_eq!(models.load(Ordering::SeqCst), 0);
    let store = SqliteSessionStore::open_async(dir.path().join("sessions"))
        .await
        .unwrap();
    let run = store.unfinished_runs().await.unwrap().remove(0);
    assert_eq!(run.recovery_attempts, 5);
    assert!(matches!(run.phase, RunPhase::Parked { .. }));
    r.recover(RecoveryTarget::Conversation(SessionId::new("cli:main")))
        .await
        .unwrap();
    let audit = std::fs::read_dir(dir.path().join("sessions/recovery-audit"))
        .unwrap()
        .map(|p| std::fs::read_to_string(p.unwrap().path()).unwrap())
        .collect::<String>();
    assert_eq!(
        audit
            .lines()
            .filter(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["event"] == "park")
            .count(),
        1
    );
    assert!(!audit.contains("work"));
}

#[tokio::test]
async fn recovery_manual_retry_keeps_never_policy_and_legacy_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let id = seed(
        dir.path().into(),
        RunPhase::ExecutingTool {
            call_id: "call".into(),
            replay: ReplayPolicy::Never,
        },
        ReplayPolicy::Never,
    )
    .await;
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    // Version-1 fixture predating crash-loop accounting: no invented active attempt.
    store.with_connection({let id=id.clone();move|db|{db.execute("UPDATE runs SET payload=CAST(json_remove(payload,'$.attempt_active','$.recovery_attempts','$.manual_retry','$.interrupted_phase') AS BLOB) WHERE id=?1",[id])?;Ok(())}}).await.unwrap();
    let (r, e, m) = runner(dir.path().into(), Some(ReplayPolicy::Never), true).await;
    let r = r.with_restart_limit(0);
    r.recover(RecoveryTarget::Conversation(SessionId::new("cli:main")))
        .await
        .unwrap();
    assert_eq!(
        store
            .load_run(&id)
            .await
            .unwrap()
            .unwrap()
            .0
            .recovery_attempts,
        0
    );
    store.retry_parked(&id).await.unwrap();
    r.recover(RecoveryTarget::Conversation(SessionId::new("cli:main")))
        .await
        .unwrap();
    assert_eq!(e.load(Ordering::SeqCst), 0);
    assert_eq!(m.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn runs_show_pending_cancellation_before_worker_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let id = seed(dir.path().into(), RunPhase::NeedModel, ReplayPolicy::Never).await;
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    let _lock = store.claim_execution("cli:main", "cli:main").await.unwrap();
    store.cancel_run(&id).await.unwrap();
    let rows = store.run_diagnostics(Some(&id)).await.unwrap();
    assert_eq!(rows[0]["cancel_requested"], true);
}
