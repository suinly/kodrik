use serde::{Deserialize, Serialize};

use super::{
    message::{ToolCall, ToolOutput},
    session::{Session, SessionId, SqliteSessionStore},
    tools::ReplayPolicy,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum RunPhase {
    NeedModel,
    NeedTool {
        call_id: String,
    },
    ExecutingTool {
        call_id: String,
        replay: ReplayPolicy,
    },
    Parked {
        reason: String,
    },
    Completed,
    Cancelled,
    Failed {
        reason: String,
    },
}
impl RunPhase {
    fn semantic_key(&self) -> String {
        match self {
            Self::NeedTool { call_id } | Self::ExecutingTool { call_id, .. } => {
                format!("tool:{call_id}")
            }
            Self::NeedModel => "model".into(),
            Self::Parked { .. } => "parked".into(),
            Self::Completed => "completed".into(),
            Self::Cancelled => "cancelled".into(),
            Self::Failed { .. } => "failed".into(),
        }
    }

    pub(crate) fn terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Cancelled | Self::Failed { .. }
        )
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RunRecord {
    pub id: String,
    pub conversation_id: String,
    pub session_id: String,
    pub schema_version: u32,
    pub revision: i64,
    pub phase: RunPhase,
    pub model_attempts: u64,
    #[serde(default)]
    pub history_len: Option<usize>,
    #[serde(default)]
    pub recovery_attempts: u32,
    #[serde(default)]
    pub attempt_active: bool,
    #[serde(default)]
    pub interrupted_phase: Option<RunPhase>,
    #[serde(default)]
    pub manual_retry: bool,
    #[serde(default)]
    pub model_limit: Option<u64>,
    #[serde(default)]
    pub deadline_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub next_retry_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub model_retries: u32,
    #[serde(default)]
    pub overflow_retry_used: bool,
    // Prefix before forced compaction; retained until the smaller context is checkpointed.
    #[serde(default)]
    pub overflow_compaction_from: Option<usize>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub pending_calls: Vec<ToolCall>,
    #[serde(default)]
    pub submission_ids: Vec<String>,
    #[serde(default)]
    pub routing_from: Option<String>,
    #[serde(default)]
    pub isolated: Option<bool>,
    #[serde(default)]
    pub cancel_requested: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ToolExecution {
    pub call: ToolCall,
    pub replay: ReplayPolicy,
    pub result: Option<ToolOutput>,
}

pub(crate) struct Checkpoint {
    pub(super) store: SqliteSessionStore,
    pub(super) ownership: Option<std::sync::Arc<super::session::ExecutionOwnership>>,
    pub record: RunRecord,
    // Set before awaiting: cancellation cannot turn an unconfirmed commit into a blind overwrite.
    pub storage_uncertain: bool,
}
impl Checkpoint {
    pub fn new(store: &SqliteSessionStore, conversation: &SessionId, session: &Session) -> Self {
        Self {
            store: store.clone(),
            ownership: None,
            storage_uncertain: false,
            record: RunRecord {
                id: uuid::Uuid::new_v4().to_string(),
                conversation_id: conversation.as_str().into(),
                session_id: session.id().as_str().into(),
                schema_version: 1,
                revision: 0,
                phase: RunPhase::NeedModel,
                model_attempts: 0,
                history_len: None,
                recovery_attempts: 0,
                attempt_active: false,
                interrupted_phase: None,
                manual_retry: false,
                model_limit: None,
                deadline_at: None,
                next_retry_at: None,
                model_retries: 0,
                overflow_retry_used: false,
                overflow_compaction_from: None,
                started_at: chrono::Utc::now(),
                pending_calls: vec![],
                submission_ids: vec![],
                routing_from: None,
                isolated: None,
                cancel_requested: false,
            },
        }
    }
    pub(super) fn resume(
        store: &SqliteSessionStore,
        record: RunRecord,
        ownership: std::sync::Arc<super::session::ExecutionOwnership>,
    ) -> Self {
        Self {
            store: store.clone(),
            record,
            ownership: Some(ownership),
            storage_uncertain: false,
        }
    }
    pub(super) async fn prepare_recovery(
        &mut self,
        session: &Session,
        limit: u32,
    ) -> anyhow::Result<bool> {
        if matches!(self.record.phase, RunPhase::Parked { .. }) {
            return Ok(false);
        }
        anyhow::ensure!(!self.record.phase.terminal(), "Cannot recover terminal run");
        if self.record.attempt_active {
            self.record.recovery_attempts = self.record.recovery_attempts.saturating_add(1);
        }
        let manual = std::mem::take(&mut self.record.manual_retry);
        if !manual && (limit == 0 || self.record.recovery_attempts >= limit) {
            self.record.interrupted_phase = Some(self.record.phase.clone());
            self.record.phase = RunPhase::Parked {
                reason: if limit == 0 {
                    "automatic_recovery_disabled"
                } else {
                    "crash_limit"
                }
                .into(),
            };
            self.record.attempt_active = false;
            self.persist(session, None).await?;
            self.store.audit_run(&self.record, "park").await?;
            return Ok(false);
        }
        self.record.attempt_active = true;
        self.persist(session, None).await?;
        self.store.audit_run(&self.record, "recovery").await?;
        Ok(true)
    }
    pub(super) async fn clean_suspend(&mut self) -> anyhow::Result<()> {
        if let Some((record, session)) = self.store.load_run(&self.record.id).await? {
            self.record = record;
            self.storage_uncertain = false;
            if !self.record.phase.terminal() && !self.record.cancel_requested {
                self.record.attempt_active = false;
                self.persist(&session, None).await?;
            }
        }
        Ok(())
    }
    pub async fn save(
        &mut self,
        session: &Session,
        phase: RunPhase,
        tool: Option<ToolExecution>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.storage_uncertain,
            "Previous durable commit is unconfirmed; recovery required"
        );
        if self.record.phase.semantic_key() != phase.semantic_key()
            && !matches!(phase, RunPhase::Parked { .. })
        {
            self.record.recovery_attempts = 0;
        }
        self.record.history_len = Some(session.messages().len());
        self.record.phase = phase;
        self.record.attempt_active =
            !self.record.phase.terminal() && !matches!(self.record.phase, RunPhase::Parked { .. });
        self.persist(session, tool).await
    }
    pub(super) async fn persist(
        &mut self,
        session: &Session,
        tool: Option<ToolExecution>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.storage_uncertain,
            "Previous durable commit is unconfirmed; recovery required"
        );
        if !matches!(
            self.record.phase,
            RunPhase::Cancelled | RunPhase::Failed { .. } | RunPhase::Parked { .. }
        ) {
            self.store.guard_compaction_storage(&self.record.id).await?;
        }
        let expected = self.record.revision;
        self.record.revision += 1;
        self.storage_uncertain = true;
        self.store
            .commit_owned_step(
                expected,
                session,
                &self.record,
                tool.as_ref(),
                self.ownership.clone(),
            )
            .await?;
        self.storage_uncertain = false;
        Ok(())
    }
}

tokio::task_local! {
    pub(crate) static CURRENT_TOOL: Option<(String,String)>;
    pub(crate) static CURRENT_EXECUTION: Option<std::sync::Arc<super::session::ExecutionOwnership>>;
}

pub(crate) fn execution_ownership() -> Option<std::sync::Arc<super::session::ExecutionOwnership>> {
    CURRENT_EXECUTION.try_with(Clone::clone).ok().flatten()
}

/// Blocking effects cannot be cancelled by dropping their JoinHandle. Keep the
/// execution locks until the worker itself exits, including on panic.
pub(crate) fn spawn_owned_blocking<F, R>(work: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let ownership = execution_ownership();
    tokio::task::spawn_blocking(move || {
        let _ownership = ownership;
        work()
    })
}
