use super::{protocol::*, transport::ResultAssembly};
use crate::agent::session::SessionId;
use serde_json::json;
use uuid::Uuid;

fn call() -> Invocation {
    Invocation {
        environment: EnvironmentKey {
            connection_id: Uuid::new_v4(),
            generation: Uuid::new_v4(),
        },
        session_id: SessionId::new(Uuid::new_v4().to_string()),
        run_id: "run".into(),
        call_id: "call".into(),
        tool_id: "exec".into(),
        arguments: json!({}),
        deadline: chrono::Utc::now() + chrono::Duration::seconds(30),
    }
}
fn chunk(call: &Invocation, seq: u32, done: bool, data: Vec<u8>) -> ClientFrame {
    ClientFrame::ResultChunk {
        environment: call.environment.clone(),
        run_id: call.run_id.clone(),
        call_id: call.call_id.clone(),
        arguments_hash: arguments_hash(&call.arguments).unwrap(),
        sequence: seq,
        final_chunk: done,
        data,
    }
}
#[test]
fn result_chunks_require_exact_call_hash_sequence_and_bound() {
    let call = call();
    let mut assembly = ResultAssembly::new(call.clone()).unwrap();
    assert!(
        assembly
            .push(chunk(&call, 0, false, b"{".to_vec()))
            .unwrap()
            .is_none()
    );
    assert!(
        assembly
            .push(chunk(&call, 0, false, b"{".to_vec()))
            .is_err()
    );
    let mut changed = call.clone();
    changed.environment.generation = Uuid::new_v4();
    assert!(
        assembly
            .push(chunk(&changed, 1, true, b"}".to_vec()))
            .is_err()
    );
    let mut changed = call.clone();
    changed.arguments = json!({"wrong":true});
    assert!(
        assembly
            .push(chunk(&changed, 1, true, b"}".to_vec()))
            .is_err()
    );
    let mut assembly = ResultAssembly::new(call.clone()).unwrap();
    for seq in 0..256 {
        assert!(
            assembly
                .push(chunk(&call, seq, false, vec![b' '; MAX_CHUNK_BYTES]))
                .unwrap()
                .is_none()
        );
    }
    assert!(assembly.push(chunk(&call, 256, true, vec![b' '])).is_err());
}
#[test]
fn chunked_tool_output_is_decoded_once_and_closed_to_more_data() {
    let call = call();
    let mut assembly = ResultAssembly::new(call.clone()).unwrap();
    assert!(
        assembly
            .push(chunk(
                &call,
                0,
                false,
                b"{\"output\":{\"stdout\":\"".to_vec()
            ))
            .unwrap()
            .is_none()
    );
    let output = assembly
        .push(chunk(&call, 1, true, br#"ok"},"images":[]}"#.to_vec()))
        .unwrap()
        .unwrap();
    assert_eq!(output.output["stdout"], "ok");
    assert!(assembly.push(chunk(&call, 2, true, vec![])).is_err());
}
#[test]
fn insecure_or_credential_bearing_server_urls_are_rejected() {
    for url in [
        "http://example.com/",
        "https://user:secret@example.com/",
        "https://example.com/?token=x",
        "https://example.com/#x",
    ] {
        assert!(super::client::endpoint(&url.parse().unwrap()).is_err());
    }
    assert_eq!(
        super::client::endpoint(&"https://example.com/kodrik/".parse().unwrap())
            .unwrap()
            .as_str(),
        "wss://example.com/kodrik/api/remote/environment"
    );
    assert_eq!(
        super::client::endpoint(&"http://127.0.0.1:1234/".parse().unwrap())
            .unwrap()
            .as_str(),
        "ws://127.0.0.1:1234/api/remote/environment"
    );
}

#[test]
fn suspend_gap_invalidates_live_authority_even_before_fifteen_second_expiry() {
    use super::transport::LeaseTimer;
    use std::time::Duration;
    let mut timer = LeaseTimer::new(Duration::ZERO, Duration::ZERO);
    assert!(timer.observe(Duration::from_secs(5), Duration::from_secs(5)));
    assert!(!timer.expired());
    timer.renew();
    assert!(timer.observe(Duration::from_secs(19), Duration::from_secs(19)));
    assert!(!timer.expired());
    assert!(!timer.observe(Duration::from_secs(20), Duration::from_secs(20)));
    assert!(timer.expired());
    timer.renew();
    assert!(timer.expired(), "expired authority cannot be revived");
    let mut timer = LeaseTimer::new(Duration::ZERO, Duration::ZERO);
    assert!(!timer.observe(Duration::from_millis(100), Duration::from_secs(3)));
    timer.renew();
    assert!(timer.expired(), "resume requires a fresh handshake");
}

#[tokio::test]
async fn client_runs_local_effect_once_and_closes_environment_when_socket_drops() {
    use super::{client::RemoteClient, config::RemoteConfig, local::LocalEnvironment};
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::{accept_async, tungstenite::Message};
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let credential = dir.path().join("device");
    std::fs::write(
        &credential,
        serde_json::to_vec(&json!({"device_id":Uuid::new_v4(),"token":"fixture-token"})).unwrap(),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o600)).unwrap();
    let config = RemoteConfig {
        server: format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
        credential_file: credential,
        label: "fixture".into(),
        allow_shell: true,
        skill_roots: vec![],
        mcp: Default::default(),
        optional_mcp: vec![],
    };
    let environment = LocalEnvironment::open(&config, dir.path()).await.unwrap();
    let call = Invocation {
        tool_id: "exec_command".into(),
        arguments: json!({"cmd":"echo once >> effect"}),
        ..call()
    };
    let key = call.environment.clone();
    let server = tokio::spawn({
        let call = call.clone();
        async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let hello = socket.next().await.unwrap().unwrap();
            assert!(matches!(
                decode_frame::<ClientFrame>(&hello.into_data()).unwrap(),
                ClientFrame::Hello { .. }
            ));
            socket
                .send(Message::Binary(
                    encode_frame(&ServerFrame::Welcome {
                        environment: call.environment.clone(),
                        heartbeat_seconds: 5,
                        lease_seconds: 15,
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            for _ in 0..2 {
                socket
                    .send(Message::Binary(
                        encode_frame(&ServerFrame::Invoke {
                            invocation: call.clone(),
                        })
                        .unwrap()
                        .into(),
                    ))
                    .await
                    .unwrap();
                loop {
                    let frame = socket.next().await.unwrap().unwrap();
                    match decode_frame::<ClientFrame>(&frame.into_data()).unwrap() {
                        ClientFrame::Receipt { receipt } => {
                            assert_eq!(receipt.state, OperationState::Completed);
                            socket
                                .send(Message::Binary(
                                    encode_frame(&ServerFrame::ReceiptAck {
                                        environment: call.environment.clone(),
                                        run_id: call.run_id.clone(),
                                        call_id: call.call_id.clone(),
                                        arguments_hash: receipt.arguments_hash,
                                    })
                                    .unwrap()
                                    .into(),
                                ))
                                .await
                                .unwrap();
                            break;
                        }
                        ClientFrame::Heartbeat { environment } => socket
                            .send(Message::Binary(
                                encode_frame(&ServerFrame::HeartbeatAck { environment })
                                    .unwrap()
                                    .into(),
                            ))
                            .await
                            .unwrap(),
                        _ => panic!("unexpected client frame"),
                    }
                }
            }
        }
    });
    let mut client = RemoteClient::connect(&config).await.unwrap();
    assert_eq!(
        client
            .attach(call.session_id.clone(), environment.manifest().unwrap())
            .await
            .unwrap(),
        key
    );
    let served = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.serve_local(environment.clone()),
    )
    .await
    .unwrap();
    assert!(served.is_err());
    server.await.unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("effect")).unwrap(),
        "once\n"
    );
    assert!(
        environment.execute(call).await.is_err(),
        "socket loss left local authority alive"
    );
}

#[test]
fn bounded_peer_rejects_fifth_call_and_closes_on_cancel_backpressure() {
    use super::transport::Peer;
    let (peer, _reader) = Peer::new();
    let mut pending = Vec::new();
    for index in 0..4 {
        let mut call = call();
        call.call_id = index.to_string();
        pending.push(peer.register(call).unwrap());
    }
    assert!(peer.register(call()).is_err());
    for _ in 0..32 {
        peer.outgoing
            .try_send(ServerFrame::Error {
                message: "fixture".into(),
            })
            .unwrap();
    }
    drop(pending);
    assert!(
        *peer.closed.borrow(),
        "lost cancellation left local calls authorized"
    );
}

#[tokio::test]
async fn aborted_client_future_closes_environment_while_caller_retains_it() {
    use super::{client::RemoteClient, config::RemoteConfig, local::LocalEnvironment};
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::{accept_async, tungstenite::Message};
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let credential = dir.path().join("device");
    std::fs::write(
        &credential,
        serde_json::to_vec(&json!({"device_id":Uuid::new_v4(),"token":"fixture"})).unwrap(),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o600)).unwrap();
    let config = RemoteConfig {
        server: format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
        credential_file: credential,
        label: "fixture".into(),
        allow_shell: true,
        skill_roots: vec![],
        mcp: Default::default(),
        optional_mcp: vec![],
    };
    let environment = LocalEnvironment::open(&config, dir.path()).await.unwrap();
    let mut invocation = call();
    invocation.tool_id = "exec_command".into();
    invocation.arguments = json!({"cmd":"true"});
    let key = invocation.environment.clone();
    let (ready, received) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let _ = socket.next().await;
        socket
            .send(Message::Binary(
                encode_frame(&ServerFrame::Welcome {
                    environment: key,
                    heartbeat_seconds: 5,
                    lease_seconds: 15,
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let _ = socket.next().await;
        let _ = ready.send(());
        std::future::pending::<()>().await;
    });
    let mut client = RemoteClient::connect(&config).await.unwrap();
    client
        .attach(
            invocation.session_id.clone(),
            environment.manifest().unwrap(),
        )
        .await
        .unwrap();
    let task = tokio::spawn({
        let environment = environment.clone();
        async move { client.serve_local(environment).await }
    });
    received.await.unwrap();
    task.abort();
    let _ = task.await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let rejected = environment.execute(invocation).await.is_err();
    environment.close().await.unwrap();
    server.abort();
    assert!(
        rejected,
        "aborting serve_local left the retained environment authorized"
    );
}

#[tokio::test]
async fn client_missing_heartbeat_ack_revokes_local_environment_after_fifteen_seconds() {
    use super::{client::RemoteClient, config::RemoteConfig, local::LocalEnvironment};
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::{accept_async, tungstenite::Message};
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let credential = dir.path().join("device");
    std::fs::write(
        &credential,
        serde_json::to_vec(&json!({"device_id":Uuid::new_v4(),"token":"fixture"})).unwrap(),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o600)).unwrap();
    let config = RemoteConfig {
        server: format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
        credential_file: credential,
        label: "fixture".into(),
        allow_shell: true,
        skill_roots: vec![],
        mcp: Default::default(),
        optional_mcp: vec![],
    };
    let env = LocalEnvironment::open(&config, dir.path()).await.unwrap();
    let call = call();
    let key = call.environment.clone();
    let (ready, received) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        let _ = socket.next().await;
        socket
            .send(Message::Binary(
                encode_frame(&ServerFrame::Welcome {
                    environment: key,
                    heartbeat_seconds: 5,
                    lease_seconds: 15,
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let frame = socket.next().await.unwrap().unwrap();
        assert!(matches!(
            decode_frame::<ClientFrame>(&frame.into_data()).unwrap(),
            ClientFrame::Heartbeat { .. }
        ));
        let _ = ready.send(());
        while socket.next().await.is_some() {} // consume heartbeats without acknowledging
    });
    let mut client = RemoteClient::connect(&config).await.unwrap();
    client
        .attach(call.session_id.clone(), env.manifest().unwrap())
        .await
        .unwrap();
    let task = tokio::spawn({
        let env = env.clone();
        async move { client.serve_local(env).await }
    });
    received.await.unwrap();
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(14)).await;
    tokio::task::yield_now().await;
    assert!(!task.is_finished());
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    let mut task = task;
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), &mut task).await;
    let expired = result.is_ok();
    task.abort();
    env.close().await.unwrap();
    server.abort();
    assert!(expired, "client retained authority without heartbeat ACK");
    assert!(result.unwrap().unwrap().is_err());
}

