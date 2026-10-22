use crate::{
    Interface,
    agent::{AgentRunner, reminders::ReminderStore},
};
use chrono::{DateTime, Utc};
use std::{sync::Arc, time::Duration};

pub(crate) struct Reminders {
    store: ReminderStore,
    runner: Arc<AgentRunner>,
}
impl Reminders {
    pub(crate) fn new(store: ReminderStore, runner: Arc<AgentRunner>) -> Self {
        Self { store, runner }
    }

    pub(crate) async fn run(&self, interfaces: Vec<Arc<dyn Interface>>) {
        if let Err(error) = self.run_owned(interfaces).await {
            log::error!(
                "Reminder scheduler stopped; restart the gateway after fixing storage: {error:#}"
            );
        }
    }

    async fn run_owned(&self, interfaces: Vec<Arc<dyn Interface>>) -> anyhow::Result<()> {
        // Only the scheduler owner recovers sending entries; a second gateway cannot
        // reinterpret a live attempt as a crashed one.
        let store = self.store.clone();
        let owner = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let Some(owner) = store.scheduler_lock()? else {
                return Ok(None);
            };
            let store = store.with_scheduler_owner(owner);
            store.recover()?;
            Ok(Some(store))
        })
        .await??;
        let Some(store) = owner else {
            log::warn!("Reminder scheduler already running for this data directory");
            return Ok(());
        };
        let scheduler = Self::new(store, self.runner.clone());
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            scheduler.check(&interfaces, Utc::now()).await?;
        }
    }

    async fn check(
        &self,
        interfaces: &[Arc<dyn Interface>],
        now: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        let store = self.store.clone();
        let due = tokio::task::spawn_blocking(move || store.due(now)).await??;
        // ponytail: serial sends, at most 15s per request; parallelize per chat if volume warrants it.
        for reminder in due {
            let interface = interfaces.iter().find(|interface| {
                interface.id().0 == reminder.recipient.interface
                    && interface.allows_recipient(&reminder.recipient.chat)
            });
            if let Some(interface) = interface {
                let conversation = crate::agent::session::SessionId::new(format!(
                    "{}:{}",
                    reminder.recipient.interface, reminder.recipient.chat
                ));
                if !self.runner.conversation_busy(&conversation).await {
                    self.runner
                        .deliver_reminder(interface.as_ref(), &self.store, &reminder)
                        .await?;
                }
            } else {
                let store = self.store.clone();
                tokio::task::spawn_blocking(move || {
                    store.note_unavailable(
                        &reminder.id,
                        "Recipient interface unavailable or invalid chat ID".into(),
                    )
                })
                .await??;
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
            ContextBuilder, LlmContext,
            llm::LlmProvider,
            message::Message,
            reminders::{Recipient, ReminderRequest},
            session::{JsonSessionStore, SessionId},
            tools::ToolRegistry,
        },
        gateway::{Event, InterfaceId},
        interfaces::SendRejection,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;

    struct NoModel;
    #[async_trait::async_trait]
    impl LlmProvider for NoModel {
        async fn generate(
            &self,
            _: &LlmContext<'_>,
            _: &[&dyn crate::agent::tools::Tool],
            _: Option<&mpsc::Sender<crate::agent::AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            panic!("Reminder delivery must not call the model")
        }
    }
    struct FakeInterface {
        mode: &'static str,
        attempts: AtomicUsize,
        messages: std::sync::Mutex<Vec<String>>,
        break_history: Option<std::path::PathBuf>,
    }
    impl FakeInterface {
        fn new(mode: &'static str) -> Self {
            Self {
                mode,
                attempts: AtomicUsize::new(0),
                messages: Default::default(),
                break_history: None,
            }
        }
    }
    #[async_trait::async_trait]
    impl Interface for FakeInterface {
        fn id(&self) -> InterfaceId {
            InterfaceId("telegram:test".into())
        }
        fn allows_recipient(&self, chat: &str) -> bool {
            self.mode != "revoked" && chat == "42"
        }
        async fn run(&self, _: mpsc::Sender<Event>) -> anyhow::Result<()> {
            Ok(())
        }
        async fn send(&self, chat: &str, text: &str) -> anyhow::Result<MessageId> {
            assert_eq!(chat, "42");
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            match self.mode {
                "retry" if attempt == 0 => Err(SendRejection {
                    error_code: 429,
                    description: "rate limited".into(),
                    retry_after: Some(120),
                }
                .into()),
                "failed" => Err(SendRejection {
                    error_code: 403,
                    description: "blocked".into(),
                    retry_after: None,
                }
                .into()),
                "uncertain" => anyhow::bail!("connection lost"),
                _ => {
                    self.messages.lock().unwrap().push(text.into());
                    if let Some(path) = &self.break_history {
                        std::fs::rename(path, path.with_extension("previous")).unwrap();
                        std::fs::write(path, "blocked").unwrap();
                    }
                    Ok(MessageId("1".into()))
                }
            }
        }
    }
    fn setup(path: &std::path::Path) -> (Reminders, Arc<AgentRunner>) {
        let runner = Arc::new(AgentRunner::new(
            Arc::new(crate::agent::Agent::new(
                Arc::new(NoModel),
                ToolRegistry::default(),
                ContextBuilder::new("test"),
            )),
            Arc::new(JsonSessionStore::new(path.join("sessions"))),
        ));
        (
            Reminders::new(ReminderStore::new(path), runner.clone()),
            runner,
        )
    }
    fn create(store: &ReminderStore) -> String {
        let now = Utc::now();
        let request: ReminderRequest = serde_json::from_value(serde_json::json!({"action":"create","text":"Call doctor","due_at":(now - chrono::Duration::minutes(5)).to_rfc3339()})).unwrap();
        store
            .execute(
                &Recipient {
                    interface: "telegram:test".into(),
                    chat: "42".into(),
                },
                request,
                now - chrono::Duration::minutes(10),
            )
            .unwrap()["reminder"]["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    fn status(store: &ReminderStore) -> serde_json::Value {
        store
            .execute(
                &Recipient {
                    interface: "telegram:test".into(),
                    chat: "42".into(),
                },
                serde_json::from_value(serde_json::json!({"action":"list"})).unwrap(),
                Utc::now(),
            )
            .unwrap()
    }

    #[tokio::test]
    async fn reminders_deliver_overdue_to_active_history_without_model() {
        let directory = tempfile::tempdir().unwrap();
        let (scheduler, runner) = setup(directory.path());
        let id = create(&scheduler.store);
        let conversation = SessionId::new("telegram:test:42");
        let active = runner.new_session(&conversation).await.unwrap();
        let interface = Arc::new(FakeInterface::new("sent"));
        let interfaces: Vec<Arc<dyn Interface>> = vec![interface.clone()];
        scheduler.check(&interfaces, Utc::now()).await.unwrap();
        scheduler.check(&interfaces, Utc::now()).await.unwrap();
        assert!(scheduler.store.claim(&id, Utc::now()).unwrap().is_none());
        assert_eq!(interface.attempts.load(Ordering::SeqCst), 1);
        assert!(interface.messages.lock().unwrap()[0].contains("⏱"));
        let session = runner.load_session(conversation).await.unwrap();
        assert_eq!(session.id(), active.id());
        assert!(
            matches!(&session.messages()[0], Message::Assistant(m) if m.content.contains("Call doctor"))
        );
    }

    #[tokio::test]
    async fn reminders_failures_retry_only_confirmed_temporary_rejections() {
        for (mode, expected) in [
            ("retry", "pending"),
            ("failed", "failed"),
            ("uncertain", "uncertain"),
            ("revoked", "pending"),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let (scheduler, _) = setup(directory.path());
            create(&scheduler.store);
            let interface = Arc::new(FakeInterface::new(mode));
            let interfaces: Vec<Arc<dyn Interface>> = vec![interface.clone()];
            scheduler.check(&interfaces, Utc::now()).await.unwrap();
            let state = status(&scheduler.store);
            assert_eq!(state["reminders"][0]["status"], expected);
            assert!(state["reminders"][0]["last_error"].is_string());
            scheduler.check(&interfaces, Utc::now()).await.unwrap();
            assert_eq!(
                interface.attempts.load(Ordering::SeqCst),
                usize::from(mode != "revoked")
            );
            if mode == "retry" {
                let retry = DateTime::parse_from_rfc3339(
                    state["reminders"][0]["retry_at"].as_str().unwrap(),
                )
                .unwrap()
                .with_timezone(&Utc);
                assert!(retry > Utc::now() + chrono::Duration::seconds(100));
            }
        }
    }

    #[tokio::test]
    async fn reminders_recovery_and_missing_interface_never_duplicate() {
        let directory = tempfile::tempdir().unwrap();
        let (scheduler, _) = setup(directory.path());
        let id = create(&scheduler.store);
        scheduler.check(&[], Utc::now()).await.unwrap();
        assert_eq!(
            status(&scheduler.store)["reminders"][0]["status"],
            "pending"
        );
        scheduler.store.claim(&id, Utc::now()).unwrap().unwrap();
        let owner = scheduler.store.scheduler_lock().unwrap().unwrap();
        scheduler.store.recover().unwrap();
        let interface = Arc::new(FakeInterface::new("sent"));
        let interfaces: Vec<Arc<dyn Interface>> = vec![interface.clone()];
        scheduler.check(&interfaces, Utc::now()).await.unwrap();
        assert_eq!(interface.attempts.load(Ordering::SeqCst), 0);
        assert_eq!(
            status(&scheduler.store)["reminders"][0]["status"],
            "uncertain"
        );
        drop(owner);
    }

    #[tokio::test]
    async fn reminders_history_failure_after_send_does_not_retry() {
        let directory = tempfile::tempdir().unwrap();
        let (scheduler, runner) = setup(directory.path());
        let id = create(&scheduler.store);
        runner
            .new_session(&SessionId::new("telegram:test:42"))
            .await
            .unwrap();
        let mut interface = FakeInterface::new("sent");
        interface.break_history = Some(directory.path().join("sessions"));
        let interface = Arc::new(interface);
        let interfaces: Vec<Arc<dyn Interface>> = vec![interface.clone()];
        scheduler.check(&interfaces, Utc::now()).await.unwrap();
        scheduler.check(&interfaces, Utc::now()).await.unwrap();
        assert_eq!(interface.attempts.load(Ordering::SeqCst), 1);
        assert!(scheduler.store.claim(&id, Utc::now()).unwrap().is_none());
        let records: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.path().join("reminders.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(records[0]["status"], "sent");
    }
}
