use super::*;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    response::Response,
};
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::Mutex};
use tokio::sync::mpsc;

struct ServerState {
    replies: Mutex<VecDeque<Response>>,
    requests: mpsc::Sender<(String, String, String, Value)>,
}

struct Fixture {
    client: ChatClient,
    config: RemoteConfig,
    requests: mpsc::Receiver<(String, String, String, Value)>,
    task: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fixture(replies: Vec<Response>) -> Fixture {
    async fn handler(State(state): State<Arc<ServerState>>, request: Request) -> Response {
        let method = request.method().to_string();
        let uri = request.uri().to_string();
        let auth = request
            .headers()
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let body = to_bytes(request.into_body(), 128 * 1024).await.unwrap();
        let body = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).unwrap()
        };
        state
            .requests
            .send((method, uri, auth, body))
            .await
            .unwrap();
        state.replies.lock().unwrap().pop_front().unwrap()
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (tx, requests) = mpsc::channel(16);
    let state = Arc::new(ServerState {
        replies: Mutex::new(replies.into()),
        requests: tx,
    });
    let app = Router::new().fallback(handler).with_state(state);
    let server = format!("http://{}/prefix/", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let credential_file = dir.path().join("device");
    Credential {
        device_id: uuid::Uuid::new_v4(),
        token: "test-private-token".into(),
    }
    .save(&credential_file)
    .unwrap();
    let config = RemoteConfig {
        server,
        credential_file,
        label: "test".into(),
        allow_shell: false,
        skill_roots: vec![],
        mcp: Default::default(),
        optional_mcp: vec![],
    };
    Fixture {
        client: ChatClient::new(&config).unwrap(),
        config,
        requests,
        task,
        _dir: dir,
    }
}

fn snapshot() -> Value {
    json!({"session_id":"session-1","revision":7,"environment_revision":3,"messages":[{"text":"Привет 🌍"}],"previous_cursor":"older","run":null,"versions":{},"environment":null})
}

fn json_response(value: Value) -> Response {
    Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(value.to_string()))
        .unwrap()
}

#[tokio::test]
async fn independent_chat_preserves_prefix_auth_cursor_and_command_id() {
    let mut f = fixture(vec![
        json_response(snapshot()),
        json_response(json!({"sessions":[]})),
        json_response(snapshot()),
        json_response(json!({"ok":true})),
    ])
    .await;
    let session = SessionId::new("session-1");
    assert_eq!(f.client.snapshot(&session).await.unwrap().revision, 7);
    assert_eq!(
        f.client.clone().sessions().await.unwrap(),
        json!({"sessions":[]})
    );
    assert_eq!(
        f.client
            .history(&session, "older+/=&?")
            .await
            .unwrap()
            .previous_cursor
            .as_deref(),
        Some("older")
    );
    let id = uuid::Uuid::parse_str("00000000-0000-4000-8000-000000000001").unwrap();
    assert_eq!(
        f.client
            .command(
                &session,
                ChatCommand::Send {
                    request_id: id,
                    text: "hello".into()
                }
            )
            .await
            .unwrap(),
        json!({"ok":true})
    );
    let requests = [
        f.requests.recv().await.unwrap(),
        f.requests.recv().await.unwrap(),
        f.requests.recv().await.unwrap(),
        f.requests.recv().await.unwrap(),
    ];
    for request in &requests {
        assert_eq!(request.2, "Bearer test-private-token");
        assert!(!request.1.contains("test-private-token"));
    }
    assert_eq!(requests[0].0, "GET");
    assert_eq!(requests[0].1, "/prefix/api/remote/chat/sessions/session-1");
    assert_eq!(requests[1].1, "/prefix/api/remote/chat/sessions");
    assert_eq!(
        requests[2].1,
        "/prefix/api/remote/chat/sessions/session-1?cursor=older%2B%2F%3D%26%3F"
    );
    assert_eq!(requests[3].0, "POST");
    assert_eq!(
        requests[3].1,
        "/prefix/api/remote/chat/sessions/session-1/commands"
    );
    assert_eq!(
        requests[3].3,
        json!({"kind":"send","request_id":"00000000-0000-4000-8000-000000000001","text":"hello"})
    );
}

