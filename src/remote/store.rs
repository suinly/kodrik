use super::protocol::*;
use crate::agent::{
    run_state::RunRecord,
    session::{SessionId, SqliteSessionStore},
};
use anyhow::{Context, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use uuid::Uuid;

pub(crate) fn migrate(db: &Connection) -> anyhow::Result<()> {
    db.execute_batch(
        "CREATE TABLE remote_bindings (
        connection_id TEXT NOT NULL, generation TEXT NOT NULL, device_id TEXT NOT NULL,
        session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
        manifest BLOB NOT NULL, PRIMARY KEY(connection_id,generation));
        CREATE TABLE remote_operations (
        run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE, call_id TEXT NOT NULL,
        device_id TEXT NOT NULL, connection_id TEXT NOT NULL, generation TEXT NOT NULL,
        payload BLOB NOT NULL, PRIMARY KEY(run_id,call_id),
        FOREIGN KEY(connection_id,generation) REFERENCES remote_bindings(connection_id,generation));
        CREATE INDEX remote_operations_environment ON remote_operations(connection_id,generation);",
    )?;
    for table in ["remote_bindings", "remote_operations"] {
        for action in ["INSERT", "UPDATE", "DELETE"] {
            db.execute_batch(&format!("CREATE TRIGGER view_{table}_{action} AFTER {action} ON {table} BEGIN UPDATE schema_meta SET value=CAST(value AS INTEGER)+1 WHERE key='run_view_revision'; END;"))?;
        }
    }
    db.pragma_update(None, "user_version", 12)?;
    Ok(())
}

pub(crate) fn migrate_attachments(db: &Connection) -> anyhow::Result<()> {
    db.execute_batch(
        "CREATE TABLE remote_tool_attachments (
        run_id TEXT NOT NULL, call_id TEXT NOT NULL, payload BLOB NOT NULL,
        PRIMARY KEY(run_id,call_id),
        FOREIGN KEY(run_id,call_id) REFERENCES remote_operations(run_id,call_id) ON DELETE CASCADE);
        PRAGMA user_version=13;",
    )?;
    Ok(())
}

fn save_selected_text(
    db: &Connection,
    directory: &std::path::Path,
    receipt: &OperationReceipt,
) -> anyhow::Result<()> {
    let call = &receipt.invocation;
    if receipt.state != OperationState::Completed || call.arguments["save_attachment"] != true {
        return Ok(());
    }
    let manifest: Vec<u8> = db.query_row(
        "SELECT manifest FROM remote_bindings WHERE connection_id=?1 AND generation=?2",
        params![
            call.environment.connection_id.to_string(),
            call.environment.generation.to_string()
        ],
        |row| row.get(0),
    )?;
    let manifest: EnvironmentManifest = serde_json::from_slice(&manifest)?;
    if !manifest
        .tools
        .iter()
        .any(|tool| tool.id == call.tool_id && tool.kind == ToolKind::ReadFile)
    {
        return Ok(());
    }
    let Some(text) = receipt
        .result
        .as_ref()
        .and_then(|output| output.output["text"].as_str())
    else {
        return Ok(());
    };
    ensure!(text.len() <= 1024 * 1024, "Selected text exceeds 1 MiB");
    if text.is_empty() {
        return Ok(());
    }
    // Only bytes from the admitted receipt are stored; client paths/attachment metadata are never opened.
    let filename = call.arguments["path"]
        .as_str()
        .unwrap_or("selected.txt")
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("selected.txt");
    let filename = if !filename.is_empty()
        && filename.len() <= 255
        && !filename.chars().any(char::is_control)
        && crate::agent::file::file_type(filename, Some("text/plain")).is_ok_and(|mime| {
            mime.starts_with("text/")
                || matches!(
                    mime,
                    "application/json" | "application/javascript" | "application/xml"
                )
        }) {
        filename.to_owned()
    } else {
        "selected.txt".into()
    };
    let file = crate::agent::FileAttachment::save(
        directory,
        &filename,
        Some("text/plain"),
        text.as_bytes(),
    )?;
    db.execute(
        "INSERT INTO remote_tool_attachments(run_id,call_id,payload) VALUES(?1,?2,?3)",
        params![call.run_id, call.call_id, serde_json::to_vec(&file)?],
    )?;
    Ok(())
}

