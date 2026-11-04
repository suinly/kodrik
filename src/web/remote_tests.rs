use crate::{
    agent::session::{Session, SessionId, SessionStore, SqliteSessionStore},
    remote::{protocol::*, service::EnvironmentService},
    web::{
        Settings,
        auth::{AuthStore, Identity, devices::PairingPoll},
    },
};
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use uuid::Uuid;

#[tokio::test]
async fn authenticated_prefixed_socket_binds_and_revocation_closes_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
    let session = SessionId::new(Uuid::new_v4().to_string());
    store.save(&Session::new(session.clone())).await.unwrap();
    let auth = AuthStore::new(dir.path());
    let now = chrono::Utc::now().timestamp();
    let pair = auth.start_device_pairing("fixture", now).unwrap();
    auth.approve_device_pairing(pair.request_id, &pair.code, &Identity::Owner, now)
        .unwrap();
    let PairingPoll::Authorized {
        device_id, token, ..
    } = auth
        .poll_device_pairing(pair.request_id, &pair.polling_secret, now)
        .unwrap()
    else {
        panic!()
    };
    let service = Arc::new(EnvironmentService::new(store, auth.clone(), true));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let settings = Settings {
        trusted_proxies: vec![],
        listen: address,
        base_url: format!("http://{address}/kodrik/").parse().unwrap(),
    };
    let app = axum::Router::new().nest(
        "/kodrik",
        super::remote::routes(settings, auth.clone(), Some(service.clone())),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let url = format!("ws://{address}/kodrik/api/remote/environment");
    assert!(connect_async(&url).await.is_err());
    let mut request = url.clone().into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    request
        .headers_mut()
        .insert("Origin", "https://wrong.invalid".parse().unwrap());
    assert!(connect_async(request).await.is_err());
    let mut request = url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    let (mut socket, _) = connect_async(request).await.unwrap();
    socket
        .send(Message::Binary(
            encode_frame(&ClientFrame::Hello {
                session_id: session.clone(),
                manifest: EnvironmentManifest {
                    label: "fixture".into(),
                    project_root: "/project".into(),
                    tools: vec![ToolDescriptor {
                        id: "exec_command".into(),
                        kind: ToolKind::Exec,
                        description: "shell".into(),
                        parameters: serde_json::json!({"type":"object"}),
                        deferred: false,
                    }],
                    skills: vec![],
                    project_instructions: None,
                },
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    let frame = socket.next().await.unwrap().unwrap();
    let ServerFrame::Welcome {
        environment: key,
        heartbeat_seconds,
        lease_seconds,
        ..
    } = decode_frame(frame.into_data().as_ref()).unwrap()
    else {
        panic!()
    };
    assert_eq!((heartbeat_seconds, lease_seconds), (5, 15));
    assert!(service.status(&session).await.is_some());
    // A real unreplayable checkpoint is mandatory before transport can dispatch.
    let call = crate::agent::message::ToolCall {
        call_id: "local-call".into(),
        name: "local_exec".into(),
        arguments: serde_json::json!({"cmd":"echo once >> effect"}),
    };
    let checkpoint_store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
    let mut history = checkpoint_store.load(&session).await.unwrap().unwrap();
    history.push(crate::agent::message::Message::ToolCall(call.clone()));
    let mut cp = crate::agent::run_state::Checkpoint::new(&checkpoint_store, &session, &history);
    cp.record.pending_calls.push(call.clone());
    cp.save(
        &history,
        crate::agent::run_state::RunPhase::ExecutingTool {
            call_id: call.call_id.clone(),
            replay: crate::agent::tools::ReplayPolicy::Never,
        },
        Some(crate::agent::run_state::ToolExecution {
            call: call.clone(),
            replay: crate::agent::tools::ReplayPolicy::Never,
            result: None,
        }),
    )
    .await
    .unwrap();
    let invocation = Invocation {
        environment: key.clone(),
        session_id: session.clone(),
        run_id: cp.record.id.clone(),
        call_id: call.call_id.clone(),
        tool_id: "exec_command".into(),
        arguments: call.arguments,
        deadline: chrono::Utc::now() + chrono::Duration::seconds(30),
    };
    let running = tokio::spawn({
        let service = service.clone();
        let invocation = invocation.clone();
        async move { service.invoke(invocation).await }
    });
    let wire = socket.next().await.unwrap().unwrap();
    let ServerFrame::Invoke {
        invocation: received,
    } = decode_frame(wire.into_data().as_ref()).unwrap()
    else {
        panic!()
    };
    assert_eq!(received.call_id, "local-call");
    let mut receipt = OperationReceipt {
        invocation: received,
        arguments_hash: arguments_hash(&invocation.arguments).unwrap(),
        state: OperationState::Completed,
        result: Some(serde_json::json!({"stdout":"once","large":"x".repeat(300_000)}).into()),
    };
    let bytes = serde_json::to_vec(&receipt.result.take().unwrap()).unwrap();
    let count = bytes.chunks(48 * 1024).len();
    for (sequence, data) in bytes.chunks(48 * 1024).enumerate() {
        socket
            .send(Message::Binary(
                encode_frame(&ClientFrame::ResultChunk {
                    environment: key.clone(),
                    run_id: invocation.run_id.clone(),
                    call_id: invocation.call_id.clone(),
                    arguments_hash: receipt.arguments_hash.clone(),
                    sequence: sequence as u32,
                    final_chunk: sequence + 1 == count,
                    data: data.to_vec(),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
    }
    socket
        .send(Message::Binary(
            encode_frame(&ClientFrame::Receipt { receipt })
                .unwrap()
                .into(),
        ))
        .await
        .unwrap();
    let ack = socket.next().await.unwrap().unwrap();
    assert!(matches!(
        decode_frame::<ServerFrame>(ack.into_data().as_ref()).unwrap(),
        ServerFrame::ReceiptAck { .. }
    ));
    assert_eq!(
        running.await.unwrap().unwrap().result.unwrap().output["stdout"],
        "once"
    );
    // A retry returns saved evidence without another frame or second effect.
    assert_eq!(
        service.invoke(invocation).await.unwrap().state,
        OperationState::Completed
    );
    auth.revoke_device(device_id).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(Ok(message)) = socket.next().await {
            if message.is_close() {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(service.status(&session).await.is_none());
    server.abort();
}

struct Fixture {
    _dir: tempfile::TempDir,
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    service: Arc<EnvironmentService>,
    session: SessionId,
    key: EnvironmentKey,
    server: tokio::task::JoinHandle<()>,
    device: Uuid,
    config: crate::remote::config::RemoteConfig,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
    let session = SessionId::new(Uuid::new_v4().to_string());
    store.save(&Session::new(session.clone())).await.unwrap();
    let auth = AuthStore::new(dir.path());
    let now = chrono::Utc::now().timestamp();
    let pair = auth.start_device_pairing("fixture", now).unwrap();
    auth.approve_device_pairing(pair.request_id, &pair.code, &Identity::Owner, now)
        .unwrap();
    let PairingPoll::Authorized { token, device_id } = auth
        .poll_device_pairing(pair.request_id, &pair.polling_secret, now)
        .unwrap()
    else {
        panic!()
    };
    let service = Arc::new(EnvironmentService::new(store, auth.clone(), true));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let credential = dir.path().join("device");
    std::fs::write(
        &credential,
        serde_json::to_vec(&serde_json::json!({"device_id":device_id,"token":token})).unwrap(),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o600)).unwrap();
    let config = crate::remote::config::RemoteConfig {
        server: format!("http://{address}/").parse().unwrap(),
        credential_file: credential,
        label: "fixture".into(),
        allow_shell: true,
        skill_roots: vec![],
        mcp: Default::default(),
        optional_mcp: vec![],
    };
    let settings = Settings {
        trusted_proxies: vec![],
        listen: address,
        base_url: format!("http://{address}/").parse().unwrap(),
    };
    let app = super::remote::routes(settings, auth, Some(service.clone()));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut request = format!("ws://{address}/api/remote/environment")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    let (mut socket, _) = connect_async(request).await.unwrap();
    socket
        .send(Message::Binary(
            encode_frame(&ClientFrame::Hello {
                session_id: session.clone(),
                manifest: EnvironmentManifest {
                    label: "fixture".into(),
                    project_root: "/project".into(),
                    tools: vec![ToolDescriptor {
                        id: "exec_command".into(),
                        kind: ToolKind::Exec,
                        description: "shell".into(),
                        parameters: serde_json::json!({"type":"object"}),
                        deferred: false,
                    }],
                    skills: vec![],
                    project_instructions: None,
                },
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    let ServerFrame::Welcome {
        environment: key, ..
    } = decode_frame(&socket.next().await.unwrap().unwrap().into_data()).unwrap()
    else {
        panic!()
    };
    Fixture {
        _dir: dir,
        socket,
        service,
        session,
        key,
        server,
        device: device_id,
        config,
    }
}

async fn pending_call(fixture: &Fixture) -> Invocation {
    pending_with_args(fixture, serde_json::json!({"cmd":"echo once"})).await
}
async fn pending_with_args(fixture: &Fixture, arguments: serde_json::Value) -> Invocation {
    use crate::agent::{
        message::{Message, ToolCall},
        run_state::{Checkpoint, RunPhase, ToolExecution},
        tools::ReplayPolicy,
    };
    let store = SqliteSessionStore::open(fixture._dir.path().join("sessions")).unwrap();
    let mut history = store.load(&fixture.session).await.unwrap().unwrap();
    let call = ToolCall {
        call_id: Uuid::new_v4().to_string(),
        name: "local_exec".into(),
        arguments,
    };
    history.push(Message::ToolCall(call.clone()));
    let mut cp = Checkpoint::new(&store, &fixture.session, &history);
    if let Some(run) = store
        .unfinished_runs()
        .await
        .unwrap()
        .into_iter()
        .find(|run| run.session_id == fixture.session.as_str())
    {
        cp.record = run;
    }
    cp.record.pending_calls.push(call.clone());
    cp.save(
        &history,
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
    Invocation {
        environment: fixture.key.clone(),
        session_id: fixture.session.clone(),
        run_id: cp.record.id,
        call_id: call.call_id,
        tool_id: "exec_command".into(),
        arguments: call.arguments,
        deadline: chrono::Utc::now() + chrono::Duration::seconds(30),
    }
}

#[tokio::test]
async fn real_client_reconciles_old_result_on_fresh_lease_without_replaying_effect() {
    use crate::remote::{client::RemoteClient, local::LocalEnvironment};
    let fixture = fixture().await;
    let call = pending_with_args(&fixture, serde_json::json!({"cmd":"echo once >> effect"})).await;
    let store = SqliteSessionStore::open(fixture._dir.path().join("sessions")).unwrap();
    store
        .remote_intent(fixture.device, call.clone())
        .await
        .unwrap();
    assert!(
        store
            .remote_dispatch(fixture.device, call.clone())
            .await
            .unwrap()
    );
    let env = LocalEnvironment::open(&fixture.config, fixture._dir.path())
        .await
        .unwrap();
    env.attach(fixture.device, fixture.key.clone(), fixture.session.clone())
        .await
        .unwrap();
    assert_eq!(
        env.execute(call.clone()).await.unwrap().state,
        OperationState::Completed
    );
    env.close().await.unwrap();
    drop(env);
    fixture.service.disconnect(&fixture.key).await.unwrap();
    let env = LocalEnvironment::open(&fixture.config, fixture._dir.path())
        .await
        .unwrap();
    let mut client = RemoteClient::connect(&fixture.config).await.unwrap();
    let new_key = client
        .attach(fixture.session.clone(), env.manifest().unwrap())
        .await
        .unwrap();
    assert_ne!(new_key, fixture.key);
    let serving = tokio::spawn({
        let env = env.clone();
        async move { client.serve_local(env).await }
    });
    let reconciled = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if store
                .remote_operation(&call.run_id, &call.call_id)
                .await
                .unwrap()
                .unwrap()
                .state
                == OperationState::Completed
                && env.pending_receipts().unwrap().is_empty()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    let mut next = pending_with_args(
        &fixture,
        serde_json::json!({"cmd":"echo fresh >> effect; printf real-client"}),
    )
    .await;
    next.environment = new_key;
    let fresh = fixture.service.invoke(next).await.unwrap();
    assert_eq!(fresh.result.unwrap().output["stdout"], "real-client");
    fixture.service.close().await;
    let _ = serving.await.unwrap();
    assert!(
        reconciled.is_ok(),
        "fresh client never delivered saved old-key evidence"
    );
    assert_eq!(
        std::fs::read_to_string(fixture._dir.path().join("effect")).unwrap(),
        "once\nfresh\n"
    );
}

#[tokio::test]
async fn duplicate_chunk_disconnects_and_preserves_unknown_outcome() {
    let mut fixture = fixture().await;
    let call = pending_call(&fixture).await;
    let running = tokio::spawn({
        let service = fixture.service.clone();
        let call = call.clone();
        async move { service.invoke(call).await }
    });
    let frame = fixture.socket.next().await.unwrap().unwrap();
    assert!(matches!(
        decode_frame::<ServerFrame>(&frame.into_data()).unwrap(),
        ServerFrame::Invoke { .. }
    ));
    let chunk = ClientFrame::ResultChunk {
        environment: call.environment.clone(),
        run_id: call.run_id.clone(),
        call_id: call.call_id.clone(),
        arguments_hash: arguments_hash(&call.arguments).unwrap(),
        sequence: 0,
        final_chunk: false,
        data: b"{".to_vec(),
    };
    for _ in 0..2 {
        fixture
            .socket
            .send(Message::Binary(encode_frame(&chunk).unwrap().into()))
            .await
            .unwrap();
    }
    disconnected(&mut fixture).await;
    assert_eq!(
        running.await.unwrap().unwrap().state,
        OperationState::Unknown
    );
}

#[tokio::test]
async fn result_chunk_does_not_fabricate_running_evidence_before_receipt() {
    let mut fixture = fixture().await;
    let call = pending_call(&fixture).await;
    let running = tokio::spawn({
        let service = fixture.service.clone();
        let call = call.clone();
        async move { service.invoke(call).await }
    });
    let _ = fixture.socket.next().await.unwrap().unwrap();
    fixture
        .socket
        .send(Message::Binary(
            encode_frame(&ClientFrame::ResultChunk {
                environment: call.environment.clone(),
                run_id: call.run_id.clone(),
                call_id: call.call_id.clone(),
                arguments_hash: arguments_hash(&call.arguments).unwrap(),
                sequence: 0,
                final_chunk: false,
                data: b"{".to_vec(),
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    fixture
        .socket
        .send(Message::Binary(
            encode_frame(&ClientFrame::Heartbeat {
                environment: fixture.key.clone(),
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    let _ = fixture.socket.next().await.unwrap().unwrap();
    let saved = fixture
        .service
        .operation(&call.run_id, &call.call_id)
        .await
        .unwrap()
        .unwrap();
    fixture.service.close().await;
    let _ = running.await;
    assert_eq!(
        saved.state,
        OperationState::Unknown,
        "transport invented running receipt"
    );
}

#[tokio::test]
async fn one_call_deadline_does_not_downgrade_another_running_call() {
    let mut fixture = fixture().await;
    let mut first = pending_call(&fixture).await;
    first.deadline = chrono::Utc::now() + chrono::Duration::seconds(2);
    let second = pending_call(&fixture).await;
    let mut tasks = Vec::new();
    for call in [&first, &second] {
        tasks.push(tokio::spawn({
            let service = fixture.service.clone();
            let call = call.clone();
            async move { service.invoke(call).await }
        }));
        let frame = fixture.socket.next().await.unwrap().unwrap();
        let frame = decode_frame::<ServerFrame>(&frame.into_data()).unwrap();
        if !matches!(frame, ServerFrame::Invoke { .. }) {
            let failed = tasks.pop().unwrap().await.unwrap();
            panic!("expected invoke, got {frame:?}; invoke returned {failed:?}");
        }
        fixture
            .socket
            .send(Message::Binary(
                encode_frame(&ClientFrame::Receipt {
                    receipt: OperationReceipt {
                        invocation: call.clone(),
                        arguments_hash: arguments_hash(&call.arguments).unwrap(),
                        state: OperationState::Running,
                        result: None,
                    },
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
    }
    fixture
        .socket
        .send(Message::Binary(
            encode_frame(&ClientFrame::Heartbeat {
                environment: fixture.key.clone(),
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    let _ = fixture.socket.next().await.unwrap().unwrap();
    let first_task = tasks.remove(0);
    assert_eq!(
        first_task.await.unwrap().unwrap().state,
        OperationState::Unknown
    );
    let second_saved = fixture
        .service
        .operation(&second.run_id, &second.call_id)
        .await
        .unwrap()
        .unwrap();
    fixture.service.close().await;
    let _ = tasks.remove(0).await;
    assert_eq!(
        second_saved.state,
        OperationState::Running,
        "one timeout downgraded unrelated evidence"
    );
}

#[tokio::test]
async fn final_old_key_evidence_cannot_grant_execution_or_accept_unsaved_calls() {
    let fixture = fixture().await;
    let call = pending_call(&fixture).await;
    let store = SqliteSessionStore::open(fixture._dir.path().join("sessions")).unwrap();
    store
        .remote_intent(fixture.device, call.clone())
        .await
        .unwrap();
    assert!(
        store
            .remote_dispatch(fixture.device, call.clone())
            .await
            .unwrap()
    );
    fixture.service.disconnect(&fixture.key).await.unwrap();
    let receipt = OperationReceipt {
        invocation: call.clone(),
        arguments_hash: arguments_hash(&call.arguments).unwrap(),
        state: OperationState::Completed,
        result: Some(serde_json::json!({"stdout":"late"}).into()),
    };
    fixture
        .service
        .receipt(fixture.device, receipt.clone())
        .await
        .unwrap();
    assert!(fixture.service.status(&fixture.session).await.is_none());
    assert!(fixture.service.invoke(call).await.is_err());
    let mut different = receipt;
    different.invocation.call_id = "unsaved".into();
    assert!(
        fixture
            .service
            .receipt(fixture.device, different)
            .await
            .is_err()
    );
}
async fn disconnected(fixture: &mut Fixture) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(Ok(frame)) = fixture.socket.next().await {
            if frame.is_close() {
                break;
            }
        }
    })
    .await
    .expect("remote socket retained revoked authority");
    assert!(fixture.service.status(&fixture.session).await.is_none());
}
#[tokio::test]
async fn malformed_and_oversized_frames_revoke_the_binding() {
    for bytes in [b"{bad-json".to_vec(), vec![b' '; MAX_FRAME_BYTES + 1]] {
        let mut fixture = fixture().await;
        fixture
            .socket
            .send(Message::Binary(bytes.into()))
            .await
            .unwrap();
        disconnected(&mut fixture).await;
    }
}
#[tokio::test]
async fn missing_heartbeat_and_gateway_close_revoke_live_socket() {
    let mut fixture = fixture().await;
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(14)).await;
    assert!(fixture.service.status(&fixture.session).await.is_some());
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    disconnected(&mut fixture).await;
    tokio::time::resume();
    let mut fixture = crate::web::remote_tests::fixture().await;
    fixture.service.close().await;
    disconnected(&mut fixture).await;
}
#[tokio::test]
async fn heartbeat_renews_only_live_socket_not_expired_generation() {
    let mut fixture = fixture().await;
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(14)).await;
    fixture
        .socket
        .send(Message::Binary(
            encode_frame(&ClientFrame::Heartbeat {
                environment: fixture.key.clone(),
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    let ack = fixture.socket.next().await.unwrap().unwrap();
    assert!(matches!(
        decode_frame::<ServerFrame>(&ack.into_data()).unwrap(),
        ServerFrame::HeartbeatAck { .. }
    ));
    tokio::time::advance(std::time::Duration::from_secs(14)).await;
    assert!(fixture.service.status(&fixture.session).await.is_some());
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    disconnected(&mut fixture).await;
}
