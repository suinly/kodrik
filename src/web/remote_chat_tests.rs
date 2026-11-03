use super::*;
use crate::{
    agent::{
        Agent, AssistantMessage, ContextBuilder, UserMessage,
        message::{Message, ToolCall, ToolResult},
        tools::ToolRegistry,
    },
    remote::protocol::ChatCommand,
};

struct Echo;
#[async_trait::async_trait]
impl crate::agent::llm::LlmProvider for Echo {
    async fn generate(
        &self,
        context: &crate::agent::LlmContext<'_>,
        _: &[&dyn crate::agent::tools::Tool],
        _: Option<&mpsc::Sender<crate::agent::AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        let text = context
            .messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::User(u) => Some(u.content.clone()),
                _ => None,
            })
            .unwrap_or_default();
        Ok((
            vec![Message::Assistant(AssistantMessage {
                content: format!("echo {text}"),
            })],
            None,
        ))
    }
}

async fn fixture() -> (tempfile::TempDir, Chat, WebState, SessionId) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteSessionStore::open(dir.path().join("sessions")).unwrap());
    let runner = AgentRunner::new(
        Arc::new(Agent::new(
            Arc::new(Echo),
            ToolRegistry::default(),
            ContextBuilder::new("private system instructions"),
        )),
        store.clone(),
    );
    let chat = Chat::new(&runner, dir.path()).await.unwrap();
    let state = WebState {
        settings: Settings {
            trusted_proxies: vec![],
            listen: "127.0.0.1:0".parse().unwrap(),
            base_url: "http://127.0.0.1/kodrik/".parse().unwrap(),
        },
        auth: AuthStore::new(dir.path()),
        chat: Some(chat.clone()),
        streams: Arc::new(Semaphore::new(64)),
    };
    let id = SessionId::new(uuid::Uuid::new_v4().to_string());
    let mut session = Session::new(id.clone());
    session.initialize_system_prompt("secret personality and provider snapshot");
    session.push(Message::User(UserMessage {
        content: "from telegram".into(),
        files: vec![],
        images: vec![],
    }));
    chat.sessions.save(&session).await.unwrap();
    (dir, chat, state, id)
}

