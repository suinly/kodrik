use super::{
    AgentRunner,
    session::{SessionId, SqliteSessionStore},
};
use crate::gateway::IncomingMessage;
use anyhow::{Context, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) const MAX_QUEUED_SUBMISSIONS: usize = 100;

pub(super) fn active_session(db: &Connection, conversation: &str) -> anyhow::Result<String> {
    Ok(db
        .query_row(
            "SELECT session_id FROM active_sessions WHERE conversation_id=?1",
            [conversation],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_else(|| conversation.to_owned()))
}

pub(super) fn session_busy_in(db: &Connection, id: &str) -> anyhow::Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM runs WHERE session_id=?1 AND terminal=0)
         OR EXISTS(SELECT 1 FROM submissions s LEFT JOIN active_sessions a ON a.conversation_id=s.conversation_id
             WHERE s.status='Queued' AND coalesce(s.session_id,a.session_id,s.conversation_id)=?1)",
        [id], |r| r.get(0),
    )?)
}

#[cfg(test)]
mod shared_session_tests {
    use super::*;
    use crate::{
        agent::{
            run_state::{Checkpoint, RunPhase},
            session::{Session, SessionStore},
        },
        gateway::InterfaceId,
    };

    fn request(channel: &str, key: &str) -> SubmissionRequest {
        let mut request = SubmissionRequest::new(
            &SessionId::new(channel),
            IncomingMessage::new(InterfaceId("test".into()), "chat", "question"),
            BusyMode::Steer,
        );
        request.request_id = key.into();
        request
    }

    struct NoGeneration;
    #[async_trait::async_trait]
    impl crate::agent::llm::LlmProvider for NoGeneration {
        async fn generate(
            &self,
            _: &crate::agent::LlmContext<'_>,
            _: &[&dyn crate::agent::tools::Tool],
            _: Option<&tokio::sync::mpsc::Sender<crate::agent::AgentEvent>>,
        ) -> anyhow::Result<(Vec<crate::agent::message::Message>, Option<u64>)> {
            panic!("Stop must not execute the queued input")
        }
    }

