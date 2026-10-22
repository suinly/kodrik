use super::{
    inbox::*,
    session::{SessionId, SqliteSessionStore},
};
use crate::gateway::{IncomingMessage, InterfaceId};
fn request(conversation: &str, key: &str, text: &str) -> SubmissionRequest {
    SubmissionRequest {
        conversation_id: conversation.into(),
        request_id: key.into(),
        mode: BusyMode::FollowUp,
        isolated: false,
        control: false,
        message: IncomingMessage::new(InterfaceId("test".into()), "chat", text),
    }
}
#[tokio::test]
async fn inbox_deduplicates_across_reopen_and_rejects_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    let first = store
        .submit(request("chat", "message:1", "hello"))
        .await
        .unwrap();
    let again = store
        .submit(request("chat", "message:1", "hello"))
        .await
        .unwrap();
    assert_eq!(first.id, again.id);
    assert_eq!(first.sequence, again.sequence);
    assert!(
        store
            .submit(request("chat", "message:1", "changed"))
            .await
            .is_err()
    );
    let mut changed = request("chat", "message:1", "hello");
    changed.mode = BusyMode::Steer;
    assert!(store.submit(changed).await.is_err());
    drop(store);
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    assert_eq!(
        store
            .submit(request("chat", "message:1", "hello"))
            .await
            .unwrap()
            .id,
        first.id
    );
    assert_ne!(
        store
            .submit(request("other-chat", "message:1", "hello"))
            .await
            .unwrap()
            .id,
        first.id
    );
}
#[tokio::test]
async fn inbox_concurrent_duplicates_reserve_one_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let mut tasks = vec![];
    for _ in 0..8 {
        let store = SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap();
        tasks.push(tokio::spawn(async move {
            store
                .submit(request("chat", "same", "hello"))
                .await
                .unwrap()
        }));
    }
    let first = tasks.remove(0).await.unwrap();
    for task in tasks {
        assert_eq!(task.await.unwrap().id, first.id);
    }
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    assert_eq!(
        store
            .submit(request("chat", "next", "hello"))
            .await
            .unwrap()
            .sequence,
        first.sequence + 1
    );
}
#[tokio::test]
async fn inbox_bounds_and_reject_if_busy_do_not_accept_work() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    let first = store.submit(request("chat", "0", "hello")).await.unwrap();
    let mut busy = request("chat", "rejected", "hello");
    busy.mode = BusyMode::RejectIfBusy;
    assert!(store.submit(busy).await.is_err());
    assert!(
        store
            .submission("chat", "rejected")
            .await
            .unwrap()
            .is_none()
    );
    for i in 1..MAX_QUEUED_SUBMISSIONS {
        store
            .submit(request("chat", &i.to_string(), "hello"))
            .await
            .unwrap();
    }
    assert!(
        store
            .submit(request("chat", "overflow", "hello"))
            .await
            .is_err()
    );
    assert_eq!(
        store
            .submit(request("chat", "0", "hello"))
            .await
            .unwrap()
            .id,
        first.id
    );
    assert_eq!(
        store
            .next_submission(&SessionId::new("chat"))
            .await
            .unwrap()
            .unwrap()
            .id,
        first.id
    );
}

