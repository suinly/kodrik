use super::{
    session::{SessionId, SqliteSessionStore},
    tools::ToolRegistry,
    *,
};
use crate::{
    Interface, MessageId,
    gateway::{Event, IncomingMessage, InterfaceId},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
struct Model;
#[async_trait::async_trait]
impl llm::LlmProvider for Model {
    async fn generate(
        &self,
        _: &LlmContext<'_>,
        _: &[&dyn tools::Tool],
        _: Option<&tokio::sync::mpsc::Sender<AgentEvent>>,
    ) -> anyhow::Result<(Vec<message::Message>, Option<u64>)> {
        Ok((
            vec![message::Message::Assistant(AssistantMessage {
                content: "answer".into(),
            })],
            None,
        ))
    }
}
struct Chat(AtomicUsize, bool);
#[async_trait::async_trait]
impl Interface for Chat {
    fn id(&self) -> InterfaceId {
        InterfaceId("telegram:test".into())
    }
    fn allows_recipient(&self, c: &str) -> bool {
        c == "42"
    }
    async fn run(&self, _: tokio::sync::mpsc::Sender<Event>) -> anyhow::Result<()> {
        Ok(())
    }
    async fn send(&self, _: &str, _: &str) -> anyhow::Result<MessageId> {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        if self.1 && n == 0 {
            return Err(crate::interfaces::SendRejection {
                description: "rate".into(),
                error_code: 429,
                retry_after: Some(30),
            }
            .into());
        }
        Ok(MessageId("sent".into()))
    }
}
#[tokio::test]
async fn delivery_receipts_survive_terminal_crash_and_never_resend_unknown() {
    for crash_sending in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SqliteSessionStore::open_async(dir.path().into())
                .await
                .unwrap(),
        );
        let runner = AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(Model),
                ToolRegistry::default(),
                ContextBuilder::new("test"),
            )),
            store.clone(),
        );
        runner
            .run(
                SessionId::new("telegram:test:42"),
                IncomingMessage::new(InterfaceId("telegram:test".into()), "42", "hi"),
                None,
            )
            .await
            .unwrap();
        let ids = store.pending_deliveries("telegram:test:42").await.unwrap();
        assert_eq!(ids.len(), 1);
        if crash_sending {
            store
                .with_connection({
                    let id = ids[0].clone();
                    move |db| {
                        let payload: Vec<u8> = db.query_row(
                            "SELECT payload FROM deliveries WHERE id=?1",
                            [&id],
                            |r| r.get(0),
                        )?;
                        let mut value: serde_json::Value = serde_json::from_slice(&payload)?;
                        value["status"] = "Sending".into();
                        db.execute(
                            "UPDATE deliveries SET payload=?2 WHERE id=?1",
                            rusqlite::params![id, serde_json::to_vec(&value)?],
                        )?;
                        Ok(())
                    }
                })
                .await
                .unwrap();
        }
        drop(runner);
        drop(store);
        let store = SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap();
        let chat = Chat(AtomicUsize::new(0), false);
        store
            .deliver_receipt(&chat, &ids[0], chrono::Utc::now(), None)
            .await
            .unwrap();
        store
            .deliver_receipt(&chat, &ids[0], chrono::Utc::now(), None)
            .await
            .unwrap();
        assert_eq!(chat.0.load(Ordering::SeqCst), usize::from(!crash_sending));
        assert!(
            store
                .pending_deliveries("telegram:test:42")
                .await
                .unwrap()
                .is_empty()
        );
    }
}
#[tokio::test]
async fn delivery_retry_after_is_persisted_before_returning() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap(),
    );
    let runner = AgentRunner::new(
        Arc::new(Agent::new(
            Arc::new(Model),
            ToolRegistry::default(),
            ContextBuilder::new("test"),
        )),
        store.clone(),
    );
    runner
        .run(
            SessionId::new("telegram:test:42"),
            IncomingMessage::new(InterfaceId("telegram:test".into()), "42", "hi"),
            None,
        )
        .await
        .unwrap();
    let id = store
        .pending_deliveries("telegram:test:42")
        .await
        .unwrap()
        .remove(0);
    let chat = Chat(AtomicUsize::new(0), true);
    let now = chrono::Utc::now();
    assert!(
        store
            .deliver_receipt(&chat, &id, now, None)
            .await
            .unwrap()
            .is_none()
    );
    store
        .deliver_receipt(&chat, &id, now + chrono::Duration::seconds(29), None)
        .await
        .unwrap();
    assert_eq!(chat.0.load(Ordering::SeqCst), 1);
    assert!(
        store
            .deliver_receipt(&chat, &id, now + chrono::Duration::seconds(31), None)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(chat.0.load(Ordering::SeqCst), 2);
}

