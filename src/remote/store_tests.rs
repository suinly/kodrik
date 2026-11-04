use super::protocol::*;
use crate::agent::{
    message::{Message, ToolCall},
    run_state::{Checkpoint, RunPhase, ToolExecution},
    session::{Session, SessionId, SqliteSessionStore},
    tools::ReplayPolicy,
};
use serde_json::json;
use uuid::Uuid;

async fn fixture() -> (tempfile::TempDir, SqliteSessionStore, Uuid, Invocation) {
    fixture_with_args(json!({"cmd":"echo once"})).await
}

async fn fixture_with_args(
    arguments: serde_json::Value,
) -> (tempfile::TempDir, SqliteSessionStore, Uuid, Invocation) {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    let mut session = Session::new(SessionId::new(Uuid::new_v4().to_string()));
    let call = ToolCall {
        call_id: "call".into(),
        name: "local_exec".into(),
        arguments,
    };
    session.push(Message::ToolCall(call.clone()));
    let mut cp = Checkpoint::new(&store, session.id(), &session);
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
    let device = Uuid::new_v4();
    let invocation = Invocation {
        environment: EnvironmentKey {
            connection_id: Uuid::new_v4(),
            generation: Uuid::new_v4(),
        },
        session_id: session.id().clone(),
        run_id: cp.record.id,
        call_id: call.call_id,
        tool_id: "exec".into(),
        arguments: call.arguments,
        deadline: chrono::Utc::now() + chrono::Duration::minutes(1),
    };
    (dir, store, device, invocation)
}

fn manifest() -> EnvironmentManifest {
    EnvironmentManifest {
        label: "Laptop".into(),
        project_root: "/project".into(),
        tools: vec![ToolDescriptor {
            id: "exec".into(),
            kind: ToolKind::Exec,
            description: "Shell".into(),
            parameters: json!({"type":"object"}),
            deferred: false,
        }],
        skills: vec![],
        project_instructions: None,
    }
}

async fn bind(store: &SqliteSessionStore, device: Uuid, call: &Invocation) {
    store
        .remote_binding(
            device,
            call.environment.clone(),
            call.session_id.clone(),
            manifest(),
        )
        .await
        .unwrap();
}

fn receipt(call: &Invocation, state: OperationState) -> OperationReceipt {
    OperationReceipt {
        invocation: call.clone(),
        arguments_hash: arguments_hash(&call.arguments).unwrap(),
        state,
        result: None,
    }
}