    #[tokio::test]
    async fn persisted_stop_waits_for_another_workers_execution_ownership() {
        use crate::agent::{Agent, ContextBuilder, tools::ToolRegistry};
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SqliteSessionStore::open_async(dir.path().into())
                .await
                .unwrap(),
        );
        let runner = Arc::new(AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(NoGeneration),
                ToolRegistry::default(),
                ContextBuilder::new("test"),
            )),
            store.clone(),
        ));
        let conversation = SessionId::new("telegram:bot:42");
        let pending = store
            .submit(request(conversation.as_str(), "before-stop"))
            .await
            .unwrap();
        let ownership = store
            .claim_execution(conversation.as_str(), conversation.as_str())
            .await
            .unwrap();
        let command = IncomingMessage::new(InterfaceId("web".into()), "42", "/stop");
        let request_id = command.request_id.clone();
        let source = conversation.clone();
        let mut task = tokio::spawn(async move { runner.apply_control(&source, &command).await });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut task)
                .await
                .is_err(),
            "a persisted Stop must wait for the execution owner"
        );
        let control = store
            .submission(conversation.as_str(), &request_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(control.status, SubmissionStatus::Queued);
        drop(ownership);
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .submission_by_id(&control.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            SubmissionStatus::Done
        );
        assert_eq!(
            store
                .submission_by_id(&pending.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            SubmissionStatus::Cancelled
        );
        assert!(
            store
                .inbox_blocked(conversation.as_str())
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn shared_session_rejects_another_transport_before_first_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap();
        let session = Session::new(SessionId::new(uuid::Uuid::new_v4().to_string()));
        store.save(&session).await.unwrap();
        for channel in ["telegram:bot:42", "web:legacy"] {
            store
                .select(&SessionId::new(channel), session.id())
                .await
                .unwrap();
        }
        store
            .submit(request("telegram:bot:42", "first"))
            .await
            .unwrap();
        let error = store
            .submit(request("web:legacy", "second"))
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<AdmissionRejected>().is_some());
        assert!(
            store
                .submission("web:legacy", "second")
                .await
                .unwrap()
                .is_none()
        );
        assert!(store.session_busy(session.id().as_str()).await.unwrap());
        assert!(!store.session_busy("unrelated").await.unwrap());
        assert!(
            store
                .load(session.id())
                .await
                .unwrap()
                .unwrap()
                .messages()
                .is_empty()
        );
        // Same-channel steering and control semantics remain available.
        store
            .submit(request("telegram:bot:42", "follow"))
            .await
            .unwrap();
        let mut control = request("web:legacy", "stop");
        control.control = true;
        control.message.text = "/stop".into();
        store.submit(control).await.unwrap();
    }

    #[tokio::test]
    async fn shared_session_admission_race_accepts_only_one_transport() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap();
        let other = SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap();
        let session = Session::new(SessionId::new("shared"));
        store.save(&session).await.unwrap();
        for channel in ["telegram:bot:42", "web:legacy"] {
            store
                .select(&SessionId::new(channel), session.id())
                .await
                .unwrap();
        }
        let (first, second) = tokio::join!(
            store.submit(request("telegram:bot:42", "first")),
            other.submit(request("web:legacy", "second"))
        );
        assert_ne!(first.is_ok(), second.is_ok());
        assert_eq!(store.queued_conversations().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn shared_running_session_rejects_other_channel_and_bound_queue_stays_busy() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap();
        let session = Session::new(SessionId::new("shared"));
        store.save(&session).await.unwrap();
        for channel in ["telegram:bot:42", "web:legacy"] {
            store
                .select(&SessionId::new(channel), session.id())
                .await
                .unwrap();
        }
        let mut cp = Checkpoint::new(&store, &SessionId::new("telegram:bot:42"), &session);
        cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
        assert!(store.submit(request("web:legacy", "second")).await.is_err());
        let queued = store
            .submit(request("telegram:bot:42", "follow"))
            .await
            .unwrap();
        cp.save(&session, RunPhase::Completed, None).await.unwrap();
        assert!(store.session_busy("shared").await.unwrap());
        let other = Session::new(SessionId::new("other"));
        store.save(&other).await.unwrap();
        // A late steering record keeps its explicitly bound session after a channel moves.
        store
            .select(&SessionId::new("telegram:bot:42"), other.id())
            .await
            .unwrap();
        assert_eq!(
            store
                .submission_by_id(&queued.id)
                .await
                .unwrap()
                .unwrap()
                .session_id
                .as_deref(),
            Some("shared")
        );
        assert!(store.session_busy("shared").await.unwrap());
        assert!(store.submit(request("web:legacy", "third")).await.is_err());
    }
}

