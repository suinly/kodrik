use super::*;
use crate::{
    agent::{
        AssistantMessage,
        message::Message,
        run_state::{CURRENT_TOOL, Checkpoint, RunPhase},
        session::{Session, SessionId, SessionStore},
    },
    gateway::{Event, InterfaceId},
    interfaces::SendRejection,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{Mutex, mpsc};

#[derive(Default)]
struct Recorder {
    seen: Mutex<Vec<(String, String)>>,
    failure: AtomicUsize,
}

#[async_trait::async_trait]
impl Interface for Recorder {
    fn id(&self) -> InterfaceId {
        InterfaceId("telegram:test".into())
    }
    fn allows_recipient(&self, _: &str) -> bool {
        true
    }
    async fn run(&self, _: mpsc::Sender<Event>) -> anyhow::Result<()> {
        Ok(())
    }
    async fn send(&self, chat: &str, text: &str) -> anyhow::Result<MessageId> {
        self.seen.lock().await.push((chat.into(), text.into()));
        match self.failure.swap(0, Ordering::SeqCst) {
            1 => anyhow::bail!("Network outcome unknown"),
            2 => Err(SendRejection {
                description: "Rate limited".into(),
                error_code: 429,
                retry_after: Some(1),
            }
            .into()),
            _ => Ok(MessageId("100".into())),
        }
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: SqliteSessionStore,
    tool: MessagesTool,
    recorder: Arc<Recorder>,
    checkpoint: Checkpoint,
    source: Session,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap();
        let source = Session::new(SessionId::new(uuid::Uuid::new_v4().to_string()));
        store.save(&source).await.unwrap();
        let conversation = SessionId::new("telegram:test:42");
        store.select(&conversation, source.id()).await.unwrap();
        let mut checkpoint = Checkpoint::new(&store, &conversation, &source);
        checkpoint
            .save(&source, RunPhase::NeedModel, None)
            .await
            .unwrap();
        let recorder = Arc::new(Recorder::default());
        let tool = MessagesTool::new(
            store.clone(),
            vec![MessageChannel {
                name: "telegram".into(),
                interface: recorder.clone(),
                recipient: "42".into(),
            }],
            Some("http://localhost/kodrik/".parse().unwrap()),
        );
        Self {
            _dir: dir,
            store,
            tool,
            recorder,
            checkpoint,
            source,
        }
    }

    async fn send(&self, call: &str, args: Value) -> anyhow::Result<ToolOutput> {
        CURRENT_TOOL
            .scope(
                Some((self.checkpoint.record.id.clone(), call.into())),
                self.tool.execute(args),
            )
            .await
    }
}

fn telegram() -> Value {
    json!({"action":"send","channel":"telegram","text":"Site is down","conversation_id":null})
}

#[tokio::test]
async fn messages_telegram_binds_owner_outside_telegram_and_replays_once() {
    let f = Fixture::new().await;
    let channels = f
        .tool
        .execute(json!({"action":"list_channels"}))
        .await
        .unwrap()
        .output;
    assert_eq!(channels["channels"][0]["channel"], "telegram");
    assert_eq!(channels["channels"][1]["channel"], "web");
    let first = f.send("one", telegram()).await.unwrap().output;
    assert_eq!(first["status"], "sent");
    assert_eq!(f.send("one", telegram()).await.unwrap().output, first);
    let mut changed = telegram();
    changed["text"] = "different".into();
    assert!(f.send("one", changed).await.is_err());
    for args in [
        json!({"action":"send","channel":"telegram","text":"x","conversation_id":"7"}),
        json!({"action":"send","channel":"telegram","text":"x","chat_id":"7"}),
        json!({"action":"send","channel":"missing","text":"x"}),
        json!({"action":"send","channel":"telegram","text":"  "}),
    ] {
        assert!(f.send("invalid", args).await.is_err());
    }
    assert_eq!(
        *f.recorder.seen.lock().await,
        vec![("42".into(), "Site is down".into())]
    );
}

#[tokio::test]
async fn messages_uncertain_delivery_is_an_error_and_is_never_resent() {
    let f = Fixture::new().await;
    f.recorder.failure.store(1, Ordering::SeqCst);
    assert!(f.send("uncertain", telegram()).await.is_err());
    assert!(
        f.send("uncertain", telegram()).await.is_err(),
        "An uncertain receipt must not be reported as pending for retry"
    );
    assert_eq!(f.recorder.seen.lock().await.len(), 1);
    assert!(f.store.pending_notifications().await.unwrap().is_empty());
}

#[tokio::test]
async fn messages_recovery_does_not_deliver_to_a_previous_owner() {
    let f = Fixture::new().await;
    let id = f
        .store
        .queue_notification(
            &f.checkpoint.record.id,
            "old-owner",
            &f.checkpoint.record.conversation_id,
            "telegram:test",
            "99",
            "private".into(),
        )
        .await
        .unwrap();
    assert!(f.tool.deliver(&id).await.is_err());
    assert!(f.recorder.seen.lock().await.is_empty());
    assert!(f.store.pending_notifications().await.unwrap().is_empty());
}

#[tokio::test]
async fn messages_notifications_are_not_flushed_as_current_chat_replies() {
    let f = Fixture::new().await;
    f.store
        .queue_notification(
            &f.checkpoint.record.id,
            "notify",
            &f.checkpoint.record.conversation_id,
            "telegram:test",
            "42",
            "Notification".into(),
        )
        .await
        .unwrap();
    assert!(
        f.store
            .pending_deliveries("telegram:test:42")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        f.store
            .pending_delivery_conversations()
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(f.store.pending_notifications().await.unwrap().len(), 1);
}

#[tokio::test]
async fn messages_web_waits_for_session_owner_without_an_active_run() {
    let f = Fixture::new().await;
    let mut target = Session::new(SessionId::new(uuid::Uuid::new_v4().to_string()));
    target.push(Message::Assistant(AssistantMessage {
        content: "Earlier message".into(),
    }));
    f.store.save(&target).await.unwrap();
    let ownership = f
        .store
        .execution_lock("session", target.id().as_str())
        .await
        .unwrap();
    let mut older_snapshot = f.store.load(target.id()).await.unwrap().unwrap();
    let result = f.send("shared-session", json!({"action":"send", "channel":"web", "text":"Notification", "conversation_id":target.id().as_str()})).await.unwrap().output;
    assert_eq!(result["status"], "pending");
    let delivery = result["delivery_id"].as_str().unwrap();
    assert!(
        f.store
            .notification_result(delivery)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        f.store
            .load(target.id())
            .await
            .unwrap()
            .unwrap()
            .messages()
            .len(),
        1
    );
    older_snapshot.push(Message::Assistant(AssistantMessage {
        content: "Reminder delivered".into(),
    }));
    f.store.save(&older_snapshot).await.unwrap();
    drop(ownership);
    f.tool.deliver(delivery).await.unwrap().unwrap();
    f.tool.deliver(delivery).await.unwrap().unwrap();
    let saved = f.store.load(target.id()).await.unwrap().unwrap();
    let texts: Vec<_> = saved
        .messages()
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(message) => Some(message.content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        vec!["Earlier message", "Reminder delivered", "Notification"]
    );
}

#[tokio::test]
async fn messages_web_waits_for_busy_history_and_appends_only_once() {
    let f = Fixture::new().await;
    let mut target = Session::new(SessionId::new(uuid::Uuid::new_v4().to_string()));
    target.push(Message::Assistant(AssistantMessage {
        content: "Earlier message".into(),
    }));
    f.store.save(&target).await.unwrap();
    let mut active = Checkpoint::new(&f.store, target.id(), &target);
    active
        .save(&target, RunPhase::NeedModel, None)
        .await
        .unwrap();
    let result = f.send("web",json!({"action":"send","channel":"web","text":"Notification","conversation_id":target.id().as_str()})).await.unwrap().output;
    assert_eq!(result["status"], "pending");
    assert_eq!(
        f.store
            .load(target.id())
            .await
            .unwrap()
            .unwrap()
            .messages()
            .len(),
        1
    );
    target.push(Message::Assistant(AssistantMessage {
        content: "Completed answer".into(),
    }));
    active
        .save(&target, RunPhase::Completed, None)
        .await
        .unwrap();
    let delivery = result["delivery_id"].as_str().unwrap();
    f.tool.deliver(delivery).await.unwrap().unwrap();
    f.tool.deliver(delivery).await.unwrap().unwrap();
    let saved = f.store.load(target.id()).await.unwrap().unwrap();
    let texts: Vec<_> = saved
        .messages()
        .iter()
        .filter_map(|m| {
            if let Message::Assistant(a) = m {
                Some(a.content.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        texts,
        vec!["Earlier message", "Completed answer", "Notification"]
    );
}

#[tokio::test]
async fn messages_notification_preserves_original_final_reply() {
    let mut f = Fixture::new().await;
    f.send(
        "notify",
        json!({"action":"send","channel":"web","text":"Notification"}),
    )
    .await
    .unwrap();
    f.source.push(Message::Assistant(AssistantMessage {
        content: "Sent your notification".into(),
    }));
    f.checkpoint
        .save(&f.source, RunPhase::Completed, None)
        .await
        .unwrap();
    assert_eq!(
        f.store
            .pending_deliveries("telegram:test:42")
            .await
            .unwrap(),
        vec![f.checkpoint.record.id.clone()]
    );
}

#[tokio::test]
async fn messages_deleted_web_destination_does_not_retry_forever() {
    let f = Fixture::new().await;
    let target = Session::new(SessionId::new(uuid::Uuid::new_v4().to_string()));
    f.store.save(&target).await.unwrap();
    let id = f
        .store
        .queue_notification(
            &f.checkpoint.record.id,
            "deleted",
            &f.checkpoint.record.conversation_id,
            "web",
            target.id().as_str(),
            "Notification".into(),
        )
        .await
        .unwrap();
    f.store.delete(target.id()).await.unwrap();
    assert!(f.tool.deliver(&id).await.is_err());
    assert!(f.store.pending_notifications().await.unwrap().is_empty());
    assert!(f.store.load(target.id()).await.unwrap().is_none());
}

#[tokio::test]
async fn messages_pending_remote_delivery_recovers_without_tool_context() {
    let f = Fixture::new().await;
    f.recorder.failure.store(2, Ordering::SeqCst);
    let result = f.send("rate-limited", telegram()).await.unwrap().output;
    assert_eq!(result["status"], "pending");
    let id = result["delivery_id"].as_str().unwrap().to_owned();
    f.store
        .with_connection(|db| {
            db.execute(
                "UPDATE deliveries SET payload=CAST(json_set(payload,'$.retry_at',null) AS BLOB)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(f.tool.clone().run(receiver));
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !f.store.pending_notifications().await.unwrap().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    shutdown.send(true).unwrap();
    worker.await.unwrap();
    assert!(f.tool.deliver(&id).await.unwrap().is_some());
    assert_eq!(f.recorder.seen.lock().await.len(), 2);
}

#[tokio::test]
async fn messages_ready_notification_is_not_starved_by_blocked_backlog() {
    let f = Fixture::new().await;
    for i in 0..100 {
        f.store
            .queue_notification(
                &f.checkpoint.record.id,
                &format!("blocked-{i}"),
                &f.checkpoint.record.conversation_id,
                "telegram:removed",
                "42",
                "Old notification".into(),
            )
            .await
            .unwrap();
    }
    let ready = f
        .store
        .queue_notification(
            &f.checkpoint.record.id,
            "ready",
            &f.checkpoint.record.conversation_id,
            "web",
            "new",
            "Ready notification".into(),
        )
        .await
        .unwrap();
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(f.tool.clone().run(receiver));
    let delivered = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while f.store.notification_result(&ready).await.unwrap().is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    shutdown.send(true).unwrap();
    worker.await.unwrap();
    delivered.unwrap();
}
