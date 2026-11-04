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

impl SqliteSessionStore {
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
            ensure!(session == invocation.session_id.as_str() && !done, "Run is not bound or is terminal");
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
            tx.commit()?;
            Ok(())
        }).await
    }
}
