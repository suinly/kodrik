use crate::{
    Interface,
    agent::{
        AgentRunner,
        reminders::{ReminderStore, Status},
    },
    config::CompanionConfig,
};
use chrono::{DateTime, Utc};
use std::{sync::Arc, time::Duration};

pub(crate) struct Wakeups {
    store: ReminderStore,
    runner: Arc<AgentRunner>,
    config: CompanionConfig,
    access: Option<(crate::web::auth::AuthStore, u64)>,
}

impl Wakeups {
    pub(crate) fn new(
        store: ReminderStore,
        runner: Arc<AgentRunner>,
        config: CompanionConfig,
    ) -> Self {
        Self {
            store,
            runner,
            config,
            access: None,
        }
    }

    pub(crate) fn with_access(
        mut self,
        access: Option<(crate::web::auth::AuthStore, u64)>,
    ) -> Self {
        self.access = access;
        self
    }

    pub(crate) fn run(
        self,
        interfaces: Vec<Arc<dyn Interface>>,
    ) -> impl std::future::Future<Output = ()> {
        let mut shutdown = self.runner.shutdown_receiver();
        let runner = Arc::downgrade(&self.runner);
        let Self {
            store,
            config,
            access,
            ..
        } = self;
        async move {
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.wait_for(|stopped| *stopped) => return,
                    result = Box::pin(Self::run_owned(store.clone(), runner.clone(), config.clone(), access.clone(), interfaces.clone())) => {
                        if let Err(error) = result {
                            log::error!("Wakeup scheduler stopped; restart Kodrik after fixing storage: {error:#}");
                            return;
                        }
                    }
                }
                tokio::select! {
                    _ = shutdown.wait_for(|stopped| *stopped) => return,
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                }
            }
        }
    }

    async fn run_owned(
        store: ReminderStore,
        runner: std::sync::Weak<AgentRunner>,
        config: CompanionConfig,
        access: Option<(crate::web::auth::AuthStore, u64)>,
        interfaces: Vec<Arc<dyn Interface>>,
    ) -> anyhow::Result<()> {
        let store = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let Some(owner) = store.scheduler_lock()? else {
                return Ok(None);
            };
            let store = store.with_scheduler_owner(owner);
            store.recover()?;
            Ok(Some(store))
        })
        .await??;
        let Some(store) = store else {
            log::debug!("Wakeup scheduler already running for this data directory");
            return Ok(());
        };
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let journal = store.clone();
            let now = Utc::now();
            if tokio::task::spawn_blocking(move || journal.due(now))
                .await??
                .is_empty()
            {
                continue;
            }
            let Some(runner) = runner.upgrade() else {
                return Ok(());
            };
            let scheduler =
                Self::new(store.clone(), runner, config.clone()).with_access(access.clone());
            scheduler.check(&interfaces, Utc::now()).await?;
        }
    }

    async fn check(
        &self,
        interfaces: &[Arc<dyn Interface>],
        now: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        let elapsed = std::time::Instant::now();
        let store = self.store.clone();
        let due = tokio::task::spawn_blocking(move || store.due(now)).await??;
        // ponytail: serial attempts, up to five minutes each; use per-chat workers if volume warrants it.
        for task in due {
            if let Some((auth, user)) = &self.access
                && !auth.allowed_user(*user)?
            {
                return Ok(());
            }
            let attempt_now = now + chrono::Duration::from_std(elapsed.elapsed())?;
            let interface = interfaces.iter().find(|i| {
                i.id().0 == task.recipient.interface
                    && i.allows_recipient(&task.recipient.chat)
                    && self
                        .access
                        .as_ref()
                        .is_none_or(|(_, user)| task.recipient.chat == user.to_string())
            });
            if interface.is_none() && !task.recipient.is_local() {
                let store = self.store.clone();
                tokio::task::spawn_blocking(move || {
                    store.note_unavailable(
                        &task.id,
                        "Recipient interface unavailable or invalid chat ID".into(),
                    )
                })
                .await??;
                continue;
            }
            let conversation = task.recipient.conversation();
            if self.runner.conversation_busy(&conversation).await {
                continue;
            }
            if matches!(task.status, Status::Pending | Status::Running) {
                if let Err(error) = self
                    .runner
                    .execute_wakeup(
                        interface.map(|i| i.as_ref()),
                        &self.store,
                        &task,
                        attempt_now,
                    )
                    .await
                {
                    if matches!(
                        error.downcast_ref::<std::fs::TryLockError>(),
                        Some(std::fs::TryLockError::WouldBlock)
                    ) {
                        continue;
                    }
                    return Err(error);
                }
            }
            if let Some((auth, user)) = &self.access
                && !auth.allowed_user(*user)?
            {
                return Ok(());
            }
            let now = now + chrono::Duration::from_std(elapsed.elapsed())?;
            let store = self.store.clone();
            let id = task.id.clone();
            let current = tokio::task::spawn_blocking(move || store.due(now))
                .await??
                .into_iter()
                .find(|task| task.id == id);
            let Some(task) = current else {
                continue;
            };
            if let Err(error) = self
                .runner
                .deliver_wakeup(
                    interface.map(|i| i.as_ref()),
                    &self.store,
                    &task,
                    &self.config,
                    now,
                )
                .await
            {
                if matches!(
                    error.downcast_ref::<std::fs::TryLockError>(),
                    Some(std::fs::TryLockError::WouldBlock)
                ) {
                    continue;
                }
                return Err(error);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        MessageId,
        agent::{
            Agent, AgentEvent, ContextBuilder, LlmContext,
            llm::LlmProvider,
            message::{Message, ToolCall},
            reminders::{Recipient, ReminderRequest},
            session::{JsonSessionStore, SessionId},
            tools::{ExecCommandTool, SessionSearchTool, Tool, ToolRegistry, WakeupTool},
        },
        gateway::{Event, InterfaceId},
        interfaces::SendRejection,
    };
    use serde_json::json;
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use tokio::sync::mpsc;

    struct TaskModel {
        root: std::path::PathBuf,
        result: &'static str,
        calls: AtomicUsize,
        seen: Mutex<Vec<String>>,
    }

    struct IsolatedTaskModel(std::path::PathBuf);
    #[async_trait::async_trait]
    impl LlmProvider for IsolatedTaskModel {
        async fn generate(
            &self,
            context: &LlmContext<'_>,
            _: &[&dyn Tool],
            _: Option<&mpsc::Sender<AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            let message = if context.messages.len() == 1 {
                Message::ToolCall(ToolCall {
                    call_id: "probe".into(),
                    name: "exec_command".into(),
                    arguments: json!({"cmd":format!("cat '{}'; echo awake > marker", self.0.display())}),
                })
            } else {
                Message::Assistant(crate::agent::AssistantMessage {
                    content: "{\"message\":null}".into(),
                })
            };
            Ok((vec![message], None))
        }
    }

    #[tokio::test]
    async fn isolated_wakeup_keeps_virtual_shell_after_session_switch() {
        let root = tempfile::tempdir().unwrap();
        let secret = root.path().join("owner-secret");
        std::fs::write(&secret, "PRIVATE_WAKEUP_SECRET").unwrap();
        let space = root.path().join("group");
        let sandbox = Arc::new(crate::agent::sandbox::Sandbox::new(space.clone()));
        let sessions = Arc::new(JsonSessionStore::new(space.join("sessions")));
        let make_runner = || {
            AgentRunner::new(
                Arc::new(
                    Agent::new(
                        Arc::new(IsolatedTaskModel(secret.clone())),
                        ToolRegistry::default().register(sandbox.tool("exec_command")),
                        ContextBuilder::new("group"),
                    )
                    .with_sandbox(Arc::new(crate::agent::sandbox::Sandbox::new(space.clone()))),
                ),
                Arc::new(JsonSessionStore::new(space.join("sessions"))),
            )
        };
        let runner = make_runner();
        let store = ReminderStore::wakeups(&space);
        let source = runner
            .new_session(&SessionId::new("telegram:test:-42"))
            .await
            .unwrap();
        let id = store.execute_wakeup(
            &Recipient { interface: "telegram:test".into(), chat: "-42".into() }, source.id().as_str(),
            serde_json::from_value(json!({"action":"create", "text":"Check isolation", "due_at":(Utc::now() - chrono::Duration::minutes(1)).to_rfc3339()})).unwrap(),
            Utc::now() - chrono::Duration::minutes(2)).unwrap()["wakeup"]["id"].as_str().unwrap().to_owned();
        drop(store);
        let store = ReminderStore::wakeups(&space);
        runner
            .new_session(&SessionId::new("telegram:test:-42"))
            .await
            .unwrap();
        drop(runner);
        let runner = make_runner();
        let task = store.due(Utc::now()).unwrap().pop().unwrap();
        runner
            .execute_wakeup(Some(&Chat::new(false)), &store, &task, Utc::now())
            .await
            .unwrap();
        let all: serde_json::Value =
            serde_json::from_slice(&std::fs::read(space.join("wakeups.json")).unwrap()).unwrap();
        assert_eq!(all[0]["status"], "completed");
        assert_eq!(
            sandbox
                .tool("exec_command")
                .execute(json!({"cmd":"cat marker"}))
                .await
                .unwrap()
                .output["stdout"],
            "awake\n"
        );
        assert!(!space.join("marker").exists());
        use crate::agent::session::SessionStore;
        let history = sessions
            .load(&SessionId::new(format!("companion:wakeup:{id}")))
            .await
            .unwrap()
            .unwrap();
        assert!(
            !serde_json::to_string(&history)
                .unwrap()
                .contains("PRIVATE_WAKEUP_SECRET")
        );
    }
    #[async_trait::async_trait]
    impl LlmProvider for TaskModel {
        async fn generate(
            &self,
            context: &LlmContext<'_>,
            tools: &[&dyn Tool],
            _: Option<&mpsc::Sender<AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let Message::User(input) = &context.messages[0] else {
                panic!("missing event")
            };
            self.seen.lock().unwrap().push(input.content.clone());
            assert!(tools.iter().any(|tool| tool.name() == "exec_command"));
            assert!(tools.iter().any(|tool| tool.name() == "wakeups"));
            let results = context
                .messages
                .iter()
                .filter(|m| matches!(m, Message::ToolResult(_)))
                .count();
            if self.result == "stall" {
                return futures::future::pending().await;
            }
            if self.result == "loop" {
                return Ok((
                    vec![Message::ToolCall(ToolCall {
                        call_id: format!("shell-{results}"),
                        name: "exec_command".into(),
                        arguments: json!({"cmd":"sh -c 'printf x >> marker'", "workdir":self.root}),
                    })],
                    None,
                ));
            }
            let message = match results {
                0 => Message::ToolCall(ToolCall {
                    call_id: "shell".into(),
                    name: "exec_command".into(),
                    arguments: json!({"cmd":"sh -c 'printf x >> marker'", "workdir":self.root}),
                }),
                1 => Message::ToolCall(ToolCall {
                    call_id: "follow-up".into(),
                    name: "wakeups".into(),
                    arguments: json!({"action":"create", "instruction":"Follow up on build", "due_at":(Utc::now() + chrono::Duration::days(30)).to_rfc3339()}),
                }),
                _ => Message::Assistant(crate::agent::AssistantMessage {
                    content: self.result.into(),
                }),
            };
            Ok((vec![message], None))
        }
    }

    struct Chat {
        reject_once: AtomicBool,
        unknown: bool,
        allowed: AtomicBool,
        sent: Mutex<Vec<String>>,
    }
    impl Chat {
        fn new(reject: bool) -> Self {
            Self {
                reject_once: AtomicBool::new(reject),
                unknown: false,
                allowed: AtomicBool::new(true),
                sent: Mutex::new(vec![]),
            }
        }
    }
    #[async_trait::async_trait]
    impl Interface for Chat {
        fn id(&self) -> InterfaceId {
            InterfaceId("telegram:test".into())
        }
        fn allows_recipient(&self, chat: &str) -> bool {
            self.allowed.load(Ordering::SeqCst) && matches!(chat, "42" | "-42")
        }
        async fn run(&self, _: mpsc::Sender<Event>) -> anyhow::Result<()> {
            Ok(())
        }
        async fn send(&self, chat: &str, text: &str) -> anyhow::Result<MessageId> {
            assert_eq!(chat, "42");
            if self.unknown {
                anyhow::bail!("Connection lost");
            }
            if self.reject_once.swap(false, Ordering::SeqCst) {
                return Err(SendRejection {
                    error_code: 429,
                    description: "rate limited".into(),
                    retry_after: Some(60),
                }
                .into());
            }
            self.sent.lock().unwrap().push(text.into());
            Ok(MessageId("1".into()))
        }
    }

    fn setup(root: &std::path::Path, result: &'static str) -> (Wakeups, Arc<TaskModel>) {
        let model = Arc::new(TaskModel {
            root: root.to_owned(),
            result,
            calls: AtomicUsize::new(0),
            seen: Mutex::new(vec![]),
        });
        let runner = Arc::new(AgentRunner::new(
            Arc::new(Agent::new(
                model.clone(),
                ToolRegistry::default()
                    .register(Arc::new(ExecCommandTool))
                    .register(Arc::new(WakeupTool::new(root)))
                    .register(Arc::new(SessionSearchTool::new(root.join("sessions")))),
                ContextBuilder::new("test"),
            )),
            Arc::new(JsonSessionStore::new(root.join("sessions"))),
        ));
        (
            Wakeups::new(
                ReminderStore::wakeups(root),
                runner,
                crate::config::CompanionConfig {
                    timezone: "UTC".into(),
                    quiet_start: 22,
                    quiet_end: 9,
                    ..Default::default()
                },
            ),
            model,
        )
    }

    #[tokio::test]
    async fn wakeups_deliver_local_results_once_without_telegram_or_enabled_web() {
        use crate::agent::session::{SessionStore, SqliteSessionStore};
        for interface in ["web", "webhook", "cli"] {
            let root = tempfile::tempdir().unwrap();
            let (mut scheduler, model) = setup(root.path(), "{\"message\":\"Build passed\"}");
            let sessions = Arc::new(
                SqliteSessionStore::open_async(root.path().join("sessions"))
                    .await
                    .unwrap(),
            );
            scheduler.runner = Arc::new(AgentRunner::new(
                Arc::new(Agent::new(
                    model.clone(),
                    ToolRegistry::default()
                        .register(Arc::new(ExecCommandTool))
                        .register(Arc::new(WakeupTool::new(root.path()))),
                    ContextBuilder::new("test"),
                )),
                sessions.clone(),
            ));
            let chat = if interface == "cli" {
                "main".into()
            } else {
                uuid::Uuid::new_v4().to_string()
            };
            let conversation = SessionId::new(if interface == "cli" {
                format!("cli:{chat}")
            } else {
                chat.clone()
            });
            let source = scheduler.runner.new_session(&conversation).await.unwrap();
            let now = Utc::now();
            use chrono::Timelike;
            scheduler.config.quiet_start = (now.hour() + 1) % 24;
            scheduler.config.quiet_end = (now.hour() + 2) % 24;
            let recipient = Recipient {
                interface: interface.into(),
                chat,
            };
            let created = scheduler.store.execute_wakeup(&recipient, source.id().as_str(), serde_json::from_value(json!({"action":"create","text":"Check build","due_at":(now-chrono::Duration::minutes(1)).to_rfc3339()})).unwrap(), now-chrono::Duration::minutes(2)).unwrap();
            let id = created["wakeup"]["id"].as_str().unwrap();
            let guard = sessions
                .execution_lock("conversation", conversation.as_str())
                .await
                .unwrap();
            scheduler.check(&[], now).await.unwrap();
            assert_eq!(model.calls.load(Ordering::SeqCst), 0);
            drop(guard);
            scheduler.check(&[], now).await.unwrap();
            let saved = sessions.load(source.id()).await.unwrap().unwrap();
            assert!(
                matches!(saved.messages().last(), Some(Message::Assistant(answer)) if answer.content == "Build passed"),
                "{}",
                serde_json::to_string(&saved).unwrap()
            );
            let calls = model.calls.load(Ordering::SeqCst);
            // Crash after the atomic history write but before journal completion.
            let path = root.path().join("wakeups.json");
            let mut records: Vec<crate::agent::reminders::Reminder> =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            records.iter_mut().find(|r| r.id == id).unwrap().status = Status::Ready;
            std::fs::write(&path, serde_json::to_vec(&records).unwrap()).unwrap();
            scheduler.store.recover().unwrap();
            scheduler.check(&[], now).await.unwrap();
            assert_eq!(model.calls.load(Ordering::SeqCst), calls);
            let saved = sessions.load(source.id()).await.unwrap().unwrap();
            assert_eq!(
                saved
                    .messages()
                    .iter()
                    .filter(|m| matches!(m, Message::Assistant(a) if a.content == "Build passed"))
                    .count(),
                1
            );
        }
    }

    fn create(store: &ReminderStore, source: &str) -> String {
        store.execute_wakeup(&Recipient { interface:"telegram:test".into(), chat:"42".into() }, source,
            serde_json::from_value::<ReminderRequest>(json!({"action":"create", "text":"Check build", "due_at":(Utc::now() - chrono::Duration::minutes(1)).to_rfc3339()})).unwrap(),
            Utc::now() - chrono::Duration::minutes(2)).unwrap()["wakeup"]["id"].as_str().unwrap().into()
    }

    #[tokio::test]
    async fn wakeups_recheck_revocation_after_execution_before_delivery() {
        struct RevokingModel {
            model: Arc<TaskModel>,
            auth: crate::web::auth::AuthStore,
        }
        #[async_trait::async_trait]
        impl LlmProvider for RevokingModel {
            async fn generate(
                &self,
                context: &LlmContext<'_>,
                tools: &[&dyn Tool],
                progress: Option<&mpsc::Sender<AgentEvent>>,
            ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
                let result = self.model.generate(context, tools, progress).await?;
                if result.0.iter().any(|m| matches!(m, Message::Assistant(_))) {
                    self.auth.revoke_user(42)?;
                }
                Ok(result)
            }
        }
        let root = tempfile::tempdir().unwrap();
        let auth = crate::web::auth::AuthStore::new(root.path());
        auth.grant_test_user(42).unwrap();
        let (mut scheduler, model) = setup(root.path(), "{\"message\":\"Build passed\"}");
        scheduler.runner = Arc::new(AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(RevokingModel {
                    model,
                    auth: auth.clone(),
                }),
                ToolRegistry::default()
                    .register(Arc::new(ExecCommandTool))
                    .register(Arc::new(WakeupTool::new(root.path()))),
                ContextBuilder::new("test"),
            )),
            Arc::new(JsonSessionStore::new(root.path().join("sessions"))),
        ));
        scheduler.access = Some((auth, 42));
        let source = scheduler
            .runner
            .new_session(&SessionId::new("telegram:test:42"))
            .await
            .unwrap();
        create(&scheduler.store, source.id().as_str());
        let chat = Arc::new(Chat::new(false));
        let now = Utc::now();
        use chrono::Timelike;
        scheduler.config.quiet_start = (now.hour() + 1) % 24;
        scheduler.config.quiet_end = (now.hour() + 2) % 24;
        scheduler.check(&[chat.clone()], now).await.unwrap();
        assert!(
            !scheduler
                .access
                .as_ref()
                .unwrap()
                .0
                .allowed_user(42)
                .unwrap()
        );
        assert!(chat.sent.lock().unwrap().is_empty());
        assert_eq!(records(&scheduler.store)["wakeups"][0]["status"], "ready");
    }

    #[tokio::test]
    async fn wakeups_recheck_delivery_locks_and_defer_a_busy_telegram_session() {
        use crate::agent::session::SqliteSessionStore;
        use chrono::Timelike;
        let root = tempfile::tempdir().unwrap();
        let (mut scheduler, model) = setup(root.path(), "{\"message\":null}");
        let sessions = Arc::new(
            SqliteSessionStore::open_async(root.path().join("sessions"))
                .await
                .unwrap(),
        );
        scheduler.runner = Arc::new(AgentRunner::new(
            Arc::new(Agent::new(
                model,
                ToolRegistry::default(),
                ContextBuilder::new("test"),
            )),
            sessions.clone(),
        ));
        let conversation = SessionId::new("telegram:test:42");
        let source = scheduler.runner.new_session(&conversation).await.unwrap();
        let id = create(&scheduler.store, source.id().as_str());
        let now = Utc::now();
        scheduler.config.quiet_start = (now.hour() + 1) % 24;
        scheduler.config.quiet_end = (now.hour() + 2) % 24;
        scheduler.store.claim(&id, now).unwrap().unwrap();
        scheduler
            .store
            .complete_execution(&id, Some("Ready result".into()))
            .unwrap();
        let chat = Arc::new(Chat::new(false));
        let guard = sessions
            .execution_lock("session", source.id().as_str())
            .await
            .unwrap();
        scheduler.check(&[chat.clone()], now).await.unwrap();
        assert!(chat.sent.lock().unwrap().is_empty());
        drop(guard);
        scheduler.check(&[chat.clone()], now).await.unwrap();
        assert_eq!(*chat.sent.lock().unwrap(), ["Ready result"]);
        assert!(scheduler.store.due(now).unwrap().is_empty());
    }
    // Read through list instead of exposing store internals for tests.
    fn records(store: &ReminderStore) -> serde_json::Value {
        store
            .execute_wakeup(
                &Recipient {
                    interface: "telegram:test".into(),
                    chat: "42".into(),
                },
                "session",
                serde_json::from_value(json!({"action":"list"})).unwrap(),
                Utc::now(),
            )
            .unwrap()
    }

    #[tokio::test]
    async fn wakeups_execute_tools_after_new_and_deliver_saved_result_once() {
        let root = tempfile::tempdir().unwrap();
        let (scheduler, model) = setup(root.path(), "{\"message\":\"Build passed\"}");
        let conversation = SessionId::new("telegram:test:42");
        let source = scheduler.runner.new_session(&conversation).await.unwrap();
        let id = create(&scheduler.store, source.id().as_str());
        let current = scheduler.runner.new_session(&conversation).await.unwrap();
        let chat = Arc::new(Chat::new(true));
        let interfaces: Vec<Arc<dyn Interface>> = vec![chat.clone()];
        let quiet = Utc::now()
            .date_naive()
            .and_hms_opt(23, 0, 0)
            .unwrap()
            .and_utc();
        scheduler.check(&interfaces, quiet).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("marker")).unwrap(),
            "x"
        );
        assert!(chat.sent.lock().unwrap().is_empty());
        assert!(model.seen.lock().unwrap()[0].contains(source.id().as_str()));
        assert!(model.seen.lock().unwrap()[0].contains(current.id().as_str()));
        assert_eq!(
            records(&scheduler.store)["wakeups"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let reopened = Wakeups::new(
            ReminderStore::wakeups(root.path()),
            scheduler.runner.clone(),
            scheduler.config.clone(),
        );
        reopened.store.recover().unwrap();
        let day = quiet + chrono::Duration::hours(13);
        reopened.check(&interfaces, day).await.unwrap();
        assert_eq!(records(&reopened.store)["wakeups"][0]["status"], "ready");
        reopened
            .check(&interfaces, day + chrono::Duration::minutes(2))
            .await
            .unwrap();
        reopened
            .check(&interfaces, day + chrono::Duration::minutes(3))
            .await
            .unwrap();
        assert_eq!(*chat.sent.lock().unwrap(), vec!["Build passed"]);
        assert_eq!(model.calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            std::fs::read_to_string(root.path().join("marker")).unwrap(),
            "x"
        );
        let active = scheduler.runner.load_session(conversation).await.unwrap();
        assert_eq!(active.id(), current.id());
        assert!(
            matches!(&active.messages()[0], Message::Assistant(m) if m.content == "Build passed")
        );
        assert!(
            scheduler
                .runner
                .list_sessions()
                .await
                .unwrap()
                .iter()
                .all(|s| !s.id().as_str().starts_with("companion:wakeup:"))
        );
        let output = SessionSearchTool::new(root.path().join("sessions"))
            .execute(json!({"query":"Check build"}))
            .await
            .unwrap();
        assert_eq!(output.output["results"], json!([]));
        assert!(reopened.store.claim(&id, day).unwrap().is_none());
    }

    #[tokio::test]
    async fn wakeups_silence_and_invalid_results_never_repeat_tools() {
        for (result, expected) in [
            ("{\"message\":null}", "completed"),
            ("invalid", "uncertain"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let (scheduler, model) = setup(root.path(), result);
            let id = create(&scheduler.store, "source");
            let chat = Arc::new(Chat::new(false));
            let interfaces: Vec<Arc<dyn Interface>> = vec![chat.clone()];
            scheduler.check(&interfaces, Utc::now()).await.unwrap();
            scheduler.check(&interfaces, Utc::now()).await.unwrap();
            let all: serde_json::Value =
                serde_json::from_slice(&std::fs::read(root.path().join("wakeups.json")).unwrap())
                    .unwrap();
            assert_eq!(all[0]["id"], id);
            assert_eq!(all[0]["status"], expected);
            assert_eq!(model.calls.load(Ordering::SeqCst), 3);
            assert!(chat.sent.lock().unwrap().is_empty());
            assert_eq!(
                std::fs::read_to_string(root.path().join("marker")).unwrap(),
                "x"
            );
        }
    }

    #[tokio::test]
    async fn wakeups_exhausted_budget_preserves_transcript_without_replaying_tools() {
        let root = tempfile::tempdir().unwrap();
        let (scheduler, model) = setup(root.path(), "loop");
        let id = create(&scheduler.store, "source");
        let chat = Arc::new(Chat::new(false));
        let interfaces: Vec<Arc<dyn Interface>> = vec![chat.clone()];
        scheduler.check(&interfaces, Utc::now()).await.unwrap();
        scheduler.check(&interfaces, Utc::now()).await.unwrap();
        assert_eq!(
            records(&scheduler.store)["wakeups"][0]["status"],
            "uncertain"
        );
        assert_eq!(model.calls.load(Ordering::SeqCst), 16);
        assert_eq!(
            std::fs::read_to_string(root.path().join("marker")).unwrap(),
            "xxxxxxxxxxxxxxxx"
        );
        let saved: crate::agent::session::Session = serde_json::from_slice(
            &std::fs::read(
                root.path()
                    .join("sessions")
                    .join(format!("companion:wakeup:{id}.json")),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            saved
                .messages()
                .iter()
                .filter(|m| matches!(m, Message::ToolResult(_)))
                .count(),
            16
        );
        assert!(chat.sent.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn wakeups_timeout_is_uncertain_and_saves_internal_event_without_retry() {
        let root = tempfile::tempdir().unwrap();
        let (scheduler, model) = setup(root.path(), "stall");
        let id = create(&scheduler.store, "source");
        let chat = Arc::new(Chat::new(false));
        let interfaces: Vec<Arc<dyn Interface>> = vec![chat.clone()];
        let started = tokio::time::Instant::now();
        scheduler.check(&interfaces, Utc::now()).await.unwrap();
        assert_eq!(
            tokio::time::Instant::now() - started,
            Duration::from_secs(300)
        );
        scheduler.check(&interfaces, Utc::now()).await.unwrap();
        let listed = records(&scheduler.store);
        assert_eq!(listed["wakeups"][0]["status"], "uncertain");
        assert!(
            listed["wakeups"][0]["last_error"]
                .as_str()
                .unwrap()
                .contains("timed out")
        );
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        let saved: crate::agent::session::Session = serde_json::from_slice(
            &std::fs::read(
                root.path()
                    .join("sessions")
                    .join(format!("companion:wakeup:{id}.json")),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(
            matches!(&saved.messages()[0], Message::User(m) if m.content.contains("Check build"))
        );
        assert!(chat.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn wakeups_unknown_delivery_does_not_retry_and_revoked_chat_does_not_execute() {
        let root = tempfile::tempdir().unwrap();
        let (scheduler, model) = setup(root.path(), "{\"message\":\"Build passed\"}");
        let id = create(&scheduler.store, "source");
        let mut chat = Chat::new(false);
        chat.unknown = true;
        chat.allowed.store(false, Ordering::SeqCst);
        let chat = Arc::new(chat);
        let interfaces: Vec<Arc<dyn Interface>> = vec![chat.clone()];
        let day = Utc::now()
            .date_naive()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc()
            + chrono::Duration::days(1);
        scheduler.check(&interfaces, day).await.unwrap();
        assert_eq!(model.calls.load(Ordering::SeqCst), 0);
        assert_eq!(records(&scheduler.store)["wakeups"][0]["status"], "pending");
        chat.allowed.store(true, Ordering::SeqCst);
        scheduler.check(&interfaces, day).await.unwrap();
        scheduler.check(&interfaces, day).await.unwrap();
        assert_eq!(model.calls.load(Ordering::SeqCst), 3);
        let listed = records(&scheduler.store);
        assert_eq!(listed["wakeups"][0]["id"], id);
        assert_eq!(listed["wakeups"][0]["status"], "uncertain");
        assert!(
            listed["wakeups"][0]["last_error"]
                .as_str()
                .unwrap()
                .contains("Connection lost")
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("marker")).unwrap(),
            "x"
        );
    }
}