struct InboxModel(std::sync::Arc<std::sync::atomic::AtomicUsize>);
#[async_trait::async_trait]
impl super::llm::LlmProvider for InboxModel {
    async fn generate(
        &self,
        _: &super::LlmContext<'_>,
        _: &[&dyn super::tools::Tool],
        _: Option<&tokio::sync::mpsc::Sender<super::AgentEvent>>,
    ) -> anyhow::Result<(Vec<super::message::Message>, Option<u64>)> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok((
            vec![super::message::Message::Assistant(
                super::AssistantMessage {
                    content: "done".into(),
                },
            )],
            None,
        ))
    }
}
#[tokio::test]
async fn inbox_runner_duplicate_reopen_never_generates_again() {
    let dir = tempfile::tempdir().unwrap();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let message = IncomingMessage::new(InterfaceId("cli".into()), "main", "original");
    for _ in 0..2 {
        let store = std::sync::Arc::new(
            SqliteSessionStore::open_async(dir.path().into())
                .await
                .unwrap(),
        );
        let runner = super::AgentRunner::new(
            std::sync::Arc::new(super::Agent::new(
                std::sync::Arc::new(InboxModel(calls.clone())),
                super::tools::ToolRegistry::default(),
                super::ContextBuilder::new("test"),
            )),
            store.clone(),
        );
        assert_eq!(
            runner
                .run(SessionId::new("cli:main"), message.clone(), None)
                .await
                .unwrap()
                .content,
            "done"
        );
        let record = store
            .submission("cli:main", &message.request_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.status, SubmissionStatus::Done);
        assert!(record.run_id.is_some());
        assert_eq!(record.session_id.as_deref(), Some("cli:main"));
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

struct ControlledInboxModel {
    contexts: tokio::sync::mpsc::UnboundedSender<Vec<super::message::Message>>,
    replies: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<bool>>,
}
#[async_trait::async_trait]
impl super::llm::LlmProvider for ControlledInboxModel {
    async fn generate(
        &self,
        context: &super::LlmContext<'_>,
        _: &[&dyn super::tools::Tool],
        _: Option<&tokio::sync::mpsc::Sender<super::AgentEvent>>,
    ) -> anyhow::Result<(Vec<super::message::Message>, Option<u64>)> {
        self.contexts.send(context.messages.to_vec()).unwrap();
        anyhow::ensure!(
            self.replies.lock().await.recv().await.unwrap(),
            "fixture generation failure"
        );
        Ok((
            vec![super::message::Message::Assistant(
                super::AssistantMessage {
                    content: "done".into(),
                },
            )],
            None,
        ))
    }
}
#[tokio::test]
async fn inbox_steering_is_durable_and_cancel_does_not_run_followups() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        for stop in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = std::sync::Arc::new(
                SqliteSessionStore::open_async(dir.path().into())
                    .await
                    .unwrap(),
            );
            let (contexts, mut seen) = tokio::sync::mpsc::unbounded_channel();
            let (replies, rx) = tokio::sync::mpsc::unbounded_channel();
            let runner = std::sync::Arc::new(super::AgentRunner::new(
                std::sync::Arc::new(super::Agent::new(
                    std::sync::Arc::new(ControlledInboxModel {
                        contexts,
                        replies: tokio::sync::Mutex::new(rx),
                    }),
                    super::tools::ToolRegistry::default(),
                    super::ContextBuilder::new("test"),
                )),
                store.clone(),
            ));
            let worker = runner.clone();
            let (cancel, cancelled) = tokio::sync::oneshot::channel();
            let (progress, mut events) = tokio::sync::mpsc::channel(32);
            let task = tokio::spawn(async move {
                worker
                    .run_with_cancel(
                        SessionId::new("chat"),
                        IncomingMessage::new(InterfaceId("test".into()), "chat", "initial"),
                        Some(progress),
                        Some(cancelled),
                        None,
                    )
                    .await
            });
            seen.recv().await.unwrap();
            while let Ok(event) = events.try_recv() {
                assert!(!matches!(event, super::AgentEvent::InputConsumed));
            }
            let mut steer = request("chat", "steer", "correction");
            steer.mode = BusyMode::Steer;
            store.submit(steer).await.unwrap();
            if stop {
                store
                    .submit(request("chat", "follow", "later"))
                    .await
                    .unwrap();
                cancel.send(()).unwrap();
                assert!(task.await.unwrap().is_err());
                assert_eq!(
                    store
                        .submission("chat", "steer")
                        .await
                        .unwrap()
                        .unwrap()
                        .status,
                    SubmissionStatus::Cancelled
                );
                assert_eq!(
                    store
                        .submission("chat", "follow")
                        .await
                        .unwrap()
                        .unwrap()
                        .status,
                    SubmissionStatus::Queued
                );
                assert!(
                    store
                        .next_submission(&SessionId::new("chat"))
                        .await
                        .unwrap()
                        .is_none()
                );
                assert!(store.inbox_blocked("chat").await.unwrap().is_some());
            } else {
                replies.send(true).unwrap();
                let context = seen.recv().await.unwrap();
                let mut consumed = 0;
                while let Ok(event) = events.try_recv() {
                    if matches!(event, super::AgentEvent::InputConsumed) {
                        consumed += 1;
                    }
                }
                assert_eq!(consumed, 1);
                assert!(context.iter().any(
                    |m| matches!(m,super::message::Message::User(u) if u.content=="correction")
                ));
                let steer = store.submission("chat", "steer").await.unwrap().unwrap();
                assert_eq!(steer.status, SubmissionStatus::Running);
                replies.send(true).unwrap();
                assert!(task.await.unwrap().is_ok());
                assert_eq!(
                    store
                        .submission("chat", "steer")
                        .await
                        .unwrap()
                        .unwrap()
                        .status,
                    SubmissionStatus::Done
                );
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn inbox_recovery_starts_admitted_unbound_work() {
    let dir = tempfile::tempdir().unwrap();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let store = SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap();
        store
            .submit(request("cli:main", "pending", "work"))
            .await
            .unwrap();
    }
    let store = std::sync::Arc::new(
        SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap(),
    );
    let runner = super::AgentRunner::new(
        std::sync::Arc::new(super::Agent::new(
            std::sync::Arc::new(InboxModel(calls.clone())),
            super::tools::ToolRegistry::default(),
            super::ContextBuilder::new("test"),
        )),
        store.clone(),
    );
    runner
        .recover(super::recovery::RecoveryTarget::Conversation(
            SessionId::new("cli:main"),
        ))
        .await
        .unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        store
            .submission("cli:main", "pending")
            .await
            .unwrap()
            .unwrap()
            .status,
        SubmissionStatus::Done
    );
}

#[tokio::test]
async fn inbox_control_survives_restart_and_new_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(
        SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap(),
    );
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let runner = super::AgentRunner::new(
        std::sync::Arc::new(super::Agent::new(
            std::sync::Arc::new(InboxModel(calls.clone())),
            super::tools::ToolRegistry::default(),
            super::ContextBuilder::new("test"),
        )),
        store.clone(),
    );
    let mut message = IncomingMessage::new(InterfaceId("cli".into()), "main", "/new");
    message.request_id = "command".into();
    runner
        .submit(SubmissionRequest::new(
            &SessionId::new("cli:main"),
            message.clone(),
            BusyMode::FollowUp,
        ))
        .await
        .unwrap();
    runner
        .recover(super::recovery::RecoveryTarget::Conversation(
            SessionId::new("cli:main"),
        ))
        .await
        .unwrap();
    use super::session::SessionStore;
    let first = store.active_id(&SessionId::new("cli:main")).await.unwrap();
    assert_ne!(first.as_str(), "cli:main");
    runner
        .apply_control(&SessionId::new("cli:main"), &message)
        .await
        .unwrap();
    assert_eq!(
        first,
        store.active_id(&SessionId::new("cli:main")).await.unwrap()
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn inbox_failed_run_blocks_fifo_until_new_input() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(
            SqliteSessionStore::open_async(dir.path().into())
                .await
                .unwrap(),
        );
        let (contexts, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let (replies, rx) = tokio::sync::mpsc::unbounded_channel();
        let runner = std::sync::Arc::new(super::AgentRunner::new(
            std::sync::Arc::new(super::Agent::new(
                std::sync::Arc::new(ControlledInboxModel {
                    contexts,
                    replies: tokio::sync::Mutex::new(rx),
                }),
                super::tools::ToolRegistry::default(),
                super::ContextBuilder::new("test"),
            )),
            store.clone(),
        ));
        let worker = runner.clone();
        let task = tokio::spawn(async move {
            worker
                .run(
                    SessionId::new("chat"),
                    IncomingMessage::new(InterfaceId("test".into()), "chat", "first"),
                    None,
                )
                .await
        });
        seen.recv().await.unwrap();
        store
            .submit(request("chat", "second", "second"))
            .await
            .unwrap();
        replies.send(false).unwrap();
        assert!(task.await.unwrap().is_err());
        assert!(
            store
                .next_submission(&SessionId::new("chat"))
                .await
                .unwrap()
                .is_none()
        );
        // Repeated delivery must not release the block.
        store
            .submit(request("chat", "second", "second"))
            .await
            .unwrap();
        assert!(
            store
                .next_submission(&SessionId::new("chat"))
                .await
                .unwrap()
                .is_none()
        );
        let worker = runner.clone();
        let task = tokio::spawn(async move {
            worker
                .run(
                    SessionId::new("chat"),
                    IncomingMessage::new(InterfaceId("test".into()), "chat", "third"),
                    None,
                )
                .await?;
            worker
                .recover(super::recovery::RecoveryTarget::Conversation(
                    SessionId::new("chat"),
                ))
                .await
        });
        for text in ["second", "third"] {
            let context = seen.recv().await.unwrap();
            let last = context.iter().rev().find_map(|m| {
                if let super::message::Message::User(u) = m {
                    Some(u.content.as_str())
                } else {
                    None
                }
            });
            assert_eq!(last, Some(text));
            replies.send(true).unwrap();
        }
        assert!(task.await.unwrap().is_ok());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn inbox_attachment_hash_ignores_download_path_but_not_contents() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open_async(dir.path().join("sessions"))
        .await
        .unwrap();
    let mut first = request("chat", "file", "read");
    first
        .message
        .files
        .push(super::file::FileAttachment::save(dir.path(), "a.txt", None, b"original").unwrap());
    let saved = store.submit(first.clone()).await.unwrap();
    let mut repeated = first.clone();
    repeated.message.files[0] =
        super::file::FileAttachment::save(dir.path(), "a.txt", None, b"original").unwrap();
    assert_eq!(store.submit(repeated).await.unwrap().id, saved.id);
    std::fs::remove_file(&first.message.files[0].path).unwrap();
    assert_eq!(store.submit(first.clone()).await.unwrap().id, saved.id);
    first.message.files[0] =
        super::file::FileAttachment::save(dir.path(), "a.txt", None, b"changed").unwrap();
    assert!(store.submit(first).await.is_err());
}

#[tokio::test]
async fn inbox_late_steering_keeps_original_session_after_new() {
    use super::{
        message::{Message, UserMessage},
        run_state::{Checkpoint, RunPhase},
        session::{Session, SessionStore},
    };
    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(
        SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap(),
    );
    let root = store
        .submit(request("chat", "root", "original"))
        .await
        .unwrap();
    let mut session = Session::new(SessionId::new("chat"));
    session.initialize_system_prompt("test");
    session.push(Message::User(UserMessage {
        content: "original".into(),
        files: vec![],
        images: vec![],
    }));
    let mut cp = Checkpoint::new(&store, &SessionId::new("chat"), &session);
    cp.record.isolated = Some(false);
    cp.record.submission_ids.push(root.id);
    cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
    let mut steer = request("chat", "late", "late correction");
    steer.mode = BusyMode::Steer;
    store.submit(steer).await.unwrap();
    session.push(Message::Assistant(super::AssistantMessage {
        content: "first answer".into(),
    }));
    cp.save(&session, RunPhase::Completed, None).await.unwrap();
    let new = store.new_session(&SessionId::new("chat")).await.unwrap();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let runner = super::AgentRunner::new(
        std::sync::Arc::new(super::Agent::new(
            std::sync::Arc::new(InboxModel(calls)),
            super::tools::ToolRegistry::default(),
            super::ContextBuilder::new("test"),
        )),
        store.clone(),
    );
    runner
        .recover(super::recovery::RecoveryTarget::Conversation(
            SessionId::new("chat"),
        ))
        .await
        .unwrap();
    let late = store.submission("chat", "late").await.unwrap().unwrap();
    assert_eq!(late.status, SubmissionStatus::Done);
    assert_eq!(late.session_id.as_deref(), Some("chat"));
    assert_eq!(
        store.active_id(&SessionId::new("chat")).await.unwrap(),
        *new.id()
    );
    assert!(
        store
            .load(&SessionId::new("chat"))
            .await
            .unwrap()
            .unwrap()
            .messages()
            .iter()
            .any(|m| matches!(m,Message::User(u) if u.content=="late correction"))
    );
}

#[tokio::test]
async fn inbox_routing_binding_and_active_mapping_commit_atomically() {
    use super::{
        run_state::{Checkpoint, RunPhase},
        session::{Session, SessionStore},
    };
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    let old = Session::new(SessionId::new("chat"));
    store.save(&old).await.unwrap();
    let record = store
        .submit(request("chat", "switch", "new task"))
        .await
        .unwrap();
    let new = Session::new(SessionId::new("new-session"));
    let mut cp = Checkpoint::new(&store, &SessionId::new("chat"), &new);
    cp.record.routing_from = Some("chat".into());
    cp.record.submission_ids.push(record.id.clone());
    store.with_connection(|db|{db.execute_batch("CREATE TRIGGER reject_run BEFORE INSERT ON runs BEGIN SELECT RAISE(ABORT,'injected disk failure'); END;")?;Ok(())}).await.unwrap();
    assert!(
        format!(
            "{:#}",
            cp.save(&new, RunPhase::NeedModel, None).await.unwrap_err()
        )
        .contains("injected disk failure")
    );
    assert_eq!(
        store.active_id(&SessionId::new("chat")).await.unwrap(),
        *old.id()
    );
    assert!(store.load(new.id()).await.unwrap().is_none());
    assert_eq!(
        store
            .submission("chat", "switch")
            .await
            .unwrap()
            .unwrap()
            .status,
        SubmissionStatus::Queued
    );
    store
        .with_connection(|db| {
            db.execute_batch("DROP TRIGGER reject_run")?;
            Ok(())
        })
        .await
        .unwrap();
    let mut cp = Checkpoint::new(&store, &SessionId::new("chat"), &new);
    cp.record.routing_from = Some("chat".into());
    cp.record.submission_ids.push(record.id);
    cp.save(&new, RunPhase::NeedModel, None).await.unwrap();
    assert_eq!(
        store.active_id(&SessionId::new("chat")).await.unwrap(),
        *new.id()
    );
    let record = store.submission("chat", "switch").await.unwrap().unwrap();
    assert_eq!(record.status, SubmissionStatus::Running);
    assert_eq!(record.session_id.as_deref(), Some("new-session"));
}

#[tokio::test]
async fn inbox_control_preserves_newer_input() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    use super::{
        run_state::{Checkpoint, RunPhase},
        session::Session,
    };
    let root = store
        .submit(request("chat", "root", "running"))
        .await
        .unwrap();
    let session = Session::new(SessionId::new("chat"));
    let mut cp = Checkpoint::new(&store, &SessionId::new("chat"), &session);
    cp.record.submission_ids.push(root.id);
    cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
    let mut before = request("chat", "before", "old");
    before.mode = BusyMode::Steer;
    store.submit(before).await.unwrap();
    let mut command = request("chat", "control", "/new");
    command.control = true;
    let command = store.submit(command).await.unwrap();
    let mut after = request("chat", "after", "new");
    after.mode = BusyMode::Steer;
    store.submit(after).await.unwrap();
    cp.save(&session, RunPhase::Cancelled, None).await.unwrap();
    store.commit_control(&command.id).await.unwrap();
    assert_eq!(
        store
            .submission("chat", "after")
            .await
            .unwrap()
            .unwrap()
            .status,
        SubmissionStatus::Queued
    );
    assert_eq!(
        store
            .submission("chat", "before")
            .await
            .unwrap()
            .unwrap()
            .status,
        SubmissionStatus::Cancelled
    );
    assert!(store.inbox_blocked("chat").await.unwrap().is_none());
}
#[tokio::test]
async fn inbox_independent_runs_keep_separate_results() {
    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(
        SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap(),
    );
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let runner = super::AgentRunner::new(
        std::sync::Arc::new(super::Agent::new(
            std::sync::Arc::new(InboxModel(calls.clone())),
            super::tools::ToolRegistry::default(),
            super::ContextBuilder::new("test"),
        )),
        store.clone(),
    );
    let first = store.submit(request("chat", "first", "one")).await.unwrap();
    let second = store
        .submit(request("chat", "second", "two"))
        .await
        .unwrap();
    runner
        .run(SessionId::new("chat"), first.request.message, None)
        .await
        .unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        store
            .submission("chat", "second")
            .await
            .unwrap()
            .unwrap()
            .status,
        SubmissionStatus::Queued
    );
    runner
        .run(SessionId::new("chat"), second.request.message, None)
        .await
        .unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}