#[derive(Debug)]
pub(crate) struct AdmissionRejected(pub &'static str);
impl std::fmt::Display for AdmissionRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for AdmissionRejected {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BusyMode {
    FollowUp,
    Steer,
    RejectIfBusy,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SubmissionRequest {
    pub conversation_id: String,
    pub request_id: String,
    pub mode: BusyMode,
    pub isolated: bool,
    pub control: bool,
    pub message: IncomingMessage,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum SubmissionStatus {
    Queued,
    Running,
    Done,
    Cancelled,
    Unanswered,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SubmissionRecord {
    pub id: String,
    pub request: SubmissionRequest,
    pub payload_hash: String,
    pub sequence: i64,
    pub accepted_at: chrono::DateTime<chrono::Utc>,
    pub status: SubmissionStatus,
    pub run_id: Option<String>,
    pub session_id: Option<String>,
    #[serde(default)]
    pub answer: Option<super::AssistantMessage>,
    #[serde(skip)]
    pub duplicate: bool,
}
impl SubmissionRequest {
    pub(crate) fn new(conversation: &SessionId, message: IncomingMessage, mode: BusyMode) -> Self {
        let control = message.guest_query_id.is_none()
            && message.files.is_empty()
            && message.images.is_empty()
            && matches!(message.text.trim(), "/stop" | "/new");
        Self {
            conversation_id: conversation.as_str().into(),
            request_id: message.request_id.clone(),
            message,
            mode,
            isolated: false,
            control,
        }
    }
    pub(crate) fn hash(&self) -> anyhow::Result<String> {
        let mut payload = serde_json::to_value(self)?;
        // File downloads may allocate different private paths on repeated delivery.
        // Their immutable hashes, names, MIME and lengths identify the same input.
        if let Some(files) = payload["message"]["files"].as_array_mut() {
            for file in files {
                file.as_object_mut().unwrap().remove("path");
            }
        }
        payload["message"]
            .as_object_mut()
            .unwrap()
            .remove("request_id");
        Ok(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&payload)?)
        ))
    }
}

pub(super) fn read_record(db: &Connection, id: &str) -> anyhow::Result<Option<SubmissionRecord>> {
    let payload: Option<Vec<u8>> = db
        .query_row("SELECT payload FROM submissions WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .optional()?;
    payload.map(|p| Ok(serde_json::from_slice(&p)?)).transpose()
}
pub(super) fn write_record(db: &Connection, record: &SubmissionRecord) -> anyhow::Result<()> {
    db.execute(
        "UPDATE submissions SET payload=?2,status=?3,run_id=?4,session_id=?5 WHERE id=?1",
        params![
            record.id,
            serde_json::to_vec(record)?,
            format!("{:?}", record.status),
            record.run_id,
            record.session_id
        ],
    )?;
    Ok(())
}
// Share atomic admission with web edits that create their history in the same transaction.
pub(crate) fn admit_submission(
    tx: &rusqlite::Transaction<'_>,
    request: SubmissionRequest,
    hash: String,
) -> anyhow::Result<SubmissionRecord> {
    let old: Option<String> = tx
        .query_row(
            "SELECT id FROM submissions WHERE conversation_id=?1 AND request_id=?2",
            params![request.conversation_id, request.request_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = old {
        let mut record = read_record(&tx, &id)?.context("Missing submission")?;
        ensure!(
            record.payload_hash == hash,
            AdmissionRejected("Request ID payload conflict")
        );
        record.duplicate = true;
        return Ok(record);
    }
    let target = active_session(tx, &request.conversation_id)?;
    if !request.control {
        let conflict: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runs WHERE session_id=?1 AND conversation_id<>?2 AND terminal=0)
             OR EXISTS(SELECT 1 FROM submissions s LEFT JOIN active_sessions a ON a.conversation_id=s.conversation_id
                 WHERE s.status='Queued' AND s.conversation_id<>?2 AND json_extract(s.payload,'$.request.control')=0
                 AND coalesce(s.session_id,a.session_id,s.conversation_id)=?1)",
            params![target, request.conversation_id], |r| r.get(0),
        )?;
        ensure!(
            !conflict,
            AdmissionRejected("Session is busy in another conversation")
        );
    }
    let queued: i64 = tx.query_row(
        "SELECT count(*) FROM submissions WHERE conversation_id=?1 AND status='Queued'",
        [&request.conversation_id],
        |r| r.get(0),
    )?;
    ensure!(
        request.control || queued < MAX_QUEUED_SUBMISSIONS as i64,
        AdmissionRejected("Inbox queue is full")
    );
    let live: Option<(String, String)> = tx
        .query_row(
            "SELECT id,session_id FROM runs WHERE conversation_id=?1 AND terminal=0",
            [&request.conversation_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let blocked: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM inbox_controls WHERE conversation_id=?1)",
        [&request.conversation_id],
        |r| r.get(0),
    )?;
    ensure!(
        request.mode != BusyMode::RejectIfBusy || (live.is_none() && (queued == 0 || blocked)),
        AdmissionRejected("Conversation is busy")
    );
    let sequence: i64 = tx.query_row(
        "SELECT coalesce(max(sequence),0)+1 FROM submissions WHERE conversation_id=?1",
        [&request.conversation_id],
        |r| r.get(0),
    )?;
    let control_pending:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM submissions WHERE conversation_id=?1 AND status='Queued' AND json_extract(payload,'$.request.control')=1)",[&request.conversation_id],|r|r.get(0))?;
    let (run_id, session_id) = if request.mode == BusyMode::Steer && !control_pending {
        live.map(|(r, s)| {
            if s == target {
                (Some(r), Some(s))
            } else {
                // A notification may switch the channel while its previous run finishes.
                (None, Some(target))
            }
        })
        .unwrap_or_default()
    } else {
        (None, None)
    };
    // Only genuinely new input resumes a blocked queue; duplicate delivery cannot.
    if !request.control {
        tx.execute(
            "DELETE FROM inbox_controls WHERE conversation_id=?1",
            [&request.conversation_id],
        )?;
    }
    let record = SubmissionRecord {
        id: uuid::Uuid::new_v4().to_string(),
        request,
        payload_hash: hash,
        sequence,
        accepted_at: chrono::Utc::now(),
        status: SubmissionStatus::Queued,
        run_id,
        session_id,
        answer: None,
        duplicate: false,
    };
    tx.execute("INSERT INTO submissions(id,conversation_id,request_id,payload_hash,sequence,payload,status,run_id,session_id) VALUES(?1,?2,?3,?4,?5,?6,'Queued',?7,?8)",params![record.id,record.request.conversation_id,record.request.request_id,record.payload_hash,record.sequence,serde_json::to_vec(&record)?,record.run_id,record.session_id])?;
    Ok(record)
}
impl SqliteSessionStore {
    #[cfg(test)]
    pub(crate) async fn session_busy(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_owned();
        self.with_connection(move |db| session_busy_in(db, &id))
            .await
    }

    pub(crate) async fn submission(
        &self,
        conversation: &str,
        request: &str,
    ) -> anyhow::Result<Option<SubmissionRecord>> {
        let (conversation, request) = (conversation.to_owned(), request.to_owned());
        self.with_connection(move |db| {
            let id: Option<String> = db
                .query_row(
                    "SELECT id FROM submissions WHERE conversation_id=?1 AND request_id=?2",
                    params![conversation, request],
                    |r| r.get(0),
                )
                .optional()?;
            id.map(|id| read_record(db, &id).map(Option::unwrap))
                .transpose()
        })
        .await
    }
    pub(crate) async fn submit(
        &self,
        mut request: SubmissionRequest,
    ) -> anyhow::Result<SubmissionRecord> {
        request.message.request_id = request.request_id.clone();
        ensure!(
            !request.request_id.is_empty() && request.request_id.len() <= 256,
            "Invalid request ID"
        );
        ensure!(
            !request.conversation_id.is_empty(),
            "Invalid conversation ID"
        );
        let hash = request.hash()?;
        if let Some(mut record) = self
            .submission(&request.conversation_id, &request.request_id)
            .await?
        {
            ensure!(
                record.payload_hash == hash,
                AdmissionRejected("Request ID payload conflict")
            );
            record.duplicate = true;
            return Ok(record);
        }
        // Verify originals before admitting a reference that has to survive restart.
        for file in &request.message.files {
            file.bytes().await?;
        }
        self.with_connection(move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let record = admit_submission(&tx, request, hash)?;
            #[cfg(test)]
            super::durable_fixture::boundary("before_admission")?;
            tx.commit()?;
            #[cfg(test)]
            super::durable_fixture::boundary("after_admission")?;
            Ok(record)
        })
        .await
    }
    pub(crate) async fn next_submission(
        &self,
        conversation: &SessionId,
    ) -> anyhow::Result<Option<SubmissionRecord>> {
        let conversation = conversation.as_str().to_owned();
        self.with_connection(move|db|{
            let blocked:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM inbox_controls WHERE conversation_id=?1)",[&conversation],|r|r.get(0))?;
            if blocked {return Ok(None);}
            let id:Option<String>=db.query_row("SELECT id FROM submissions WHERE conversation_id=?1 AND status='Queued' AND json_extract(payload,'$.request.control')=0 ORDER BY sequence LIMIT 1",[conversation],|r|r.get(0)).optional()?;
            id.map(|id|read_record(db,&id).map(Option::unwrap)).transpose()
        }).await
    }
    pub(crate) async fn inbox_blocked(&self, conversation: &str) -> anyhow::Result<Option<String>> {
        let conversation = conversation.to_owned();
        self.with_connection(move |db| {
            Ok(db
                .query_row(
                    "SELECT reason FROM inbox_controls WHERE conversation_id=?1",
                    [conversation],
                    |r| r.get(0),
                )
                .optional()?)
        })
        .await
    }

    pub(crate) async fn resume_inbox(&self, conversation: &str) -> anyhow::Result<()> {
        let conversation = conversation.to_owned();
        self.with_connection(move|db| {
            let tx=db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let blocked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM inbox_controls WHERE conversation_id=?1)",[&conversation],|r|r.get(0))?;
            if !blocked {return Ok(());}
            let stopping:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE conversation_id=?1 AND terminal=0) OR EXISTS(SELECT 1 FROM submissions WHERE conversation_id=?1 AND status='Queued' AND json_extract(payload,'$.request.control')=1)",[&conversation],|r|r.get(0))?;
            ensure!(!stopping,AdmissionRejected("Conversation is busy"));
            tx.execute("DELETE FROM inbox_controls WHERE conversation_id=?1",[conversation])?;
            tx.commit()?;
            Ok(())
        }).await
    }
}
impl AgentRunner {
    pub(crate) fn prepare_submission(&self, request: &mut SubmissionRequest) {
        request.isolated = self.agent.isolated();
    }