#[tokio::test]
async fn sessions_page_preserves_array_prefix_auth_and_offset() {
    let rows = json!([{"id":"older","title":"Older conversation"}]);
    let mut f = fixture(vec![json_response(rows.clone())]).await;
    assert_eq!(f.client.sessions_page(60).await.unwrap(), rows);
    let (method, uri, auth, body) = f.requests.recv().await.unwrap();
    assert_eq!(method, "GET");
    assert_eq!(uri, "/prefix/api/remote/chat/sessions?limit=30&offset=60");
    assert_eq!(auth, "Bearer test-private-token");
    assert_eq!(body, Value::Null);
}

#[tokio::test]
async fn observer_decodes_fragmented_utf8_and_stops_at_eof() {
    let event = format!(": keepalive\r\n\r\ndata: {}\r\n\r\n", snapshot());
    let chunks = event
        .into_bytes()
        .into_iter()
        .map(|byte| Ok::<_, std::io::Error>(vec![byte]));
    let body = Body::from_stream(futures::stream::iter(chunks));
    let response = Response::builder()
        .header("content-type", "text/event-stream")
        .body(body)
        .unwrap();
    let mut f = fixture(vec![response]).await;
    let (tx, mut rx) = mpsc::channel(1);
    let result = f.client.observe(&SessionId::new("session-1"), tx).await;
    assert!(result.is_err(), "EOF must trigger parent resynchronization");
    let received = rx.recv().await.expect("snapshot event missing");
    assert_eq!(received.messages[0]["text"], "Привет 🌍");
    assert_eq!(
        f.requests.recv().await.unwrap().1,
        "/prefix/api/remote/chat/sessions/session-1/events"
    );
}

#[tokio::test]
async fn http_rejects_redirects_and_sanitizes_error_bodies_without_retry() {
    for status in [302, 401, 503] {
        let response = Response::builder()
            .status(status)
            .header("location", "/token-in-redirect?test-private-token")
            .body(Body::from("test-private-token malicious failure"))
            .unwrap();
        let mut f = fixture(vec![response]).await;
        let error = f
            .client
            .command(&SessionId::new("session-1"), ChatCommand::Stop)
            .await
            .unwrap_err();
        let text = format!("{error:?}");
        assert!(
            !text.contains("test-private-token"),
            "secret leaked: {text}"
        );
        assert!(text.contains(&status.to_string()), "missing status: {text}");
        assert_eq!(f.requests.recv().await.unwrap().0, "POST");
        assert!(
            tokio::time::timeout(Duration::from_millis(25), f.requests.recv())
                .await
                .is_err(),
            "request retried or redirected"
        );
    }
}

#[tokio::test]
async fn json_response_is_bounded_even_without_content_length() {
    let chunks = (0..3).map(|_| Ok::<_, std::io::Error>(vec![b' '; 1024 * 1024]));
    let response = Response::builder()
        .body(Body::from_stream(futures::stream::iter(chunks)))
        .unwrap();
    let f = fixture(vec![response]).await;
    let error = f.client.sessions().await.unwrap_err();
    assert!(
        error.to_string().contains("limit"),
        "wrong failure: {error}"
    );
}

#[tokio::test]
async fn observer_rejects_unbounded_lines_and_multiline_events() {
    for bytes in [vec![b'x'; 3 * 1024 * 1024], b"data: x\n".repeat(1_100_000)] {
        let response = Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from(bytes))
            .unwrap();
        let f = fixture(vec![response]).await;
        let (tx, mut rx) = mpsc::channel(1);
        let error = f
            .client
            .observe(&SessionId::new("session-1"), tx)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("limit"),
            "wrong failure: {error}"
        );
        assert!(rx.recv().await.is_none());
    }
}

#[tokio::test]
async fn observer_rejects_wrong_content_type_and_malformed_data() {
    for (content_type, body) in [
        ("application/json", snapshot().to_string()),
        ("text/event-stream", "data: test-private-token\n\n".into()),
    ] {
        let response = Response::builder()
            .header("content-type", content_type)
            .body(Body::from(body))
            .unwrap();
        let f = fixture(vec![response]).await;
        let (tx, mut rx) = mpsc::channel(1);
        let error = f
            .client
            .observe(&SessionId::new("session-1"), tx)
            .await
            .unwrap_err();
        assert!(!format!("{error:?}").contains("test-private-token"));
        assert!(
            error
                .to_string()
                .contains(if content_type == "application/json" {
                    "content type"
                } else {
                    "snapshot"
                }),
            "wrong failure: {error}"
        );
        assert!(rx.recv().await.is_none());
    }
}