#[tokio::test]
async fn client_rejects_untrusted_tls_certificate_without_plaintext_fallback() {
    use super::{client::RemoteClient, config::RemoteConfig};
    use tokio_rustls::{
        TlsAcceptor,
        rustls::{
            ServerConfig,
            pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
        },
    };
    let cert =
        CertificateDer::from(include_bytes!("../../tests/fixtures/remote-tls/cert.der").to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        include_bytes!("../../tests/fixtures/remote-tls/key.der").to_vec(),
    ));
    let acceptor = TlsAcceptor::from(std::sync::Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .unwrap(),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let credential = dir.path().join("device");
    std::fs::write(
        &credential,
        serde_json::to_vec(&json!({"device_id":Uuid::new_v4(),"token":"fixture"})).unwrap(),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o600)).unwrap();
    let config = RemoteConfig {
        server: format!("https://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
        credential_file: credential,
        label: "fixture".into(),
        allow_shell: false,
        skill_roots: vec![],
        mcp: Default::default(),
        optional_mcp: vec![],
    };
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        assert!(
            acceptor.accept(stream).await.is_err(),
            "client trusted test certificate"
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "client retried with fallback connection"
        );
    });
    let error = RemoteClient::connect(&config)
        .await
        .err()
        .expect("untrusted TLS accepted");
    assert!(
        format!("{error:#}").contains("certificate"),
        "unexpected failure: {error:#}"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn slow_reader_exhausts_bounded_writer_and_closes_local_authority() {
    use super::{client::RemoteClient, config::RemoteConfig, local::LocalEnvironment};
    use futures::{SinkExt, StreamExt};
    use std::os::{fd::AsRawFd, unix::fs::PermissionsExt};
    use tokio_tungstenite::{accept_async, tungstenite::Message};
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("large"), vec![b'x'; 1024 * 1024]).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let credential = dir.path().join("device");
    std::fs::write(
        &credential,
        serde_json::to_vec(&json!({"device_id":Uuid::new_v4(),"token":"fixture"})).unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o600)).unwrap();
    let config = RemoteConfig {
        server: format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
        credential_file: credential,
        label: "fixture".into(),
        allow_shell: false,
        skill_roots: vec![],
        mcp: Default::default(),
        optional_mcp: vec![],
    };
    let env = LocalEnvironment::open(&config, dir.path()).await.unwrap();
    let mut invocation = call();
    invocation.tool_id = "read_file".into();
    invocation.arguments = json!({"path":"large"});
    let (ready, received) = tokio::sync::oneshot::channel();
    let server = tokio::spawn({
        let invocation = invocation.clone();
        async move {
            let (stream, _) = listener.accept().await.unwrap();
            let size: libc::c_int = 1024;
            // SAFETY: a live socket and properly sized integer buffer configure this fixture only.
            assert_eq!(
                unsafe {
                    libc::setsockopt(
                        stream.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_RCVBUF,
                        (&size as *const libc::c_int).cast(),
                        std::mem::size_of_val(&size) as libc::socklen_t,
                    )
                },
                0
            );
            let mut socket = accept_async(stream).await.unwrap();
            let _ = socket.next().await;
            socket
                .send(Message::Binary(
                    encode_frame(&ServerFrame::Welcome {
                        environment: invocation.environment.clone(),
                        heartbeat_seconds: 5,
                        lease_seconds: 15,
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            for index in 0..4 {
                let mut invocation = invocation.clone();
                invocation.call_id = index.to_string();
                socket
                    .send(Message::Binary(
                        encode_frame(&ServerFrame::Invoke { invocation })
                            .unwrap()
                            .into(),
                    ))
                    .await
                    .unwrap();
            }
            let _ = ready.send(());
            std::future::pending::<()>().await; // socket held open but never drained
        }
    });
    let mut client = RemoteClient::connect(&config).await.unwrap();
    client
        .attach(invocation.session_id.clone(), env.manifest().unwrap())
        .await
        .unwrap();
    let mut task = tokio::spawn({
        let env = env.clone();
        async move { client.serve_local(env).await }
    });
    received.await.unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(6), &mut task).await;
    task.abort();
    server.abort();
    env.close().await.unwrap();
    let error = result
        .expect("writer retained a stalled peer past bounded write timeout")
        .unwrap()
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("deadline has elapsed")
            || format!("{error:#}").contains("no available capacity"),
        "unexpected disconnect: {error:#}"
    );
    assert!(env.execute(invocation).await.is_err());
}