#[tokio::test]
async fn native_snapshot_reads_shared_history_without_private_snapshots() {
    let (_dir, chat, _state, id) = fixture().await;
    let snapshot = chat.remote_snapshot(&id).await.unwrap();
    let text = serde_json::to_string(&snapshot).unwrap();
    assert!(text.contains("from telegram"));
    assert!(!text.contains("secret personality"));
    assert_eq!(snapshot.messages[0]["role"], "user");
    assert!(
        chat.remote_snapshot(&SessionId::new("companion:private"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn native_send_retries_do_not_duplicate_and_edit_keeps_branches() {
    let (_dir, chat, _state, id) = fixture().await;
    let request_id = uuid::Uuid::new_v4();
    let command = ChatCommand::Send {
        request_id,
        text: "hello".into(),
    };
    let first = chat.remote_command(&id, command.clone()).await.unwrap();
    let second = chat.remote_command(&id, command).await.unwrap();
    assert_eq!(first["submission_id"], second["submission_id"]);
    assert!(
        chat.remote_command(
            &id,
            ChatCommand::Send {
                request_id,
                text: "different".into()
            }
        )
        .await
        .is_err()
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while chat.sessions.session_busy(id.as_str()).await.unwrap()
            || chat.runs.lock().await.contains(id.as_str())
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let request_id = uuid::Uuid::new_v4();
    let edit = ChatCommand::Edit {
        request_id,
        user_turn: 1,
        text: "edited".into(),
    };
    let first = chat.remote_command(&id, edit.clone()).await.unwrap();
    let second = chat.remote_command(&id, edit).await.unwrap();
    assert_eq!(first["id"], second["id"]);
    let selected: Option<String> = chat
        .sessions
        .with_connection(|db| {
            use rusqlite::OptionalExtension;
            Ok(db
                .query_row(
                    "SELECT session_id FROM web_chat_selection LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .optional()?)
        })
        .await
        .unwrap();
    assert!(selected.is_none(), "native edit changed browser selection");
    let branch = SessionId::new(first["id"].as_str().unwrap());
    tokio::time::timeout(Duration::from_secs(5), async {
        while chat.sessions.session_busy(branch.as_str()).await.unwrap()
            || chat.runs.lock().await.contains(branch.as_str())
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let snapshot = chat.remote_snapshot(&branch).await.unwrap();
    assert!(
        snapshot
            .messages
            .iter()
            .any(|m| m["versions"].as_array().is_some_and(|ids| ids.len() == 2))
    );
    assert!(
        chat.remote_command(
            &id,
            ChatCommand::SelectVersion {
                session_id: branch.clone(),
                user_turn: 1
            }
        )
        .await
        .is_ok()
    );
    let unrelated = SessionId::new(uuid::Uuid::new_v4().to_string());
    chat.sessions
        .save(&Session::new(unrelated.clone()))
        .await
        .unwrap();
    assert!(
        chat.remote_command(
            &id,
            ChatCommand::SelectVersion {
                session_id: unrelated,
                user_turn: 1
            }
        )
        .await
        .is_err()
    );
    let original = chat.sessions.load(&id).await.unwrap().unwrap();
    assert_eq!(
        original
            .messages()
            .iter()
            .filter(|m| matches!(m,Message::User(u) if u.content=="hello"))
            .count(),
        1
    );
}

#[tokio::test]
async fn native_full_result_is_chunked_without_system_snapshots() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let (dir, chat, state, id) = fixture().await;
    let mut session = chat.sessions.load(&id).await.unwrap().unwrap();
    session.push(Message::Assistant(AssistantMessage {
        content: "я".repeat(80000),
    }));
    chat.sessions.save(&session).await.unwrap();
    let now = chrono::Utc::now().timestamp();
    let pairing = state.auth.start_device_pairing("Laptop", now).unwrap();
    state
        .auth
        .approve_device_pairing(pairing.request_id, &pairing.code, &Identity::Owner, now)
        .unwrap();
    let crate::web::auth::devices::PairingPoll::Authorized { token, .. } = state
        .auth
        .poll_device_pairing(pairing.request_id, &pairing.polling_secret, now)
        .unwrap()
    else {
        panic!()
    };
    let app = crate::web::server::router_with_chat(state.settings.clone(), dir.path(), Some(chat));
    let mut offset = 0;
    let mut text = String::new();
    loop {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/kodrik/api/remote/chat/sessions/{}/history/1?offset={offset}",
                        id.as_str()
                    ))
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let bytes = axum::body::to_bytes(response.into_body(), 512 * 1024)
            .await
            .unwrap();
        let chunk: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(chunk["data"].as_str().unwrap().len() <= 64 * 1024);
        text.push_str(chunk["data"].as_str().unwrap());
        let Some(next) = chunk["next_offset"].as_u64() else {
            break;
        };
        assert!(next > offset);
        offset = next;
    }
    let row: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        row["parts"][0]["text"].as_str().unwrap().chars().count(),
        80000
    );
    assert!(!text.contains("secret personality"));
}

#[tokio::test]
async fn native_attachment_references_include_tool_images_without_inline_blobs() {
    use axum::{body::Body, http::Request};
    use base64::{Engine, engine::general_purpose::STANDARD};
    use tower::ServiceExt;
    let (dir, chat, state, id) = fixture().await;
    let image = crate::agent::Image {
        data_url: format!(
            "data:image/png;base64,{}",
            STANDARD.encode(b"image fixture")
        ),
        width: 1,
        height: 1,
    };
    let file =
        crate::agent::FileAttachment::save(dir.path(), "notes.txt", None, b"private file").unwrap();
    let mut session = chat.sessions.load(&id).await.unwrap().unwrap();
    session.push(Message::User(UserMessage {
        content: "attachments".into(),
        images: vec![image.clone()],
        files: vec![file],
    }));
    session.push(Message::ToolCall(ToolCall {
        call_id: "picture".into(),
        name: "view_image".into(),
        arguments: json!({}),
    }));
    session.push(Message::ToolResult(ToolResult {
        call_id: "picture".into(),
        content: crate::agent::message::ToolOutput {
            output: json!({"ok":true}),
            images: vec![image],
        },
    }));
    chat.sessions.save(&session).await.unwrap();
    let snapshot = chat.remote_snapshot(&id).await.unwrap();
    let text = serde_json::to_string(&snapshot).unwrap();
    assert!(!text.contains("base64"));
    assert!(!text.contains("private file"));
    let urls: Vec<_> = snapshot
        .messages
        .iter()
        .flat_map(|m| m["parts"].as_array().unwrap())
        .filter_map(|p| p["url"].as_str())
        .collect();
    assert_eq!(urls.len(), 3, "tool output image lost from native history");
    let now = chrono::Utc::now().timestamp();
    let pairing = state.auth.start_device_pairing("Laptop", now).unwrap();
    state
        .auth
        .approve_device_pairing(pairing.request_id, &pairing.code, &Identity::Owner, now)
        .unwrap();
    let crate::web::auth::devices::PairingPoll::Authorized { token, .. } = state
        .auth
        .poll_device_pairing(pairing.request_id, &pairing.polling_secret, now)
        .unwrap()
    else {
        panic!()
    };
    let app = crate::web::server::router_with_chat(state.settings.clone(), dir.path(), Some(chat));
    for url in urls {
        let url = state.settings.base_url.join(url).unwrap();
        assert!(url.path().starts_with("/kodrik/api/remote/chat/"));
        let request = |authorized: bool| {
            let mut builder = Request::builder().uri(url.path());
            if authorized {
                builder = builder.header("Authorization", format!("Bearer {token}"));
            }
            builder.body(Body::empty()).unwrap()
        };
        assert_eq!(
            app.clone().oneshot(request(false)).await.unwrap().status(),
            401
        );
        let response = app.clone().oneshot(request(true)).await.unwrap();
        assert_eq!(response.status(), 200);
        let bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert!(bytes == b"image fixture"[..] || bytes == b"private file"[..]);
    }
}

#[tokio::test]
async fn native_history_bounds_preserve_tool_groups_and_durable_content() {
    let (_dir, chat, _state, id) = fixture().await;
    let mut session = chat.sessions.load(&id).await.unwrap().unwrap();
    for i in 0..220 {
        session.push(Message::Assistant(AssistantMessage {
            content: format!("message {i}"),
        }));
        session.push(Message::User(UserMessage {
            content: format!("user {i}"),
            files: vec![],
            images: vec![],
        }));
    }
    session.push(Message::ToolCall(ToolCall {
        call_id: "large".into(),
        name: "test".into(),
        arguments: json!({}),
    }));
    session.push(Message::ToolResult(ToolResult {
        call_id: "large".into(),
        content: json!({"payload":"x".repeat(2*1024*1024)}).into(),
    }));
    chat.sessions.save(&session).await.unwrap();
    let snapshot = chat.remote_snapshot(&id).await.unwrap();
    assert!(serde_json::to_vec(&snapshot).unwrap().len() <= 1024 * 1024);
    assert!(snapshot.messages.len() <= 200);
    assert!(snapshot.previous_cursor.is_some());
    let cursor = snapshot.previous_cursor.unwrap();
    let previous = chat.remote_history(&id, &cursor).await.unwrap();
    assert_ne!(previous.previous_cursor, Some(cursor));
    let text = serde_json::to_string(&snapshot.messages).unwrap();
    assert!(text.contains("truncated"));
    assert!(text.contains("/api/remote/chat/sessions/"));
    assert!(text.contains("/history/"));
    let tool = snapshot
        .messages
        .iter()
        .flat_map(|m| m["parts"].as_array().unwrap())
        .find(|p| p["toolCallId"] == "large")
        .unwrap();
    assert_eq!(tool["toolName"], "test");
    assert_eq!(tool["state"], "output-available");
    assert_eq!(tool["truncated"], true);
    assert_eq!(
        chat.sessions
            .load(&id)
            .await
            .unwrap()
            .unwrap()
            .messages()
            .len(),
        443
    );
}

#[tokio::test]
async fn native_events_send_initial_snapshot_and_close_after_revoke() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let (dir, chat, state, id) = fixture().await;
    chat.runner.enable_environments(state.auth.clone()).unwrap();
    let environments = chat.runner.environments().unwrap().clone();
    let now = chrono::Utc::now().timestamp();
    let pairing = state.auth.start_device_pairing("Laptop", now).unwrap();
    state
        .auth
        .approve_device_pairing(pairing.request_id, &pairing.code, &Identity::Owner, now)
        .unwrap();
    let crate::web::auth::devices::PairingPoll::Authorized { device_id, token } = state
        .auth
        .poll_device_pairing(pairing.request_id, &pairing.polling_secret, now)
        .unwrap()
    else {
        panic!()
    };
    let app = crate::web::server::router_with_chat(state.settings.clone(), dir.path(), Some(chat));
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/kodrik/api/remote/chat/sessions/{}/events",
                    id.as_str()
                ))
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut stream = response.into_body().into_data_stream();
    let initial = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let initial = std::str::from_utf8(&initial).unwrap();
    assert!(initial.contains("from telegram"));
    assert!(initial.contains("revision"));
    let key = environments
        .attach(
            crate::web::auth::devices::DeviceIdentity { device_id },
            id.clone(),
            crate::remote::protocol::EnvironmentManifest {
                label: "Connected laptop".into(),
                project_root: "/project".into(),
                tools: vec![],
                skills: vec![],
                project_instructions: None,
            },
        )
        .await
        .unwrap();
    let updated = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        std::str::from_utf8(&updated)
            .unwrap()
            .contains("Connected laptop")
    );
    environments.revoke(&key, "closed").await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        std::str::from_utf8(&closed)
            .unwrap()
            .contains("\"environment\":null")
    );
    state.auth.revoke_device(device_id).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn native_http_requires_bearer_rechecks_revocation_and_keeps_prefix() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let (dir, chat, state, id) = fixture().await;
    let now = chrono::Utc::now().timestamp();
    let pairing = state.auth.start_device_pairing("Laptop", now).unwrap();
    state
        .auth
        .approve_device_pairing(pairing.request_id, &pairing.code, &Identity::Owner, now)
        .unwrap();
    let crate::web::auth::devices::PairingPoll::Authorized { device_id, token } = state
        .auth
        .poll_device_pairing(pairing.request_id, &pairing.polling_secret, now)
        .unwrap()
    else {
        panic!()
    };
    let app = crate::web::server::router_with_chat(state.settings.clone(), dir.path(), Some(chat));
    for (path, expected) in [
        (format!("api/remote/chat/sessions/{}", id.as_str()), 200),
        ("api/remote/chat/sessions".into(), 200),
        ("api/profile".into(), 401),
        ("api/chat/sessions".into(), 401),
        ("api/remote/chat/sessions/companion:private".into(), 404),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/kodrik/{path}"))
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{path}");
    }
    let path = format!("/kodrik/api/remote/chat/sessions/{}", id.as_str());
    let response = app
        .clone()
        .oneshot(Request::builder().uri(&path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    state.auth.revoke_device(device_id).unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .uri(path)
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
}