#[tokio::test]
async fn oversized_command_is_rejected_before_transmission() {
    let mut f = fixture(vec![]).await;
    let error = f
        .client
        .command(
            &SessionId::new("session-1"),
            ChatCommand::Send {
                request_id: uuid::Uuid::new_v4(),
                text: "x".repeat(64 * 1024),
            },
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("limit"),
        "wrong failure: {error}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(25), f.requests.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn observer_exits_when_consumer_already_closed() {
    let mut f = fixture(vec![]).await;
    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    assert!(
        f.client
            .observe(&SessionId::new("session-1"), tx)
            .await
            .is_ok()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(25), f.requests.recv())
            .await
            .is_err()
    );
}

#[test]
fn sse_handles_bom_multiline_data_all_line_endings_and_fragment_boundaries() {
    for ending in ["\n", "\r", "\r\n"] {
        let lines = serde_json::to_string_pretty(&snapshot())
            .unwrap()
            .lines()
            .map(|line| format!("data: {line}{ending}"))
            .collect::<String>();
        let bytes = format!(
            "\u{feff}: keepalive{ending}{ending}id: ignored{ending}event: message{ending}{lines}{ending}{lines}{ending}"
        );
        let mut parser = ChatEvents::default();
        let mut snapshots = Vec::new();
        for byte in bytes.bytes() {
            if let Some(snapshot) = parser.push(byte).unwrap() {
                snapshots.push(snapshot);
            }
        }
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[1].messages[0]["text"], "Привет 🌍");
        assert!(parser.line.is_empty());
        assert!(parser.data.is_empty());
    }
}

#[tokio::test]
async fn serving_cancellation_delivers_close_and_revokes_local_admission() {
    let f = fixture(vec![json_response(snapshot())]).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = f.config.clone();
    config.server = format!("http://{}/", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let environment = LocalEnvironment::open(&config, f._dir.path())
        .await
        .unwrap();
    let key = EnvironmentKey {
        connection_id: uuid::Uuid::new_v4(),
        generation: uuid::Uuid::new_v4(),
    };
    let expected = key.clone();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let hello = socket.next().await.unwrap().unwrap();
        assert!(matches!(
            decode_frame::<ClientFrame>(&hello.into_data()).unwrap(),
            ClientFrame::Hello { .. }
        ));
        socket
            .send(Message::Binary(
                encode_frame(&ServerFrame::Welcome {
                    environment: expected.clone(),
                    heartbeat_seconds: 5,
                    lease_seconds: 15,
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let mut ready_tx = Some(ready_tx);
        loop {
            let frame = socket
                .next()
                .await
                .expect("client closed without Close frame")
                .unwrap();
            match decode_frame::<ClientFrame>(&frame.into_data()).unwrap() {
                ClientFrame::Heartbeat { environment } => {
                    socket
                        .send(Message::Binary(
                            encode_frame(&ServerFrame::HeartbeatAck { environment })
                                .unwrap()
                                .into(),
                        ))
                        .await
                        .unwrap();
                    if let Some(ready) = ready_tx.take() {
                        ready.send(()).unwrap();
                    }
                }
                ClientFrame::Close { environment } => {
                    assert_eq!(environment, expected);
                    break;
                }
                _ => panic!("unexpected client frame"),
            }
        }
    });
    let mut client = RemoteClient::connect(&config).await.unwrap();
    let session = SessionId::new("session-1");
    assert_eq!(
        client
            .attach(session.clone(), environment.manifest().unwrap())
            .await
            .unwrap(),
        key
    );
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let retained = environment.clone();
    let task = tokio::spawn(async move { client.serve_local_until(retained, stop_rx).await });
    let ready = tokio::time::timeout(Duration::from_secs(2), ready_rx).await;
    let _ = stop_tx.send(());
    let result = tokio::time::timeout(Duration::from_secs(3), task).await;
    let rejected = environment
        .execute(Invocation {
            environment: key,
            session_id: session,
            run_id: "run".into(),
            call_id: "call".into(),
            tool_id: "apply_patch".into(),
            arguments: json!({}),
            deadline: chrono::Utc::now() + chrono::Duration::seconds(60),
        })
        .await
        .is_err();
    environment.close().await.unwrap();
    assert!(
        ready.is_ok_and(|ready| ready.is_ok()),
        "serving loop did not become ready"
    );
    assert!(
        result.unwrap().unwrap().is_ok(),
        "graceful cancellation failed"
    );
    server.await.unwrap();
    assert!(rejected, "cancelled environment accepted a call");
    assert_eq!(
        f.client
            .snapshot(&SessionId::new("session-1"))
            .await
            .unwrap()
            .revision,
        7
    );
}