struct RichModel;
#[async_trait::async_trait]
impl llm::LlmProvider for RichModel {
    async fn generate(
        &self,
        context: &LlmContext<'_>,
        _: &[&dyn tools::Tool],
        _: Option<&tokio::sync::mpsc::Sender<AgentEvent>>,
    ) -> anyhow::Result<(Vec<message::Message>, Option<u64>)> {
        let output = if context
            .messages
            .iter()
            .any(|m| matches!(m, message::Message::ToolResult(_)))
        {
            message::Message::Assistant(AssistantMessage {
                content: "final should not duplicate rich reply".into(),
            })
        } else {
            message::Message::ToolCall(message::ToolCall {
                call_id: "send".into(),
                name: "telegram_reply".into(),
                arguments: serde_json::json!({"message":{"type":"post","title":"Title","body":"Rich reply"}}),
            })
        };
        Ok((vec![output], None))
    }
}
struct RichChat(AtomicUsize, bool);
#[async_trait::async_trait]
impl Interface for RichChat {
    fn id(&self) -> InterfaceId {
        InterfaceId("telegram:test".into())
    }
    fn allows_recipient(&self, c: &str) -> bool {
        c == "42"
    }
    async fn run(&self, _: tokio::sync::mpsc::Sender<Event>) -> anyhow::Result<()> {
        Ok(())
    }
    async fn send(&self, _: &str, _: &str) -> anyhow::Result<MessageId> {
        panic!("duplicate final reply")
    }
    async fn send_content(
        &self,
        _: &str,
        _: &crate::interfaces::OutgoingMessage,
    ) -> anyhow::Result<MessageId> {
        self.0.fetch_add(1, Ordering::SeqCst);
        if self.1 {
            anyhow::bail!("connection lost after send")
        };
        Ok(MessageId("rich".into()))
    }
}
#[tokio::test]
async fn explicit_delivery_receipt_suppresses_final_after_success_or_unknown() {
    for unknown in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SqliteSessionStore::open_async(dir.path().into())
                .await
                .unwrap(),
        );
        let registry = ToolRegistry::default().register(Arc::new(
            tools::telegram_reply::TelegramReplyTool::new(None),
        ));
        let runner = AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(RichModel),
                registry,
                ContextBuilder::new("test"),
            )),
            store.clone(),
        );
        let chat = Arc::new(RichChat(AtomicUsize::new(0), unknown));
        let reply = tools::telegram_reply::ReplyContext::new(chat.clone(), "42".into(), None);
        tools::telegram_reply::REPLY_CONTEXT
            .scope(
                reply,
                runner.run(
                    SessionId::new("telegram:test:42"),
                    IncomingMessage::new(InterfaceId("telegram:test".into()), "42", "hi"),
                    None,
                ),
            )
            .await
            .unwrap();
        assert_eq!(chat.0.load(Ordering::SeqCst), 1);
        assert!(
            store
                .pending_deliveries("telegram:test:42")
                .await
                .unwrap()
                .is_empty()
        );
        let count = store
            .with_connection(|db| {
                Ok(db.query_row("SELECT count(*) FROM deliveries", [], |r| {
                    r.get::<_, i64>(0)
                })?)
            })
            .await
            .unwrap();
        assert_eq!(count, 1);
    }
}

#[tokio::test]
async fn stopped_conversation_cancels_pending_delivery_retry() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap(),
    );
    let runner = AgentRunner::new(
        Arc::new(Agent::new(
            Arc::new(Model),
            ToolRegistry::default(),
            ContextBuilder::new("test"),
        )),
        store.clone(),
    );
    let conversation = SessionId::new("telegram:test:42");
    runner
        .run(
            conversation.clone(),
            IncomingMessage::new(InterfaceId("telegram:test".into()), "42", "hi"),
            None,
        )
        .await
        .unwrap();
    let id = store
        .pending_deliveries(conversation.as_str())
        .await
        .unwrap()
        .remove(0);
    let chat = Chat(AtomicUsize::new(0), true);
    let now = chrono::Utc::now();
    store.deliver_receipt(&chat, &id, now, None).await.unwrap();
    runner.cancel_conversation(&conversation).await.unwrap();
    store
        .deliver_receipt(&chat, &id, now + chrono::Duration::seconds(31), None)
        .await
        .unwrap();
    assert_eq!(chat.0.load(Ordering::SeqCst), 1);
}

