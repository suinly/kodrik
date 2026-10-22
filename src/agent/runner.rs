use super::run_state::{Checkpoint, RunPhase};
use std::{collections::HashMap, sync::Arc};

use tokio::sync::{mpsc, oneshot};

#[path = "wakeups.rs"]
mod wakeups;

#[path = "group_reply.rs"]
mod group_reply;

#[path = "conversation_title.rs"]
mod conversation_title;

use crate::{
    agent::{
        Agent, AgentEvent, AssistantMessage, UserMessage,
        session::{Session, SessionId, SessionStore},
    },
    gateway::IncomingMessage,
};

pub(super) struct ConversationGuard {
    _local: tokio::sync::OwnedMutexGuard<()>,
    pub(super) ownership: Option<Arc<super::session::ExecutionOwnership>>,
}

pub struct AgentRunner {
    pub(super) agent: Arc<Agent>,
    pub(super) sessions: Arc<dyn SessionStore>,
    learning: Option<Arc<super::learning::LearningService>>,
    session_router: Option<super::session_router::SessionRouter>,
    group_decider: Option<super::system_one::SystemOne>,
    pub(super) restart_limit: u32,
    pub(super) shutdown: tokio::sync::watch::Sender<bool>,
    conversations: tokio::sync::Mutex<HashMap<SessionId, Arc<tokio::sync::Mutex<()>>>>,
}

impl AgentRunner {
    pub(crate) fn sqlite_store(&self) -> Option<super::session::SqliteSessionStore> {
        self.sessions.runtime_store().cloned()
    }

    pub(crate) fn for_web(&self, sessions: Arc<dyn SessionStore>) -> Self {
        let mut runner = Self::new(self.agent.clone(), sessions);
        runner.restart_limit = self.restart_limit;
        runner.learning = self.learning.clone();
        runner.shutdown = self.shutdown.clone();
        runner
    }

    pub fn new(agent: Arc<Agent>, sessions: Arc<dyn SessionStore>) -> Self {
        Self {
            agent,
            sessions,
            learning: None,
            restart_limit: 5,
            session_router: None,
            group_decider: None,
            conversations: Default::default(),
            shutdown: tokio::sync::watch::channel(false).0,
        }
    }

    pub(crate) fn with_restart_limit(mut self, limit: u32) -> Self {
        self.restart_limit = limit;
        self
    }

    pub(crate) fn with_learning(mut self, learning: Arc<super::learning::LearningService>) -> Self {
        self.learning = Some(learning);
        self
    }

    pub(crate) fn with_group_decider(mut self, decider: super::system_one::SystemOne) -> Self {
        self.group_decider = Some(decider);
        self
    }

    pub(crate) fn with_session_router(
        mut self,
        router: super::session_router::SessionRouter,
    ) -> Self {
        self.session_router = Some(router);
        self
    }

    pub(super) async fn lock_local_conversation(
        &self,
        id: &SessionId,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self
            .conversations
            .lock()
            .await
            .entry(id.clone())
            .or_default()
            .clone();
        lock.lock_owned().await
    }

    pub(super) async fn lock_conversation(
        &self,
        id: &SessionId,
    ) -> anyhow::Result<ConversationGuard> {
        let local = self.lock_local_conversation(id).await;
        let ownership = if let Some(store) = self.sessions.runtime_store() {
            let conversation = store.execution_lock("conversation", id.as_str()).await?;
            let active = store.active_id(id).await?;
            let session = store.execution_lock("session", active.as_str()).await?;
            Some(super::session::ExecutionOwnership::join(vec![
                conversation,
                session,
            ]))
        } else {
            None
        };
        Ok(ConversationGuard {
            _local: local,
            ownership,
        })
    }

    pub(crate) async fn conversation_busy(&self, id: &SessionId) -> bool {
        self.conversations
            .lock()
            .await
            .get(id)
            .is_some_and(|lock| lock.try_lock().is_err())
    }

    pub(crate) async fn finish_learning(&self) {
        if let Some(learning) = &self.learning {
            learning.finish().await;
        }
    }

    pub(crate) async fn deliver_proactive(
        &self,
        interface: &dyn crate::Interface,
        chat: &str,
        content: &str,
        activity: &std::sync::atomic::AtomicU64,
        expected: u64,
    ) -> anyhow::Result<bool> {
        let id = SessionId::new(format!("{}:{chat}", interface.id()));
        let _guard = self.lock_conversation(&id).await?;
        crate::agent::run_state::CURRENT_EXECUTION
            .scope(_guard.ownership.clone(), async {
                if activity.load(std::sync::atomic::Ordering::SeqCst) != expected {
                    return Ok(false);
                }
                let mut session = self.load_session(id).await?;
                let audit_context = crate::audit::Context::new(session.id().as_str(), "proactive");
                crate::audit::scope(
                    audit_context,
                    tokio::time::timeout(
                        std::time::Duration::from_secs(15),
                        interface.send(chat, content),
                    ),
                )
                .await??;
                self.record_sent_message(&mut session, content).await?;
                Ok(true)
            })
            .await
    }

    async fn record_sent_message(
        &self,
        session: &mut Session,
        content: &str,
    ) -> anyhow::Result<()> {
        session.push(crate::agent::message::Message::Assistant(
            AssistantMessage {
                content: content.into(),
            },
        ));
        self.sessions.save(session).await
    }

    pub(crate) async fn deliver_reminder(
        &self,
        interface: &dyn crate::Interface,
        store: &super::reminders::ReminderStore,
        reminder: &super::reminders::Reminder,
    ) -> anyhow::Result<()> {
        use chrono::Utc;
        let chat = &reminder.recipient.chat;
        let id = SessionId::new(format!("{}:{chat}", interface.id()));
        let _guard = self.lock_conversation(&id).await?;
        crate::agent::run_state::CURRENT_EXECUTION
            .scope(_guard.ownership.clone(), async {
                let mut session = self.load_session(id).await?;
                if interface.id().0 != reminder.recipient.interface
                    || !interface.allows_recipient(chat)
                {
                    return Ok(());
                }
                let claimed = {
                    let store = store.clone();
                    let id = reminder.id.clone();
                    crate::agent::run_state::spawn_owned_blocking(move || {
                        store.claim(&id, Utc::now())
                    })
                    .await??
                };
                let Some(claimed) = claimed else {
                    return Ok(());
                };
                anyhow::ensure!(
                    claimed.recipient == reminder.recipient,
                    "Reminder recipient changed during delivery"
                );
                let delay = (Utc::now() - claimed.due_at).num_seconds();
                let mut content = format!("⏰ {}", claimed.text);
                if delay > 5 {
                    content.push_str(&format!("\n\n⏱ +{delay}s"));
                }
                let audit_context = crate::audit::Context::new(session.id().as_str(), "reminder")
                    .with_source_id(&claimed.id);
                let outcome = crate::audit::scope(
                    audit_context,
                    tokio::time::timeout(
                        std::time::Duration::from_secs(15),
                        interface.send(chat, &content),
                    ),
                )
                .await;
                let result = match outcome {
                    Ok(result) => result,
                    Err(_) => Err(anyhow::anyhow!("Delivery timed out; outcome unknown")),
                };
                let (status, error, retry_at) = delivery_outcome(&result);
                let store = store.clone();
                let id = claimed.id;
                // Persist delivery outcome before history: a history error must never resend.
                crate::agent::run_state::spawn_owned_blocking(move || {
                    store.finish(&id, status, error, retry_at)
                })
                .await??;
                if result.is_ok() {
                    if let Err(error) = self.record_sent_message(&mut session, &content).await {
                        log::warn!("Reminder delivered but history could not be saved: {error:#}");
                    }
                } else {
                    log::warn!("Reminder delivery recorded as {status:?}");
                }
                Ok(())
            })
            .await
    }