fn operation(
    db: &Connection,
    run: &str,
    call: &str,
) -> anyhow::Result<Option<(Uuid, OperationReceipt)>> {
    let row: Option<(String, Vec<u8>)> = db
        .query_row(
            "SELECT device_id,payload FROM remote_operations WHERE run_id=?1 AND call_id=?2",
            params![run, call],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(device, bytes)| Ok((Uuid::parse_str(&device)?, serde_json::from_slice(&bytes)?)))
        .transpose()
}

fn same_call(saved: &OperationReceipt, invocation: &Invocation) -> anyhow::Result<()> {
    ensure!(
        serde_json::to_value(&saved.invocation)? == serde_json::to_value(invocation)?
            && saved.arguments_hash == arguments_hash(&invocation.arguments)?,
        "Remote call identity changed"
    );
    Ok(())
}

fn save(db: &Connection, receipt: &OperationReceipt) -> anyhow::Result<()> {
    db.execute(
        "UPDATE remote_operations SET payload=?3 WHERE run_id=?1 AND call_id=?2",
        params![
            receipt.invocation.run_id,
            receipt.invocation.call_id,
            serde_json::to_vec(receipt)?
        ],
    )?;
    Ok(())
}

fn terminal(state: OperationState) -> bool {
    matches!(
        state,
        OperationState::Completed | OperationState::Stopped | OperationState::Rejected
    )
}