struct CountingRichModel(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl llm::LlmProvider for CountingRichModel {
    async fn generate(
        &self,
        c: &LlmContext<'_>,
        t: &[&dyn tools::Tool],
        p: Option<&tokio::sync::mpsc::Sender<AgentEvent>>,
    ) -> anyhow::Result<(Vec<message::Message>, Option<u64>)> {
        self.0.fetch_add(1, Ordering::SeqCst);
        llm::LlmProvider::generate(&RichModel, c, t, p).await
    }
}
#[tokio::test]
async fn receipt_storage_failure_blocks_following_generation_and_effects() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap(),
    );
    store.with_connection(|db|{db.execute_batch("CREATE TRIGGER refuse_delivery BEFORE INSERT ON deliveries BEGIN SELECT RAISE(ABORT,'injected receipt failure'); END;")?;Ok(())}).await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let registry = ToolRegistry::default().register(Arc::new(
        tools::telegram_reply::TelegramReplyTool::new(None),
    ));
    let runner = AgentRunner::new(
        Arc::new(Agent::new(
            Arc::new(CountingRichModel(calls.clone())),
            registry,
            ContextBuilder::new("test"),
        )),
        store.clone(),
    );
    let chat = Arc::new(RichChat(AtomicUsize::new(0), false));
    let reply = tools::telegram_reply::ReplyContext::new(chat.clone(), "42".into(), None);
    assert!(
        tools::telegram_reply::REPLY_CONTEXT
            .scope(
                reply,
                runner.run(
                    SessionId::new("telegram:test:42"),
                    IncomingMessage::new(InterfaceId("telegram:test".into()), "42", "hi"),
                    None
                )
            )
            .await
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(chat.0.load(Ordering::SeqCst), 0);
}

struct PausedChat {
    calls: AtomicUsize,
    started: tokio::sync::mpsc::UnboundedSender<()>,
    release: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}
