use std::{num::NonZeroU64, sync::Arc};

use anyhow::Context;

use crate::agent::{
    AgentRunner,
    session::{ExecutionOwnership, SessionId},
};

/// A reusable conversation slot held for the entire lifetime of one TUI.
/// Unlike execution locks, this also protects an idle instance's selection.
pub(super) struct Instance {
    pub conversation: SessionId,
    pub slot: u64,
    _ownership: Arc<ExecutionOwnership>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::{
            Agent, ContextBuilder, UserMessage,
            inbox::{BusyMode, SubmissionRequest},
            message::Message,
            recovery::RecoveryTarget,
            session::{Session, SessionStore, SqliteSessionStore},
            tools::ToolRegistry,
        },
        gateway::{IncomingMessage, InterfaceId},
    };

    struct Model;

    #[async_trait::async_trait]
    impl crate::agent::llm::LlmProvider for Model {
        async fn generate(
            &self,
            _: &crate::agent::LlmContext<'_>,
            _: &[&dyn crate::agent::tools::Tool],
            _: Option<&tokio::sync::mpsc::Sender<crate::agent::AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            Ok((
                vec![Message::Assistant(crate::agent::AssistantMessage {
                    content: "reply".into(),
                })],
                None,
            ))
        }
    }

    async fn runner(path: &std::path::Path) -> AgentRunner {
        AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(Model),
                ToolRegistry::default(),
                ContextBuilder::new("test"),
            )),
            Arc::new(SqliteSessionStore::open_async(path.into()).await.unwrap()),
        )
    }

    fn request(id: &SessionId) -> SubmissionRequest {
        SubmissionRequest::new(
            id,
            IncomingMessage::new(
                InterfaceId("cli".into()),
                id.as_str().strip_prefix("cli:").unwrap(),
                "hello",
            ),
            BusyMode::Steer,
        )
    }

    #[tokio::test]
    #[ignore = "Private subprocess fixture for slot_locks_are_released_after_process_exit"]
    async fn instance_process_fixture() {
        let path = std::path::PathBuf::from(std::env::var_os("KODRIK_TUI_SLOT_FIXTURE").unwrap());
        let runner = runner(&path.join("sessions")).await;
        let instance = Instance::claim(&runner, None).await.unwrap();
        assert_eq!(instance.conversation.as_str(), "cli:main");
        std::fs::write(path.join("ready"), "ready").unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }

    #[tokio::test]
    async fn slot_locks_are_released_after_process_exit() {
        // A second process must skip even an idle TUI; abrupt exit must not
        // leave a stale reservation behind.
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let mut child = Child(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "commands::tui::instance::tests::instance_process_fixture",
                    "--ignored",
                ])
                .env("KODRIK_TUI_SLOT_FIXTURE", dir.path())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !dir.path().join("ready").exists() {
                assert!(
                    child.0.try_wait().unwrap().is_none(),
                    "slot fixture exited early"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let runner = runner(&dir.path().join("sessions")).await;
        let second = Instance::claim(&runner, None).await.unwrap();
        assert_eq!(second.conversation.as_str(), "cli:tui:2");
        drop(child);
        let replacement = Instance::claim(&runner, None).await.unwrap();
        assert_eq!(replacement.conversation.as_str(), "cli:main");
    }

    #[tokio::test]
    async fn explicit_slot_restores_its_selection_and_rejects_an_existing_owner() {
        use std::num::NonZeroU64;

        let dir = tempfile::tempdir().unwrap();
        let runner = runner(dir.path()).await;
        let slot = NonZeroU64::new(2);
        let instance = Instance::claim(&runner, slot).await.unwrap();
        assert_eq!(instance.conversation.as_str(), "cli:tui:2");
        let selected = runner.new_session(&instance.conversation).await.unwrap();
        assert!(Instance::claim(&runner, slot).await.is_err());
        drop(instance);
        let resumed = Instance::claim(&runner, slot).await.unwrap();
        assert_eq!(resumed.conversation.as_str(), "cli:tui:2");
        assert_eq!(
            runner
                .load_session(resumed.conversation)
                .await
                .unwrap()
                .id(),
            selected.id()
        );
        let automatic = Instance::claim(&runner, None).await.unwrap();
        assert_eq!(automatic.conversation.as_str(), "cli:main");
    }

    #[tokio::test]
    async fn slot_errors_are_not_treated_as_occupied_slots() {
        let dir = tempfile::tempdir().unwrap();
        let runner = runner(dir.path()).await;
        std::fs::write(dir.path().join("execution-locks"), "not a directory").unwrap();
        let error = Instance::claim(&runner, None).await.err().unwrap();
        assert!(
            error
                .to_string()
                .contains("Could not reserve a TUI conversation")
        );
    }

    #[tokio::test]
    async fn parallel_instances_reserve_distinct_slots_and_reuse_released_slots() {
        let dir = tempfile::tempdir().unwrap();
        let first_runner = runner(dir.path()).await;
        let second_runner = runner(dir.path()).await;
        let (first, second) = tokio::join!(
            Instance::claim(&first_runner, None),
            Instance::claim(&second_runner, None)
        );
        let (first, second) = (first.unwrap(), second.unwrap());
        assert_ne!(first.conversation, second.conversation);
        let third = Instance::claim(&first_runner, None).await.unwrap();
        assert_eq!(third.conversation.as_str(), "cli:tui:3");
        let released = first.conversation.clone();
        drop(first);
        let replacement = Instance::claim(&second_runner, None).await.unwrap();
        assert_eq!(replacement.conversation, released);
        assert_ne!(replacement.conversation, second.conversation);
    }

    #[tokio::test]
    async fn selection_new_sessions_and_message_versions_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let a = runner(dir.path()).await;
        let b = runner(dir.path()).await;
        let first = Instance::claim(&a, None).await.unwrap();
        let second = Instance::claim(&b, None).await.unwrap();
        let mut original = Session::new(SessionId::new("saved"));
        original.push(Message::User(UserMessage {
            browser_client_id: None,
            browser_context: None,
            content: "original".into(),
            files: vec![],
            images: vec![],
        }));
        a.sqlite_store().unwrap().save(&original).await.unwrap();
        a.select_session(&first.conversation, original.id())
            .await
            .unwrap();
        b.select_session(&second.conversation, original.id())
            .await
            .unwrap();
        // Concurrent subprocess tests can briefly retain inherited lock files
        // between fork and exec. Tolerate only transient OS lock contention;
        // logical busy/admission failures must still fail this test immediately.
        let (_, edit) = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                match b
                    .edit_message(&second.conversation, original.id(), 0, "edited".into())
                    .await
                {
                    Err(error)
                        if matches!(
                            error.downcast_ref::<std::fs::TryLockError>(),
                            Some(std::fs::TryLockError::WouldBlock)
                        ) =>
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    result => break result.unwrap(),
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(edit.session_id(), second.conversation);
        b.run(second.conversation.clone(), edit, None)
            .await
            .unwrap();
        let edited = b.load_session(second.conversation.clone()).await.unwrap();
        assert_ne!(edited.id(), original.id());
        assert_eq!(
            a.load_session(first.conversation.clone())
                .await
                .unwrap()
                .id(),
            original.id()
        );
        a.new_session(&first.conversation).await.unwrap();
        assert_eq!(
            b.load_session(second.conversation.clone())
                .await
                .unwrap()
                .id(),
            edited.id()
        );
        a.select_session(&first.conversation, original.id())
            .await
            .unwrap();
        assert_eq!(
            b.load_session(second.conversation.clone())
                .await
                .unwrap()
                .id(),
            edited.id()
        );
        let listed = a.list_conversations().await.unwrap();
        assert!(listed.iter().any(|s| s.id() == original.id()));
    }

    #[tokio::test]
    async fn reused_secondary_slot_recovers_only_its_own_queue_and_selection() {
        let dir = tempfile::tempdir().unwrap();
        let a = runner(dir.path()).await;
        let b = runner(dir.path()).await;
        let first = Instance::claim(&a, None).await.unwrap();
        assert_eq!(first.conversation.as_str(), "cli:main");
        let second = Instance::claim(&b, None).await.unwrap();
        let selected = b.new_session(&second.conversation).await.unwrap();
        a.submit(request(&first.conversation)).await.unwrap();
        b.submit(request(&second.conversation)).await.unwrap();
        let id = second.conversation.clone();
        drop(second);
        drop(b);
        let restarted = runner(dir.path()).await;
        let resumed = Instance::claim(&restarted, None).await.unwrap();
        assert_eq!(resumed.conversation, id);
        assert_eq!(
            restarted.load_session(id.clone()).await.unwrap().id(),
            selected.id()
        );
        restarted
            .recover(RecoveryTarget::Conversation(id.clone()))
            .await
            .unwrap();
        let store = restarted.sqlite_store().unwrap();
        assert!(store.next_submission(&id).await.unwrap().is_none());
        assert!(
            store
                .next_submission(&first.conversation)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            restarted
                .load_session(id)
                .await
                .unwrap()
                .messages()
                .iter()
                .any(|m| matches!(m, Message::Assistant(_)))
        );
    }

    #[tokio::test]
    async fn shared_busy_history_cannot_be_steered_from_another_instance() {
        let dir = tempfile::tempdir().unwrap();
        let a = runner(dir.path()).await;
        let b = runner(dir.path()).await;
        let first = Instance::claim(&a, None).await.unwrap();
        let second = Instance::claim(&b, None).await.unwrap();
        let session = a.new_session(&first.conversation).await.unwrap();
        b.select_session(&second.conversation, session.id())
            .await
            .unwrap();
        a.submit(request(&first.conversation)).await.unwrap();
        assert!(b.submit(request(&second.conversation)).await.is_err());
        assert_eq!(
            b.load_session(second.conversation.clone())
                .await
                .unwrap()
                .id(),
            session.id()
        );
    }
}

impl Instance {
    pub async fn claim(
        runner: &AgentRunner,
        preferred: Option<NonZeroU64>,
    ) -> anyhow::Result<Self> {
        let store = runner.sqlite_store().context("TUI requires SQLite")?;
        for slot in preferred.map_or(1, NonZeroU64::get).. {
            // Preserve the existing default conversation and its recovery state.
            let conversation = SessionId::new(if slot == 1 {
                "cli:main".into()
            } else {
                format!("cli:tui:{slot}")
            });
            match store
                .execution_lock("tui-instance", conversation.as_str())
                .await
            {
                Ok(ownership) => {
                    return Ok(Self {
                        conversation,
                        slot,
                        _ownership: ownership,
                    });
                }
                Err(error)
                    if preferred.is_none()
                        && matches!(
                            error.downcast_ref::<std::fs::TryLockError>(),
                            Some(std::fs::TryLockError::WouldBlock)
                        ) =>
                {
                    continue;
                }
                Err(error) => return Err(error).context("Could not reserve a TUI conversation"),
            }
        }
        anyhow::bail!("No available TUI conversation slot")
    }
}
