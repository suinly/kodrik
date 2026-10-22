// Read-only run and conversation snapshots; standalone run observers are also available.
#![allow(dead_code)]
use super::{
    message::Message,
    run_state::RunRecord,
    session::{Session, SqliteSessionStore},
    tools::TaskPlan,
};
use anyhow::{Context, ensure};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) enum PartialStatus {
    Active,
    Ready,
    Interrupted,
    Committed,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Partial {
    pub attempt: u64,
    pub text: String,
    pub status: PartialStatus,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Live {
    pub current: Option<Partial>,
    pub interrupted: Option<Partial>,
}
pub(crate) struct RunView {
    pub revision: i64,
    pub run: RunRecord,
    pub session: Session,
    pub live: Live,
    pub plan: Option<TaskPlan>,
    pub queued: Vec<serde_json::Value>,
    pub deliveries: Vec<serde_json::Value>,
    pub compaction: Option<serde_json::Value>,
}

pub(crate) fn migrate(db: &Connection) -> anyhow::Result<()> {
    db.execute_batch("ALTER TABLE runs ADD COLUMN live BLOB; INSERT INTO schema_meta VALUES('run_view_revision','0');")?;
    for table in [
        "runs",
        "sessions",
        "submissions",
        "deliveries",
        "run_controls",
        "inbox_controls",
    ] {
        for action in ["INSERT", "UPDATE", "DELETE"] {
            db.execute_batch(&format!("CREATE TRIGGER view_{table}_{action} AFTER {action} ON {table} BEGIN UPDATE schema_meta SET value=CAST(value AS INTEGER)+1 WHERE key='run_view_revision'; END;"))?;
        }
    }
    db.pragma_update(None, "user_version", 5)?;
    Ok(())
}
pub(crate) fn checkpoint_live(
    db: &Connection,
    run: &RunRecord,
    history_advanced: bool,
    interrupted_attempt: bool,
) -> anyhow::Result<()> {
    let bytes: Option<Vec<u8>> =
        db.query_row("SELECT live FROM runs WHERE id=?1", [&run.id], |r| r.get(0))?;
    let Some(bytes) = bytes else {
        return Ok(());
    };
    let mut live: Live = serde_json::from_slice(&bytes)?;
    if let Some(current) = live.current.as_mut()
        && current.status != PartialStatus::Committed
    {
        use super::run_state::RunPhase;
        let next = if interrupted_attempt
            || current.attempt != run.model_attempts
            || matches!(
                run.phase,
                RunPhase::Cancelled | RunPhase::Failed { .. } | RunPhase::Parked { .. }
            ) {
            PartialStatus::Interrupted
        } else if history_advanced && current.status == PartialStatus::Ready {
            PartialStatus::Committed
        } else {
            current.status.clone()
        };
        if current.status != next {
            current.status = next;
            db.execute(
                "UPDATE runs SET live=?2 WHERE id=?1",
                params![run.id, serde_json::to_vec(&live)?],
            )?;
        }
    }
    Ok(())
}

pub(crate) struct ConversationView {
    pub revision: i64,
    pub session: Session,
    pub run: Option<RunView>,
    pub busy: bool,
    pub blocked_reason: Option<String>,
    pub queued: Vec<serde_json::Value>,
}
fn read_run_view(tx: &Connection, id: &str) -> anyhow::Result<RunView> {
    let (payload,history,live):(Vec<u8>,Vec<u8>,Option<Vec<u8>>)=tx.query_row("SELECT r.payload,s.payload,r.live FROM runs r JOIN sessions s ON s.id=r.session_id WHERE r.id=?1",[&id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).context("Run not found in selected store")?;
    let mut run: RunRecord = serde_json::from_slice(&payload)?;
    let mut session: Session = serde_json::from_slice(&history)?;
    ensure!(
        run.id == id && run.schema_version == 1 && run.session_id == session.id().as_str(),
        "Unsupported or invalid run view checkpoint"
    );
    if let Some(len) = run.history_len {
        session.truncate_view(len)?;
    }
    run.cancel_requested |= tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM run_controls WHERE run_id=?1)",
        [&id],
        |r| r.get::<_, bool>(0),
    )?;
    let revision = tx.query_row(
        "SELECT CAST(value AS INTEGER) FROM schema_meta WHERE key='run_view_revision'",
        [],
        |r| r.get(0),
    )?;
    let mut live: Live = live
        .map(|bytes| serde_json::from_slice(&bytes))
        .transpose()?
        .unwrap_or_default();
    if let Some(current) = live.current.as_mut()
        && matches!(current.status, PartialStatus::Active | PartialStatus::Ready)
        && (!run.attempt_active || current.attempt != run.model_attempts)
    {
        current.status = PartialStatus::Interrupted;
    }
    let mut plan = None;
    for message in session.messages() {
        if let Message::ToolCall(call)=message && call.name=="update_plan" && session.messages().iter().any(|m|matches!(m,Message::ToolResult(result) if result.call_id==call.call_id && result.content.output.get("error").is_none() && result.content.output.get("interrupted").is_none())) {
                    plan=Some(TaskPlan::parse(call.arguments.clone())?);
                }
    }
    let mut queued = vec![];
    let mut stmt=tx.prepare("SELECT s.payload FROM submissions s LEFT JOIN active_sessions a ON a.conversation_id=s.conversation_id WHERE s.status='Queued' AND coalesce(s.session_id,a.session_id,s.conversation_id)=?1 ORDER BY s.rowid")?;
    for bytes in stmt.query_map([&run.session_id], |r| r.get::<_, Vec<u8>>(0))? {
        let value: serde_json::Value = serde_json::from_slice(&bytes?)?;
        queued.push(serde_json::json!({"id":value["id"],"sequence":value["sequence"],"text":value["request"]["message"]["text"],"mode":value["request"]["mode"]}));
    }
    drop(stmt);
    let mut deliveries = vec![];
    let mut stmt = tx.prepare("SELECT payload FROM deliveries WHERE run_id=?1 ORDER BY rowid")?;
    for bytes in stmt.query_map([&id], |r| r.get::<_, Vec<u8>>(0))? {
        let value: serde_json::Value = serde_json::from_slice(&bytes?)?;
        deliveries.push(serde_json::json!({"id":value["id"],"status":value["status"],"message_id":value["message_id"]}));
    }
    drop(stmt);
    let compaction = super::compact::view(tx, session.id().as_str())?;
    Ok(RunView {
        compaction,
        revision,
        run,
        session,
        live,
        plan,
        queued,
        deliveries,
    })
}
impl SqliteSessionStore {
    pub(crate) async fn view_revision(&self) -> anyhow::Result<i64> {
        self.with_connection(|db| {
            Ok(db.query_row(
                "SELECT CAST(value AS INTEGER) FROM schema_meta WHERE key='run_view_revision'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
    }
    pub(crate) async fn conversation_view(&self, id: &str) -> anyhow::Result<ConversationView> {
        let id = id.to_owned();
        self.with_connection(move |db| {
            use rusqlite::OptionalExtension;
            let tx=db.transaction()?;
            let revision=tx.query_row("SELECT CAST(value AS INTEGER) FROM schema_meta WHERE key='run_view_revision'",[],|r|r.get(0))?;
            let bytes:Vec<u8>=tx.query_row("SELECT payload FROM sessions WHERE id=?1",[&id],|r|r.get(0))?;
            let session:Session=serde_json::from_slice(&bytes)?;
            let run_id:Option<String>=tx.query_row("SELECT id FROM runs WHERE session_id=?1 ORDER BY terminal,rowid DESC LIMIT 1",[&id],|r|r.get(0)).optional()?;
            let run=run_id.map(|id|read_run_view(&tx,&id)).transpose()?;
            let busy=tx.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE session_id=?1 AND terminal=0) OR EXISTS(SELECT 1 FROM submissions s LEFT JOIN active_sessions a ON a.conversation_id=s.conversation_id WHERE s.status='Queued' AND coalesce(s.session_id,a.session_id,s.conversation_id)=?1 AND NOT EXISTS(SELECT 1 FROM inbox_controls c WHERE c.conversation_id=s.conversation_id))",[&id],|r|r.get(0))?;
            let blocked_reason=tx.query_row("SELECT c.reason FROM inbox_controls c LEFT JOIN active_sessions a ON a.conversation_id=c.conversation_id WHERE coalesce(a.session_id,c.conversation_id)=?1 OR EXISTS(SELECT 1 FROM submissions s WHERE s.conversation_id=c.conversation_id AND s.status='Queued' AND s.session_id=?1) ORDER BY c.rowid DESC LIMIT 1",[&id],|r|r.get(0)).optional()?;
            let mut stmt=tx.prepare("SELECT s.payload FROM submissions s LEFT JOIN active_sessions a ON a.conversation_id=s.conversation_id WHERE s.status='Queued' AND coalesce(s.session_id,a.session_id,s.conversation_id)=?1 ORDER BY s.rowid")?;
            let mut queued=vec![];
            for bytes in stmt.query_map([&id],|r|r.get::<_,Vec<u8>>(0))? {
                let value:serde_json::Value=serde_json::from_slice(&bytes?)?;
                let message=&value["request"]["message"];
                let files:Vec<_>=message["files"].as_array().into_iter().flatten().filter_map(|file|file["filename"].as_str()).collect();
                let photos=message["images"].as_array().map_or(0,Vec::len);
                queued.push(serde_json::json!({"id":value["id"],"text":message["text"],"files":files,"photos":photos}));
            }
            Ok(ConversationView{revision,session,run,busy,blocked_reason,queued})
        }).await
    }
    pub(crate) async fn save_live(
        &self,
        id: &str,
        attempt: u64,
        text: String,
        status: PartialStatus,
    ) -> anyhow::Result<()> {
        let id = id.to_owned();
        self.with_connection(move |db| {
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let (payload, live): (Vec<u8>, Option<Vec<u8>>) =
                tx.query_row("SELECT payload,live FROM runs WHERE id=?1", [&id], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
            let run: RunRecord = serde_json::from_slice(&payload)?;
            ensure!(
                run.schema_version == 1
                    && run.id == id
                    && run.model_attempts == attempt
                    && !run.phase.terminal(),
                "Stale live attempt"
            );
            let mut live: Live = live
                .map(|bytes| serde_json::from_slice(&bytes))
                .transpose()?
                .unwrap_or_default();
            if let Some(mut old) = live.current.take()
                && old.attempt != attempt
                && old.status != PartialStatus::Committed
                && !old.text.is_empty()
            {
                old.status = PartialStatus::Interrupted;
                live.interrupted = Some(old);
            }
            live.current = Some(Partial {
                attempt,
                text,
                status,
            });
            tx.execute(
                "UPDATE runs SET live=?2 WHERE id=?1",
                params![id, serde_json::to_vec(&live)?],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }
    pub(crate) async fn run_view(&self, id: &str) -> anyhow::Result<RunView> {
        let id = id.to_owned();
        self.with_connection(move |db| {
            // All components and the revision share one SQLite read snapshot.
            let tx = db.transaction()?;
            read_run_view(&tx, &id)
        })
        .await
    }
    pub(crate) async fn subscribe_run(&self, id: &str) -> anyhow::Result<RunObserver> {
        let initial = self.run_view(id).await?;
        Ok(RunObserver {
            store: self.clone(),
            id: id.into(),
            initial: Some(initial),
            revision: None,
        })
    }
}
// Latest-only snapshots bound observer backlog to one frame (below the 100-frame cap).
// Polling the durable revision also observes other processes, with no subscribe race.
pub(crate) struct RunObserver {
    store: SqliteSessionStore,
    id: String,
    initial: Option<RunView>,
    revision: Option<i64>,
}
impl RunObserver {
    pub(crate) async fn next(&mut self) -> anyhow::Result<RunView> {
        if let Some(initial) = self.initial.take() {
            self.revision = Some(initial.revision);
            return Ok(initial);
        }
        loop {
            let revision = self.store.view_revision().await?;
            if self.revision.is_none_or(|last| revision > last) {
                let view = self.store.run_view(&self.id).await?;
                self.revision = Some(view.revision);
                return Ok(view);
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
}
impl super::AgentRunner {
    pub(crate) async fn run_view(&self, id: &str) -> anyhow::Result<RunView> {
        self.sessions
            .runtime_store()
            .context("Run view requires SQLite")?
            .run_view(id)
            .await
    }
    pub(crate) async fn subscribe_run(&self, id: &str) -> anyhow::Result<RunObserver> {
        self.sessions
            .runtime_store()
            .context("Run view requires SQLite")?
            .subscribe_run(id)
            .await
    }
}

pub(super) async fn collect_generation<F>(
    store: &SqliteSessionStore,
    run: &RunRecord,
    progress: Option<&tokio::sync::mpsc::Sender<super::AgentEvent>>,
    mut events: tokio::sync::mpsc::Receiver<super::AgentEvent>,
    work: F,
) -> anyhow::Result<(Vec<Message>, Option<u64>)>
where
    F: std::future::Future<Output = anyhow::Result<(Vec<Message>, Option<u64>)>>,
{
    store
        .save_live(
            &run.id,
            run.model_attempts,
            String::new(),
            PartialStatus::Active,
        )
        .await?;
    let mut text = String::new();
    let mut pending = String::new();
    let mut tick = tokio::time::interval_at(
        tokio::time::Instant::now() + std::time::Duration::from_millis(250),
        std::time::Duration::from_millis(250),
    );
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tokio::pin!(work);
    let result = loop {
        tokio::select! {
            result=&mut work=>break result,
            event=events.recv()=>if let Some(event)=event {collect_event(event,&mut text,&mut pending,progress);},
            _=tick.tick()=>if !pending.is_empty() {
                store.save_live(&run.id,run.model_attempts,text.clone(),PartialStatus::Active).await?;
                if let Some(tx)=progress {let _=tx.try_send(super::AgentEvent::TextDelta(std::mem::take(&mut pending)));}else{pending.clear();}
            },
        }
    };
    while let Ok(event) = events.try_recv() {
        collect_event(event, &mut text, &mut pending, progress);
    }
    store
        .save_live(
            &run.id,
            run.model_attempts,
            text,
            if result.is_ok() {
                PartialStatus::Ready
            } else {
                PartialStatus::Interrupted
            },
        )
        .await?;
    if !pending.is_empty()
        && let Some(tx) = progress
    {
        let _ = tx.try_send(super::AgentEvent::TextDelta(pending));
    }
    result
}
fn collect_event(
    event: super::AgentEvent,
    text: &mut String,
    pending: &mut String,
    progress: Option<&tokio::sync::mpsc::Sender<super::AgentEvent>>,
) {
    match event {
        super::AgentEvent::TextDelta(delta) => {
            text.push_str(&delta);
            pending.push_str(&delta);
        }
        event => {
            if let Some(tx) = progress {
                let _ = tx.try_send(event);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{
        UserMessage,
        message::Message,
        run_state::{Checkpoint, RunPhase},
        session::{Session, SessionId, SqliteSessionStore},
    };

    #[tokio::test]
    async fn conversation_view_projects_shared_session_run_across_transports() {
        use crate::agent::session::SessionStore;
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap();
        let mut session = Session::new(SessionId::new(uuid::Uuid::new_v4().to_string()));
        session.push(Message::User(UserMessage {
            content: "Telegram question".into(),
            files: vec![],
            images: vec![],
        }));
        let channel = SessionId::new("telegram:bot:42");
        store.save(&session).await.unwrap();
        store.select(&channel, session.id()).await.unwrap();
        let mut cp = Checkpoint::new(&store, &channel, &session);
        cp.record.model_attempts = 1;
        cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
        store
            .save_live(&cp.record.id, 1, "progress".into(), PartialStatus::Active)
            .await
            .unwrap();
        let running = store
            .conversation_view(session.id().as_str())
            .await
            .unwrap();
        assert!(running.busy);
        assert_eq!(running.run.as_ref().unwrap().run.id, cp.record.id);
        assert_eq!(running.run.unwrap().live.current.unwrap().text, "progress");
        session.push(Message::Assistant(crate::agent::AssistantMessage {
            content: "answer".into(),
        }));
        cp.save(&session, RunPhase::Completed, None).await.unwrap();
        let completed = store
            .conversation_view(session.id().as_str())
            .await
            .unwrap();
        assert!(!completed.busy);
        assert!(matches!(
            completed.run.unwrap().run.phase,
            RunPhase::Completed
        ));
        assert_eq!(
            serde_json::to_value(completed.session.messages()).unwrap(),
            serde_json::to_value(session.messages()).unwrap()
        );
        let request = crate::agent::inbox::SubmissionRequest::new(
            &channel,
            crate::gateway::IncomingMessage::new(
                crate::gateway::InterfaceId("telegram:bot".into()),
                "42",
                "follow up",
            ),
            crate::agent::inbox::BusyMode::FollowUp,
        );
        store.submit(request).await.unwrap();
        let queued = store
            .conversation_view(session.id().as_str())
            .await
            .unwrap();
        assert!(queued.busy);
        assert_eq!(queued.queued[0]["text"], "follow up");
        store
            .with_connection(|db| {
                db.execute(
                    "INSERT INTO inbox_controls VALUES('telegram:bot:42','paused')",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let blocked = store
            .conversation_view(session.id().as_str())
            .await
            .unwrap();
        assert!(!blocked.busy);
        assert_eq!(blocked.blocked_reason.as_deref(), Some("paused"));
    }

    #[tokio::test]
    async fn conversation_view_never_adopts_a_selector_run_for_another_session() {
        use crate::agent::session::SessionStore;
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap();
        let old = Session::new(SessionId::new("selector"));
        store.save(&old).await.unwrap();
        let selected = Session::new(SessionId::new("selected"));
        store.save(&selected).await.unwrap();
        store.select(old.id(), selected.id()).await.unwrap();
        let mut cp = Checkpoint::new(&store, old.id(), &selected);
        cp.save(&selected, RunPhase::NeedModel, None).await.unwrap();
        let view = store.conversation_view(old.id().as_str()).await.unwrap();
        assert!(view.run.is_none());
        assert!(!view.busy);
        assert_eq!(view.session.id(), old.id());
    }
    async fn seeded() -> (tempfile::TempDir, SqliteSessionStore, Session, Checkpoint) {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap();
        let mut session = Session::new(SessionId::new("cli:view"));
        session.push(Message::User(UserMessage {
            content: "question".into(),
            files: vec![],
            images: vec![],
        }));
        let mut cp = Checkpoint::new(&store, session.id(), &session);
        cp.record.model_attempts = 1;
        cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
        (dir, store, session, cp)
    }
    #[tokio::test]
    async fn run_view_partial_is_separate_and_attempts_never_concatenate() {
        let (_dir, store, session, mut cp) = seeded().await;
        let before = store
            .with_connection(|db| {
                Ok(db.query_row("SELECT payload FROM sessions", [], |r| {
                    r.get::<_, Vec<u8>>(0)
                })?)
            })
            .await
            .unwrap();
        store
            .save_live(&cp.record.id, 1, "first".into(), PartialStatus::Active)
            .await
            .unwrap();
        let view = store.run_view(&cp.record.id).await.unwrap();
        assert_eq!(view.live.current.unwrap().text, "first");
        assert_eq!(
            before,
            store
                .with_connection(|db| Ok(db.query_row(
                    "SELECT payload FROM sessions",
                    [],
                    |r| r.get::<_, Vec<u8>>(0)
                )?))
                .await
                .unwrap()
        );
        cp.record.model_attempts = 2;
        cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
        store
            .save_live(&cp.record.id, 2, "second".into(), PartialStatus::Active)
            .await
            .unwrap();
        let view = store.run_view(&cp.record.id).await.unwrap();
        assert_eq!(view.live.current.unwrap().text, "second");
        assert_eq!(view.live.interrupted.unwrap().text, "first");
    }
    struct Streaming;
    #[async_trait::async_trait]
    impl crate::agent::llm::LlmProvider for Streaming {
        async fn generate(
            &self,
            _: &crate::agent::LlmContext<'_>,
            _: &[&dyn crate::agent::tools::Tool],
            progress: Option<&tokio::sync::mpsc::Sender<crate::agent::AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            if let Some(tx) = progress {
                for _ in 0..150 {
                    let _ = tx
                        .send(crate::agent::AgentEvent::TextDelta("x".into()))
                        .await;
                }
            }
            Ok((
                vec![Message::Assistant(crate::agent::AssistantMessage {
                    content: "final".into(),
                })],
                None,
            ))
        }
    }
    #[tokio::test]
    async fn run_view_full_disconnected_progress_does_not_block_completion() {
        use crate::agent::{Agent, AgentRunner, ContextBuilder, tools::ToolRegistry};
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(
            SqliteSessionStore::open_async(dir.path().into())
                .await
                .unwrap(),
        );
        let runner = AgentRunner::new(
            std::sync::Arc::new(Agent::new(
                std::sync::Arc::new(Streaming),
                ToolRegistry::default(),
                ContextBuilder::new("test"),
            )),
            store.clone(),
        );
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(crate::agent::AgentEvent::TextDelta("occupied".into()))
            .await
            .unwrap();
        let input = crate::gateway::IncomingMessage::new(
            crate::gateway::InterfaceId("cli".into()),
            "view",
            "question",
        );
        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            runner.run(SessionId::new("cli:view"), input, Some(tx)),
        )
        .await
        .expect("UI blocked runtime")
        .unwrap();
        drop(rx);
        assert_eq!(answer.content, "final");
        let id = store
            .with_connection(|db| {
                Ok(db.query_row("SELECT id FROM runs", [], |r| r.get::<_, String>(0))?)
            })
            .await
            .unwrap();
        let view = store.run_view(&id).await.unwrap();
        assert!(matches!(view.run.phase, RunPhase::Completed));
        assert_eq!(view.live.current.unwrap().text, "x".repeat(150));
    }
    #[tokio::test]
    async fn run_view_slow_observer_reconnect_has_latest_without_execution() {
        let (_dir, store, session, mut cp) = seeded().await;
        let mut observer = store.subscribe_run(&cp.record.id).await.unwrap();
        let first = observer.next().await.unwrap();
        for _ in 0..105 {
            cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
        }
        let next = observer.next().await.unwrap();
        assert!(next.revision > first.revision);
        assert_eq!(next.run.revision, cp.record.revision);
        drop(observer);
        let mut observer = store.subscribe_run(&cp.record.id).await.unwrap();
        let same = observer.next().await.unwrap();
        assert_eq!(same.revision, next.revision);
        assert_eq!(same.run.model_attempts, 1);
    }
    #[tokio::test]
    async fn run_view_tool_plan_queue_and_uncertain_receipt_survive_reopen() {
        use crate::agent::{
            message::{ToolCall, ToolResult},
            run_state::ToolExecution,
            tools::ReplayPolicy,
        };
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap();
        let mut session = Session::new(SessionId::new("telegram:test:42"));
        let mut cp = Checkpoint::new(&store, session.id(), &session);
        let call = ToolCall {
            call_id: "plan-call".into(),
            name: "update_plan".into(),
            arguments: serde_json::json!({"explanation":null,"plan":[{"step":"Inspect","status":"in_progress"}]}),
        };
        session.push(Message::ToolCall(call.clone()));
        cp.record.pending_calls = vec![call.clone()];
        cp.save(
            &session,
            RunPhase::ExecutingTool {
                call_id: call.call_id.clone(),
                replay: ReplayPolicy::Never,
            },
            Some(ToolExecution {
                call: call.clone(),
                replay: ReplayPolicy::Never,
                result: None,
            }),
        )
        .await
        .unwrap();
        let mut observer = store.subscribe_run(&cp.record.id).await.unwrap();
        let initial = observer.next().await.unwrap();
        assert!(matches!(initial.run.phase, RunPhase::ExecutingTool { .. }));
        assert!(initial.plan.is_none());
        let output = crate::agent::tools::ToolOutput::from(serde_json::json!("Plan updated"));
        session.push(Message::ToolResult(ToolResult {
            call_id: call.call_id.clone(),
            content: output.clone(),
        }));
        cp.record.pending_calls.clear();
        cp.save(
            &session,
            RunPhase::NeedModel,
            Some(ToolExecution {
                call,
                replay: ReplayPolicy::Never,
                result: Some(output),
            }),
        )
        .await
        .unwrap();
        let after = observer.next().await.unwrap();
        assert_eq!(after.plan.unwrap().plan[0].step, "Inspect");
        let input = crate::gateway::IncomingMessage::new(
            crate::gateway::InterfaceId("telegram:test".into()),
            "42",
            "queued",
        );
        store
            .submit(crate::agent::inbox::SubmissionRequest::new(
                session.id(),
                input,
                crate::agent::inbox::BusyMode::FollowUp,
            ))
            .await
            .unwrap();
        assert_eq!(
            store.run_view(&cp.record.id).await.unwrap().queued[0]["text"],
            "queued"
        );
        session.push(Message::Assistant(crate::agent::AssistantMessage {
            content: "answer".into(),
        }));
        cp.save(&session, RunPhase::Completed, None).await.unwrap();
        store.with_connection(|db|{db.execute("UPDATE deliveries SET payload=CAST(json_set(payload,'$.status','Uncertain') AS BLOB)",[])?;Ok(())}).await.unwrap();
        let id = cp.record.id.clone();
        drop(cp);
        drop(observer);
        drop(store);
        let store = SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap();
        let view = store.run_view(&id).await.unwrap();
        assert!(matches!(view.run.phase, RunPhase::Completed));
        assert_eq!(view.deliveries[0]["status"], "Uncertain");
        assert_eq!(
            serde_json::to_value(view.session.messages()).unwrap(),
            serde_json::to_value(session.messages()).unwrap()
        );
    }

    #[tokio::test]
    async fn run_view_coalesces_partial_and_flushes_final_tail() {
        let (_dir, store, _session, cp) = seeded().await;
        let (tx, rx) = tokio::sync::mpsc::channel(100);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let other = store.clone();
        let record = cp.record.clone();
        let task = tokio::spawn(async move {
            collect_generation(&other, &record, None, rx, async move {
                for _ in 0..50 {
                    tx.send(crate::agent::AgentEvent::TextDelta("x".into()))
                        .await
                        .unwrap();
                }
                started_tx.send(()).unwrap();
                finish_rx.await.unwrap();
                tx.send(crate::agent::AgentEvent::TextDelta("tail".into()))
                    .await
                    .unwrap();
                Ok((vec![], None))
            })
            .await
            .unwrap();
        });
        started_rx.await.unwrap();
        let early = store.run_view(&cp.record.id).await.unwrap();
        assert_eq!(early.live.current.unwrap().text, "");
        let ready = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let view = store.run_view(&cp.record.id).await.unwrap();
                if view
                    .live
                    .current
                    .as_ref()
                    .is_some_and(|p| p.text.len() == 50)
                {
                    break view;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(ready.revision, early.revision + 1);
        finish_tx.send(()).unwrap();
        task.await.unwrap();
        let end = store.run_view(&cp.record.id).await.unwrap();
        let partial = end.live.current.unwrap();
        assert_eq!(partial.text, format!("{}tail", "x".repeat(50)));
        assert_eq!(partial.status, PartialStatus::Ready);
    }

    #[tokio::test]
    async fn run_view_completed_run_does_not_adopt_later_turns() {
        use crate::agent::session::SessionStore;
        let (_dir, store, mut session, mut cp) = seeded().await;
        session.push(Message::Assistant(crate::agent::AssistantMessage {
            content: "first answer".into(),
        }));
        cp.save(&session, RunPhase::Completed, None).await.unwrap();
        let expected = session.messages().len();
        session.push(Message::User(UserMessage {
            content: "next turn".into(),
            files: vec![],
            images: vec![],
        }));
        store.save(&session).await.unwrap();
        let view = store.run_view(&cp.record.id).await.unwrap();
        assert_eq!(view.session.messages().len(), expected);
    }
    #[tokio::test]
    async fn run_view_model_return_is_not_a_confirmed_history_commit() {
        let (_dir, store, mut session, mut cp) = seeded().await;
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        collect_generation(&store, &cp.record, None, rx, async move {
            tx.send(crate::agent::AgentEvent::TextDelta("partial".into()))
                .await
                .unwrap();
            Ok((vec![], None))
        })
        .await
        .unwrap();
        let before = store.run_view(&cp.record.id).await.unwrap();
        assert_ne!(
            before.live.current.unwrap().status,
            PartialStatus::Committed
        );
        session.push(Message::Assistant(crate::agent::AssistantMessage {
            content: "confirmed".into(),
        }));
        cp.save(&session, RunPhase::Completed, None).await.unwrap();
        assert_eq!(
            store
                .run_view(&cp.record.id)
                .await
                .unwrap()
                .live
                .current
                .unwrap()
                .status,
            PartialStatus::Committed
        );
    }

    #[tokio::test]
    async fn run_view_recovery_interrupts_returned_but_uncommitted_partial() {
        let (_dir, store, session, mut cp) = seeded().await;
        store
            .save_live(
                &cp.record.id,
                1,
                "not committed".into(),
                PartialStatus::Ready,
            )
            .await
            .unwrap();
        assert!(cp.prepare_recovery(&session, 5).await.unwrap());
        assert_eq!(
            store
                .run_view(&cp.record.id)
                .await
                .unwrap()
                .live
                .current
                .unwrap()
                .status,
            PartialStatus::Interrupted
        );
    }
}