    pub(crate) async fn submit(
        &self,
        mut request: SubmissionRequest,
    ) -> anyhow::Result<SubmissionRecord> {
        self.prepare_submission(&mut request);
        self.sessions
            .runtime_store()
            .context("Persistent inbox requires SQLite")?
            .submit(request)
            .await
    }
}

/// Called inside the checkpoint transaction, never as a second commit.
pub(super) fn checkpoint_submissions(
    db: &Connection,
    run: &super::run_state::RunRecord,
    session: &super::session::Session,
) -> anyhow::Result<()> {
    use super::run_state::RunPhase;
    for id in &run.submission_ids {
        let mut record = read_record(db, id)?.context("Run submission missing")?;
        ensure!(
            record.request.conversation_id == run.conversation_id,
            "Submission conversation changed"
        );
        ensure!(
            record.run_id.as_deref().is_none_or(|id| id == run.id),
            "Submission already bound to another run"
        );
        ensure!(
            record
                .session_id
                .as_deref()
                .is_none_or(|id| id == run.session_id),
            "Submission session changed"
        );
        ensure!(
            matches!(
                record.status,
                SubmissionStatus::Queued | SubmissionStatus::Running
            ),
            "Submission is terminal"
        );
        record.run_id = Some(run.id.clone());
        record.session_id = Some(run.session_id.clone());
        record.status = match &run.phase {
            RunPhase::Completed => SubmissionStatus::Done,
            RunPhase::Cancelled => SubmissionStatus::Cancelled,
            RunPhase::Failed { .. } => SubmissionStatus::Unanswered,
            _ => SubmissionStatus::Running,
        };
        if matches!(run.phase, RunPhase::Completed) {
            record.answer = session.messages().iter().rev().find_map(|m| {
                if let super::message::Message::Assistant(a) = m {
                    Some(a.clone())
                } else {
                    None
                }
            });
        }
        write_record(db, &record)?;
    }
    if run.phase.terminal() {
        let ids = db
            .prepare("SELECT id FROM submissions WHERE run_id=?1 AND status='Queued'")?
            .query_map([&run.id], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for id in ids {
            let mut record = read_record(db, &id)?.unwrap();
            if matches!(run.phase, RunPhase::Cancelled) {
                record.status = SubmissionStatus::Cancelled;
            } else {
                record.run_id = None;
            } // A late correction becomes a follow-up in its original session.
            write_record(db, &record)?;
        }
    }
    if matches!(run.phase, RunPhase::Failed { .. } | RunPhase::Cancelled) {
        let reason = match &run.phase {
            RunPhase::Failed { reason } => reason.as_str(),
            _ => "Stopped by user; queued follow-ups require new input or continue",
        };
        db.execute("INSERT INTO inbox_controls VALUES(?1,?2) ON CONFLICT(conversation_id) DO UPDATE SET reason=excluded.reason",params![run.conversation_id,reason])?;
    }
    Ok(())
}
impl SqliteSessionStore {
    pub(crate) async fn submission_by_id(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<SubmissionRecord>> {
        let id = id.to_owned();
        self.with_connection(move |db| read_record(db, &id)).await
    }
    pub(crate) async fn steering(
        &self,
        run: &super::run_state::RunRecord,
    ) -> anyhow::Result<Vec<SubmissionRecord>> {
        let run = run.clone();
        self.with_connection(move|db|{
            // A queued follow-up is a FIFO barrier. Never pull later steering ahead of it.
            let mut statement=db.prepare("SELECT payload FROM submissions WHERE conversation_id=?1 AND status='Queued' ORDER BY sequence")?;
            let rows=statement.query_map([&run.conversation_id],|r|r.get::<_,Vec<u8>>(0))?;
            let mut result=vec![];
            for row in rows {
                let record:SubmissionRecord=serde_json::from_slice(&row?)?;
                if run.submission_ids.contains(&record.id){continue;}
                if record.request.isolated!=run.isolated.unwrap_or(record.request.isolated) || record.session_id.as_deref().is_some_and(|id|id!=run.session_id) || record.request.control || record.request.mode!=BusyMode::Steer || record.run_id.as_deref().is_some_and(|id|id!=run.id) {break;}
                result.push(record);
            }
            Ok(result)
        }).await
    }
    pub(crate) async fn queued_conversations(&self) -> anyhow::Result<Vec<SessionId>> {
        self.with_connection(|db| {
            Ok(db
                .prepare("SELECT DISTINCT conversation_id FROM submissions WHERE status='Queued'")?
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .map(SessionId::new)
                .collect())
        })
        .await
    }
    pub(crate) async fn submission_answer(
        &self,
        record: &SubmissionRecord,
    ) -> anyhow::Result<super::AssistantMessage> {
        ensure!(
            record.status == SubmissionStatus::Done,
            "Submission is {:?}",
            record.status
        );
        record.answer.clone().context("Completed answer missing")
    }
}

#[derive(Debug)]
pub(crate) struct Observed;
impl std::fmt::Display for Observed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Group input observed without participation")
    }
}
impl std::error::Error for Observed {}

