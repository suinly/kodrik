use super::{Session, SqliteSessionStore};
use crate::agent::run_state::{RunRecord, ToolExecution};
use anyhow::{Context, ensure};
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    sync::Arc,
};

pub(crate) struct ExecutionOwnership {
    pub(super) _locks: Vec<File>,
    _parents: Vec<Arc<ExecutionOwnership>>,
    _store: Option<SqliteSessionStore>,
}
impl ExecutionOwnership {
    pub(crate) fn join(parents: Vec<Arc<Self>>) -> Arc<Self> {
        Arc::new(Self {
            _locks: vec![],
            _parents: parents,
            _store: None,
        })
    }
}
impl SqliteSessionStore {
    pub(crate) async fn execution_lock(
        &self,
        namespace: &str,
        id: &str,
    ) -> anyhow::Result<Arc<ExecutionOwnership>> {
        let dir = self.directory.clone();
        let store = self.clone();
        let key = format!("{namespace}:{id}");
        let wait = namespace == "recovery";
        tokio::task::spawn_blocking(move || {
            let dir = dir.join("execution-locks");
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)?;
            ensure!(
                std::fs::symlink_metadata(&dir)?.is_dir(),
                "Invalid execution lock directory"
            );
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(dir.join(format!("{:x}.lock", Sha256::digest(key.as_bytes()))))?;
            if wait {
                file.lock().context("Cannot acquire recovery scan lock")?;
            } else {
                file.try_lock()
                    .context("Execution is owned by another Kodrik worker")?;
            }
            Ok(Arc::new(ExecutionOwnership {
                _locks: vec![file],
                _parents: vec![],
                _store: Some(store),
            }))
        })
        .await?
    }
    pub(crate) async fn claim_execution(
        &self,
        conversation: &str,
        session: &str,
    ) -> anyhow::Result<Arc<ExecutionOwnership>> {
        let mut conversation = self.execution_lock("conversation", conversation).await?;
        let mut session = self.execution_lock("session", session).await?;
        Arc::get_mut(&mut conversation)
            .unwrap()
            ._locks
            .append(&mut Arc::get_mut(&mut session).unwrap()._locks);
        Ok(conversation)
    }
    pub(crate) async fn unfinished_runs(&self) -> anyhow::Result<Vec<RunRecord>> {
        self.with_connection(|db| {
            let mut stmt = db.prepare("SELECT payload,EXISTS(SELECT 1 FROM run_controls c WHERE c.run_id=runs.id) FROM runs WHERE terminal=0 ORDER BY id")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?,r.get::<_,bool>(1)?)))?;
            rows.map(|r| {let (payload,cancelled)=r?;let mut run:RunRecord=serde_json::from_slice(&payload)?;run.cancel_requested|=cancelled;Ok(run)}).collect()
        })
        .await
    }
    pub(crate) async fn load_run(&self, id: &str) -> anyhow::Result<Option<(RunRecord, Session)>> {
        let id = id.to_owned();
        self.with_connection(move |db| {
   let row:Option<(Vec<u8>,Vec<u8>)>=db.query_row("SELECT r.payload,s.payload FROM runs r JOIN sessions s ON s.id=r.session_id WHERE r.id=?1",[&id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
   row.map(|(run,session)|{let mut run:RunRecord=serde_json::from_slice(&run)?;let session:Session=serde_json::from_slice(&session)?;
    ensure!(run.id==id && run.session_id==session.id().as_str() && run.schema_version==1,"Invalid saved run binding or version");
    run.cancel_requested |= db.query_row("SELECT EXISTS(SELECT 1 FROM run_controls WHERE run_id=?1)",[&id],|r|r.get::<_,bool>(0))?;
    Ok((run,session))}).transpose()
  }).await
    }
    pub(crate) async fn tool_execution(
        &self,
        id: &str,
        call: &str,
    ) -> anyhow::Result<Option<ToolExecution>> {
        let (id, call) = (id.to_owned(), call.to_owned());
        self.with_connection(move |db| {
            let data: Option<Vec<u8>> = db
                .query_row(
                    "SELECT payload FROM tool_executions WHERE run_id=?1 AND call_id=?2",
                    rusqlite::params![id, call],
                    |r| r.get(0),
                )
                .optional()?;
            data.map(|data| Ok(serde_json::from_slice(&data)?))
                .transpose()
        })
        .await
    }
    pub(crate) async fn request_cancel(&self, id: &str) -> anyhow::Result<()> {
        let id = id.to_owned();
        self.with_connection(move|db| {db.execute("INSERT INTO run_controls(run_id,cancel_reason) VALUES(?1,'user') ON CONFLICT(run_id) DO NOTHING",[id])?;Ok(())}).await
    }
    pub(crate) async fn cancellation_requested(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_owned();
        self.with_connection(move |db| {
            Ok(db.query_row(
                "SELECT EXISTS(SELECT 1 FROM run_controls WHERE run_id=?1)",
                [id],
                |r| r.get(0),
            )?)
        })
        .await
    }
}

impl SqliteSessionStore {
    pub(crate) async fn audit_run(
        &self,
        run: &RunRecord,
        category: &'static str,
    ) -> anyhow::Result<()> {
        crate::audit::Journal::new(self.directory.join("recovery-audit"))
            .run_transition(&run.id, &run.session_id, category, run.recovery_attempts)
            .await
    }
    pub(crate) async fn retry_parked(&self, id: &str) -> anyhow::Result<()> {
        use crate::agent::run_state::{Checkpoint, RunPhase};
        let (run, _) = self
            .load_run(id)
            .await?
            .context("Run not found in selected store")?;
        let lock = self
            .claim_execution(&run.conversation_id, &run.session_id)
            .await?;
        let (run, session) = self.load_run(id).await?.context("Run not found")?;
        ensure!(
            !run.cancel_requested && matches!(run.phase, RunPhase::Parked { .. }),
            "Only parked, non-cancelled runs may be retried"
        );
        let mut cp = Checkpoint::resume(self, run, lock);
        let phase = cp
            .record
            .interrupted_phase
            .take()
            .context("Parked run has no interrupted phase")?;
        ensure!(
            !phase.terminal() && !matches!(phase, RunPhase::Parked { .. }),
            "Invalid interrupted phase"
        );
        cp.record.phase = phase;
        cp.record.recovery_attempts = 0;
        cp.record.attempt_active = false;
        cp.record.manual_retry = true;
        cp.persist(&session, None).await?;
        self.audit_run(&cp.record, "retry").await
    }
    pub(crate) async fn cancel_run(&self, id: &str) -> anyhow::Result<()> {
        use crate::agent::run_state::{Checkpoint, RunPhase};
        let (run, _) = self
            .load_run(id)
            .await?
            .context("Run not found in selected store")?;
        if run.phase.terminal() {
            return Ok(());
        }
        self.request_cancel(id).await?;
        let lock = match self
            .claim_execution(&run.conversation_id, &run.session_id)
            .await
        {
            Ok(lock) => lock,
            Err(error)
                if matches!(
                    error.downcast_ref::<std::fs::TryLockError>(),
                    Some(std::fs::TryLockError::WouldBlock)
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let (run, mut session) = self.load_run(id).await?.context("Run not found")?;
        if run.phase.terminal() {
            return Ok(());
        }
        let mut cp = Checkpoint::resume(self, run, lock);
        session.finish_interrupted_tools();
        cp.save(&session, RunPhase::Cancelled, None).await
    }
}

impl SqliteSessionStore {
    pub(crate) async fn run_diagnostics(
        &self,
        id: Option<&str>,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        let id = id.map(str::to_owned);
        self.with_connection(move|db|{
            let mut stmt=db.prepare("SELECT id,conversation_id,session_id,payload FROM runs WHERE (?1 IS NULL OR id=?1) ORDER BY id")?;
            let rows=stmt.query_map([id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,Vec<u8>>(3)?)))?;
            let mut output=vec![];
            for row in rows {
                let (id,source,session,bytes)=row?;
                let parsed=serde_json::from_slice::<RunRecord>(&bytes);
                let valid=parsed.as_ref().ok().filter(|run|run.schema_version==1 && run.id==id && run.session_id==session && run.conversation_id==source);
                let Some(run)=valid else {
                    let reason=if parsed.as_ref().is_ok_and(|run|run.schema_version!=1){"unsupported_run_version"}else{"invalid_run_checkpoint"};
                    output.push(serde_json::json!({"id":id,"session":session,"source":source,"phase":"Blocked","reason":reason}));continue;
                };
                use crate::agent::run_state::RunPhase;
                let (phase,reason)=match &run.phase {
                    RunPhase::NeedModel=>("NeedModel",None),RunPhase::NeedTool{..}=>("NeedTool",None),RunPhase::ExecutingTool{..}=>("ExecutingTool",None),
                    RunPhase::Completed=>("Completed",None),RunPhase::Cancelled=>("Cancelled",None),RunPhase::Failed{..}=>("Failed",Some("execution_failed")),
                    RunPhase::Parked{reason}=>("Parked",Some(if reason=="automatic_recovery_disabled"{"automatic_recovery_disabled"}else{"crash_limit"})),
                };
                let cancelled:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM run_controls WHERE run_id=?1)",[&id],|r|r.get(0))?;
                let confirmed:Option<String>=db.query_row("SELECT call_id FROM tool_executions WHERE run_id=?1 AND json_extract(payload,'$.result') IS NOT NULL ORDER BY rowid DESC LIMIT 1",[&id],|r|r.get(0)).optional()?;
                output.push(serde_json::json!({"id":id,"session":session,"source":source,"phase":phase,"reason":reason,"attempts":run.recovery_attempts,"model_attempts":run.model_attempts,"deadline":run.deadline_at,"last_confirmed_action":confirmed,"cancel_requested":run.cancel_requested || cancelled}));
            }
            Ok(output)
        }).await
    }
}