    pub async fn run_once(&self, prompt: impl Into<String>) -> anyhow::Result<String> {
        let id = SessionId::new(format!("cli:once:{}", uuid::Uuid::new_v4()));
        let (tx, mut rx) = mpsc::channel(32);
        let display = async {
            while let Some(event) = rx.recv().await {
                if let AgentEvent::PlanUpdated(plan) = event {
                    eprintln!("{}", plan.render());
                }
            }
        };
        let work = self.run(
            id,
            IncomingMessage::new(
                crate::gateway::InterfaceId("cli".into()),
                "once",
                prompt.into(),
            ),
            Some(tx),
        );
        let (result, ()) = tokio::join!(work, display);
        Ok(result?.content)
    }

    pub async fn run(
        &self,
        session_id: SessionId,
        message: IncomingMessage,
        progress: Option<mpsc::Sender<AgentEvent>>,
    ) -> anyhow::Result<AssistantMessage> {
        self.run_with_cancel(session_id, message, progress, None, None)
            .await
    }

    /// Input left in `incoming` after completion belongs to a follow-up run.
    /// The caller keeps the receiver and waits for this run (including its save) to finish.
    pub async fn run_with_cancel(
        &self,
        session_id: SessionId,
        message: IncomingMessage,
        progress: Option<mpsc::Sender<AgentEvent>>,
        cancel: Option<oneshot::Receiver<()>>,
        incoming: Option<&mut mpsc::UnboundedReceiver<UserMessage>>,
    ) -> anyhow::Result<AssistantMessage> {
        let Some(store) = self.sessions.runtime_store() else {
            return self
                .run_single_with_cancel(session_id, message, progress, cancel, incoming, None)
                .await;
        };
        let mut request = super::inbox::SubmissionRequest::new(
            &session_id,
            message,
            super::inbox::BusyMode::FollowUp,
        );
        let submitted = match store
            .submission(session_id.as_str(), &request.request_id)
            .await?
        {
            Some(record) => {
                request.mode = record.request.mode;
                request.control = record.request.control;
                self.submit(request).await?
            }
            None => self.submit(request).await?,
        };
        if submitted.status != super::inbox::SubmissionStatus::Queued {
            return store.submission_answer(&submitted).await;
        }
        let mut cancel = cancel;
        let Some(next) = store.next_submission(&session_id).await? else {
            return Err(super::inbox::Observed.into());
        };
        if *self.shutdown_receiver().borrow() {
            return Err(super::recovery::Suspended.into());
        }
        anyhow::ensure!(
            next.request.isolated == self.agent.isolated(),
            "Submission execution permissions changed"
        );
        if next.request.message.is_group
            && next.request.message.guest_query_id.is_none()
            && !self
                .should_reply_to_group(&session_id, &next.request.message, &mut cancel)
                .await?
        {
            return Err(super::inbox::Observed.into());
        }
        self.run_single_with_cancel(
            session_id,
            next.request.message.clone(),
            progress,
            cancel,
            incoming,
            Some(next),
        )
        .await
    }