// Runs inside the same checkpoint transaction as history, terminal status and inbox updates.
pub(crate) fn prepare_environment_pause(
    db: &Connection,
    run: &mut RunRecord,
    session: &mut crate::agent::session::Session,
) -> anyhow::Result<()> {
    use crate::agent::{
        inbox::{SubmissionRecord, SubmissionStatus},
        message::{Message, ToolResult},
        run_state::ToolExecution,
        tools::ReplayPolicy,
    };
    ensure!(
        run.session_id == session.id().as_str(),
        "Pause session changed"
    );
    let receipts = db
        .prepare("SELECT payload FROM remote_operations WHERE run_id=?1")?
        .query_map([&run.id], |row| row.get::<_, Vec<u8>>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for bytes in receipts {
        let mut receipt: OperationReceipt = serde_json::from_slice(&bytes)?;
        if matches!(
            receipt.state,
            OperationState::Accepted | OperationState::Running
        ) {
            receipt.state = OperationState::Unknown;
            save(db, &receipt)?;
        }
    }
    for call in std::mem::take(&mut run.pending_calls) {
        let intent = db
            .query_row(
                "SELECT payload FROM tool_executions WHERE run_id=?1 AND call_id=?2",
                params![run.id, call.call_id],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
            .map(|bytes| serde_json::from_slice::<ToolExecution>(&bytes))
            .transpose()?;
        let existing = session.messages().iter().find_map(|message| match message {
            Message::ToolResult(result) if result.call_id == call.call_id => {
                Some(result.content.clone())
            }
            _ => None,
        });
        let output = if let Some(output) = existing {
            output
        } else {
            let saved = operation(db, &run.id, &call.call_id)?.map(|(_, receipt)| receipt);
            let result = saved.as_ref().and_then(|receipt| receipt.result.clone());
            result.unwrap_or_else(|| json_unavailable(saved.as_ref(), intent.is_some()))
        };
        if !session.messages().iter().any(
            |message| matches!(message,Message::ToolResult(result) if result.call_id==call.call_id),
        ) {
            session.push(Message::ToolResult(ToolResult {
                call_id: call.call_id.clone(),
                content: output.clone(),
            }));
        }
        let execution = ToolExecution {
            call: call.clone(),
            replay: intent
                .map(|execution| execution.replay)
                .unwrap_or(ReplayPolicy::Never),
            result: Some(output),
        };
        db.execute("INSERT INTO tool_executions VALUES(?1,?2,?3) ON CONFLICT(run_id,call_id) DO UPDATE SET payload=excluded.payload",params![run.id,call.call_id,serde_json::to_vec(&execution)?])?;
    }
    let queued=db.prepare("SELECT s.payload FROM submissions s LEFT JOIN active_sessions a ON a.conversation_id=s.conversation_id WHERE coalesce(s.session_id,a.session_id,s.conversation_id)=?1 AND s.status='Queued' AND json_extract(s.payload,'$.request.control')=0 ORDER BY s.rowid")?.query_map([&run.session_id],|row|row.get::<_,Vec<u8>>(0))?.collect::<Result<Vec<_>,_>>()?;
    for bytes in queued {
        let mut record: SubmissionRecord = serde_json::from_slice(&bytes)?;
        if record
            .session_id
            .as_deref()
            .is_some_and(|id| id != run.session_id)
        {
            continue;
        }
        if run.submission_ids.contains(&record.id) {
            continue;
        }
        let message = record.request.message.clone();
        session.push(Message::User(crate::agent::UserMessage {
            browser_client_id: None,
            browser_context: None,
            content: message.text,
            files: message.files,
            images: message.images,
        }));
        record.status = SubmissionStatus::Unanswered;
        record.session_id = Some(run.session_id.clone());
        record.run_id = Some(run.id.clone());
        crate::agent::inbox::write_record(db, &record)?;
    }
    run.attempt_active = false;
    run.history_len = Some(session.messages().len());
    Ok(())
}

fn json_unavailable(
    saved: Option<&OperationReceipt>,
    has_intent: bool,
) -> crate::agent::tools::ToolOutput {
    // A server tool can have executed without ever creating a remote operation row.
    let uncertain = saved.map_or(has_intent, |receipt| {
        receipt.state != OperationState::NotSent
    });
    serde_json::json!({"error":"Environment unavailable; old invocation will not resume. Do not repeat uncertain effects","outcome":if uncertain {"unknown"} else {"unavailable"},"environment":saved.map(|receipt|&receipt.invocation.environment)}).into()
}

pub(crate) fn reconciliation(
    db: &Connection,
    session: &str,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let mut stmt = db.prepare("SELECT json_object('run_id',o.run_id,'call_id',o.call_id,'connection_id',o.connection_id,'generation',o.generation,'tool_id',json_extract(o.payload,'$.invocation.tool_id'),'state',json_extract(o.payload,'$.state'),'arguments_hash',json_extract(o.payload,'$.arguments_hash')) FROM remote_operations o JOIN runs r ON r.id=o.run_id WHERE r.session_id=?1 ORDER BY o.rowid DESC LIMIT 256")?;
    stmt.query_map([session], |row| row.get::<_, String>(0))?
        .map(|row| Ok(serde_json::from_str(&row?)?))
        .collect()
}

impl SqliteSessionStore {
    pub(crate) async fn remote_saved_file(
        &self,
        session: &SessionId,
        call: crate::agent::message::ToolCall,
        output: serde_json::Value,
    ) -> anyhow::Result<Option<crate::agent::FileAttachment>> {
        let session = session.as_str().to_owned();
        self.with_connection(move |db| {
            let mut query=db.prepare("SELECT a.payload,o.payload,b.manifest FROM remote_tool_attachments a JOIN remote_operations o USING(run_id,call_id) JOIN runs r ON r.id=o.run_id JOIN remote_bindings b ON b.connection_id=o.connection_id AND b.generation=o.generation WHERE r.session_id=?1 AND o.call_id=?2 ORDER BY o.rowid DESC")?;
            for row in query.query_map(params![session,call.call_id],|row|Ok((row.get::<_,Vec<u8>>(0)?,row.get::<_,Vec<u8>>(1)?,row.get::<_,Vec<u8>>(2)?)))? {
                let (file,receipt,manifest)=row?;
                let receipt:OperationReceipt=serde_json::from_slice(&receipt)?;
                let manifest:EnvironmentManifest=serde_json::from_slice(&manifest)?;
                if receipt.invocation.arguments==call.arguments
                    && receipt.result.as_ref().is_some_and(|result|result.output==output)
                    && manifest.tools.iter().any(|tool|tool.id==receipt.invocation.tool_id && super::tools::alias(&receipt.invocation.environment,tool)==call.name) {
                    return Ok(Some(serde_json::from_slice(&file)?));
                }
            }
            Ok(None)
        }).await
    }

    pub(crate) async fn remote_history_origins(
        &self,
        session: &SessionId,
        calls: Vec<(String, String)>,
    ) -> anyhow::Result<std::collections::HashMap<String, serde_json::Value>> {
        let session = session.as_str().to_owned();
        self.with_connection(move |db| {
            let mut origins = std::collections::HashMap::new();
            let mut query = db.prepare("SELECT o.connection_id,o.generation,o.payload,b.manifest FROM remote_operations o JOIN runs r ON r.id=o.run_id JOIN remote_bindings b ON b.connection_id=o.connection_id AND b.generation=o.generation WHERE r.session_id=?1 AND o.call_id=?2 ORDER BY o.rowid DESC")?;
            // Bound the query to displayed call IDs, not the latest run or receipt window.
            for (call, name) in calls {
                for row in query.query_map(params![session,call], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,Vec<u8>>(2)?,row.get::<_,Vec<u8>>(3)?)))? {
                    let (connection,generation,payload,manifest) = row?;
                    let key = EnvironmentKey { connection_id: connection.parse()?, generation: generation.parse()? };
                    let receipt: OperationReceipt = serde_json::from_slice(&payload)?;
                    let manifest: EnvironmentManifest = serde_json::from_slice(&manifest)?;
                    if manifest.tools.iter().any(|tool| tool.id == receipt.invocation.tool_id && super::tools::alias(&key,tool) == name) {
                        origins.insert(call.clone(),serde_json::json!({"label":manifest.label,"project_root":manifest.project_root,"environment":key,"tool_id":receipt.invocation.tool_id}));
                        break;
                    }
                }
            }
            Ok(origins)
        }).await
    }

    pub(crate) async fn remote_reconciliation(
        &self,
        session: &SessionId,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        let session = session.as_str().to_owned();
        self.with_connection(move |db| reconciliation(db, &session))
            .await
    }
    pub(crate) async fn yield_environment_run(
        &self,
        run_id: &str,
        session: &crate::agent::session::Session,
    ) -> anyhow::Result<()> {
        let (mut run, _) = self.load_run(run_id).await?.context("Run missing")?;
        if matches!(
            run.phase,
            crate::agent::run_state::RunPhase::EnvironmentPaused { .. }
        ) {
            return Ok(());
        }
        ensure!(!run.phase.terminal(), "Cannot pause terminal run");
        let revision = run.revision;
        run.revision += 1;
        run.phase = crate::agent::run_state::RunPhase::EnvironmentPaused {
            reason: "Laptop environment unavailable; explicit new input required".into(),
        };
        run.attempt_active = false;
        self.commit_owned_step(revision, session, &run, None, None)
            .await
    }

    pub(crate) async fn pending_remote_target(
        &self,
        run: &RunRecord,
    ) -> anyhow::Result<Option<super::tools::AuditTarget>> {
        let run = run.clone();
        self.with_connection(move |db| {
            let owner = if run.session_id.starts_with(crate::agent::subagents::PREFIX) {
                db.query_row(
                    "SELECT parent FROM subagent_sessions WHERE id=?1",
                    [&run.session_id],
                    |r| r.get::<_, String>(0),
                )?
            } else {
                run.session_id.clone()
            };
            let mut calls = run.pending_calls;
            let mut stmt = db.prepare("SELECT payload FROM tool_executions WHERE run_id=?1")?;
            for bytes in stmt.query_map([&run.id], |row| row.get::<_, Vec<u8>>(0))? {
                calls.push(
                    serde_json::from_slice::<crate::agent::run_state::ToolExecution>(&bytes?)?.call,
                );
            }
            for call in calls {
                if let Some(target) = super::tools::saved_target(db, &owner, &call.name)? {
                    return Ok(Some(target));
                }
            }
            Ok(None)
        })
        .await
    }
    pub(crate) async fn revoke_remote_approvals(&self, key: EnvironmentKey) -> anyhow::Result<()> {
        self.with_connection(move |db| {
            db.execute("UPDATE command_audits SET payload=CAST(json_set(CAST(payload AS TEXT),'$.decision','Revoked') AS BLOB) WHERE json_extract(payload,'$.approval.target.environment.connection_id')=?1 AND json_extract(payload,'$.approval.target.environment.generation')=?2",params![key.connection_id.to_string(),key.generation.to_string()])?;
            Ok(())
        }).await
    }

    // Provenance only: this row never grants live authority after restart.
    pub(crate) async fn remote_binding(
        &self,
        device: Uuid,
        key: EnvironmentKey,
        session: SessionId,
        manifest: EnvironmentManifest,
    ) -> anyhow::Result<()> {
        validate_manifest(&manifest)?;
        self.with_connection(move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let old: Option<(String,String,Vec<u8>)> = tx.query_row("SELECT device_id,session_id,manifest FROM remote_bindings WHERE connection_id=?1 AND generation=?2",params![key.connection_id.to_string(),key.generation.to_string()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            let manifest = serde_json::to_vec(&manifest)?;
            if let Some((owner,id,payload)) = old {
                ensure!(owner == device.to_string() && id == session.as_str() && payload == manifest, "Environment provenance changed");
            } else {
                tx.execute("INSERT INTO remote_bindings VALUES(?1,?2,?3,?4,?5)",params![key.connection_id.to_string(),key.generation.to_string(),device.to_string(),session.as_str(),manifest])?;
            }
            tx.commit()?;
            Ok(())
        }).await
    }

    pub(crate) async fn remote_intent(
        &self,
        device: Uuid,
        invocation: Invocation,
    ) -> anyhow::Result<OperationReceipt> {
        encode_frame(&ServerFrame::Invoke {
            invocation: invocation.clone(),
        })?;
        self.with_connection(move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some((owner,saved)) = operation(&tx,&invocation.run_id,&invocation.call_id)? {
                ensure!(owner == device,"Device provenance changed");
                same_call(&saved,&invocation)?;
                return Ok(saved);
            }
            let (owner,session,manifest): (String,String,Vec<u8>) = tx.query_row("SELECT device_id,session_id,manifest FROM remote_bindings WHERE connection_id=?1 AND generation=?2",params![invocation.environment.connection_id.to_string(),invocation.environment.generation.to_string()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            ensure!(owner == device.to_string() && session == invocation.session_id.as_str(), "Environment provenance mismatch");
            let manifest: EnvironmentManifest = serde_json::from_slice(&manifest)?;
            ensure!(manifest.tools.iter().any(|tool|tool.id == invocation.tool_id), "Unknown local tool");
            let (session,done,payload): (String,bool,Vec<u8>) = tx.query_row("SELECT session_id,terminal,payload FROM runs WHERE id=?1",[&invocation.run_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            let inherited=if session!=invocation.session_id.as_str() && session.starts_with(crate::agent::subagents::PREFIX) {
                let relation:Option<(String,String)>=tx.query_row("SELECT parent,generation FROM subagent_sessions WHERE id=?1",[&session],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
                if let Some((parent,generation))=relation.filter(|(parent,_)|parent==invocation.session_id.as_str()) {
                    let bytes:Vec<u8>=tx.query_row("SELECT payload FROM sessions WHERE id=?1",[parent],|r|r.get(0))?;
                    serde_json::from_slice::<crate::agent::session::Session>(&bytes)?.generation()==generation
                } else {false}
            } else {false};
            ensure!((session == invocation.session_id.as_str() || inherited) && !done, "Run is not bound or is terminal");
            let run: RunRecord = serde_json::from_slice(&payload)?;
            ensure!(run.pending_calls.iter().any(|call|call.call_id == invocation.call_id && call.arguments == invocation.arguments), "Call is not pending in run");
            let tool: Vec<u8> = tx.query_row("SELECT payload FROM tool_executions WHERE run_id=?1 AND call_id=?2",params![invocation.run_id,invocation.call_id],|r|r.get(0))?;
            serde_json::from_slice::<crate::agent::run_state::ToolExecution>(&tool)?.validate_remote(&invocation)?;
            let receipt = OperationReceipt {arguments_hash:arguments_hash(&invocation.arguments)?,invocation,state:OperationState::NotSent,result:None};
            tx.execute("INSERT INTO remote_operations VALUES(?1,?2,?3,?4,?5,?6)",params![receipt.invocation.run_id,receipt.invocation.call_id,device.to_string(),receipt.invocation.environment.connection_id.to_string(),receipt.invocation.environment.generation.to_string(),serde_json::to_vec(&receipt)?])?;
            tx.commit()?;
            Ok(receipt)
        }).await
    }

    // Commit uncertainty means no send. Even a lost success response cannot grant a second claim.
    pub(crate) async fn remote_dispatch(
        &self,
        device: Uuid,
        invocation: Invocation,
    ) -> anyhow::Result<bool> {
        self.with_connection(move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let (owner, mut saved) = operation(&tx, &invocation.run_id, &invocation.call_id)?
                .context("Intent not saved")?;
            ensure!(owner == device, "Device provenance changed");
            same_call(&saved, &invocation)?;
            if saved.state != OperationState::NotSent {
                return Ok(false);
            }
            let stopped: bool = tx.query_row("SELECT terminal OR EXISTS(SELECT 1 FROM run_controls WHERE run_id=?1) FROM runs WHERE id=?1",[&invocation.run_id],|r|r.get(0))?;
            ensure!(!stopped && invocation.deadline > chrono::Utc::now(), "Run stopped or invocation expired");
            saved.state = OperationState::Unknown;
            save(&tx, &saved)?;
            tx.commit()?;
            Ok(true)
        })
        .await
    }

    // device comes from authenticated transport, never from the receipt body.
    pub(crate) async fn remote_receipt(
        &self,
        device: Uuid,
        receipt: OperationReceipt,
    ) -> anyhow::Result<()> {
        ensure!(
            receipt.state != OperationState::NotSent,
            "Client cannot declare an unsent operation"
        );
        ensure!(
            receipt.arguments_hash == arguments_hash(&receipt.invocation.arguments)?,
            "Receipt hash mismatch"
        );
        ensure!(
            receipt.state != OperationState::Completed || receipt.result.is_some(),
            "Completed receipt requires a result"
        );
        ensure!(
            terminal(receipt.state) || receipt.result.is_none(),
            "Nonterminal result"
        );
        if let Some(result) = &receipt.result {
            ensure!(
                serde_json::to_vec(result)?.len() <= MAX_OUTPUT_BYTES,
                "Output exceeds 16 MiB"
            );
        }
        let directory = self.attachment_directory();
        self.with_connection(move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let (owner, saved) =
                operation(&tx, &receipt.invocation.run_id, &receipt.invocation.call_id)?
                    .context("Unsolicited receipt")?;
            ensure!(owner == device, "Device provenance changed");
            same_call(&saved, &receipt.invocation)?;
            ensure!(
                saved.state != OperationState::NotSent,
                "Operation was never dispatched"
            );
            if terminal(saved.state) {
                if terminal(receipt.state) {
                    ensure!(
                        serde_json::to_value(&saved)? == serde_json::to_value(&receipt)?,
                        "Conflicting final receipt"
                    );
                }
                return Ok(());
            }
            if saved.state == OperationState::Running && receipt.state == OperationState::Accepted {
                return Ok(());
            }
            if serde_json::to_value(&saved)? != serde_json::to_value(&receipt)? {
                save_selected_text(&tx, &directory, &receipt)?;
                save(&tx, &receipt)?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn remote_operation(
        &self,
        run: &str,
        call: &str,
    ) -> anyhow::Result<Option<OperationReceipt>> {
        let (run, call) = (run.to_owned(), call.to_owned());
        self.with_connection(move |db| Ok(operation(db, &run, &call)?.map(|(_, receipt)| receipt)))
            .await
    }

    pub(crate) async fn remote_result_call(
        &self,
        device: Uuid,
        run: String,
        call: String,
    ) -> anyhow::Result<OperationReceipt> {
        self.with_connection(move |db| {
            let (owner, saved) = operation(db, &run, &call)?.context("Unsolicited chunk")?;
            ensure!(
                owner == device && saved.state != OperationState::NotSent,
                "Chunk provenance mismatch"
            );
            Ok(saved)
        })
        .await
    }

    pub(crate) async fn remote_call_uncertain(
        &self,
        device: Uuid,
        invocation: Invocation,
    ) -> anyhow::Result<()> {
        self.with_connection(move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let (owner, mut saved) = operation(&tx, &invocation.run_id, &invocation.call_id)?
                .context("Remote evidence missing")?;
            ensure!(owner == device, "Device provenance changed");
            same_call(&saved, &invocation)?;
            if matches!(
                saved.state,
                OperationState::Accepted | OperationState::Running
            ) {
                saved.state = OperationState::Unknown;
                save(&tx, &saved)?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn remote_disconnect(&self, key: EnvironmentKey) -> anyhow::Result<()> {
        self.with_connection(move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let rows = tx.prepare("SELECT payload FROM remote_operations WHERE connection_id=?1 AND generation=?2")?.query_map(params![key.connection_id.to_string(),key.generation.to_string()],|r|r.get::<_,Vec<u8>>(0))?.collect::<Result<Vec<_>,_>>()?;
            for bytes in rows {
                let mut saved: OperationReceipt = serde_json::from_slice(&bytes)?;
                if matches!(saved.state, OperationState::Accepted | OperationState::Running) {
                    saved.state = OperationState::Unknown;
                    save(&tx,&saved)?;
                }
            }
            tx.commit()?;
            Ok(())
        }).await
    }

    // Called once before the gateway accepts connections, not on arbitrary store opens.
    pub(crate) async fn remote_recover(&self) -> anyhow::Result<()> {
        self.with_connection(move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute("UPDATE remote_operations SET payload=CAST(json_set(CAST(payload AS TEXT),'$.state','unknown') AS BLOB) WHERE json_extract(payload,'$.state') IN ('accepted','running')",[])?;
            tx.execute("UPDATE command_audits SET payload=CAST(json_set(CAST(payload AS TEXT),'$.decision','Revoked') AS BLOB) WHERE json_type(payload,'$.approval.target.environment')='object'",[])?;
            tx.commit()?;
            Ok(())
        }).await
    }
}
