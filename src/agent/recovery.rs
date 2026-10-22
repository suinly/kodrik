use super::{
    AgentRunner, AssistantMessage,
    message::Message,
    run_state::{Checkpoint, RunPhase},
    session::{Session, SessionId, SqliteSessionStore},
};
use anyhow::Context;

pub(crate) enum RecoveryTarget {
    Conversation(SessionId),
    OneShots,
}
impl RecoveryTarget {
    fn matches(&self, id: &str) -> bool {
        match self {
            Self::Conversation(c) => c.as_str() == id,
            Self::OneShots => id.starts_with("cli:once:"),
        }
    }
}
#[derive(Default)]
pub(crate) struct RecoveryReport {
    pub completed: Vec<(String, AssistantMessage)>,
    pub cancelled: Vec<String>,
    pub blocked: Vec<(String, String)>,
}
#[derive(Debug)]
pub(crate) struct Suspended;
impl std::fmt::Display for Suspended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Process suspended; run will resume on restart")
    }
}
impl std::error::Error for Suspended {}

impl AgentRunner {
    pub(crate) fn shutdown_receiver(&self) -> tokio::sync::watch::Receiver<bool> {
        self.shutdown.subscribe()
    }
    pub(crate) fn suspend(&self) {
        self.shutdown.send_replace(true);
    }
    pub(crate) async fn cancel_run(&self, id: &str) -> anyhow::Result<()> {
        self.sessions
            .runtime_store()
            .context("Durable cancellation requires SQLite")?
            .cancel_run(id)
            .await
    }
    pub(crate) async fn unfinished_conversations(&self) -> anyhow::Result<Vec<SessionId>> {
        match self.sessions.runtime_store() {
            Some(store) => {
                let mut ids: Vec<_> = store
                    .unfinished_runs()
                    .await?
                    .into_iter()
                    .map(|r| SessionId::new(r.conversation_id))
                    .collect();
                for id in store.queued_conversations().await? {
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
                for id in store.pending_delivery_conversations().await? {
                    let id = SessionId::new(id);
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
                Ok(ids)
            }
            None => Ok(vec![]),
        }
    }
    pub(crate) async fn cancel_conversation(&self, id: &SessionId) -> anyhow::Result<()> {
        if let Some(store) = self.sessions.runtime_store() {
            store.cancel_pending_deliveries(id.as_str()).await?;
            for run in store.unfinished_runs().await? {
                if run.conversation_id == id.as_str() {
                    self.cancel_run(&run.id).await?;
                }
            }
        }
        Ok(())
    }
    pub(super) async fn wait_cancel(
        &self,
        store: &SqliteSessionStore,
        id: &str,
    ) -> anyhow::Result<()> {
        let mut shutdown = self.shutdown.subscribe();
        loop {
            if *shutdown.borrow() {
                return Err(Suspended.into());
            }
            if store.cancellation_requested(id).await? {
                return Ok(());
            }
            tokio::select! {_ = shutdown.changed()=>{},_ = tokio::time::sleep(std::time::Duration::from_millis(100))=>{}}
        }
    }
    pub(super) async fn finish_cancelled(
        &self,
        store: &SqliteSessionStore,
        cp: &mut Checkpoint,
        session: &mut Session,
        incoming: Option<&mut tokio::sync::mpsc::UnboundedReceiver<super::UserMessage>>,
    ) -> anyhow::Result<()> {
        if let Some((record, saved)) = store.load_run(&cp.record.id).await? {
            if record.phase.terminal() {
                return Ok(());
            }
            cp.record = record;
            *session = saved;
        } else {
            cp.record.revision = 0;
        }
        cp.storage_uncertain = false;
        cp.record.cancel_requested = true;
        session.finish_interrupted_tools();
        if let Some(incoming) = incoming {
            while let Ok(input) = incoming.try_recv() {
                session.push(Message::User(input));
            }
        }
        cp.save(session, RunPhase::Cancelled, None).await
    }
    pub(super) async fn execute_controlled(
        &self,
        session: &mut Session,
        input: super::UserMessage,
        progress: Option<&tokio::sync::mpsc::Sender<super::AgentEvent>>,
        incoming: Option<&mut tokio::sync::mpsc::UnboundedReceiver<super::UserMessage>>,
        checkpoint: Option<&mut Checkpoint>,
        cancel: Option<tokio::sync::oneshot::Receiver<()>>,
    ) -> anyhow::Result<AssistantMessage> {
        if let Some(cp) = checkpoint {
            let store = cp.store.clone();
            let id = cp.record.id.clone();
            let stopping = async {
                if let Some(cancel) = cancel {
                    tokio::select! {result=cancel=>{
                     if result.is_err(){return Err(anyhow::Error::from(Suspended));}
                     store.request_cancel(&id).await?;Ok(())
                    }, result=self.wait_cancel(&store,&id)=>result}
                } else {
                    self.wait_cancel(&store, &id).await
                }
            };
            let work = super::run_state::CURRENT_EXECUTION.scope(
                cp.ownership.clone(),
                self.agent
                    .run_durable(session, input, progress, incoming, Some(cp)),
            );
            tokio::pin!(work);
            tokio::select! {biased;stopped=stopping=>{stopped?;Err(anyhow::anyhow!("Stopped by user"))},result=&mut work=>result}
        } else {
            let work = self
                .agent
                .run_durable(session, input, progress, incoming, None);
            tokio::pin!(work);
            if let Some(cancel) = cancel {
                tokio::select! {biased;_=cancel=>Err(anyhow::anyhow!("Stopped by user")),result=&mut work=>result}
            } else {
                work.await
            }
        }
    }
    pub(crate) async fn recover(&self, target: RecoveryTarget) -> anyhow::Result<RecoveryReport> {
        let Some(store) = self.sessions.runtime_store() else {
            return Ok(RecoveryReport::default());
        };
        let mut report = RecoveryReport::default();
        for command in store.pending_controls().await? {
            if target.matches(&command.request.conversation_id) {
                self.cancel_conversation(&SessionId::new(&command.request.conversation_id))
                    .await?;
            }
        }

        let scan = match store.execution_lock("recovery", "scan").await {
            Ok(lock) => lock,
            Err(e) => {
                report.blocked.push(("scan".into(), e.to_string()));
                return Ok(report);
            }
        };
        let mut claimed = vec![];
        for run in store.unfinished_runs().await? {
            if !target.matches(&run.conversation_id) {
                continue;
            }
            if !run.cancel_requested && run.isolated != Some(self.agent.isolated()) {
                report.blocked.push((
                    run.id,
                    "Execution permissions changed or legacy run has no isolation provenance"
                        .into(),
                ));
                continue;
            }
            match store
                .claim_execution(&run.conversation_id, &run.session_id)
                .await
            {
                Ok(lock) => claimed.push((run.id, lock)),
                Err(e) => report.blocked.push((run.id, e.to_string())),
            }
        }
        drop(scan);
        for (id, ownership) in claimed {
            let Some((record, mut session)) = store.load_run(&id).await? else {
                continue;
            };
            if record.phase.terminal() {
                continue;
            }
            let _local = self
                .lock_local_conversation(&SessionId::new(&record.conversation_id))
                .await;
            let mut cp = Checkpoint::resume(store, record, ownership);
            if cp.record.cancel_requested {
                self.finish_cancelled(store, &mut cp, &mut session, None)
                    .await?;
                report.cancelled.push(id);
                continue;
            }
            // Referenced files must still exist; never degrade a recovered attachment into text only.
            let missing = session
                .messages()
                .iter()
                .filter_map(|m| {
                    if let Message::User(u) = m {
                        Some(&u.files)
                    } else {
                        None
                    }
                })
                .flatten()
                .any(|f| !f.path.is_file());
            if missing {
                report
                    .blocked
                    .push((id, "Recovered attachment is unavailable".into()));
                continue;
            }
            if !cp.prepare_recovery(&session, self.restart_limit).await? {
                report.blocked.push((
                    id,
                    "Run parked; inspect with kodrik runs show and explicitly retry or cancel"
                        .into(),
                ));
                continue;
            }
            let response = {
                let wakeup = cp
                    .record
                    .conversation_id
                    .rsplit_once(':')
                    .filter(|(interface, _)| interface.starts_with("telegram:"))
                    .map(|(interface, chat)| super::reminders::WakeupContext {
                        recipient: super::reminders::Recipient {
                            interface: interface.into(),
                            chat: chat.into(),
                        },
                        session: cp.record.session_id.clone(),
                    });
                let work = super::run_state::CURRENT_EXECUTION.scope(
                    cp.ownership.clone(),
                    super::reminders::WAKEUP_CONTEXT
                        .scope(wakeup, self.agent.resume_durable(&mut session, &mut cp)),
                );
                tokio::pin!(work);
                tokio::select! {result=&mut work=>result, stopped=self.wait_cancel(store,&id)=>match stopped {Ok(())=>Err(anyhow::anyhow!("Stopped by user")),Err(e)=>Err(e)}}
            };
            if response.as_ref().is_err_and(|e| e.is::<Suspended>()) {
                cp.clean_suspend().await?;
                break;
            }
            if store.cancellation_requested(&id).await? {
                self.finish_cancelled(store, &mut cp, &mut session, None)
                    .await?;
                report.cancelled.push(id);
                continue;
            }
            self.finish_run(&session, Some(&mut cp), &response).await?;
            match response {
                Ok(answer) => report.completed.push((id, answer)),
                Err(error) => {
                    report.blocked.push((id, format!("{error:#}")));
                }
            }
        }
        for command in store.pending_controls().await? {
            if target.matches(&command.request.conversation_id)
                && let Err(error) = self
                    .apply_control(
                        &SessionId::new(&command.request.conversation_id),
                        &command.request.message,
                    )
                    .await
            {
                report.blocked.push((command.id, format!("{error:#}")));
            }
        }
        for conversation in store.queued_conversations().await? {
            if !target.matches(conversation.as_str()) || *self.shutdown_receiver().borrow() {
                continue;
            }
            if self.restart_limit == 0 {
                report.blocked.push((
                    conversation.as_str().into(),
                    "Automatic recovery disabled".into(),
                ));
                continue;
            }
            if let Some(reason) = store.inbox_blocked(conversation.as_str()).await? {
                report.blocked.push((conversation.as_str().into(), reason));
                continue;
            }
            while let Some(next) = store.next_submission(&conversation).await? {
                if next.request.isolated != self.agent.isolated()
                    || next.request.message.guest_query_id.is_some()
                {
                    report.blocked.push((
                        next.id,
                        "Original execution permissions or guest response slot unavailable".into(),
                    ));
                    break;
                }
                match self
                    .run_with_cancel(conversation.clone(), next.request.message, None, None, None)
                    .await
                {
                    Ok(answer) => report.completed.push((next.id, answer)),
                    Err(error) if error.is::<super::inbox::Observed>() => {}
                    Err(error) => {
                        report.blocked.push((next.id, format!("{error:#}")));
                        break;
                    }
                }
            }
        }
        Ok(report)
    }
}
