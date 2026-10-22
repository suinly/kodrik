use super::{
    CompactionCandidate, CompactionSnapshot, Compactor, ContextBuilder, Session, Tool, hash,
};
use crate::agent::{run_state::Checkpoint, session::SqliteSessionStore};
use anyhow::{Context, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Job {
    pub id: String,
    pub run_id: String,
    conversation_id: String,
    pub status: String,
    snapshot: CompactionSnapshot,
    candidate: Option<CompactionCandidate>,
    error: Option<String>,
    pub model_calls: u64,
    pub known_input_tokens: u64,
    unknown_calls: u64,
}
pub(crate) fn migrate(db: &Connection) -> anyhow::Result<()> {
    db.execute_batch("CREATE TABLE compaction_jobs(session_id TEXT PRIMARY KEY,id TEXT NOT NULL,run_id TEXT NOT NULL,status TEXT NOT NULL,payload BLOB NOT NULL);")?;
    for action in ["INSERT", "UPDATE", "DELETE"] {
        db.execute_batch(&format!("CREATE TRIGGER view_compaction_jobs_{action} AFTER {action} ON compaction_jobs BEGIN UPDATE schema_meta SET value=CAST(value AS INTEGER)+1 WHERE key='run_view_revision'; END;"))?;
    }
    db.pragma_update(None, "user_version", 6)?;
    Ok(())
}
fn read(db: &Connection, session: &str) -> anyhow::Result<Option<Job>> {
    let bytes: Option<Vec<u8>> = db
        .query_row(
            "SELECT payload FROM compaction_jobs WHERE session_id=?1",
            [session],
            |r| r.get(0),
        )
        .optional()?;
    bytes.map(|b| Ok(serde_json::from_slice(&b)?)).transpose()
}
fn write(db: &Connection, job: &Job) -> anyhow::Result<()> {
    db.execute("INSERT INTO compaction_jobs VALUES(?1,?2,?3,?4,?5) ON CONFLICT(session_id) DO UPDATE SET id=excluded.id,run_id=excluded.run_id,status=excluded.status,payload=excluded.payload",params![job.snapshot.session.id().as_str(),job.id,job.run_id,job.status,serde_json::to_vec(job)?])?;
    Ok(())
}
pub(crate) fn view(db: &Connection, session: &str) -> anyhow::Result<Option<serde_json::Value>> {
    Ok(read(db,session)?.map(|job|serde_json::json!({"id":job.id,"run_id":job.run_id,"status":job.status,"through":job.snapshot.through,"model":job.snapshot.model_identity,"model_calls":job.model_calls,"input_tokens":(job.unknown_calls==0).then_some(job.known_input_tokens),"error":job.error})))
}
impl CompactionSnapshot {
    fn compatible(&self, session: &Session) -> anyhow::Result<bool> {
        Ok(self.session.id() == session.id()
            && self.session.generation() == session.generation()
            && self.session.compaction() == session.compaction()
            && session.messages().len() >= self.through
            && hash(&self.session.messages()[..self.through])?
                == hash(&session.messages()[..self.through])?)
    }
}
impl SqliteSessionStore {
    pub(crate) async fn guard_compaction_storage(&self, run: &str) -> anyhow::Result<()> {
        let errors = self.compaction_errors.lock().unwrap().clone();
        if errors.is_empty() {
            return Ok(());
        }
        let run = run.to_owned();
        self.with_connection(move |db| {
            let id: Option<String> = db
                .query_row(
                    "SELECT id FROM compaction_jobs WHERE run_id=?1",
                    [run],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(error) = id.and_then(|id| errors.get(&id).cloned()) {
                anyhow::bail!("Compaction storage failed: {error}");
            }
            Ok(())
        })
        .await
    }
    pub(crate) async fn compaction_job(&self, session: &str) -> anyhow::Result<Option<Job>> {
        let session = session.to_owned();
        self.with_connection(move |db| read(db, &session)).await
    }
    async fn put_compaction(&self, job: Job) -> anyhow::Result<()> {
        self.with_connection(move |db| {
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if matches!(job.status.as_str(), "preparing" | "ready") {
                let cancelled: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM run_controls WHERE run_id=?1)",
                    [&job.run_id],
                    |r| r.get(0),
                )?;
                let run: Vec<u8> =
                    tx.query_row("SELECT payload FROM runs WHERE id=?1", [&job.run_id], |r| {
                        r.get(0)
                    })?;
                let run: crate::agent::run_state::RunRecord = serde_json::from_slice(&run)?;
                ensure!(
                    !cancelled
                        && !matches!(
                            run.phase,
                            crate::agent::run_state::RunPhase::Cancelled
                                | crate::agent::run_state::RunPhase::Failed { .. }
                        ),
                    "Compaction owner was stopped"
                );
                let active: Option<String> = tx
                    .query_row(
                        "SELECT session_id FROM active_sessions WHERE conversation_id=?1",
                        [&job.conversation_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                ensure!(
                    active.as_deref().unwrap_or(&job.conversation_id)
                        == job.snapshot.session.id().as_str(),
                    "Compaction session selection changed"
                );
                let bytes: Vec<u8> = tx.query_row(
                    "SELECT payload FROM sessions WHERE id=?1",
                    [job.snapshot.session.id().as_str()],
                    |r| r.get(0),
                )?;
                let saved: Session = serde_json::from_slice(&bytes)?;
                ensure!(
                    saved.generation() == job.snapshot.session.generation(),
                    "Compaction session reset"
                );
            }
            write(&tx, &job)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }
    pub(super) async fn compaction_usage(
        &self,
        session: &str,
        id: &str,
        tokens: Option<Option<u64>>,
    ) -> anyhow::Result<()> {
        let (session, id) = (session.to_owned(), id.to_owned());
        let id_for_error = id.clone();
        self.with_connection(move |db| {
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let mut job = read(&tx, &session)?.context("Compaction job missing")?;
            if job.id != id || job.status != "preparing" {
                return Ok(());
            }
            let cancelled: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM run_controls WHERE run_id=?1)",
                [&job.run_id],
                |r| r.get(0),
            )?;
            if cancelled {
                return Ok(());
            }
            if let Some(tokens) = tokens {
                if let Some(tokens) = tokens {
                    job.unknown_calls -= 1;
                    job.known_input_tokens += tokens;
                }
            } else {
                job.model_calls += 1;
                job.unknown_calls += 1;
            }
            write(&tx, &job)?;
            tx.commit()?;
            Ok(())
        })
        .await
        .inspect_err(|e| {
            self.compaction_errors
                .lock()
                .unwrap()
                .insert(id_for_error, format!("{e:#}"));
        })
    }
    async fn finish_compaction(
        &self,
        session: &str,
        id: &str,
        result: Result<CompactionCandidate, String>,
    ) -> anyhow::Result<()> {
        let (session, id) = (session.to_owned(), id.to_owned());
        self.with_connection(move |db| {
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if let Some(mut job) = read(&tx, &session)?
                && job.id == id
                && job.status == "preparing"
            {
                let cancelled: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM run_controls WHERE run_id=?1)",
                    [&job.run_id],
                    |r| r.get(0),
                )?;
                if cancelled {
                    job.status = "cancelled".into();
                } else {
                    match result {
                        Ok(candidate) => {
                            job.candidate = Some(candidate);
                            job.status = "ready".into();
                        }
                        Err(error) => {
                            job.error = Some(error);
                            job.status = "failed".into();
                        }
                    }
                }
                write(&tx, &job)?;
            }
            tx.commit()?;
            #[cfg(test)]
            crate::agent::durable_fixture::boundary("compaction_ready")?;
            Ok(())
        })
        .await
    }
    async fn apply_compaction(&self, session: &str, id: &str) -> anyhow::Result<Option<Session>> {
        let (session, id) = (session.to_owned(), id.to_owned());
        self.with_connection(move |db| {
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let mut job = read(&tx, &session)?.context("Compaction job missing")?;
            if job.id != id || job.status != "ready" {
                return Ok(None);
            }
            let bytes: Vec<u8> = tx.query_row(
                "SELECT payload FROM sessions WHERE id=?1",
                [&session],
                |r| r.get(0),
            )?;
            let mut saved: Session = serde_json::from_slice(&bytes)?;
            let active: Option<String> = tx
                .query_row(
                    "SELECT session_id FROM active_sessions WHERE conversation_id=?1",
                    [&job.conversation_id],
                    |r| r.get(0),
                )
                .optional()?;
            let cancelled: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM run_controls WHERE run_id=?1)",
                [&job.run_id],
                |r| r.get(0),
            )?;
            let applied = !cancelled
                && active.as_deref().unwrap_or(&job.conversation_id) == session
                && job
                    .candidate
                    .as_ref()
                    .context("Ready candidate missing")?
                    .apply(&mut saved)?;
            job.status = if cancelled {
                "cancelled"
            } else if applied {
                "applied"
            } else {
                "stale"
            }
            .into();
            if applied {
                tx.execute(
                    "UPDATE sessions SET payload=?2 WHERE id=?1",
                    params![session, serde_json::to_vec(&saved)?],
                )?;
            }
            write(&tx, &job)?;
            tx.commit()?;
            Ok(applied.then_some(saved))
        })
        .await
    }
}
struct WorkerGuard {
    store: SqliteSessionStore,
    id: String,
}
impl Drop for WorkerGuard {
    fn drop(&mut self) {
        self.store
            .compaction_running
            .lock()
            .unwrap()
            .remove(&self.id);
    }
}
impl Compactor {
    fn spawn_job(&self, store: SqliteSessionStore, job: Job) {
        if !store
            .compaction_running
            .lock()
            .unwrap()
            .insert(job.id.clone())
        {
            return;
        }
        let guard = WorkerGuard {
            store: store.clone(),
            id: job.id.clone(),
        };
        let compactor = self.clone();
        tokio::spawn(async move {
            let _guard = guard;
            let session = job.snapshot.session.id().as_str();
            let result=async {
                let _lock=loop {
                    let current=store.compaction_job(session).await?.context("Compaction missing")?;
                    if current.id!=job.id || current.status!="preparing" {return Ok(());}
                    match store.execution_lock("compaction",session).await {
                        Ok(lock)=>break lock,
                        Err(error) if matches!(error.downcast_ref::<std::fs::TryLockError>(),Some(std::fs::TryLockError::WouldBlock))=>tokio::time::sleep(std::time::Duration::from_millis(100)).await,
                        Err(error)=>return Err(error),
                    }
                };
                let current=store.compaction_job(session).await?.context("Compaction missing")?;
                if current.id!=job.id || current.status!="preparing" {return Ok(());}
                let cancelled=async {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        let current=store.compaction_job(session).await?.context("Compaction missing")?;
                        if current.id!=job.id || current.status!="preparing" || store.cancellation_requested(&current.run_id).await? {return Ok::<_,anyhow::Error>(());}
                    }
                };
                let result=tokio::select! {
                    result=compactor.generate(&job.snapshot,Some((&store,session,&job.id)))=>result.map_err(|e|format!("{e:#}")),
                    result=cancelled=>{result?;return Ok(());},
                };
                store.finish_compaction(session,&job.id,result).await
            }.await;
            if let Err(error) = result {
                store
                    .compaction_errors
                    .lock()
                    .unwrap()
                    .insert(job.id.clone(), format!("{error:#}"));
                log::warn!("Compaction worker failed: {error:#}");
            }
        });
    }
    pub(crate) async fn durable_boundary(
        &self,
        session: &mut Session,
        builder: &ContextBuilder,
        tools: &[&dyn Tool],
        cp: &mut Checkpoint,
        margin: u64,
    ) -> anyhow::Result<()> {
        self.boundary(session, builder, tools, cp, margin, false)
            .await
    }

    pub(crate) async fn force_durable(
        &self,
        session: &mut Session,
        builder: &ContextBuilder,
        tools: &[&dyn Tool],
        cp: &mut Checkpoint,
    ) -> anyhow::Result<()> {
        ensure!(
            self.threshold != 0,
            "Context overflow: compaction disabled; reduce input or start a new session"
        );
        self.boundary(session, builder, tools, cp, 0, true).await
    }

    async fn boundary(
        &self,
        session: &mut Session,
        builder: &ContextBuilder,
        tools: &[&dyn Tool],
        cp: &mut Checkpoint,
        margin: u64,
        force: bool,
    ) -> anyhow::Result<()> {
        if self.threshold == 0 {
            return Ok(());
        }
        loop {
            let required = force
                || self
                    .prepare(session, builder, tools, self.threshold)?
                    .is_some();
            if let Some(mut job) = cp.store.compaction_job(session.id().as_str()).await? {
                let storage_error = cp
                    .store
                    .compaction_errors
                    .lock()
                    .unwrap()
                    .get(&job.id)
                    .cloned();
                if let Some(error) = storage_error {
                    if job.run_id == cp.record.id {
                        anyhow::bail!("Compaction storage failed: {error}");
                    }
                    job.status = "failed".into();
                    job.error = Some(error);
                    cp.store.put_compaction(job.clone()).await?;
                }
                if matches!(job.status.as_str(), "preparing" | "ready") {
                    if !job.snapshot.compatible(session)?
                        || job.snapshot.model_identity != self.llm.identity()
                    {
                        job.status = "stale".into();
                        cp.store.put_compaction(job).await?;
                        continue;
                    }
                    if job.run_id != cp.record.id {
                        let (session_id, id, run, conversation) = (
                            session.id().as_str().to_owned(),
                            job.id.clone(),
                            cp.record.id.clone(),
                            cp.record.conversation_id.clone(),
                        );
                        job = cp
                            .store
                            .with_connection(move |db| {
                                let tx = db.transaction_with_behavior(
                                    rusqlite::TransactionBehavior::Immediate,
                                )?;
                                let mut current =
                                    read(&tx, &session_id)?.context("Compaction missing")?;
                                ensure!(current.id == id, "Compaction replaced during adoption");
                                current.run_id = run;
                                current.conversation_id = conversation;
                                write(&tx, &current)?;
                                tx.commit()?;
                                Ok(current)
                            })
                            .await?;
                    }
                    if job.status == "ready" {
                        if let Some(saved) = cp
                            .store
                            .apply_compaction(session.id().as_str(), &job.id)
                            .await?
                        {
                            *session = saved;
                            if force
                                || self
                                    .prepare(session, builder, tools, self.threshold)?
                                    .is_none()
                            {
                                return Ok(());
                            }
                        }
                        continue;
                    }
                    self.spawn_job(cp.store.clone(), job);
                    if !required {
                        return Ok(());
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    continue;
                }
                if job.status == "failed" && job.run_id == cp.record.id {
                    if required {
                        anyhow::bail!(
                            "Context compaction failed; history was preserved: {}",
                            job.error.unwrap_or_default()
                        );
                    }
                    return Ok(());
                }
                if job.status == "cancelled" && job.run_id == cp.record.id {
                    anyhow::bail!("Stopped by user");
                }
            }
            let trigger = if force {
                0
            } else if margin == 0 {
                self.threshold
            } else {
                self.threshold.saturating_sub(margin)
            };
            let Some(snapshot) = self.prepare(session, builder, tools, trigger)? else {
                ensure!(
                    !force,
                    "Context overflow: no compactable history; reduce input or start a new session"
                );
                return Ok(());
            };
            let job = Job {
                id: uuid::Uuid::new_v4().to_string(),
                run_id: cp.record.id.clone(),
                conversation_id: cp.record.conversation_id.clone(),
                status: "preparing".into(),
                snapshot,
                candidate: None,
                error: None,
                model_calls: 0,
                known_input_tokens: 0,
                unknown_calls: 0,
            };
            cp.store.put_compaction(job.clone()).await?;
            self.spawn_job(cp.store.clone(), job);
            if !required {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{
        AgentEvent, AssistantMessage, LlmContext, UserMessage,
        llm::LlmProvider,
        message::Message,
        run_state::RunPhase,
        session::{SessionId, SessionStore},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Model {
        calls: AtomicUsize,
        fail: bool,
        usage: Option<u64>,
    }
    #[async_trait::async_trait]
    impl LlmProvider for Model {
        async fn generate(
            &self,
            _: &LlmContext<'_>,
            _: &[&dyn Tool],
            _: Option<&tokio::sync::mpsc::Sender<AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                anyhow::bail!("summary provider failed");
            }
            Ok((
                vec![Message::Assistant(AssistantMessage {
                    content: "summary".into(),
                })],
                self.usage,
            ))
        }
    }
    async fn seed() -> (tempfile::TempDir, SqliteSessionStore, Session, Checkpoint) {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().into()).unwrap();
        let mut session = Session::new(SessionId::new("cli:compact-jobs"));
        for _ in 0..8 {
            session.push(Message::User(UserMessage {
                content: "old ".repeat(1500),
                files: vec![],
                images: vec![],
            }));
        }
        session.push(Message::User(UserMessage {
            content: "latest".into(),
            files: vec![],
            images: vec![],
        }));
        let mut cp = Checkpoint::new(&store, session.id(), &session);
        cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
        (dir, store, session, cp)
    }
    fn job(compactor: &Compactor, session: &Session, cp: &Checkpoint) -> Job {
        Job {
            id: uuid::Uuid::new_v4().to_string(),
            run_id: cp.record.id.clone(),
            conversation_id: cp.record.conversation_id.clone(),
            status: "preparing".into(),
            snapshot: compactor
                .prepare(session, &ContextBuilder::new("rules"), &[], 10000)
                .unwrap()
                .unwrap(),
            candidate: None,
            error: None,
            model_calls: 0,
            known_input_tokens: 0,
            unknown_calls: 0,
        }
    }
    async fn status(store: &SqliteSessionStore, id: &str, expected: &str) -> Job {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let job = store.compaction_job(id).await.unwrap().unwrap();
                if job.status == expected {
                    return job;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }
    #[tokio::test]
    async fn superseded_usage_does_not_fail_owner_checkpoint() {
        let (_dir, store, session, mut cp) = seed().await;
        let compactor = Compactor {
            llm: Arc::new(Model {
                calls: AtomicUsize::new(0),
                fail: false,
                usage: None,
            }),
            threshold: 40000,
        };
        let old = job(&compactor, &session, &cp);
        store.put_compaction(old.clone()).await.unwrap();
        let newer = job(&compactor, &session, &cp);
        store.put_compaction(newer.clone()).await.unwrap();
        store
            .compaction_usage(session.id().as_str(), &old.id, None)
            .await
            .unwrap();
        cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
        let current = store
            .compaction_job(session.id().as_str())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.id, newer.id);
        assert_eq!(current.model_calls, 0);
    }
    #[tokio::test]
    async fn compact_ready_and_preparing_jobs_resume_after_store_reopen() {
        for ready in [true, false] {
            let (dir, store, mut session, mut cp) = seed().await;
            let llm = Arc::new(Model {
                calls: AtomicUsize::new(0),
                fail: false,
                usage: None,
            });
            let compactor = Compactor {
                llm: llm.clone(),
                threshold: 40000,
            };
            let mut job = job(&compactor, &session, &cp);
            if ready {
                job.candidate = Some(compactor.generate(&job.snapshot, None).await.unwrap());
                job.status = "ready".into();
            }
            store.put_compaction(job.clone()).await.unwrap();
            cp.store = SqliteSessionStore::open(dir.path().into()).unwrap();
            drop(store);
            compactor
                .durable_boundary(
                    &mut session,
                    &ContextBuilder::new("rules"),
                    &[],
                    &mut cp,
                    30000,
                )
                .await
                .unwrap();
            if !ready {
                status(&cp.store, session.id().as_str(), "ready").await;
                compactor
                    .durable_boundary(
                        &mut session,
                        &ContextBuilder::new("rules"),
                        &[],
                        &mut cp,
                        30000,
                    )
                    .await
                    .unwrap();
            }
            assert!(session.compaction().is_some());
            assert_eq!(llm.calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                cp.store
                    .compaction_job(session.id().as_str())
                    .await
                    .unwrap()
                    .unwrap()
                    .id,
                job.id
            );
            if !ready {
                let view = cp.store.run_view(&cp.record.id).await.unwrap();
                assert!(view.compaction.unwrap()["input_tokens"].is_null());
            }
        }
    }
    #[tokio::test]
    async fn compact_failed_background_is_not_retried_in_the_same_run() {
        let (_dir, store, mut session, mut cp) = seed().await;
        let llm = Arc::new(Model {
            calls: AtomicUsize::new(0),
            fail: true,
            usage: Some(1),
        });
        let compactor = Compactor {
            llm: llm.clone(),
            threshold: 40000,
        };
        compactor
            .durable_boundary(
                &mut session,
                &ContextBuilder::new("rules"),
                &[],
                &mut cp,
                30000,
            )
            .await
            .unwrap();
        status(&store, session.id().as_str(), "failed").await;
        compactor
            .durable_boundary(
                &mut session,
                &ContextBuilder::new("rules"),
                &[],
                &mut cp,
                30000,
            )
            .await
            .unwrap();
        session.input_tokens = Some(50000);
        cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
        assert!(
            compactor
                .durable_boundary(
                    &mut session,
                    &ContextBuilder::new("rules"),
                    &[],
                    &mut cp,
                    30000
                )
                .await
                .is_err()
        );
        assert_eq!(llm.calls.load(Ordering::SeqCst), 1);
        assert!(session.compaction().is_none());
    }
    #[tokio::test]
    async fn compact_stop_and_session_selection_fence_ready_candidate() {
        for cancel in [true, false] {
            let (_dir, store, session, mut cp) = seed().await;
            let compactor = Compactor {
                llm: Arc::new(Model {
                    calls: AtomicUsize::new(0),
                    fail: false,
                    usage: Some(1),
                }),
                threshold: 40000,
            };
            let mut job = job(&compactor, &session, &cp);
            job.candidate = Some(compactor.generate(&job.snapshot, None).await.unwrap());
            job.status = "ready".into();
            store.put_compaction(job.clone()).await.unwrap();
            if cancel {
                store.cancel_run(&cp.record.id).await.unwrap();
            } else {
                cp.save(&session, RunPhase::Completed, None).await.unwrap();
                let other = Session::new(SessionId::new("other"));
                store.save(&other).await.unwrap();
                store.select(session.id(), other.id()).await.unwrap();
                store.select(session.id(), session.id()).await.unwrap();
            }
            assert!(
                store
                    .apply_compaction(session.id().as_str(), &job.id)
                    .await
                    .unwrap()
                    .is_none()
            );
            let saved = store.load(session.id()).await.unwrap().unwrap();
            assert!(saved.compaction().is_none());
            assert_eq!(
                store
                    .compaction_job(session.id().as_str())
                    .await
                    .unwrap()
                    .unwrap()
                    .status,
                if cancel { "cancelled" } else { "stale" }
            );
        }
    }
    #[tokio::test]
    async fn compact_two_candidates_never_roll_back_summary() {
        let (_dir, store, mut session, mut cp) = seed().await;
        let compactor = Compactor {
            llm: Arc::new(Model {
                calls: AtomicUsize::new(0),
                fail: false,
                usage: Some(1),
            }),
            threshold: 40000,
        };
        let mut first = job(&compactor, &session, &cp);
        first.candidate = Some(compactor.generate(&first.snapshot, None).await.unwrap());
        first.status = "ready".into();
        for _ in 0..4 {
            session.push(Message::User(UserMessage {
                content: "suffix ".repeat(1500),
                files: vec![],
                images: vec![],
            }));
        }
        cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
        let mut newer = job(&compactor, &session, &cp);
        newer.candidate = Some(compactor.generate(&newer.snapshot, None).await.unwrap());
        newer.status = "ready".into();
        assert!(newer.snapshot.through > first.snapshot.through);
        store.put_compaction(newer.clone()).await.unwrap();
        let saved = store
            .apply_compaction(session.id().as_str(), &newer.id)
            .await
            .unwrap()
            .unwrap();
        store.put_compaction(first.clone()).await.unwrap();
        assert!(
            store
                .apply_compaction(session.id().as_str(), &first.id)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .load(session.id())
                .await
                .unwrap()
                .unwrap()
                .compaction(),
            saved.compaction()
        );
    }
    #[tokio::test]
    async fn compact_late_job_creation_cannot_escape_stop_or_selection() {
        for cancel in [true, false] {
            let (_dir, store, session, mut cp) = seed().await;
            let compactor = Compactor {
                llm: Arc::new(Model {
                    calls: AtomicUsize::new(0),
                    fail: false,
                    usage: None,
                }),
                threshold: 40000,
            };
            let delayed = job(&compactor, &session, &cp);
            if cancel {
                store.cancel_run(&cp.record.id).await.unwrap();
            } else {
                cp.save(&session, RunPhase::Completed, None).await.unwrap();
                let other = Session::new(SessionId::new("other"));
                store.save(&other).await.unwrap();
                store.select(session.id(), other.id()).await.unwrap();
            }
            assert!(store.put_compaction(delayed).await.is_err());
            assert!(
                store
                    .compaction_job(session.id().as_str())
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn compact_new_command_invalidates_old_job_and_disabled_modes_do_not_start_one() {
        use crate::agent::{Agent, AgentRunner, tools::ToolRegistry};
        let (_dir, store, mut session, mut cp) = seed().await;
        let model = Arc::new(Model {
            calls: AtomicUsize::new(0),
            fail: false,
            usage: None,
        });
        let compactor = Compactor {
            llm: model.clone(),
            threshold: 40000,
        };
        let builder = ContextBuilder::new("rules");
        compactor
            .durable_boundary(&mut session, &builder, &[], &mut cp, 0)
            .await
            .unwrap();
        Compactor {
            llm: model.clone(),
            threshold: 0,
        }
        .durable_boundary(&mut session, &builder, &[], &mut cp, 16000)
        .await
        .unwrap();
        assert!(
            store
                .compaction_job(session.id().as_str())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(model.calls.load(Ordering::SeqCst), 0);
        let pending = job(&compactor, &session, &cp);
        store.put_compaction(pending.clone()).await.unwrap();
        let runner = AgentRunner::new(
            Arc::new(Agent::new(model, ToolRegistry::default(), builder)),
            Arc::new(store.clone()),
        );
        runner
            .apply_control(
                session.id(),
                &crate::gateway::IncomingMessage::new(
                    crate::gateway::InterfaceId("cli".into()),
                    "compact-jobs",
                    "/new",
                ),
            )
            .await
            .unwrap();
        assert_ne!(store.active_id(session.id()).await.unwrap(), *session.id());
        assert_eq!(
            store
                .compaction_job(session.id().as_str())
                .await
                .unwrap()
                .unwrap()
                .status,
            "cancelled"
        );
        assert!(
            store
                .apply_compaction(session.id().as_str(), &pending.id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn compact_deleting_session_removes_private_job_snapshot() {
        let (_dir, store, session, mut cp) = seed().await;
        let compactor = Compactor {
            llm: Arc::new(Model {
                calls: AtomicUsize::new(0),
                fail: false,
                usage: None,
            }),
            threshold: 40000,
        };
        store
            .put_compaction(job(&compactor, &session, &cp))
            .await
            .unwrap();
        cp.save(&session, RunPhase::Completed, None).await.unwrap();
        store.delete(session.id()).await.unwrap();
        assert!(
            store
                .compaction_job(session.id().as_str())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn compact_crash_child() {
        let Some(path) = std::env::var_os("KODRIK_COMPACT_FIXTURE") else {
            return;
        };
        let path = std::path::PathBuf::from(path);
        let store = SqliteSessionStore::open(path.clone()).unwrap();
        struct Pending(std::path::PathBuf);
        #[async_trait::async_trait]
        impl LlmProvider for Pending {
            fn identity(&self) -> String {
                std::any::type_name::<Model>().into()
            }
            async fn generate(
                &self,
                _: &LlmContext<'_>,
                _: &[&dyn Tool],
                _: Option<&tokio::sync::mpsc::Sender<AgentEvent>>,
            ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
                std::fs::write(self.0.join("summary-started"), "started")?;
                std::future::pending().await
            }
        }
        let job = store
            .compaction_job("cli:compact-jobs")
            .await
            .unwrap()
            .unwrap();
        Compactor {
            llm: Arc::new(Pending(path)),
            threshold: 40000,
        }
        .spawn_job(store, job);
        std::future::pending::<()>().await;
    }
    #[tokio::test]
    async fn compact_process_kill_repeats_only_unfinished_summary() {
        let (dir, store, mut session, mut cp) = seed().await;
        let llm = Arc::new(Model {
            calls: AtomicUsize::new(0),
            fail: false,
            usage: Some(9),
        });
        let compactor = Compactor {
            llm: llm.clone(),
            threshold: 40000,
        };
        let job = job(&compactor, &session, &cp);
        store.put_compaction(job.clone()).await.unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "agent::compact::jobs::tests::compact_crash_child",
                "--nocapture",
            ])
            .env("KODRIK_COMPACT_FIXTURE", dir.path())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let started = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !dir.path().join("summary-started").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        child.kill().unwrap();
        child.wait().unwrap();
        started.unwrap();
        cp.store = SqliteSessionStore::open(dir.path().into()).unwrap();
        compactor
            .durable_boundary(
                &mut session,
                &ContextBuilder::new("rules"),
                &[],
                &mut cp,
                30000,
            )
            .await
            .unwrap();
        let ready = status(&cp.store, session.id().as_str(), "ready").await;
        assert_eq!(ready.id, job.id);
        assert_eq!(ready.model_calls, 2);
        assert_eq!(ready.unknown_calls, 1);
        compactor
            .durable_boundary(
                &mut session,
                &ContextBuilder::new("rules"),
                &[],
                &mut cp,
                30000,
            )
            .await
            .unwrap();
        assert_eq!(llm.calls.load(Ordering::SeqCst), 1);
        assert!(session.compaction().is_some());
    }

    #[tokio::test]
    async fn compact_storage_failure_fences_tool_after_inflight_model() {
        use crate::agent::{
            Agent,
            tools::{ToolOutput, ToolRegistry},
        };
        struct Foreground {
            release: tokio::sync::Notify,
            calls: AtomicUsize,
        }
        #[async_trait::async_trait]
        impl LlmProvider for Foreground {
            async fn generate(
                &self,
                _: &LlmContext<'_>,
                _: &[&dyn Tool],
                _: Option<&tokio::sync::mpsc::Sender<AgentEvent>>,
            ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    self.release.notified().await;
                    Ok((
                        vec![Message::ToolCall(crate::agent::message::ToolCall {
                            call_id: "effect".into(),
                            name: "effect".into(),
                            arguments: serde_json::json!({}),
                        })],
                        None,
                    ))
                } else {
                    Ok((
                        vec![Message::Assistant(AssistantMessage {
                            content: "done".into(),
                        })],
                        None,
                    ))
                }
            }
        }
        struct Effect(Arc<AtomicUsize>);
        #[async_trait::async_trait]
        impl Tool for Effect {
            fn name(&self) -> String {
                "effect".into()
            }
            fn description(&self) -> String {
                "effect".into()
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({"type":"object"})
            }
            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolOutput> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(serde_json::json!("done").into())
            }
        }
        let (_dir, store, session, mut cp) = seed().await;
        cp.save(&session, RunPhase::Completed, None).await.unwrap();
        store.with_connection(|db| {db.execute_batch("CREATE TRIGGER fail_compaction_usage BEFORE UPDATE ON compaction_jobs BEGIN SELECT RAISE(FAIL,'disk failed'); END;")?;Ok(())}).await.unwrap();
        let model = Arc::new(Foreground {
            release: Default::default(),
            calls: AtomicUsize::new(0),
        });
        let effects = Arc::new(AtomicUsize::new(0));
        let runner = crate::agent::AgentRunner::new(
            Arc::new(
                Agent::new(
                    model.clone(),
                    ToolRegistry::default().register(Arc::new(Effect(effects.clone()))),
                    ContextBuilder::new("rules"),
                )
                .with_compaction(
                    Arc::new(Model {
                        calls: AtomicUsize::new(0),
                        fail: false,
                        usage: Some(1),
                    }),
                    40000,
                )
                .with_background_compaction(30000),
            ),
            Arc::new(store.clone()),
        );
        let id = session.id().clone();
        let work = tokio::spawn(async move {
            runner
                .run(
                    id,
                    crate::gateway::IncomingMessage::new(
                        crate::gateway::InterfaceId("cli".into()),
                        "compact-jobs",
                        "new",
                    ),
                    None,
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if !store.compaction_errors.lock().unwrap().is_empty() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        store
            .with_connection(|db| {
                db.execute_batch("DROP TRIGGER fail_compaction_usage")?;
                Ok(())
            })
            .await
            .unwrap();
        model.release.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), work)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            effects.load(Ordering::SeqCst),
            0,
            "Storage failure must fence the next external effect"
        );
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn compact_storage_failure_blocks_foreground_even_below_threshold() {
        let (_dir, store, mut session, mut cp) = seed().await;
        let compactor = Compactor {
            llm: Arc::new(Model {
                calls: AtomicUsize::new(0),
                fail: false,
                usage: Some(1),
            }),
            threshold: 40000,
        };
        store.with_connection(|db| {db.execute_batch("CREATE TRIGGER fail_compaction_usage BEFORE UPDATE ON compaction_jobs BEGIN SELECT RAISE(FAIL,'disk failed'); END;")?;Ok(())}).await.unwrap();
        compactor
            .durable_boundary(
                &mut session,
                &ContextBuilder::new("rules"),
                &[],
                &mut cp,
                30000,
            )
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if !store.compaction_errors.lock().unwrap().is_empty() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            compactor
                .durable_boundary(
                    &mut session,
                    &ContextBuilder::new("rules"),
                    &[],
                    &mut cp,
                    30000
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("storage failed")
        );
    }
}