impl AgentRunner {
    pub(crate) async fn cancel_queued_input(
        &self,
        conversation: &SessionId,
        request: &str,
    ) -> anyhow::Result<()> {
        let Some(store) = self.sessions.runtime_store() else {
            return Ok(());
        };
        let conversation = conversation.as_str().to_owned();
        let request = request.to_owned();
        store.with_connection(move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let id: Option<String> = tx.query_row("SELECT id FROM submissions WHERE conversation_id=?1 AND request_id=?2 AND status='Queued'", params![conversation, request], |row| row.get(0)).optional()?;
            if let Some(id) = id {
                let mut record = read_record(&tx, &id)?.context("Queued submission missing")?;
                record.status = SubmissionStatus::Cancelled;
                write_record(&tx, &record)?;
            }
            tx.commit()?;
            Ok(())
        }).await
    }

    pub(crate) async fn discard_guest_queries(
        &self,
        conversation: &SessionId,
    ) -> anyhow::Result<()> {
        self.cancel_conversation(conversation).await?;
        let Some(store) = self.sessions.runtime_store() else {
            return Ok(());
        };
        let conversation = conversation.as_str().to_owned();
        store.with_connection(move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let records = tx.prepare("SELECT payload FROM submissions WHERE conversation_id=?1 AND status='Queued'")?
                .query_map([&conversation], |row| row.get::<_, Vec<u8>>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            for bytes in records {
                let mut record: SubmissionRecord = serde_json::from_slice(&bytes)?;
                if record.request.message.guest_query_id.is_some() {
                    record.status = SubmissionStatus::Cancelled;
                    write_record(&tx, &record)?;
                }
            }
            tx.commit()?;
            Ok(())
        }).await
    }

    pub(crate) fn has_inbox(&self) -> bool {
        self.sessions.runtime_store().is_some()
    }
    pub(crate) async fn next_input(
        &self,
        conversation: &SessionId,
    ) -> anyhow::Result<Option<IncomingMessage>> {
        let Some(store) = self.sessions.runtime_store() else {
            return Ok(None);
        };
        Ok(store
            .next_submission(conversation)
            .await?
            .map(|r| r.request.message))
    }

    pub(crate) async fn submission_finished(
        &self,
        conversation: &SessionId,
        request: &str,
    ) -> anyhow::Result<bool> {
        let Some(store) = self.sessions.runtime_store() else {
            return Ok(false);
        };
        Ok(store
            .submission(conversation.as_str(), request)
            .await?
            .is_some_and(|r| r.status != SubmissionStatus::Queued))
    }
}

