use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context, ensure};
use futures::FutureExt;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, oneshot, watch};

use super::{
    Agent, AgentRunner,
    inbox::{BusyMode, SubmissionRequest, SubmissionStatus},
    session::{ExecutionOwnership, Session, SessionId, SqliteSessionStore},
};
use crate::gateway::{IncomingMessage, InterfaceId};

pub(crate) const PREFIX: &str = "companion:subagent:";
const MAX_ACTIVE: usize = 3;
const MAX_CHILDREN: i64 = 16;

pub(crate) fn with_children(
    db: &rusqlite::Connection,
    mut ids: Vec<String>,
) -> anyhow::Result<Vec<String>> {
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='subagent_sessions')",
        [], |row| row.get(0))?;
    if exists {
        let mut query = db.prepare("SELECT id,payload FROM subagent_sessions WHERE parent=?1")?;
        for parent in ids.clone() {
            for child in query.query_map([parent], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })? {
                let (id, payload) = child?;
                let child: Child = serde_json::from_slice(&payload)?;
                ensure!(
                    child.status != Status::Running,
                    super::inbox::AdmissionRejected(
                        "Child agent is still running; stop it before deletion"
                    )
                );
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    Ok(ids)
}

tokio::task_local! {
    pub(crate) static CURRENT_PARENT: Option<ParentContext>;
}

#[derive(Clone)]
pub(crate) struct ParentContext {
    pub session: Session,
    pub finished: watch::Receiver<bool>,
}

impl ParentContext {
    fn active(&self) -> anyhow::Result<()> {
        ensure!(
            !*self.finished.borrow() && self.finished.has_changed().is_ok(),
            "Parent run has finished"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Running,
    Completed,
    Stopped,
    Failed,
    Interrupted,
}

#[derive(Debug)]
pub(crate) struct ChildNotFound;
impl std::fmt::Display for ChildNotFound {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Unknown child agent in this session")
    }
}
impl std::error::Error for ChildNotFound {}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Child {
    pub id: String,
    pub status: Status,
    pub result: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub task: String,
    parent: String,
    generation: String,
    submission: String,
}

impl Child {
    pub(crate) fn view(&self) -> serde_json::Value {
        serde_json::json!({"id":self.id,"status":self.status,"result":self.result,"error":self.error})
    }
}

pub(crate) struct Subagents {
    runner: AgentRunner,
    store: SqliteSessionStore,
    // Admission and retirement share this lock so late input always has a worker.
    active: Mutex<HashMap<String, watch::Sender<bool>>>,
}

fn bounded(text: String) -> String {
    text.chars().take(16_384).collect()
}

fn validate_message(text: &str) -> anyhow::Result<()> {
    ensure!(
        !text.trim().is_empty() && text.len() <= 16_384,
        "Task/message must contain 1–16384 bytes"
    );
    Ok(())
}

async fn cancelled(receiver: &mut watch::Receiver<bool>) {
    let _ = receiver.wait_for(|stopped| *stopped).await;
}

fn write_child(db: &rusqlite::Connection, child: &Child) -> anyhow::Result<()> {
    db.execute(
        "INSERT INTO subagent_sessions VALUES(?1,?2,?3,?4)
        ON CONFLICT(id) DO UPDATE SET payload=excluded.payload",
        params![
            child.id,
            child.parent,
            child.generation,
            serde_json::to_vec(child)?
        ],
    )?;
    db.execute(
        "UPDATE schema_meta SET value=CAST(value AS INTEGER)+1 WHERE key='run_view_revision'",
        [],
    )?;
    Ok(())
}

impl Subagents {
    pub(crate) async fn new(
        agent: Arc<Agent>,
        store: SqliteSessionStore,
    ) -> anyhow::Result<Arc<Self>> {
        store.with_connection(|db| {
            db.execute_batch("CREATE TABLE IF NOT EXISTS subagent_sessions (
                id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
                parent TEXT NOT NULL, generation TEXT NOT NULL, payload BLOB NOT NULL);
                CREATE INDEX IF NOT EXISTS subagents_parent ON subagent_sessions(parent,generation);")?;
            Ok(())
        }).await?;
        Ok(Arc::new(Self {
            runner: AgentRunner::new(agent, Arc::new(store.clone())),
            store,
            active: Mutex::new(HashMap::new()),
        }))
    }

    async fn save(&self, child: &Child) -> anyhow::Result<()> {
        let child = child.clone();
        self.store
            .with_connection(move |db| write_child(db, &child))
            .await
    }

    async fn read(&self, owner: String, generation: String, id: String) -> anyhow::Result<Child> {
        self.store
            .with_connection(move |db| {
                let bytes: Option<Vec<u8>> = db.query_row(
                "SELECT payload FROM subagent_sessions WHERE id=?1 AND parent=?2 AND generation=?3",
                params![id,owner,generation], |row|row.get(0)).optional()?;
                let mut child:Child=serde_json::from_slice(&bytes.ok_or(ChildNotFound)?)?;
                if child.task.is_empty() {
                    child.task=db.query_row("SELECT json_extract(payload,'$.request.message.text') FROM submissions WHERE conversation_id=?1 AND json_extract(payload,'$.request.control')=0 ORDER BY sequence LIMIT 1",
                        [&id],|row|row.get(0)).optional()?.unwrap_or_default();
                }
                Ok(child)
            })
            .await
    }

    async fn load(&self, parent: &Session, id: &str) -> anyhow::Result<Child> {
        self.read(
            parent.id().as_str().into(),
            parent.generation().into(),
            id.into(),
        )
        .await
    }

    async fn ids(&self, parent: &Session) -> anyhow::Result<Vec<String>> {
        let (owner, generation) = (
            parent.id().as_str().to_owned(),
            parent.generation().to_owned(),
        );
        self.store.with_connection(move |db| {
            Ok(db.prepare("SELECT id FROM subagent_sessions WHERE parent=?1 AND generation=?2 ORDER BY id")?
                .query_map(params![owner,generation], |row|row.get(0))?.collect::<Result<Vec<_>,_>>()?)
        }).await
    }

    fn request(&self, id: &str, text: String) -> anyhow::Result<SubmissionRequest> {
        let mut message = IncomingMessage::new(InterfaceId("subagent".into()), id, text);
        if let Some((run, call)) = super::run_state::CURRENT_TOOL
            .try_with(Clone::clone)
            .ok()
            .flatten()
        {
            use sha2::{Digest, Sha256};
            message.request_id = format!("{:x}", Sha256::digest(serde_json::to_vec(&(run, call))?));
        }
        let mut request = SubmissionRequest::new(&SessionId::new(id), message, BusyMode::Steer);
        // Literal /stop and /new in delegated text are tasks, not chat controls.
        request.control = false;
        self.runner.prepare_submission(&mut request);
        Ok(request)
    }

    #[cfg(test)]
    async fn submit(
        &self,
        id: &str,
        text: String,
    ) -> anyhow::Result<super::inbox::SubmissionRecord> {
        self.runner.submit(self.request(id, text)?).await
    }

    async fn admit(
        &self,
        mut child: Child,
        session: Option<Session>,
        text: String,
    ) -> anyhow::Result<(Child, bool)> {
        let request = self.request(&child.id, text)?;
        let hash = request.hash()?;
        self.store.with_connection(move |db| {
            let tx=db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if let Some(session)=session {
                let count:i64=tx.query_row("SELECT count(*) FROM subagent_sessions WHERE parent=?1 AND generation=?2",
                    params![child.parent,child.generation],|r|r.get(0))?;
                ensure!(count<MAX_CHILDREN,"At most sixteen child sessions per parent session");
                super::session::save_in_transaction(&tx,&session)?;
            }
            let stopping:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runs r JOIN run_controls c ON c.run_id=r.id WHERE r.session_id=?1 AND r.terminal=0)
                OR EXISTS(SELECT 1 FROM submissions WHERE conversation_id=?1 AND status='Queued' AND json_extract(payload,'$.request.control')=1)",
                [&child.id],|r|r.get(0))?;
            ensure!(!stopping,"Child agent is stopping; wait before sending more input");
            let submission=super::inbox::admit_submission(&tx,request,hash)?;
            if !submission.duplicate {
                child.submission=submission.id;
                child.status=Status::Running;
                child.result=None;
                child.error=None;
                write_child(&tx,&child)?;
            }
            tx.commit()?;
            Ok((child,submission.duplicate))
        }).await
    }

    pub(crate) async fn spawn(
        self: &Arc<Self>,
        parent: &ParentContext,
        task: String,
    ) -> anyhow::Result<Child> {
        parent.active()?;
        validate_message(&task)?;
        let mut active = self.active.lock().await;
        ensure!(
            active.len() < MAX_ACTIVE,
            "At most three child agents may run concurrently in this worker"
        );
        let id = format!("{PREFIX}{}", uuid::Uuid::new_v4());
        let ownership = self.store.execution_lock("subagent", &id).await?;
        let session = parent.session.child(SessionId::new(&id));
        let child = Child {
            id,
            parent: parent.session.id().as_str().into(),
            generation: parent.session.generation().into(),
            submission: String::new(),
            status: Status::Running,
            result: None,
            error: None,
            task: task.clone(),
        };
        let (child, _) = self.admit(child, Some(session), task).await?;
        self.start(&mut active, parent, child.clone(), ownership);
        Ok(child)
    }

    fn start(
        self: &Arc<Self>,
        active: &mut HashMap<String, watch::Sender<bool>>,
        parent: &ParentContext,
        child: Child,
        ownership: Arc<ExecutionOwnership>,
    ) {
        let (cancel, receiver) = watch::channel(false);
        active.insert(child.id.clone(), cancel);
        let manager = self.clone();
        let finished = parent.finished.clone();
        tokio::spawn(
            super::run_state::CURRENT_EXECUTION.scope(Some(ownership), async move {
                manager.drive(child, finished, receiver).await;
            }),
        );
    }

    pub(crate) async fn send(
        self: &Arc<Self>,
        parent: &ParentContext,
        id: &str,
        message: String,
    ) -> anyhow::Result<Child> {
        parent.active()?;
        validate_message(&message)?;
        let mut active = self.active.lock().await;
        let mut child = self.load(&parent.session, id).await?;
        let ownership = if let Some(cancel) = active.get(id) {
            ensure!(
                !*cancel.borrow(),
                "Child agent is stopping; wait before sending more input"
            );
            None
        } else {
            ensure!(
                active.len() < MAX_ACTIVE,
                "At most three child agents may run concurrently in this worker"
            );
            let ownership = self
                .store
                .execution_lock("subagent", id)
                .await
                .context("Child agent is owned by another worker")?;
            self.reconcile_owned(&mut child).await?;
            Some(ownership)
        };
        let (child, duplicate) = self.admit(child, None, message).await?;
        if !duplicate && let Some(ownership) = ownership {
            self.start(&mut active, parent, child.clone(), ownership);
        }
        Ok(child)
    }

    async fn reconcile_owned(&self, child: &mut Child) -> anyhow::Result<()> {
        *child = self
            .read(
                child.parent.clone(),
                child.generation.clone(),
                child.id.clone(),
            )
            .await?;
        let id = child.id.clone();
        let outstanding:bool=self.store.with_connection(move|db| Ok(db.query_row(
            "SELECT EXISTS(SELECT 1 FROM runs WHERE session_id=?1 AND terminal=0)
             OR EXISTS(SELECT 1 FROM submissions WHERE conversation_id=?1 AND status IN ('Queued','Running'))",
            [id],|r|r.get(0))?)).await?;
        if outstanding {
            // shortcut: cancel abandoned execution; add automatic recovery only with durable parent reattachment.
            self.runner
                .cancel_conversation(&SessionId::new(&child.id))
                .await?;
            self.stop_queue(&child.id).await?;
            child.status = Status::Interrupted;
            child.error=Some("Child execution was interrupted. Tool outcomes may be unknown; inspect external state before repeating actions.".into());
            self.save(child).await?;
        } else if child.status == Status::Running {
            let submission = self
                .store
                .submission_by_id(&child.submission)
                .await?
                .context("Missing child input")?;
            match submission.status {
                SubmissionStatus::Done => {
                    child.status = Status::Completed;
                    child.result = submission.answer.map(|a| bounded(a.content));
                }
                SubmissionStatus::Cancelled => child.status = Status::Stopped,
                _ => {
                    child.status = Status::Failed;
                    child.error = Some("Child input could not be completed".into());
                }
            }
            self.save(child).await?;
        }
        Ok(())
    }

    async fn inspect(
        &self,
        parent: &Session,
        id: &str,
        active: &HashMap<String, watch::Sender<bool>>,
    ) -> anyhow::Result<Child> {
        let mut child = self.load(parent, id).await?;
        if !active.contains_key(id) {
            match self.store.execution_lock("subagent", id).await {
                Ok(_ownership) => self.reconcile_owned(&mut child).await?,
                Err(error)
                    if matches!(
                        error.downcast_ref::<std::fs::TryLockError>(),
                        Some(std::fs::TryLockError::WouldBlock)
                    ) =>
                {
                    // Terminal publication can precede lock release, which produces no stream revision.
                }
                Err(error) => return Err(error),
            }
        }
        Ok(child)
    }

    pub(crate) async fn list(&self, parent: &Session) -> anyhow::Result<Vec<Child>> {
        let active = self.active.lock().await;
        let mut children = Vec::new();
        for id in self.ids(parent).await? {
            children.push(self.inspect(parent, &id, &active).await?);
        }
        Ok(children)
    }

    pub(crate) async fn cancellation_requested(&self, id: &str) -> anyhow::Result<bool> {
        if let Some(cancel) = self.active.lock().await.get(id) {
            return Ok(*cancel.borrow());
        }
        let id = id.to_owned();
        self.store.with_connection(move |db| Ok(db.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_controls WHERE run_id=(SELECT id FROM runs WHERE session_id=?1 ORDER BY rowid DESC LIMIT 1))",
            [id],|row|row.get(0))?)).await
    }

    async fn get(&self, parent: &Session, id: &str) -> anyhow::Result<Child> {
        let active = self.active.lock().await;
        self.inspect(parent, id, &active).await
    }

    pub(crate) async fn wait(
        &self,
        parent: &Session,
        id: &str,
        timeout_ms: u64,
    ) -> anyhow::Result<Child> {
        ensure!(
            (1..=60_000).contains(&timeout_ms),
            "timeout_ms must be between 1 and 60000"
        );
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let child = self.get(parent, id).await?;
            if child.status != Status::Running || tokio::time::Instant::now() >= deadline {
                return Ok(child);
            }
            tokio::time::sleep_until(
                deadline.min(tokio::time::Instant::now() + Duration::from_millis(50)),
            )
            .await;
        }
    }

    pub(crate) async fn stop(&self, parent: &Session, id: &str) -> anyhow::Result<Child> {
        let remote = {
            let active = self.active.lock().await;
            let child = self.load(parent, id).await?;
            if let Some(cancel) = active.get(id) {
                cancel.send_replace(true);
                false
            } else {
                child.status == Status::Running
            }
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        if remote {
            match tokio::time::timeout_at(deadline, self.stop_queue(id)).await {
                Ok(result) => result?,
                Err(_) => return self.get(parent, id).await,
            }
        }
        let left = deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .as_millis()
            .clamp(1, 60_000) as u64;
        self.wait(parent, id, left).await
    }

    async fn stop_queue(&self, id: &str) -> anyhow::Result<()> {
        self.runner
            .apply_control(
                &SessionId::new(id),
                &IncomingMessage::new(InterfaceId("subagent".into()), id, "/stop"),
            )
            .await
    }

    async fn effects_finished(&self, id: &str) -> anyhow::Result<()> {
        loop {
            match self.store.execution_lock("conversation", id).await {
                Ok(_ownership) => return Ok(()),
                Err(error)
                    if matches!(
                        error.downcast_ref::<std::fs::TryLockError>(),
                        Some(std::fs::TryLockError::WouldBlock)
                    ) =>
                {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn drive(
        self: Arc<Self>,
        mut child: Child,
        mut parent: watch::Receiver<bool>,
        mut cancel: watch::Receiver<bool>,
    ) {
        let result =
            std::panic::AssertUnwindSafe(self.drive_inner(&mut child, &mut parent, &mut cancel))
                .catch_unwind()
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("Child worker panicked")));
        if let Err(error) = result {
            if let Err(wait_error) = self.effects_finished(&child.id).await {
                log::error!("Child effects remain uncertain: {wait_error:#}");
            }
            let mut active = self.active.lock().await;
            child.status = Status::Interrupted;
            child.error = Some(bounded(format!("{error:#}; tool outcomes may be unknown")));
            if let Err(error) = self.save(&child).await {
                log::error!("Cannot save child agent outcome: {error:#}");
            }
            active.remove(&child.id);
        }
    }

    async fn drive_inner(
        &self,
        child: &mut Child,
        parent: &mut watch::Receiver<bool>,
        cancel: &mut watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        loop {
            let next = self
                .store
                .next_submission(&SessionId::new(&child.id))
                .await?;
            let Some(next) = next else {
                let mut active = self.active.lock().await;
                child.status = Status::Stopped;
                child.error = Some("Child input cancelled before execution".into());
                self.save(child).await?;
                active.remove(&child.id);
                return Ok(());
            };
            if next.request.control {
                self.runner
                    .apply_control(&SessionId::new(&child.id), &next.request.message)
                    .await?;
                continue;
            }
            let (stop, stopped) = oneshot::channel();
            let work = self.runner.run_with_cancel(
                SessionId::new(&child.id),
                next.request.message,
                None,
                Some(stopped),
                None,
            );
            tokio::pin!(work);
            let mut stopping = false;
            let result = tokio::select! {
                biased;
                _=cancelled(parent)=>{stopping=true;let _=stop.send(());work.await},
                _=cancelled(cancel)=>{stopping=true;let _=stop.send(());work.await},
                _=tokio::time::sleep(Duration::from_secs(300))=>{stopping=true;let _=stop.send(());work.await},
                result=&mut work=>result,
            };
            stopping |= result
                .as_ref()
                .is_err_and(|error| error.to_string() == "Stopped by user");
            if stopping {
                if let Some(cancel) = self.active.lock().await.get(&child.id) {
                    cancel.send_replace(true);
                }
                self.stop_queue(&child.id).await?;
            }
            self.effects_finished(&child.id).await?;
            let mut active = self.active.lock().await;
            // New input after a committed failure clears the inbox block and still needs a worker.
            if !stopping
                && self
                    .store
                    .next_submission(&SessionId::new(&child.id))
                    .await?
                    .is_some()
            {
                drop(active);
                continue;
            }
            *child = self
                .read(
                    child.parent.clone(),
                    child.generation.clone(),
                    child.id.clone(),
                )
                .await?;
            match result {
                Ok(answer) => {
                    child.status = Status::Completed;
                    child.result = Some(bounded(answer.content));
                }
                Err(error) if error.is::<super::recovery::Suspended>() => {
                    child.status = Status::Interrupted;
                    child.error =
                        Some("Child worker suspended; tool outcomes may be unknown".into());
                }
                Err(error) if stopping || error.to_string() == "Stopped by user" => {
                    child.status = Status::Stopped;
                    child.error =
                        Some("Child stopped; interrupted tool outcomes may be unknown".into());
                }
                Err(error) => {
                    child.status = Status::Failed;
                    child.error = Some(bounded(format!("{error:#}")));
                }
            }
            self.save(child).await?;
            active.remove(&child.id);
            return Ok(());
        }
    }
}

#[cfg(test)]
#[path = "subagent_tests.rs"]
mod tests;
