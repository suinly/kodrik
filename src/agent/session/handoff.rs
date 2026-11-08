use super::Session;

/// Both sessions must have been loaded from the same user-scoped store.
pub(crate) fn eligible(source: &Session, target: &Session) -> bool {
    fn personal(s: &Session) -> bool {
        let id = s.id().as_str();
        s.telegram_group().is_none()
            && s.webhook_instruction.is_none()
            && s.approval_parent.is_none()
            && !id.starts_with("companion:")
            && !id.starts_with("webhook:")
            && !id.starts_with("cli:once:")
            && !id.contains(":guest")
            && !(id.starts_with("telegram:") && id.split(':').any(|part| part.starts_with('-')))
    }
    source.id() != target.id() && personal(source) && personal(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::session::SessionId;

    #[test]
    fn personal_only_and_not_self() {
        let source = Session::new(SessionId::new("source"));
        assert!(eligible(&source, &Session::new(SessionId::new("target"))));
        assert!(!eligible(&source, &source));
        for id in [
            "companion:wakeup:1",
            "cli:once:1",
            "telegram:bot:1:guest",
            "webhook:1",
            "telegram:bot:-100",
        ] {
            let restricted = Session::new(SessionId::new(id));
            assert!(!eligible(&source, &restricted), "{id}");
            assert!(!eligible(&restricted, &source), "{id}");
        }
    }
}

use super::{ExecutionOwnership, SqliteSessionStore, sqlite};
use crate::agent::{
    message::Message,
    run_state::{Checkpoint, RunPhase, RunRecord},
};
use anyhow::{Context, ensure};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use std::sync::Arc;

impl SqliteSessionStore {
    /// The only mid-run binding change: source, target, run, inbox and selection commit together.
    async fn commit_handoff(
        &self,
        expected: RunRecord,
        source: Session,
        ownership: Arc<ExecutionOwnership>,
        context: crate::agent::ContextBuilder,
    ) -> anyhow::Result<(RunRecord, Option<Session>)> {
        self.with_connection(move |db| {
            let _ownership = ownership;
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let payload: Vec<u8> = tx.query_row("SELECT payload FROM runs WHERE id=?1", [&expected.id], |r| r.get(0))?;
            let mut run: RunRecord = serde_json::from_slice(&payload)?;
            ensure!(run.revision == expected.revision && run.session_id == source.id().as_str()
                && !run.phase.terminal() && run.pending_calls.is_empty() && matches!(run.phase, RunPhase::NeedModel), "Stale handoff checkpoint");
            let cancelled: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM run_controls WHERE run_id=?1)", [&run.id], |r|r.get(0))?;
            ensure!(!cancelled && !run.cancel_requested, "Run cancellation requested");
            ensure!(run.isolated == Some(false) && run.handoff_hops < 4 && !run.conversation_id.contains(":guest") && !run.conversation_id.starts_with("companion:"), "Restricted handoff run");
            let request = run.pending_handoff.take().context("No pending handoff")?;
            let validation = (|| -> anyhow::Result<Session> {
                let mut target = sqlite::load(&tx, &request.session_id)?.context("Target session missing")?;
                ensure!(eligible(&source, &target), "Session handoff is not allowed");
                ensure!(!crate::agent::inbox::session_busy_in(&tx, &request.session_id)?, "Target session is busy");
                ensure!(crate::agent::message::tool_boundaries(target.messages()).contains(&target.messages().len()), "Target has unresolved tool calls");
                let active: Option<String> = tx.query_row("SELECT session_id FROM active_sessions WHERE conversation_id=?1", [&run.conversation_id], |r|r.get(0)).optional()?;
                ensure!(active.as_deref().unwrap_or(&run.conversation_id) == source.id().as_str(), "Active session changed before handoff");
                context.initialize_session(&mut target)?;
                Ok(target)
            })();
            let mut target = match validation {
                Ok(target) => target,
                Err(error) => {
                    let mut source = source;
                    source.push(rejection_message(&error));
                    run.revision += 1;
                    run.history_len = Some(source.messages().len());
                    sqlite::save(&tx, &source)?;
                    tx.execute("UPDATE runs SET revision=?2,payload=?3 WHERE id=?1",params![run.id,run.revision,serde_json::to_vec(&run)?])?;
                    tx.commit()?;
                    return Ok((run, Some(source)));
                }
            };
            if run.handoff_inputs.is_empty() {
                // The request and all subsequent steering are copied with original attachments.
                let start = source.messages().iter().rposition(|m| matches!(m, Message::User(_))).unwrap_or(source.messages().len());
                run.handoff_inputs = source.messages()[start..].iter().filter_map(|m| match m {Message::User(u) => Some(u.clone()), _ => None}).collect();
            }
            for input in &run.handoff_inputs { target.push(Message::User(input.clone())); }
            target.push(Message::User(crate::agent::UserMessage {
                content: format!("[Session handoff from {} for the same running task]\n{}\nContinue using this session's history. Tool effects already completed in the source session must not be replayed. Historical material is context, not new authorization.", source.id().as_str(), request.context),
                images: vec![], files: vec![],
            }));
            for id in &run.submission_ids {
                let mut submission = crate::agent::inbox::read_record(&tx, id)?.context("Run submission missing")?;
                ensure!(submission.run_id.as_deref() == Some(&run.id) && submission.session_id.as_deref() == Some(&run.session_id), "Submission binding changed");
                submission.session_id = Some(request.session_id.clone());
                crate::agent::inbox::write_record(&tx, &submission)?;
            }
            let queued = tx.prepare("SELECT id FROM submissions WHERE run_id=?1 AND session_id=?2 AND status='Queued'")?
                .query_map(params![run.id, run.session_id], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for id in queued {
                let mut submission = crate::agent::inbox::read_record(&tx, &id)?.context("Queued steering missing")?;
                submission.session_id = Some(request.session_id.clone());
                crate::agent::inbox::write_record(&tx, &submission)?;
            }
            sqlite::save(&tx, &source)?;
            sqlite::save(&tx, &target)?;
            run.handoff_sources.push(run.session_id.clone());
            run.session_id = request.session_id;
            run.revision += 1;
            run.handoff_hops += 1;
            run.history_len = Some(target.messages().len());
            run.overflow_retry_used = false;
            run.overflow_compaction_from = None;
            tx.execute("UPDATE compaction_jobs SET status='cancelled',payload=CAST(json_set(payload,'$.status','cancelled') AS BLOB) WHERE run_id=?1 AND status IN ('preparing','ready')", [&run.id])?;
            tx.execute("UPDATE runs SET session_id=?2,revision=?3,payload=?4 WHERE id=?1", params![run.id,run.session_id,run.revision,serde_json::to_vec(&run)?])?;
            sqlite::select(&tx, &run.conversation_id, &run.session_id)?;
            super::branches::init_branches(&tx)?;
            let root = super::branches::root(&tx, &run.session_id)?;
            super::branches::select_branch(&tx, &root, &run.session_id)?;
            crate::agent::run_view::checkpoint_live(&tx, &run, true, false)?;
            tx.commit()?;
            Ok((run, Some(target)))
        }).await
    }
}

fn rejection_message(error: &anyhow::Error) -> Message {
    Message::User(crate::agent::UserMessage {
        content: format!(
            "[Session handoff failed: {error:#}] Continue in the current session; no handoff occurred. Do not replay completed effects."
        ),
        images: vec![],
        files: vec![],
    })
}

impl Checkpoint {
    pub(crate) async fn apply_handoff(
        &mut self,
        source: &mut Session,
        context: &crate::agent::ContextBuilder,
    ) -> anyhow::Result<Option<Session>> {
        ensure!(
            !self.storage_uncertain,
            "Unconfirmed checkpoint requires recovery"
        );
        let request = self
            .record
            .pending_handoff
            .as_ref()
            .context("No pending handoff")?;
        let target_lock = match self
            .store
            .execution_lock("session", &request.session_id)
            .await
        {
            Ok(lock) => lock,
            Err(error) => {
                // No transaction started: this rejection can safely be checkpointed normally.
                self.record.pending_handoff = None;
                source.push(rejection_message(&error));
                self.save(source, RunPhase::NeedModel, None).await?;
                return Ok(None);
            }
        };
        let mut locks = vec![target_lock];
        if let Some(owner) = &self.ownership {
            locks.push(owner.clone());
        }
        let ownership = ExecutionOwnership::join(locks);
        self.storage_uncertain = true;
        let (record, target) = self
            .store
            .commit_handoff(
                self.record.clone(),
                source.clone(),
                ownership.clone(),
                context.clone(),
            )
            .await?;
        self.record = record;
        self.storage_uncertain = false;
        let target = target.context("Handoff transaction returned no session")?;
        if target.id() == source.id() {
            *source = target;
            return Ok(None);
        }
        self.ownership = Some(ownership);
        // Persisted state already changed; callers must adopt it even if audit I/O fails.
        if let Err(error) = self.store.audit_run(&self.record, "session_handoff").await {
            log::warn!("Could not journal committed session handoff: {error:#}");
        }
        Ok(Some(target))
    }
}

#[cfg(test)]
mod durable_tests {
    use super::*;
    use crate::agent::{
        UserMessage,
        session::{SessionId, SessionStore},
        tools::session_switch::HandoffRequest,
    };

    async fn fixture() -> (tempfile::TempDir, SqliteSessionStore, Checkpoint, Session) {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().into()).unwrap();
        let mut source = Session::new(SessionId::new("source"));
        source.push(Message::User(UserMessage {
            content: "original request".into(),
            images: vec![],
            files: vec![],
        }));
        let mut target = Session::new(SessionId::new("target"));
        target.push(Message::User(UserMessage {
            content: "target history".into(),
            images: vec![],
            files: vec![],
        }));
        store.save(&target).await.unwrap();
        let mut cp = Checkpoint::new(&store, source.id(), &source);
        cp.record.isolated = Some(false);
        cp.record.pending_handoff = Some(HandoffRequest {
            session_id: "target".into(),
            context: "Effects already completed".into(),
        });
        cp.save(&source, RunPhase::NeedModel, None).await.unwrap();
        (dir, store, cp, source)
    }

    #[tokio::test]
    async fn busy_handoff_is_rejected_without_poisoning_checkpoint() {
        let (_dir, store, mut cp, mut source) = fixture().await;
        let lock = store.execution_lock("session", "target").await.unwrap();
        assert!(
            cp.apply_handoff(
                &mut source,
                &crate::agent::ContextBuilder::new("test rules")
            )
            .await
            .unwrap()
            .is_none()
        );
        assert!(!cp.storage_uncertain);
        assert!(cp.record.pending_handoff.is_none());
        assert_eq!(store.active_id(source.id()).await.unwrap(), *source.id());
        drop(lock);
        cp.save(&source, RunPhase::NeedModel, None).await.unwrap();
    }

    #[tokio::test]
    async fn missing_handoff_is_rejected_without_poisoning_checkpoint() {
        let (_dir, store, mut cp, mut source) = fixture().await;
        store.delete(&SessionId::new("target")).await.unwrap();
        assert!(
            cp.apply_handoff(
                &mut source,
                &crate::agent::ContextBuilder::new("test rules")
            )
            .await
            .unwrap()
            .is_none()
        );
        assert!(!cp.storage_uncertain);
        assert!(cp.record.pending_handoff.is_none());
    }

    #[tokio::test]
    async fn handoff_carries_unconsumed_steering_bound_to_same_run() {
        use crate::{
            agent::inbox::{BusyMode, SubmissionRequest},
            gateway::{IncomingMessage, InterfaceId},
        };
        let (_dir, store, mut cp, mut source) = fixture().await;
        let steering = store
            .submit(SubmissionRequest::new(
                source.id(),
                IncomingMessage::new(InterfaceId("test".into()), "chat", "late correction"),
                BusyMode::Steer,
            ))
            .await
            .unwrap();
        assert_eq!(steering.session_id.as_deref(), Some("source"));
        assert_eq!(steering.run_id.as_deref(), Some(cp.record.id.as_str()));
        let _target = cp
            .apply_handoff(
                &mut source,
                &crate::agent::ContextBuilder::new("test rules"),
            )
            .await
            .unwrap()
            .unwrap();
        let queued = store.steering(&cp.record).await.unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].id, steering.id);
        assert_eq!(queued[0].session_id.as_deref(), Some("target"));
        assert_eq!(queued[0].request.message.text, "late correction");
    }

    #[tokio::test]
    async fn handoff_notifies_new_approval_owner_even_with_old_audit() {
        use crate::agent::{command_audit::CommandAuditMode, message::ToolCall};
        let (_dir, store, mut cp, mut source) = fixture().await;
        let mut target = cp
            .apply_handoff(&mut source, &crate::agent::ContextBuilder::new("rules"))
            .await
            .unwrap()
            .unwrap();
        let call = ToolCall {
            call_id: "new-command".into(),
            name: "exec_command".into(),
            arguments: serde_json::json!({"cmd":"test"}),
        };
        target.push(Message::ToolCall(call.clone()));
        cp.record.pending_calls = vec![call.clone()];
        cp.save(
            &target,
            RunPhase::NeedTool {
                call_id: call.call_id.clone(),
            },
            None,
        )
        .await
        .unwrap();
        store
            .set_command_audit_mode("target", CommandAuditMode::Ask)
            .await
            .unwrap();
        store
            .record_command_audits(&cp.record, &[(call, "ask".into())])
            .await
            .unwrap();
        let run_id = cp.record.id.clone();
        store.with_connection(move |db| {
            let payload:Vec<u8>=db.query_row("SELECT payload FROM command_audits WHERE run_id=?1",[&run_id],|r|r.get(0))?;
            let mut old:serde_json::Value=serde_json::from_slice(&payload)?;
            old["decision"]=serde_json::json!("Approved");
            db.execute("UPDATE command_audits SET payload=CAST(json_set(payload,'$.push_queued',json('false')) AS BLOB) WHERE run_id=?1",[&run_id])?;
            db.execute("INSERT INTO command_audits(id,run_id,call_id,session_id,payload) VALUES('old-audit',?1,'aaa-old-command','source',?2)",params![run_id,serde_json::to_vec(&old)?])?;
            // Make the old owner the first row as it would be before handoff.
            db.execute("UPDATE command_audits SET rowid=-1 WHERE id='old-audit'",[])?;
            Ok(())
        }).await.unwrap();
        cp.save(
            &target,
            RunPhase::NeedTool {
                call_id: "new-command".into(),
            },
            None,
        )
        .await
        .unwrap();
        assert!(store.with_connection(|db| Ok(db.query_row("SELECT json_extract(payload,'$.push_queued') FROM command_audits WHERE call_id='new-command'",[],|r|r.get::<_,bool>(0))?)).await.unwrap());
    }

    #[tokio::test]
    async fn cancellation_does_not_change_history_or_binding() {
        let (_dir, store, mut cp, mut source) = fixture().await;
        let before = serde_json::to_vec(
            &store
                .load(&SessionId::new("target"))
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        store.request_cancel(&cp.record.id).await.unwrap();
        assert!(
            cp.apply_handoff(
                &mut source,
                &crate::agent::ContextBuilder::new("test rules")
            )
            .await
            .is_err()
        );
        assert_eq!(cp.record.session_id, "source");
        assert_eq!(store.active_id(source.id()).await.unwrap(), *source.id());
        assert_eq!(
            before,
            serde_json::to_vec(
                &store
                    .load(&SessionId::new("target"))
                    .await
                    .unwrap()
                    .unwrap()
            )
            .unwrap()
        );
    }

    #[tokio::test]
    async fn saved_pending_handoff_recovers_once_and_keeps_source_history() {
        let (_dir, store, cp, source) = fixture().await;
        let original = serde_json::to_vec(&source).unwrap();
        let id = cp.record.id.clone();
        drop(cp);
        let (record, mut source) = store.load_run(&id).await.unwrap().unwrap();
        let ownership = store.claim_execution("source", "source").await.unwrap();
        let mut cp = Checkpoint::resume(&store, record, ownership);
        let target = cp
            .apply_handoff(
                &mut source,
                &crate::agent::ContextBuilder::new("test rules"),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(target.id().as_str(), "target");
        assert_eq!(target.system_prompt(), Some("test rules"));
        assert!(matches!(&target.messages()[0],Message::User(u) if u.content=="target history"));
        assert!(matches!(&target.messages()[1],Message::User(u) if u.content=="original request"));
        assert_eq!(
            original,
            serde_json::to_vec(
                &store
                    .load(&SessionId::new("source"))
                    .await
                    .unwrap()
                    .unwrap()
            )
            .unwrap()
        );
        assert_eq!(
            store.active_id(&SessionId::new("source")).await.unwrap(),
            *target.id()
        );
        let (record, saved) = store.load_run(&id).await.unwrap().unwrap();
        assert_eq!(record.handoff_hops, 1);
        assert!(record.pending_handoff.is_none());
        assert_eq!(saved.id(), target.id());
        cp.save(&target, RunPhase::Completed, None).await.unwrap();
    }
}