impl SqliteSessionStore {
    pub(crate) async fn inbox_busy(&self, conversation: &str) -> anyhow::Result<bool> {
        let conversation = conversation.to_owned();
        self.with_connection(move|db|Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE conversation_id=?1 AND terminal=0) OR (EXISTS(SELECT 1 FROM submissions WHERE conversation_id=?1 AND status='Queued') AND NOT EXISTS(SELECT 1 FROM inbox_controls WHERE conversation_id=?1))",[conversation],|r|r.get(0))?)).await
    }
}

impl SqliteSessionStore {
    pub(crate) async fn pending_controls(&self) -> anyhow::Result<Vec<SubmissionRecord>> {
        self.with_connection(|db| {
            let mut statement=db.prepare("SELECT payload FROM submissions WHERE status='Queued' AND json_extract(payload,'$.request.control')=1 ORDER BY conversation_id,sequence")?;
            statement.query_map([],|r|r.get::<_,Vec<u8>>(0))?.map(|r|Ok(serde_json::from_slice(&r?)?)).collect()
        }).await
    }
}
impl AgentRunner {
    pub(crate) async fn apply_control(
        &self,
        conversation: &SessionId,
        message: &IncomingMessage,
    ) -> anyhow::Result<()> {
        let Some(store) = self.sessions.runtime_store() else {
            if message.text.trim() == "/new" {
                self.new_session(conversation).await?;
            }
            return Ok(());
        };
        let record = self
            .submit(SubmissionRequest::new(
                conversation,
                message.clone(),
                BusyMode::FollowUp,
            ))
            .await?;
        ensure!(record.request.control, "Not a control command");
        if record.status == SubmissionStatus::Done {
            return Ok(());
        }
        self.cancel_conversation(conversation).await?;
        let mut shutdown = self.shutdown_receiver();
        let _guard = loop {
            match self.lock_conversation(conversation).await {
                Ok(guard) => break guard,
                Err(error)
                    if matches!(
                        error.downcast_ref::<std::fs::TryLockError>(),
                        Some(std::fs::TryLockError::WouldBlock)
                    ) =>
                {
                    // Cancellation can finish before another worker releases its blocking effects.
                    tokio::select! {
                        _ = shutdown.wait_for(|stopped| *stopped) => return Err(super::recovery::Suspended.into()),
                        _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {},
                    }
                }
                Err(error) => return Err(error),
            }
        };
        super::run_state::CURRENT_EXECUTION
            .scope(_guard.ownership.clone(), store.commit_control(&record.id))
            .await
    }
}