#[tokio::test]
async fn intent_requires_provenance_and_claims_dispatch_only_once() {
    let (_dir, store, device, call) = fixture().await;
    assert!(store.remote_intent(device, call.clone()).await.is_err());
    bind(&store, device, &call).await;
    let revision = store.view_revision().await.unwrap();
    let saved = store.remote_intent(device, call.clone()).await.unwrap();
    assert_eq!(saved.state, OperationState::NotSent);
    assert!(store.view_revision().await.unwrap() > revision);
    store.remote_intent(device, call.clone()).await.unwrap();
    assert!(store.remote_dispatch(device, call.clone()).await.unwrap());
    assert!(!store.remote_dispatch(device, call.clone()).await.unwrap());
    assert_eq!(
        store
            .remote_operation(&call.run_id, &call.call_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        OperationState::Unknown
    );
    for changed in 0..5 {
        let mut different = call.clone();
        let mut credential = device;
        match changed {
            0 => different.arguments = json!({"cmd":"second effect"}),
            1 => different.environment.generation = Uuid::new_v4(),
            2 => different.session_id = SessionId::new("wrong"),
            3 => credential = Uuid::new_v4(),
            _ => different.deadline += chrono::Duration::seconds(1),
        }
        assert!(
            store
                .remote_intent(credential, different.clone())
                .await
                .is_err()
        );
        assert!(store.remote_dispatch(credential, different).await.is_err());
    }
}

#[tokio::test]
async fn unknown_survives_restart_and_late_result_never_resumes_run() {
    let (dir, store, device, call) = fixture().await;
    bind(&store, device, &call).await;
    store.remote_intent(device, call.clone()).await.unwrap();
    store.remote_dispatch(device, call.clone()).await.unwrap();
    store
        .remote_receipt(device, receipt(&call, OperationState::Accepted))
        .await
        .unwrap();
    store
        .remote_receipt(device, receipt(&call, OperationState::Running))
        .await
        .unwrap();
    store
        .remote_disconnect(call.environment.clone())
        .await
        .unwrap();
    let run_before =
        serde_json::to_value(store.load_run(&call.run_id).await.unwrap().unwrap().0).unwrap();
    drop(store);
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    assert_eq!(
        store
            .remote_intent(device, call.clone())
            .await
            .unwrap()
            .state,
        OperationState::Unknown
    );
    assert!(!store.remote_dispatch(device, call.clone()).await.unwrap());
    let mut done = receipt(&call, OperationState::Completed);
    done.result = Some(json!({"exit_code":0,"effect":"once"}).into());
    assert!(
        store
            .remote_receipt(Uuid::new_v4(), done.clone())
            .await
            .is_err()
    );
    store.remote_receipt(device, done.clone()).await.unwrap();
    store.remote_receipt(device, done.clone()).await.unwrap();
    assert_eq!(
        serde_json::to_value(store.load_run(&call.run_id).await.unwrap().unwrap().0).unwrap(),
        run_before
    );
    assert_eq!(
        store
            .remote_operation(&call.run_id, &call.call_id)
            .await
            .unwrap()
            .unwrap()
            .result
            .unwrap()
            .output["effect"],
        "once"
    );
    drop(store);
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    assert_eq!(
        store
            .remote_intent(device, call.clone())
            .await
            .unwrap()
            .state,
        OperationState::Completed
    );
    assert!(!store.remote_dispatch(device, call.clone()).await.unwrap());
    store
        .remote_disconnect(call.environment.clone())
        .await
        .unwrap();
    assert_eq!(
        store
            .remote_operation(&call.run_id, &call.call_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        OperationState::Completed
    );
    done.result = Some(json!({"effect":"forged"}).into());
    assert!(store.remote_receipt(device, done).await.is_err());
}

#[tokio::test]
async fn malformed_or_unsent_receipts_cannot_replace_evidence() {
    let (_dir, store, device, call) = fixture().await;
    bind(&store, device, &call).await;
    store.remote_intent(device, call.clone()).await.unwrap();
    assert!(
        store
            .remote_receipt(device, receipt(&call, OperationState::Running))
            .await
            .is_err()
    );
    store.remote_dispatch(device, call.clone()).await.unwrap();
    let mut bad = receipt(&call, OperationState::Accepted);
    bad.arguments_hash = "0".repeat(64);
    assert!(store.remote_receipt(device, bad).await.is_err());
    assert!(
        store
            .remote_receipt(device, receipt(&call, OperationState::Completed))
            .await
            .is_err()
    );
    let mut changed = receipt(&call, OperationState::Accepted);
    changed.invocation.environment.generation = Uuid::new_v4();
    assert!(store.remote_receipt(device, changed).await.is_err());
    store
        .remote_receipt(device, receipt(&call, OperationState::Running))
        .await
        .unwrap();
    store
        .remote_receipt(device, receipt(&call, OperationState::Accepted))
        .await
        .unwrap();
    assert_eq!(
        store
            .remote_operation(&call.run_id, &call.call_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        OperationState::Running
    );
}

#[tokio::test]
async fn storage_failure_never_grants_dispatch_or_partially_records_intent() {
    for full in [false, true] {
        let (_dir, store, device, call) =
            fixture_with_args(json!({"cmd":"x".repeat(120_000)})).await;
        bind(&store, device, &call).await;
        // A bounded invocation larger than SQLite's remaining free pages forces SQLITE_FULL.
        store
            .with_connection(move |db| {
                if full {
                    let pages: i64 = db.pragma_query_value(None, "page_count", |r| r.get(0))?;
                    db.pragma_update(None, "max_page_count", pages)?;
                } else {
                    db.pragma_update(None, "query_only", true)?;
                }
                Ok(())
            })
            .await
            .unwrap();
        let error = store.remote_intent(device, call.clone()).await.unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<rusqlite::Error>()
                .unwrap()
                .sqlite_error_code(),
            Some(if full {
                rusqlite::ErrorCode::DiskFull
            } else {
                rusqlite::ErrorCode::ReadOnly
            })
        );
        assert!(
            store
                .remote_operation(&call.run_id, &call.call_id)
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn gateway_recovery_marks_inflight_evidence_unknown_and_keeps_unsent_intents() {
    let (dir, store, device, call) = fixture().await;
    bind(&store, device, &call).await;
    store.remote_intent(device, call.clone()).await.unwrap();
    drop(store);
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    store.remote_recover().await.unwrap();
    assert_eq!(
        store
            .remote_operation(&call.run_id, &call.call_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        OperationState::NotSent
    );
    store.remote_dispatch(device, call.clone()).await.unwrap();
    store
        .remote_receipt(device, receipt(&call, OperationState::Running))
        .await
        .unwrap();
    drop(store);
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    store.remote_recover().await.unwrap();
    assert_eq!(
        store
            .remote_operation(&call.run_id, &call.call_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        OperationState::Unknown
    );
    assert!(!store.remote_dispatch(device, call).await.unwrap());
}

#[tokio::test]
async fn cancelled_run_cannot_dispatch_a_previously_saved_intent() {
    let (_dir, store, device, call) = fixture().await;
    bind(&store, device, &call).await;
    store.remote_intent(device, call.clone()).await.unwrap();
    store.cancel_run(&call.run_id).await.unwrap();
    assert!(store.remote_dispatch(device, call).await.is_err());
}

#[tokio::test]
async fn unknown_commit_acknowledgement_cannot_grant_another_send() {
    let (_dir, store, device, call) = fixture().await;
    bind(&store, device, &call).await;
    store.remote_intent(device, call.clone()).await.unwrap();
    // Losing the caller's response after commit must not reopen the dispatch gate.
    let _lost_ack = store.remote_dispatch(device, call.clone()).await.unwrap();
    assert!(!store.remote_dispatch(device, call.clone()).await.unwrap());
    assert_eq!(
        store
            .remote_operation(&call.run_id, &call.call_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        OperationState::Unknown
    );
}

#[tokio::test]
async fn run_observer_sees_evidence_without_changing_execution_checkpoint() {
    let (_dir, store, device, call) = fixture().await;
    bind(&store, device, &call).await;
    store.remote_intent(device, call.clone()).await.unwrap();
    store.remote_dispatch(device, call.clone()).await.unwrap();
    let view = store.run_view(&call.run_id).await.unwrap();
    assert_eq!(view.remote_operations[0]["state"], "unknown");
    assert_eq!(view.remote_operations[0]["tool_id"], "exec");
    assert_eq!(
        view.remote_operations[0]["generation"],
        call.environment.generation.to_string()
    );
    assert!(view.remote_operations[0].get("arguments").is_none());
}

#[tokio::test]
async fn remote_intent_refuses_replayable_tool_checkpoint() {
    let (_dir, store, device, call) = fixture().await;
    bind(&store, device, &call).await;
    let id = call.run_id.clone();
    store.with_connection(move |db| {
        db.execute("UPDATE tool_executions SET payload=CAST(json_set(CAST(payload AS TEXT),'$.replay','Safe') AS BLOB) WHERE run_id=?1",[id])?;
        Ok(())
    }).await.unwrap();
    assert!(store.remote_intent(device, call).await.is_err());
}

#[tokio::test]
async fn late_result_for_terminal_run_only_updates_evidence() {
    let (_dir, store, device, call) = fixture().await;
    bind(&store, device, &call).await;
    store.remote_intent(device, call.clone()).await.unwrap();
    store.remote_dispatch(device, call.clone()).await.unwrap();
    let (mut run, session) = store.load_run(&call.run_id).await.unwrap().unwrap();
    let previous = run.revision;
    run.revision += 1;
    run.phase = RunPhase::Failed {
        reason: "environment unavailable".into(),
    };
    store
        .commit_step(previous, &session, &run, None)
        .await
        .unwrap();
    let original = serde_json::to_value(&run).unwrap();
    let mut done = receipt(&call, OperationState::Completed);
    done.result = Some(json!({"late":true}).into());
    store.remote_receipt(device, done).await.unwrap();
    assert_eq!(
        serde_json::to_value(store.load_run(&call.run_id).await.unwrap().unwrap().0).unwrap(),
        original
    );
    assert_eq!(
        store
            .remote_operation(&call.run_id, &call.call_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        OperationState::Completed
    );
}

#[tokio::test]
async fn readonly_dispatch_does_not_grant_permission_to_send() {
    let (_dir, store, device, call) = fixture().await;
    bind(&store, device, &call).await;
    store.remote_intent(device, call.clone()).await.unwrap();
    store
        .with_connection(|db| {
            db.pragma_update(None, "query_only", true)?;
            Ok(())
        })
        .await
        .unwrap();
    let error = store
        .remote_dispatch(device, call.clone())
        .await
        .unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<rusqlite::Error>()
            .unwrap()
            .sqlite_error_code(),
        Some(rusqlite::ErrorCode::ReadOnly)
    );
    assert_eq!(
        store
            .remote_operation(&call.run_id, &call.call_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        OperationState::NotSent
    );
}

#[tokio::test]
async fn v11_migration_is_atomic_and_preserves_run_and_history() {
    let (dir, store, _device, call) = fixture().await;
    let original = serde_json::to_value(store.load_run(&call.run_id).await.unwrap()).unwrap();
    drop(store);
    let db = rusqlite::Connection::open(dir.path().join("state.sqlite3")).unwrap();
    db.execute_batch(
        "DROP TABLE remote_operations; DROP TABLE remote_bindings; PRAGMA user_version=11;",
    )
    .unwrap();
    drop(db);
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(store.load_run(&call.run_id).await.unwrap()).unwrap(),
        original
    );
    store
        .with_connection(|db| {
            assert_eq!(
                db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))?,
                12
            );
            assert_eq!(
                db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))?,
                "ok"
            );
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn failed_v12_migration_rolls_back_new_tables_and_version() {
    let (dir, store, _device, call) = fixture().await;
    drop(store);
    let db = rusqlite::Connection::open(dir.path().join("state.sqlite3")).unwrap();
    db.execute_batch(
        "DROP TABLE remote_operations; DROP TABLE remote_bindings; PRAGMA user_version=11;
        CREATE TRIGGER view_remote_bindings_INSERT AFTER INSERT ON sessions BEGIN SELECT 1; END;",
    )
    .unwrap();
    drop(db);
    assert!(
        SqliteSessionStore::open_async(dir.path().into())
            .await
            .is_err()
    );
    let db = rusqlite::Connection::open(dir.path().join("state.sqlite3")).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        11
    );
    assert_eq!(db.query_row("SELECT count(*) FROM sqlite_master WHERE name IN ('remote_bindings','remote_operations')",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM runs WHERE id=?1",
            [call.run_id],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}
