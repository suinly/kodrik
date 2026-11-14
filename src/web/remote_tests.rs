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
                    warnings: vec![],
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
    assert_eq!(
        service.status(&session).await.unwrap().state,
        "disconnected"
    );
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
                    warnings: vec![],
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

#[tokio::test]
async fn attachment_rejection_reports_reason_instead_of_resetting_socket() {
    let fixture = fixture().await;
    let mut client = crate::remote::client::RemoteClient::connect(&fixture.config)
        .await
        .unwrap();
    let error = client
        .attach(
            SessionId::new(Uuid::new_v4().to_string()),
            EnvironmentManifest {
                label: "fixture".into(),
                project_root: "/project".into(),
                tools: vec![ToolDescriptor {
                    id: "exec".into(),
                    kind: ToolKind::Exec,
                    description: "shell".into(),
                    parameters: serde_json::json!({"type":"object"}),
                    deferred: false,
                }],
                skills: vec![],
                project_instructions: None,
                warnings: vec![],
            },
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("Conversation not found"),
        "{error:#}"
    );
}

#[tokio::test]
async fn same_device_websockets_keep_distinct_sessions_isolated() {
    use std::time::Duration;
    type Socket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    async fn send(socket: &mut Socket, frame: ClientFrame) {
        socket
            .send(Message::Binary(encode_frame(&frame).unwrap().into()))
            .await
            .unwrap();
    }
    async fn heartbeat(socket: &mut Socket, key: &EnvironmentKey) {
        send(
            socket,
            ClientFrame::Heartbeat {
                environment: key.clone(),
            },
        )
        .await;
        let reply = socket.next().await.unwrap().unwrap();
        let ServerFrame::HeartbeatAck { environment } = decode_frame(&reply.into_data()).unwrap()
        else {
            panic!("expected heartbeat acknowledgement for live binding")
        };
        assert_eq!(&environment, key);
    }

    tokio::time::timeout(Duration::from_secs(10), async {
        let mut fixture = fixture().await;
        let store = SqliteSessionStore::open(fixture._dir.path().join("sessions")).unwrap();
        let second_session = SessionId::new(Uuid::new_v4().to_string());
        store
            .save(&Session::new(second_session.clone()))
            .await
            .unwrap();
        let credential =
            crate::remote::client::Credential::load(&fixture.config.credential_file).unwrap();
        assert_eq!(credential.device_id, fixture.device);
        let url = crate::remote::client::endpoint(&fixture.config.server).unwrap();
        let request = || {
            let mut request = url.as_str().into_client_request().unwrap();
            request.headers_mut().insert(
                "Authorization",
                format!("Bearer {}", credential.token).parse().unwrap(),
            );
            request
        };
        let manifest = EnvironmentManifest {
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
            warnings: vec![],
        };
        let (mut second, _) = connect_async(request()).await.unwrap();
        send(
            &mut second,
            ClientFrame::Hello {
                session_id: second_session.clone(),
                manifest: manifest.clone(),
            },
        )
        .await;
        let reply = second.next().await.unwrap().unwrap();
        let ServerFrame::Welcome {
            environment: second_key,
            ..
        } = decode_frame(&reply.into_data()).unwrap()
        else {
            panic!("second session on the same device must attach")
        };
        assert_ne!(fixture.key.connection_id, second_key.connection_id);
        assert_ne!(fixture.key.generation, second_key.generation);

        // Both sockets remain usable while the other binding is live.
        tokio::join!(
            heartbeat(&mut fixture.socket, &fixture.key),
            heartbeat(&mut second, &second_key),
        );

        // A competing binding for the first session must not replace its owner.
        let (mut duplicate, _) = connect_async(request()).await.unwrap();
        send(
            &mut duplicate,
            ClientFrame::Hello {
                session_id: fixture.session.clone(),
                manifest,
            },
        )
        .await;
        let rejected = duplicate.next().await.unwrap().unwrap();
        let ServerFrame::Error { message } = decode_frame(&rejected.into_data()).unwrap() else {
            panic!("duplicate must be rejected with a reason");
        };
        assert!(message.contains("already has an environment"));
        tokio::join!(
            heartbeat(&mut fixture.socket, &fixture.key),
            heartbeat(&mut second, &second_key),
        );
        for (session, key) in [
            (&fixture.session, &fixture.key),
            (&second_session, &second_key),
        ] {
            let status = fixture.service.status(session).await.unwrap();
            assert_eq!(status.state, "connected");
            assert_eq!(status.environment.as_ref(), Some(key));
        }

        send(
            &mut second,
            ClientFrame::Close {
                environment: second_key,
            },
        )
        .await;
        while let Some(Ok(message)) = second.next().await {
            if message.is_close() {
                break;
            }
        }
        assert_eq!(
            fixture.service.status(&second_session).await.unwrap().state,
            "disconnected"
        );
        heartbeat(&mut fixture.socket, &fixture.key).await;
        assert_eq!(
            fixture
                .service
                .status(&fixture.session)
                .await
                .unwrap()
                .state,
            "connected"
        );
    })
    .await
    .expect("concurrent WebSocket isolation test timed out");
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

struct ProxyModel;
struct SlowProxyModel;
#[async_trait::async_trait]
impl crate::agent::llm::LlmProvider for SlowProxyModel {
    async fn generate(
        &self,
        context: &crate::agent::LlmContext<'_>,
        tools: &[&dyn crate::agent::tools::Tool],
        progress: Option<&tokio::sync::mpsc::Sender<crate::agent::AgentEvent>>,
    ) -> anyhow::Result<(Vec<crate::agent::message::Message>, Option<u64>)> {
        let (mut messages, usage) = ProxyModel.generate(context, tools, progress).await?;
        for message in &mut messages {
            if let crate::agent::message::Message::ToolCall(call) = message {
                call.arguments = serde_json::json!({"cmd":"printf '%s' $$ > started; sleep 30; printf escaped >> effect"});
            }
        }
        Ok((messages, usage))
    }
}
#[async_trait::async_trait]
impl crate::agent::llm::LlmProvider for ProxyModel {
    async fn generate(
        &self,
        context: &crate::agent::LlmContext<'_>,
        tools: &[&dyn crate::agent::tools::Tool],
        _: Option<&tokio::sync::mpsc::Sender<crate::agent::AgentEvent>>,
    ) -> anyhow::Result<(Vec<crate::agent::message::Message>, Option<u64>)> {
        use crate::agent::message::{Message, ToolCall};
        if context
            .messages
            .iter()
            .any(|message| matches!(message, Message::ToolResult(_)))
        {
            return Ok((
                vec![Message::Assistant(crate::agent::AssistantMessage {
                    content: "done".into(),
                })],
                None,
            ));
        }
        let name = tools
            .iter()
            .find(|tool| {
                tool.audit_target()
                    .is_some_and(|target| target.tool_kind == ToolKind::Exec)
            })
            .expect("no scoped proxy")
            .name();
        Ok((
            vec![Message::ToolCall(ToolCall {
                call_id: "proxy-effect".into(),
                name,
                arguments: serde_json::json!({"cmd":"printf proxy >> effect; printf result"}),
            })],
            None,
        ))
    }
}

#[tokio::test]
async fn durable_agent_proxy_executes_on_laptop_and_preserves_normalized_provenance() {
    use crate::{
        agent::{
            Agent, ContextBuilder,
            message::Message,
            run_state::{Checkpoint, RunPhase},
            tools::ToolRegistry,
        },
        remote::{client::RemoteClient, local::LocalEnvironment, tools::CURRENT_ENVIRONMENTS},
    };
    let fixture = fixture().await;
    fixture.service.disconnect(&fixture.key).await.unwrap();
    let env = LocalEnvironment::open(&fixture.config, fixture._dir.path())
        .await
        .unwrap();
    let mut client = RemoteClient::connect(&fixture.config).await.unwrap();
    let key = client
        .attach(fixture.session.clone(), env.manifest().unwrap())
        .await
        .unwrap();
    let serving = tokio::spawn(async move { client.serve_local(env).await });
    let store = SqliteSessionStore::open(fixture._dir.path().join("sessions")).unwrap();
    let mut session = store.load(&fixture.session).await.unwrap().unwrap();
    let mut cp = Checkpoint::new(&store, &fixture.session, &session);
    cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
    let agent = Agent::new(
        Arc::new(ProxyModel),
        ToolRegistry::default(),
        ContextBuilder::new("fixture"),
    );
    let result = CURRENT_ENVIRONMENTS
        .scope(
            Some(fixture.service.clone()),
            agent.run_durable(
                &mut session,
                crate::agent::UserMessage {
                    browser_client_id: None,
                    browser_context: None,
                    content: "run once on laptop".into(),
                    images: vec![],
                    files: vec![],
                },
                None,
                None,
                Some(&mut cp),
            ),
        )
        .await;
    fixture.service.close().await;
    let _ = serving.await.unwrap();
    result.unwrap();
    assert_eq!(
        std::fs::read_to_string(fixture._dir.path().join("effect")).unwrap(),
        "proxy"
    );
    let evidence = store
        .remote_operation(&cp.record.id, "proxy-effect")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(evidence.invocation.environment, key);
    assert_eq!(
        evidence.invocation.arguments["workdir"],
        fixture
            ._dir
            .path()
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
    );
    assert_eq!(evidence.state, OperationState::Completed);
    assert!(session.messages().iter().any(|message|matches!(message,Message::ToolResult(result) if result.content.output["stdout"]=="result")));
}

#[tokio::test]
async fn child_agent_executes_only_with_parent_lease() {
    use crate::{
        agent::{
            Agent, ContextBuilder,
            subagents::{ParentContext, Subagents},
            tools::ToolRegistry,
        },
        remote::{client::RemoteClient, local::LocalEnvironment, tools::CURRENT_SCOPE},
    };
    let fixture = fixture().await;
    fixture.service.disconnect(&fixture.key).await.unwrap();
    let env = LocalEnvironment::open(&fixture.config, fixture._dir.path())
        .await
        .unwrap();
    let mut client = RemoteClient::connect(&fixture.config).await.unwrap();
    client
        .attach(fixture.session.clone(), env.manifest().unwrap())
        .await
        .unwrap();
    let serving = tokio::spawn(async move { client.serve_local(env).await });
    let store = SqliteSessionStore::open(fixture._dir.path().join("sessions")).unwrap();
    let manager = Subagents::new(
        Arc::new(Agent::new(
            Arc::new(ProxyModel),
            ToolRegistry::default(),
            ContextBuilder::new("fixture"),
        )),
        store.clone(),
    )
    .await
    .unwrap();
    let (_life, finished) = tokio::sync::watch::channel(false);
    let parent = ParentContext {
        session: store.load(&fixture.session).await.unwrap().unwrap(),
        finished,
    };
    let scope = fixture
        .service
        .step_scope(&fixture.session)
        .await
        .unwrap()
        .unwrap();
    let child = CURRENT_SCOPE
        .scope(
            Some(scope),
            manager.spawn(&parent, "run laptop effect".into()),
        )
        .await
        .unwrap();
    let done = manager
        .wait(&parent.session, &child.id, 5000)
        .await
        .unwrap();
    fixture.service.close().await;
    let _ = serving.await.unwrap();
    assert_eq!(
        format!("{:?}", done.status),
        "Completed",
        "{:?}",
        done.error
    );
    assert_eq!(
        std::fs::read_to_string(fixture._dir.path().join("effect")).unwrap(),
        "proxy"
    );
}

#[tokio::test]
async fn revoking_parent_lease_stops_child_laptop_process_before_later_effect() {
    use crate::{
        agent::{
            Agent, ContextBuilder,
            subagents::{ParentContext, Subagents},
            tools::ToolRegistry,
        },
        remote::{client::RemoteClient, local::LocalEnvironment, tools::CURRENT_SCOPE},
    };
    let fixture = fixture().await;
    fixture.service.disconnect(&fixture.key).await.unwrap();
    let env = LocalEnvironment::open(&fixture.config, fixture._dir.path())
        .await
        .unwrap();
    let mut client = RemoteClient::connect(&fixture.config).await.unwrap();
    let key = client
        .attach(fixture.session.clone(), env.manifest().unwrap())
        .await
        .unwrap();
    let serving = tokio::spawn(async move { client.serve_local(env).await });
    let store = SqliteSessionStore::open(fixture._dir.path().join("sessions")).unwrap();
    let manager = Subagents::new(
        Arc::new(Agent::new(
            Arc::new(SlowProxyModel),
            ToolRegistry::default(),
            ContextBuilder::new("fixture"),
        )),
        store.clone(),
    )
    .await
    .unwrap();
    let (_life, finished) = tokio::sync::watch::channel(false);
    let parent = ParentContext {
        session: store.load(&fixture.session).await.unwrap().unwrap(),
        finished,
    };
    let scope = fixture
        .service
        .step_scope(&fixture.session)
        .await
        .unwrap()
        .unwrap();
    let child = CURRENT_SCOPE
        .scope(
            Some(scope),
            manager.spawn(&parent, "run laptop effect".into()),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !fixture._dir.path().join("started").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    fixture.service.revoke(&key, "test").await.unwrap();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(8), serving)
        .await
        .unwrap()
        .unwrap();
    // Revocation closes the socket; serve_local reports that loss after owned cleanup.
    assert!(outcome.is_err());
    let _ = manager
        .wait(&parent.session, &child.id, 5000)
        .await
        .unwrap();
    assert!(!fixture._dir.path().join("effect").exists());
    let pid: i32 = std::fs::read_to_string(fixture._dir.path().join("started"))
        .unwrap()
        .parse()
        .unwrap();
    // SAFETY: signal zero observes the fixture shell, without signalling it.
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "child shell survived revoke"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
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
    assert_eq!(
        fixture
            .service
            .status(&fixture.session)
            .await
            .unwrap()
            .state,
        "disconnected"
    );
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
    assert_eq!(
        fixture
            .service
            .status(&fixture.session)
            .await
            .unwrap()
            .state,
        "disconnected"
    );
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