#[async_trait::async_trait]
impl Interface for PausedChat {
    fn id(&self) -> InterfaceId {
        InterfaceId("telegram:test".into())
    }
    fn allows_recipient(&self, chat: &str) -> bool {
        chat == "42"
    }
    async fn run(&self, _: tokio::sync::mpsc::Sender<Event>) -> anyhow::Result<()> {
        Ok(())
    }
    async fn send(&self, _: &str, _: &str) -> anyhow::Result<MessageId> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.started.send(()).unwrap();
            self.release.lock().await.take().unwrap().await.unwrap();
            Err(crate::interfaces::SendRejection {
                description: "rate".into(),
                error_code: 429,
                retry_after: Some(30),
            }
            .into())
        } else {
            Ok(MessageId("sent".into()))
        }
    }
}
#[tokio::test(start_paused = true)]
async fn inflight_delivery_stop_and_retry_after_respect_response_time() {
    for stopped in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SqliteSessionStore::open_async(dir.path().into())
                .await
                .unwrap(),
        );
        let runner = AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(Model),
                ToolRegistry::default(),
                ContextBuilder::new("test"),
            )),
            store.clone(),
        );
        let conversation = SessionId::new("telegram:test:42");
        runner
            .run(
                conversation.clone(),
                IncomingMessage::new(InterfaceId("telegram:test".into()), "42", "hi"),
                None,
            )
            .await
            .unwrap();
        let id = store
            .pending_deliveries(conversation.as_str())
            .await
            .unwrap()
            .remove(0);
        let now = chrono::Utc::now();
        let (started, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let chat = Arc::new(PausedChat {
            calls: AtomicUsize::new(0),
            started,
            release: tokio::sync::Mutex::new(Some(released)),
        });
        let task = tokio::spawn({
            let store = store.clone();
            let chat = chat.clone();
            let id = id.clone();
            async move { store.deliver_receipt(chat.as_ref(), &id, now, None).await }
        });
        seen.recv().await.unwrap();
        tokio::time::advance(std::time::Duration::from_secs(10)).await;
        if stopped {
            runner.cancel_conversation(&conversation).await.unwrap();
        }
        release.send(()).unwrap();
        task.await.unwrap().unwrap();
        store
            .deliver_receipt(
                chat.as_ref(),
                &id,
                now + chrono::Duration::seconds(31),
                None,
            )
            .await
            .unwrap();
        assert_eq!(chat.calls.load(Ordering::SeqCst), 1);
        store
            .deliver_receipt(
                chat.as_ref(),
                &id,
                now + chrono::Duration::seconds(41),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            chat.calls.load(Ordering::SeqCst),
            if stopped { 1 } else { 2 }
        );
    }
}
#[tokio::test]
async fn foreground_delivery_handoff_blocks_background_until_adapter_releases() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap(),
    );
    let runner = AgentRunner::new(
        Arc::new(Agent::new(
            Arc::new(Model),
            ToolRegistry::default(),
            ContextBuilder::new("test"),
        )),
        store.clone(),
    );
    let chat = Arc::new(Chat(AtomicUsize::new(0), false));
    let reply = tools::telegram_reply::ReplyContext::new(chat.clone(), "42".into(), None);
    tools::telegram_reply::REPLY_CONTEXT
        .scope(
            reply.clone(),
            runner.run(
                SessionId::new("telegram:test:42"),
                IncomingMessage::new(InterfaceId("telegram:test".into()), "42", "hi"),
                None,
            ),
        )
        .await
        .unwrap();
    // The adapter is still finalizing its preview after the Completed checkpoint.
    runner
        .deliver_pending(chat.as_ref(), "telegram:test:42")
        .await
        .unwrap();
    assert_eq!(chat.0.load(Ordering::SeqCst), 0);
    drop(reply);
    runner
        .deliver_pending(chat.as_ref(), "telegram:test:42")
        .await
        .unwrap();
    assert_eq!(chat.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn delivery_storage_failure_stops_pending_batch_before_next_effect() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap(),
    );
    let runner = AgentRunner::new(
        Arc::new(Agent::new(
            Arc::new(Model),
            ToolRegistry::default(),
            ContextBuilder::new("test"),
        )),
        store.clone(),
    );
    for _ in 0..2 {
        runner
            .run(
                SessionId::new("telegram:test:42"),
                IncomingMessage::new(InterfaceId("telegram:test".into()), "42", "hi"),
                None,
            )
            .await
            .unwrap();
    }
    assert_eq!(
        store
            .pending_deliveries("telegram:test:42")
            .await
            .unwrap()
            .len(),
        2
    );
    // Fail the outcome commit after the first HTTP send, leaving Sending durable.
    store.with_connection(|db|{db.execute_batch("CREATE TRIGGER refuse_sent BEFORE UPDATE ON deliveries WHEN json_extract(NEW.payload,'$.status')='Sent' BEGIN SELECT RAISE(ABORT,'disk failure'); END;")?;Ok(())}).await.unwrap();
    let chat = Chat(AtomicUsize::new(0), false);
    let error = runner
        .deliver_pending(&chat, "telegram:test:42")
        .await
        .unwrap_err();
    assert!(error.is::<super::delivery::StorageFailure>());
    assert_eq!(chat.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn foreground_delivery_reuses_handoff_ownership() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap(),
    );
    let runner = AgentRunner::new(
        Arc::new(Agent::new(
            Arc::new(Model),
            ToolRegistry::default(),
            ContextBuilder::new("test"),
        )),
        store.clone(),
    );
    let chat = Arc::new(Chat(AtomicUsize::new(0), false));
    let reply = tools::telegram_reply::ReplyContext::new(chat.clone(), "42".into(), None);
    let incoming = IncomingMessage::new(InterfaceId("telegram:test".into()), "42", "hi");
    let request = incoming.request_id.clone();
    let session = SessionId::new("telegram:test:42");
    tools::telegram_reply::REPLY_CONTEXT
        .scope(reply.clone(), runner.run(session.clone(), incoming, None))
        .await
        .unwrap();
    assert!(
        runner
            .deliver_final(chat.as_ref(), &session, &request, None, &reply)
            .await
            .unwrap()
            .is_some()
    );
    runner
        .deliver_pending(chat.as_ref(), session.as_str())
        .await
        .unwrap();
    assert_eq!(chat.0.load(Ordering::SeqCst), 1);
}