    async fn run_single_with_cancel(
        &self,
        session_id: SessionId,
        message: IncomingMessage,
        progress: Option<mpsc::Sender<AgentEvent>>,
        mut cancel: Option<oneshot::Receiver<()>>,
        mut incoming: Option<&mut mpsc::UnboundedReceiver<UserMessage>>,
        submission: Option<super::inbox::SubmissionRecord>,
    ) -> anyhow::Result<AssistantMessage> {
        let guard = self.lock_conversation(&session_id).await?;
        super::run_state::CURRENT_EXECUTION
            .scope(guard.ownership.clone(), async {
                if let Some(record) = &submission {
                    let store = self.sessions.runtime_store().unwrap();
                    let current = store
                        .submission_by_id(&record.id)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("Submission missing"))?;
                    if current.status != super::inbox::SubmissionStatus::Queued {
                        return store.submission_answer(&current).await;
                    }
                }
                let mut ownership: Vec<_> = guard.ownership.iter().cloned().collect();
                let mut session =
                    if let Some(bound) = submission.as_ref().and_then(|r| r.session_id.as_ref()) {
                        let store = self.sessions.runtime_store().unwrap();
                        if store.active_id(&session_id).await?.as_str() != bound {
                            ownership.push(store.execution_lock("session", bound).await?);
                        }
                        store
                            .load(&SessionId::new(bound))
                            .await?
                            .ok_or_else(|| anyhow::anyhow!("Bound submission session missing"))?
                    } else {
                        self.load_session(session_id.clone()).await?
                    };
                session.merge_telegram_group(message.telegram_group.as_ref());

                let recipient = (message.guest_query_id.is_none()
                    && message.source.interface.0.starts_with("telegram:"))
                .then_some(super::reminders::Recipient {
                    interface: message.source.interface.0,
                    chat: message.source.conversation_id,
                });

                let input = UserMessage {
                    files: message.files,
                    content: message.text,
                    images: message.images,
                };

                // Classify only at turn boundaries; incoming steering stays with this run.
                let mut routing_stopped = false;
                let mut switch = if submission.as_ref().is_some_and(|r| r.session_id.is_some()) {
                    false
                } else if let Some(router) = &self.session_router {
                    if let Some(cancel) = cancel.as_mut() {
                        tokio::select! {
                            biased;
                            _ = cancel => { routing_stopped = true; false },
                            decision = router.should_start_new(&session, &input) => decision,
                        }
                    } else {
                        router.should_start_new(&session, &input).await
                    }
                } else {
                    false
                };
                let previous_user_messages = session
                    .messages()
                    .iter()
                    .filter(|m| matches!(m, crate::agent::message::Message::User(_)))
                    .count();
                // Reserve notification capacity before changing persistent state. Cancellation
                // while a UI is slow must not leave its displayed session behind the store.
                let notification = if switch && let Some(tx) = progress.as_ref() {
                    if self.sessions.runtime_store().is_some() {
                        tx.try_reserve().ok()
                    } else if let Some(cancel) = cancel.as_mut() {
                        tokio::select! {
                            biased;
                            _ = cancel => { routing_stopped = true; switch = false; None },
                            permit = tx.reserve() => permit.ok(),
                        }
                    } else {
                        tx.reserve().await.ok()
                    }
                } else {
                    None
                };
                let routing_from = session.id().as_str().to_owned();
                if switch {
                    // Finish persistence outside cancellation: never abandon an active-id write.
                    let group = session.telegram_group().cloned();
                    session = if submission.is_some() {
                        Session::new(SessionId::new(uuid::Uuid::new_v4().to_string()))
                    } else {
                        self.sessions.new_session(&session_id).await?
                    };
                    if let Some(store) = self.sessions.runtime_store() {
                        ownership.push(
                            store
                                .execution_lock("session", session.id().as_str())
                                .await?,
                        );
                    }
                    session.merge_telegram_group(group.as_ref());
                    if let Some(permit) = notification {
                        permit.send(AgentEvent::SessionChanged {
                            previous_user_messages,
                        });
                    }
                }
                let initial_messages = session.messages().len();
                let wakeup_context = recipient.map(|recipient| super::reminders::WakeupContext {
                    recipient,
                    session: session.id().as_str().to_owned(),
                });
                let mut checkpoint = self
                    .sessions
                    .runtime_store()
                    .map(|store| Checkpoint::new(store, &session_id, &session));
                if let Some(cp) = checkpoint.as_mut() {
                    cp.ownership = Some(super::session::ExecutionOwnership::join(ownership));
                }
                if let Some(cp) = checkpoint.as_mut()
                    && let Some(record) = &submission
                {
                    cp.record.submission_ids.push(record.id.clone());
                    if switch {
                        cp.record.routing_from = Some(routing_from);
                    }
                }
                let response = super::reminders::WAKEUP_CONTEXT
                    .scope(wakeup_context, async {
                        if routing_stopped {
                            Err(anyhow::anyhow!("Stopped by user"))
                        } else {
                            self.execute_controlled(
                                &mut session,
                                input.clone(),
                                progress.as_ref(),
                                incoming.as_deref_mut(),
                                checkpoint.as_mut(),
                                cancel,
                            )
                            .await
                        }
                    })
                    .await;
                if response
                    .as_ref()
                    .is_err_and(|e| e.is::<super::recovery::Suspended>())
                {
                    if let Some(cp) = checkpoint.as_mut() {
                        cp.clean_suspend().await?;
                    }
                    return response;
                }
                if let Some(cp) = checkpoint.as_mut()
                    && cp.store.cancellation_requested(&cp.record.id).await?
                {
                    self.finish_cancelled(
                        &cp.store.clone(),
                        cp,
                        &mut session,
                        incoming.as_deref_mut(),
                    )
                    .await?;
                    return Err(anyhow::anyhow!("Stopped by user"));
                }
                if checkpoint.as_ref().is_some_and(|cp| cp.storage_uncertain) {
                    return response.and_then(|_| {
                        Err(anyhow::anyhow!(
                            "Durable commit unconfirmed; recovery required"
                        ))
                    });
                }
                if response.is_err() {
                    if session.messages().len() == initial_messages {
                        session.push(crate::agent::message::Message::User(input));
                    }
                    session.finish_interrupted_tools();
                    // Preserve accepted corrections even if generation fails or is stopped.
                    if let Some(incoming) = incoming {
                        while let Ok(input) = incoming.try_recv() {
                            session.push(crate::agent::message::Message::User(input));
                        }
                    }
                }
                self.finish_run(&session, checkpoint.as_mut(), &response)
                    .await?;

                if response.is_ok()
                    && let Some(learning) = &self.learning
                {
                    learning.schedule(&session, initial_messages);
                }

                response
            })
            .await
    }

    pub(super) async fn finish_run(
        &self,
        session: &Session,
        checkpoint: Option<&mut Checkpoint>,
        response: &anyhow::Result<AssistantMessage>,
    ) -> anyhow::Result<()> {
        if let Some(cp) = checkpoint {
            anyhow::ensure!(
                !cp.storage_uncertain,
                "Durable commit unconfirmed; recovery required"
            );
            if !cp.record.phase.terminal() {
                let phase = match response {
                    Ok(_) => RunPhase::Completed,
                    Err(error) if error.to_string() == "Stopped by user" => RunPhase::Cancelled,
                    Err(error) => RunPhase::Failed {
                        reason: format!("{error:#}"),
                    },
                };
                cp.save(session, phase, None).await?;
            }
            if response.is_ok()
                && cp.record.conversation_id.starts_with("telegram:")
                && cp
                    .record
                    .conversation_id
                    .rsplit(':')
                    .next()
                    .is_some_and(|chat| chat.parse::<u64>().is_ok_and(|id| id > 0))
                && session.telegram_group().is_none()
            {
                self.schedule_conversation_title(session.id().clone());
            }
            Ok(())
        } else {
            self.sessions.save(session).await
        }
    }

    pub(crate) async fn load_session(&self, id: SessionId) -> anyhow::Result<Session> {
        let id = self.sessions.active_id(&id).await?;
        Ok(self
            .sessions
            .load(&id)
            .await?
            .unwrap_or_else(|| Session::new(id)))
    }

    pub(crate) async fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
        Ok(self
            .sessions
            .list()
            .await?
            .into_iter()
            .filter(|s| !s.id().as_str().starts_with("companion:"))
            .collect())
    }

    pub(crate) async fn select_session(
        &self,
        conversation: &SessionId,
        id: &SessionId,
    ) -> anyhow::Result<Session> {
        let _guard = self.lock_conversation(conversation).await?;
        let mut ownership: Vec<_> = crate::agent::run_state::execution_ownership()
            .into_iter()
            .chain(_guard.ownership.iter().cloned())
            .collect();
        if let Some(store) = self.sessions.runtime_store() {
            if store.active_id(conversation).await? != *id {
                ownership.push(store.execution_lock("session", id.as_str()).await?);
            }
        }
        let ownership =
            (!ownership.is_empty()).then(|| super::session::ExecutionOwnership::join(ownership));
        crate::agent::run_state::CURRENT_EXECUTION
            .scope(ownership, async {
                match self.sessions.runtime_store() {
                    Some(store) => store.select_idle_session(conversation, id).await,
                    None => self.sessions.select(conversation, id).await,
                }
            })
            .await
    }

    pub(crate) async fn new_session(&self, conversation: &SessionId) -> anyhow::Result<Session> {
        let _guard = self.lock_conversation(conversation).await?;
        crate::agent::run_state::CURRENT_EXECUTION
            .scope(_guard.ownership.clone(), async {
                self.sessions.new_session(conversation).await
            })
            .await
    }
}

fn delivery_outcome(
    result: &anyhow::Result<crate::MessageId>,
) -> (
    super::reminders::Status,
    Option<String>,
    Option<chrono::DateTime<chrono::Utc>>,
) {
    use super::reminders::Status;
    use chrono::{Duration, Utc};
    match result {
        Ok(_) => (Status::Sent, None, None),
        Err(error) => {
            let (status, retry) =
                if let Some(rejection) = error.downcast_ref::<crate::interfaces::SendRejection>() {
                    match rejection.retry_after {
                        Some(seconds) => {
                            let retry = i64::try_from(seconds.max(60))
                                .ok()
                                .and_then(Duration::try_seconds)
                                .and_then(|delay| Utc::now().checked_add_signed(delay));
                            if retry.is_some() {
                                (Status::Pending, retry)
                            } else {
                                (Status::Failed, None)
                            }
                        }
                        None => (Status::Failed, None),
                    }
                } else {
                    (Status::Uncertain, None)
                };
            (status, Some(format!("{error:#}")), retry)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{
        ContextBuilder, Image, LlmContext,
        llm::LlmProvider,
        message::{Message, ToolCall},
        session::JsonSessionStore,
        tools::{Tool, ToolRegistry, ViewImageTool},
    };
    use crate::gateway::InterfaceId;

    #[tokio::test]
    async fn selecting_shared_sessions_rejects_queued_source_and_busy_target() {
        use crate::agent::{
            inbox::{BusyMode, SubmissionRequest},
            session::SqliteSessionStore,
        };
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SqliteSessionStore::open_async(dir.path().into())
                .await
                .unwrap(),
        );
        let runner = AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(ReplyLlm {
                    reply: "done",
                    fail: false,
                    calls: Default::default(),
                }),
                ToolRegistry::default(),
                ContextBuilder::new("test"),
            )),
            store.clone(),
        );
        let mut source = Session::new(SessionId::new("source"));
        let mut target = Session::new(SessionId::new("target"));
        for session in [&mut source, &mut target] {
            session.push(Message::User(UserMessage {
                content: "original history".into(),
                files: vec![],
                images: vec![],
            }));
        }
        store.save(&source).await.unwrap();
        store.save(&target).await.unwrap();
        let telegram = SessionId::new("telegram:bot:42");
        let web = SessionId::new("web:legacy");
        store.select(&telegram, target.id()).await.unwrap();
        store.select(&web, source.id()).await.unwrap();
        store
            .submit(SubmissionRequest::new(
                &telegram,
                IncomingMessage::new(InterfaceId("telegram:bot".into()), "42", "queued"),
                BusyMode::FollowUp,
            ))
            .await
            .unwrap();
        assert!(runner.select_session(&web, target.id()).await.is_err());
        assert_eq!(store.active_id(&web).await.unwrap(), *source.id());
        assert!(runner.select_session(&telegram, source.id()).await.is_err());
        assert_eq!(store.active_id(&telegram).await.unwrap(), *target.id());
        for session in [&source, &target] {
            let saved = store.load(session.id()).await.unwrap().unwrap();
            assert_eq!(
                serde_json::to_value(saved.messages()).unwrap(),
                serde_json::to_value(session.messages()).unwrap()
            );
        }
        let guard = store.execution_lock("session", "source").await.unwrap();
        assert!(
            runner
                .select_session(&SessionId::new("other"), source.id())
                .await
                .is_err()
        );
        drop(guard);
        assert_eq!(
            runner
                .select_session(&SessionId::new("other"), source.id())
                .await
                .unwrap()
                .id(),
            source.id()
        );
        let live = Session::new(SessionId::new("live"));
        store.save(&live).await.unwrap();
        let channel = SessionId::new("cli:main");
        store.select(&channel, live.id()).await.unwrap();
        let mut cp = Checkpoint::new(&store, &channel, &live);
        cp.save(&live, RunPhase::NeedModel, None).await.unwrap();
        assert!(runner.select_session(&web, live.id()).await.is_err());
        assert_eq!(store.active_id(&web).await.unwrap(), *source.id());
    }

    struct ReplyLlm {
        reply: &'static str,
        fail: bool,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for ReplyLlm {
        async fn generate(
            &self,
            _: &LlmContext<'_>,
            _: &[&dyn Tool],
            _: Option<&mpsc::Sender<AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            anyhow::ensure!(!self.fail, "test failure");
            Ok((answer(self.reply), None))
        }
    }

    #[tokio::test]
    async fn successful_conversations_and_one_shots_schedule_evidence_based_learning() {
        use crate::agent::{companion::CompanionStore, learning::LearningService};
        for (once, fail, text, expected) in [
            (false, false, "Мне нравится история городов", 1),
            (true, false, "Мне нравится история городов", 1),
            (false, true, "Мне нравится история городов", 0),
            (false, false, "Спасибо!", 0),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let foreground = Arc::new(ReplyLlm {
                reply: "Давай обсудим",
                fail,
                calls: 0.into(),
            });
            let review = Arc::new(ReplyLlm {
                reply: r#"{"proposals":[{"kind":"fact","content":"Интересуется историей городов","evidence":[{"index":0,"quote":"Мне нравится история городов"}],"name":null,"description":null,"question":null,"why":null}]}"#,
                fail: false,
                calls: 0.into(),
            });
            let runner = AgentRunner::new(
                Arc::new(Agent::new(
                    foreground,
                    ToolRegistry::default(),
                    ContextBuilder::new("test"),
                )),
                Arc::new(JsonSessionStore::new(dir.path().join("sessions"))),
            )
            .with_learning(Arc::new(
                LearningService::new(CompanionStore::new(dir.path()), review.clone())
                    .with_journal(crate::audit::Journal::new(dir.path().join("actions"))),
            ));
            let result = if once {
                runner.run_once(text).await
            } else {
                runner
                    .run(
                        SessionId::new("cli:main"),
                        IncomingMessage::new(InterfaceId("cli".into()), "main", text),
                        None,
                    )
                    .await
                    .map(|m| m.content)
            };
            assert_eq!(result.is_err(), fail);
            runner.finish_learning().await;
            assert_eq!(
                review.calls.load(std::sync::atomic::Ordering::SeqCst),
                expected
            );
            assert_eq!(
                CompanionStore::new(dir.path())
                    .snapshot()
                    .unwrap()
                    .contains("Интересуется историей городов"),
                expected == 1
            );
            assert_eq!(runner.list_sessions().await.unwrap().len(), 1);
            if expected == 1 {
                let file = dir
                    .path()
                    .join("actions")
                    .join(format!("{}.jsonl", chrono::Utc::now().format("%Y-%m-%d")));
                let text = std::fs::read_to_string(file).unwrap();
                let rows: Vec<serde_json::Value> = text
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0]["trigger"], "learning");
                assert_eq!(rows[1]["details"]["learned_entries"], 1);
                assert!(!text.contains("Интересуется"));
            }
        }
    }

    #[tokio::test]
    async fn routing_preserves_old_history_and_only_explicit_new_switches() {
        use crate::agent::{session_router::SessionRouter, system_one::SystemOne};
        for (decision, switch) in [
            ("new", true),
            ("continue", false),
            ("uncertain", false),
            ("invalid", false),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let store = Arc::new(JsonSessionStore::new(directory.path()));
            let conversation = SessionId::new("cli:main");
            let mut original = Session::new(conversation.clone());
            original.push(Message::User(user("old topic")));
            original.push(Message::Assistant(AssistantMessage {
                content: "old answer".into(),
            }));
            store.save(&original).await.unwrap();
            let (requests, mut contexts) = mpsc::unbounded_channel();
            let (replies, responses) = mpsc::unbounded_channel();
            let router_reply = match decision {
                "new" => r#"{"choice":"new"}"#,
                "continue" => r#"{"choice":"continue"}"#,
                "uncertain" => r#"{"choice":"uncertain"}"#,
                _ => "malformed",
            };
            let runner = Arc::new(
                AgentRunner::new(
                    Arc::new(Agent::new(
                        Arc::new(ControlledLlm {
                            requests,
                            replies: tokio::sync::Mutex::new(responses),
                        }),
                        ToolRegistry::default(),
                        ContextBuilder::new("test"),
                    )),
                    store.clone(),
                )
                .with_session_router(SessionRouter::new(
                    SystemOne::new(Arc::new(ReplyLlm {
                        reply: router_reply,
                        fail: false,
                        calls: 0.into(),
                    })),
                    0,
                )),
            );
            let task_runner = runner.clone();
            let (tx, mut events) = mpsc::channel(16);
            let task = tokio::spawn(async move {
                let mut message =
                    IncomingMessage::new(InterfaceId("cli".into()), "main", "new topic");
                message.telegram_group = Some(
                    serde_json::from_value(serde_json::json!({
                        "chat_id":-42,"name":"Group","bot_username":"@TestBot",
                        "participants":{"user:11":{"name":"Alice"}},
                    }))
                    .unwrap(),
                );
                task_runner
                    .run(SessionId::new("cli:main"), message, Some(tx))
                    .await
            });
            let context = contexts.recv().await.unwrap();
            assert_eq!(context.len(), if switch { 1 } else { 3 });
            assert!(matches!(context.last(), Some(Message::User(m)) if m.content == "new topic"));
            replies.send(Ok(answer("done"))).unwrap();
            task.await.unwrap().unwrap();
            let active = runner.load_session(conversation.clone()).await.unwrap();
            assert_eq!(active.id() != &conversation, switch);
            assert_eq!(active.messages().len(), if switch { 2 } else { 4 });
            assert_eq!(
                active.telegram_group().unwrap().participants["user:11"]
                    .name
                    .as_deref(),
                Some("Alice")
            );
            if switch {
                assert_eq!(
                    store
                        .load(&conversation)
                        .await
                        .unwrap()
                        .unwrap()
                        .messages()
                        .len(),
                    2
                );
                assert!(matches!(
                    events.recv().await,
                    Some(AgentEvent::SessionChanged { .. })
                ));
            }
            assert_eq!(store.active_id(&conversation).await.unwrap(), *active.id());
        }
    }

    #[tokio::test]
    async fn cancellation_during_routing_preserves_input_and_steering() {
        use crate::agent::{session_router::SessionRouter, system_one::SystemOne};
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(JsonSessionStore::new(directory.path()));
        let id = SessionId::new("cli:main");
        let mut original = Session::new(id.clone());
        original.push(Message::User(user("old topic")));
        store.save(&original).await.unwrap();
        let (requests, mut contexts) = mpsc::unbounded_channel();
        let (_replies, responses) = mpsc::unbounded_channel();
        let foreground = Arc::new(ReplyLlm {
            reply: "unused",
            fail: false,
            calls: 0.into(),
        });
        let runner = Arc::new(
            AgentRunner::new(
                Arc::new(Agent::new(
                    foreground.clone(),
                    ToolRegistry::default(),
                    ContextBuilder::new("test"),
                )),
                store.clone(),
            )
            .with_session_router(SessionRouter::new(
                SystemOne::new(Arc::new(ControlledLlm {
                    requests,
                    replies: tokio::sync::Mutex::new(responses),
                })),
                0,
            )),
        );
        let (cancel, cancelled) = oneshot::channel();
        let (tx, mut incoming) = mpsc::unbounded_channel();
        let task_runner = runner.clone();
        let task = tokio::spawn(async move {
            task_runner
                .run_with_cancel(
                    SessionId::new("cli:main"),
                    IncomingMessage::new(InterfaceId("cli".into()), "main", "new topic"),
                    None,
                    Some(cancelled),
                    Some(&mut incoming),
                )
                .await
        });
        contexts.recv().await.unwrap();
        tx.send(user("correction")).unwrap();
        cancel.send(()).unwrap();
        assert_eq!(
            task.await.unwrap().unwrap_err().to_string(),
            "Stopped by user"
        );
        let saved = runner.load_session(id.clone()).await.unwrap();
        assert_eq!(saved.id(), &id);
        assert_eq!(saved.messages().len(), 3);
        assert!(matches!(&saved.messages()[1], Message::User(m) if m.content == "new topic"));
        assert!(matches!(&saved.messages()[2], Message::User(m) if m.content == "correction"));
        assert_eq!(
            foreground.calls.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    struct ReminderRaceInterface(std::sync::atomic::AtomicUsize);
    #[async_trait::async_trait]
    impl crate::Interface for ReminderRaceInterface {
        fn id(&self) -> InterfaceId {
            InterfaceId("telegram:test".into())
        }
        fn allows_recipient(&self, _: &str) -> bool {
            true
        }
        async fn run(&self, _: mpsc::Sender<crate::gateway::Event>) -> anyhow::Result<()> {
            Ok(())
        }
        async fn send(&self, _: &str, _: &str) -> anyhow::Result<crate::MessageId> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::MessageId("1".into()))
        }
    }

    #[tokio::test]
    async fn reminders_recheck_cancel_and_reschedule_after_waiting_for_chat() {
        use super::super::reminders::{Recipient, ReminderStore};
        for action in ["cancel", "reschedule"] {
            let directory = tempfile::tempdir().unwrap();
            let runner = Arc::new(AgentRunner::new(
                Arc::new(Agent::new(
                    Arc::new(ReminderLlm),
                    ToolRegistry::default(),
                    ContextBuilder::new("test"),
                )),
                Arc::new(JsonSessionStore::new(directory.path().join("sessions"))),
            ));
            let store = ReminderStore::new(directory.path());
            let recipient = Recipient {
                interface: "telegram:test".into(),
                chat: "42".into(),
            };
            let now = chrono::Utc::now();
            let created = store.execute(&recipient, serde_json::from_value(serde_json::json!({"action":"create","text":"Call","due_at":(now - chrono::Duration::minutes(1)).to_rfc3339()})).unwrap(), now - chrono::Duration::minutes(2)).unwrap();
            let id = created["reminder"]["id"].as_str().unwrap();
            let reminder = store.due(now).unwrap().pop().unwrap();
            let guard = runner
                .lock_conversation(&SessionId::new("telegram:test:42"))
                .await;
            let interface = Arc::new(ReminderRaceInterface(Default::default()));
            let (delivery_runner, delivery_store, delivery_interface) =
                (runner.clone(), store.clone(), interface.clone());
            let task = tokio::spawn(async move {
                delivery_runner
                    .deliver_reminder(delivery_interface.as_ref(), &delivery_store, &reminder)
                    .await
                    .unwrap();
            });
            tokio::task::yield_now().await;
            let due = if action == "reschedule" {
                Some((now + chrono::Duration::hours(1)).to_rfc3339())
            } else {
                None
            };
            store
                .execute(
                    &recipient,
                    serde_json::from_value(
                        serde_json::json!({"action":action,"id":id,"due_at":due}),
                    )
                    .unwrap(),
                    now,
                )
                .unwrap();
            drop(guard);
            task.await.unwrap();
            assert_eq!(interface.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        }
    }

    struct ReminderLlm;

    #[tokio::test]
    async fn wakeups_recheck_cancel_and_reschedule_after_waiting_for_chat() {
        use super::super::reminders::{Recipient, ReminderStore};
        for action in ["cancel", "reschedule"] {
            let directory = tempfile::tempdir().unwrap();
            let runner = Arc::new(AgentRunner::new(
                Arc::new(Agent::new(
                    Arc::new(ReminderLlm),
                    ToolRegistry::default(),
                    ContextBuilder::new("test"),
                )),
                Arc::new(JsonSessionStore::new(directory.path().join("sessions"))),
            ));
            let store = ReminderStore::wakeups(directory.path());
            let recipient = Recipient {
                interface: "telegram:test".into(),
                chat: "42".into(),
            };
            let now = chrono::Utc::now();
            let created = store.execute_wakeup(&recipient, "source", serde_json::from_value(serde_json::json!({"action":"create", "text":"Check", "due_at":(now - chrono::Duration::minutes(1)).to_rfc3339()})).unwrap(), now - chrono::Duration::minutes(2)).unwrap();
            let id = created["wakeup"]["id"].as_str().unwrap();
            let wakeup = store.due(now).unwrap().pop().unwrap();
            let guard = runner
                .lock_conversation(&SessionId::new("telegram:test:42"))
                .await;
            let interface = Arc::new(ReminderRaceInterface(Default::default()));
            let (waiting_runner, waiting_store, waiting_interface) =
                (runner.clone(), store.clone(), interface.clone());
            let task = tokio::spawn(async move {
                waiting_runner
                    .execute_wakeup(waiting_interface.as_ref(), &waiting_store, &wakeup, now)
                    .await
                    .unwrap();
            });
            tokio::task::yield_now().await;
            let due = if action == "reschedule" {
                Some((now + chrono::Duration::hours(1)).to_rfc3339())
            } else {
                None
            };
            store
                .execute_wakeup(
                    &recipient,
                    "source",
                    serde_json::from_value(
                        serde_json::json!({"action":action, "id":id, "due_at":due}),
                    )
                    .unwrap(),
                    now,
                )
                .unwrap();
            drop(guard);
            task.await.unwrap();
            assert_eq!(interface.0.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert!(runner.list_sessions().await.unwrap().is_empty());
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for ReminderLlm {
        async fn generate(
            &self,
            context: &LlmContext<'_>,
            tools: &[&dyn Tool],
            _: Option<&mpsc::Sender<AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            let available = tools.iter().any(|tool| tool.name() == "wakeups");
            let Message::User(input) = &context.messages[0] else {
                panic!("missing input")
            };
            if input.content == "cli" {
                assert!(!available);
                return Ok((answer("no wakeups"), None));
            }
            assert!(available);
            if context.messages.len() == 1 {
                Ok((
                    vec![Message::ToolCall(ToolCall {
                        call_id: "create".into(),
                        name: "wakeups".into(),
                        arguments: serde_json::json!({"action":"create","instruction":input.content,"due_at":(chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339()}),
                    })],
                    None,
                ))
            } else {
                let Message::ToolResult(result) = context.messages.last().unwrap() else {
                    panic!("missing result")
                };
                assert_eq!(result.content.output["wakeup"]["text"], input.content);
                Ok((answer("scheduled"), None))
            }
        }
    }

    #[tokio::test]
    async fn wakeups_runner_binds_source_and_hides_tool_in_cli() {
        let directory = tempfile::tempdir().unwrap();
        let runner = Arc::new(AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(ReminderLlm),
                ToolRegistry::default().register(Arc::new(super::super::tools::WakeupTool::new(
                    directory.path(),
                ))),
                ContextBuilder::new("test"),
            )),
            Arc::new(JsonSessionStore::new(directory.path().join("sessions"))),
        ));
        let mut tasks = vec![];
        for chat in ["42", "7"] {
            let runner = runner.clone();
            tasks.push(tokio::spawn(async move {
                runner
                    .run(
                        SessionId::new(format!("telegram:test:{chat}")),
                        IncomingMessage::new(InterfaceId("telegram:test".into()), chat, chat),
                        None,
                    )
                    .await
                    .unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(runner.run_once("cli").await.unwrap(), "no wakeups");
        let records: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.path().join("wakeups.json")).unwrap())
                .unwrap();
        assert_eq!(records.as_array().unwrap().len(), 2);
        for record in records.as_array().unwrap() {
            assert_eq!(record["text"], record["recipient"]["chat"]);
        }
    }

    struct ControlledLlm {
        requests: mpsc::UnboundedSender<Vec<Message>>,
        replies: tokio::sync::Mutex<mpsc::UnboundedReceiver<anyhow::Result<Vec<Message>>>>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for ControlledLlm {
        async fn generate(
            &self,
            context: &LlmContext<'_>,
            _: &[&dyn Tool],
            _: Option<&mpsc::Sender<AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            self.requests.send(context.messages.to_vec()).unwrap();
            Ok((self.replies.lock().await.recv().await.unwrap()?, None))
        }
    }

    struct BlockingTool {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl Tool for BlockingTool {
        fn name(&self) -> String {
            "blocking".into()
        }
        fn description(&self) -> String {
            "Wait for test".into()
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            _: serde_json::Value,
        ) -> anyhow::Result<crate::agent::tools::ToolOutput> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(serde_json::json!({"done": true}).into())
        }
    }

    fn user(content: &str) -> UserMessage {
        UserMessage {
            files: Vec::new(),
            content: content.into(),
            images: Vec::new(),
        }
    }

    fn answer(content: &str) -> Vec<Message> {
        vec![Message::Assistant(AssistantMessage {
            content: content.into(),
        })]
    }

    #[tokio::test]
    async fn corrections_wait_for_tool_results_and_preserve_images_in_order() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let (requests, mut contexts) = mpsc::unbounded_channel();
            let (replies, responses) = mpsc::unbounded_channel();
            let started = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let directory = tempfile::tempdir().unwrap();
            let runner = Arc::new(AgentRunner::new(
                Arc::new(Agent::new(Arc::new(ControlledLlm {
                    requests, replies: tokio::sync::Mutex::new(responses),
                }), ToolRegistry::default().register(Arc::new(BlockingTool {
                    started: started.clone(), release: release.clone(),
                })), ContextBuilder::new("test"))),
                Arc::new(JsonSessionStore::new(directory.path())),
            ));
            let (input_tx, mut input_rx) = mpsc::unbounded_channel();
            let task_runner = runner.clone();
            let task = tokio::spawn(async move {
                task_runner.run_with_cancel(SessionId::new("steer"),
                    IncomingMessage::new(InterfaceId("test".into()), "chat", "initial"),
                    None, None, Some(&mut input_rx)).await
            });
            assert_eq!(contexts.recv().await.unwrap().len(), 1);
            input_tx.send(user("during generation")).unwrap();
            replies.send(Ok(vec![Message::ToolCall(ToolCall {
                call_id: "call".into(), name: "blocking".into(), arguments: serde_json::json!({}),
            })])).unwrap();
            started.notified().await;
            let image = Image::from_bytes(&crate::agent::image::tests::sample(image::ImageFormat::Png)).unwrap();
            input_tx.send(UserMessage { files: Vec::new(), content: "during tool".into(), images: vec![image.clone()] }).unwrap();
            assert!(contexts.try_recv().is_err());
            release.notify_one();
            let context = contexts.recv().await.unwrap();
            assert_eq!(context.len(), 5);
            assert!(matches!(&context[1], Message::ToolCall(call) if call.call_id == "call"));
            assert!(matches!(&context[2], Message::ToolResult(result) if result.content.output["done"] == true));
            assert!(matches!(&context[3], Message::User(input) if input.content == "during generation"));
            assert!(matches!(&context[4], Message::User(input) if input.content == "during tool" && input.images[0].data_url == image.data_url));
            replies.send(Ok(answer("done"))).unwrap();
            assert_eq!(task.await.unwrap().unwrap().content, "done");
            let saved = runner.load_session(SessionId::new("steer")).await.unwrap();
            assert_eq!(saved.messages().len(), 6);
            assert!(matches!(&saved.messages()[4], Message::User(input) if input.images[0].data_url == image.data_url));
        }).await.unwrap();
    }

    #[tokio::test]
    async fn accepted_corrections_are_saved_on_cancel_or_generation_error() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for stop in [true, false] {
                let (requests, mut contexts) = mpsc::unbounded_channel();
                let (replies, responses) = mpsc::unbounded_channel();
                let directory = tempfile::tempdir().unwrap();
                let runner = Arc::new(AgentRunner::new(
                    Arc::new(Agent::new(Arc::new(ControlledLlm {
                        requests, replies: tokio::sync::Mutex::new(responses),
                    }), ToolRegistry::default(), ContextBuilder::new("test"))),
                    Arc::new(JsonSessionStore::new(directory.path())),
                ));
                let (input_tx, mut input_rx) = mpsc::unbounded_channel();
                let (cancel, cancelled) = oneshot::channel();
                let task_runner = runner.clone();
                let task = tokio::spawn(async move {
                    let result = task_runner.run_with_cancel(SessionId::new("steer"),
                        IncomingMessage::new(InterfaceId("test".into()), "chat", "initial"),
                        None, Some(cancelled), Some(&mut input_rx)).await;
                    (result, input_rx)
                });
                contexts.recv().await.unwrap();
                input_tx.send(user("accepted correction")).unwrap();
                if stop { cancel.send(()).unwrap(); }
                else { replies.send(Err(anyhow::anyhow!("generation failed"))).unwrap(); }
                let (result, mut remaining) = task.await.unwrap();
                assert_eq!(result.unwrap_err().to_string(), if stop { "Stopped by user" } else { "generation failed" });
                assert!(remaining.try_recv().is_err());
                let session = runner.load_session(SessionId::new("steer")).await.unwrap();
                assert_eq!(session.messages().len(), 2);
                assert!(matches!(&session.messages()[1], Message::User(input) if input.content == "accepted correction"));
            }
        }).await.unwrap();
    }

    #[tokio::test]
    async fn durable_accepted_corrections_are_saved_on_cancel_or_generation_error() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for stop in [true, false] {
                let (requests, mut contexts) = mpsc::unbounded_channel();
                let (replies, responses) = mpsc::unbounded_channel();
                let directory = tempfile::tempdir().unwrap();
                let runner = Arc::new(AgentRunner::new(
                    Arc::new(Agent::new(Arc::new(ControlledLlm {
                        requests, replies: tokio::sync::Mutex::new(responses),
                    }), ToolRegistry::default(), ContextBuilder::new("test"))),
                    Arc::new(super::super::session::SqliteSessionStore::open_async(directory.path().into()).await.unwrap()),
                ));
                let (input_tx, mut input_rx) = mpsc::unbounded_channel();
                let (cancel, cancelled) = oneshot::channel();
                let task_runner = runner.clone();
                let task = tokio::spawn(async move {
                    let result = task_runner.run_with_cancel(SessionId::new("steer"),
                        IncomingMessage::new(InterfaceId("test".into()), "chat", "initial"),
                        None, Some(cancelled), Some(&mut input_rx)).await;
                    (result, input_rx)
                });
                contexts.recv().await.unwrap();
                input_tx.send(user("accepted correction")).unwrap();
                if stop { cancel.send(()).unwrap(); }
                else { replies.send(Err(anyhow::anyhow!("generation failed"))).unwrap(); }
                let (result, mut remaining) = task.await.unwrap();
                assert_eq!(result.unwrap_err().to_string(), if stop { "Stopped by user" } else { "generation failed" });
                assert!(remaining.try_recv().is_err());
                let session = runner.load_session(SessionId::new("steer")).await.unwrap();
                assert_eq!(session.messages().len(), 2);
                assert!(matches!(&session.messages()[1], Message::User(input) if input.content == "accepted correction"));
            }
        }).await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_with_full_progress_channel_keeps_accepted_corrections() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let (requests, mut contexts) = mpsc::unbounded_channel();
            let (replies, responses) = mpsc::unbounded_channel();
            let directory = tempfile::tempdir().unwrap();
            let runner = Arc::new(AgentRunner::new(
                Arc::new(Agent::new(
                    Arc::new(ControlledLlm {
                        requests,
                        replies: tokio::sync::Mutex::new(responses),
                    }),
                    ToolRegistry::default(),
                    ContextBuilder::new("test"),
                )),
                Arc::new(JsonSessionStore::new(directory.path())),
            ));
            let (input_tx, mut input_rx) = mpsc::unbounded_channel();
            let (progress, _events) = mpsc::channel(1);
            let task_progress = progress.clone();
            let (cancel, cancelled) = oneshot::channel();
            let task_runner = runner.clone();
            let task = tokio::spawn(async move {
                task_runner
                    .run_with_cancel(
                        SessionId::new("steer"),
                        IncomingMessage::new(InterfaceId("test".into()), "chat", "initial"),
                        Some(task_progress),
                        Some(cancelled),
                        Some(&mut input_rx),
                    )
                    .await
            });
            contexts.recv().await.unwrap();
            input_tx.send(user("keep this correction")).unwrap();
            replies.send(Ok(answer("intermediate"))).unwrap();
            // ContextTokens fills the channel; the intermediate Message blocks the agent.
            while progress.capacity() != 0 {
                tokio::task::yield_now().await;
            }
            cancel.send(()).unwrap();
            assert_eq!(
                task.await.unwrap().unwrap_err().to_string(),
                "Stopped by user"
            );
            let session = runner.load_session(SessionId::new("steer")).await.unwrap();
            assert!(session.messages().iter().any(|message| {
                matches!(message, Message::User(input) if input.content == "keep this correction")
            }));
        })
        .await
        .unwrap();
    }

    struct VisionLlm {
        path: std::path::PathBuf,
        image: Image,
    }

    #[async_trait::async_trait]
    impl LlmProvider for VisionLlm {
        async fn generate(
            &self,
            context: &LlmContext<'_>,
            _: &[&dyn Tool],
            _: Option<&mpsc::Sender<AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            let Message::User(user) = &context.messages[0] else {
                panic!("missing user message")
            };
            assert_eq!(user.images[0].data_url, self.image.data_url);
            assert_eq!(user.files[0].filename, "report.pdf");
            assert_eq!(
                user.files[0].data_url().await.unwrap(),
                "data:application/pdf;base64,JVBERi0xLjcKdGVzdA=="
            );
            let next = if context.messages.len() == 1 {
                Message::ToolCall(ToolCall {
                    call_id: "view".into(),
                    name: "view_image".into(),
                    arguments: serde_json::json!({"path":self.path}),
                })
            } else {
                let Some(Message::ToolResult(result)) = context.messages.last() else {
                    panic!("missing tool result")
                };
                assert_eq!(result.content.images[0].data_url, self.image.data_url);
                Message::Assistant(AssistantMessage {
                    content: "red image".into(),
                })
            };
            Ok((vec![next], None))
        }
    }

    #[tokio::test]
    async fn preserves_incoming_and_tool_images_in_saved_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("screen.png");
        let bytes = crate::agent::image::tests::sample(image::ImageFormat::Png);
        std::fs::write(&path, &bytes).unwrap();
        let image = Image::from_bytes(&bytes).unwrap();
        let store = Arc::new(JsonSessionStore::new(dir.path().join("sessions")));
        let runner = AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(VisionLlm {
                    path: path.clone(),
                    image: image.clone(),
                }),
                ToolRegistry::default().register(Arc::new(ViewImageTool)),
                ContextBuilder::new("test"),
            )),
            store.clone(),
        );
        let mut incoming = IncomingMessage::new(
            InterfaceId("test".into()),
            "chat",
            "Compare with screen.png",
        );
        incoming.images.push(image.clone());
        let file = crate::agent::FileAttachment::save(
            &dir.path().join("attachments"),
            "report.pdf",
            None,
            b"%PDF-1.7\ntest",
        )
        .unwrap();
        incoming.files.push(file.clone());
        let id = SessionId::new("vision");
        let (tx, mut rx) = mpsc::channel(16);
        assert_eq!(
            runner
                .run(id.clone(), incoming, Some(tx))
                .await
                .unwrap()
                .content,
            "red image"
        );
        while let Some(event) = rx.recv().await {
            if let AgentEvent::ToolFinished { output, .. } = event {
                assert_eq!(output["width"], 2);
                assert!(!output.to_string().contains("base64"));
            }
        }
        std::fs::remove_file(path).unwrap();
        let restored = store.load(&id).await.unwrap().unwrap();
        let Message::User(user) = &restored.messages()[0] else {
            panic!("missing user")
        };
        let Message::ToolResult(tool) = &restored.messages()[2] else {
            panic!("missing tool")
        };
        assert_eq!(user.images[0].data_url, image.data_url);
        assert_eq!(user.files[0].path, file.path);
        assert_eq!(tool.content.images[0].data_url, image.data_url);
    }
}
