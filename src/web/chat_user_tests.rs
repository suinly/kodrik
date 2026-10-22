use super::*;
use crate::{
    agent::{UserMessage, message::Message},
    web::auth::{Identity, session_cookie},
};

#[tokio::test]
async fn web_stop_cancels_old_session_delivery_without_stopping_newer_telegram_work() {
    use crate::agent::{
        AssistantMessage,
        run_state::{Checkpoint, RunPhase},
    };
    let data = tempfile::tempdir().unwrap();
    for user in [7, 8, 42] {
        AuthStore::new(data.path()).grant_test_user(user).unwrap();
    }
    let chat = crate::web::chat_tests::chat(data.path()).await;
    let channel = SessionId::new("telegram:bot:42");
    let mut old = Session::new(channel.clone());
    old.push(Message::Assistant(AssistantMessage {
        content: "Old answer awaiting delivery".into(),
    }));
    let mut cp = Checkpoint::new(&chat.sessions, &channel, &old);
    cp.save(&old, RunPhase::Completed, None).await.unwrap();
    let newer = Session::new(SessionId::new(uuid::Uuid::new_v4().to_string()));
    chat.sessions.save(&newer).await.unwrap();
    chat.sessions.select(&channel, newer.id()).await.unwrap();
    let mut new_run = Checkpoint::new(&chat.sessions, &channel, &newer);
    new_run
        .save(&newer, RunPhase::NeedModel, None)
        .await
        .unwrap();
    chat.stop(old.id().as_str()).await.unwrap();
    let run = chat
        .sessions
        .load_run(&new_run.record.id)
        .await
        .unwrap()
        .unwrap()
        .0;
    assert!(!run.cancel_requested);
    assert!(matches!(run.phase, RunPhase::NeedModel));
    assert_eq!(
        chat.sessions.active_id(&channel).await.unwrap(),
        *newer.id()
    );
    assert_eq!(
        chat.sessions
            .active_id(&web_conversation(old.id().as_str()))
            .await
            .unwrap(),
        *old.id()
    );
    assert_eq!(chat.sessions.list().await.unwrap().len(), 2);
    let receipt: Value = chat
        .sessions
        .with_connection(move |db| {
            let bytes: Vec<u8> = db.query_row(
                "SELECT payload FROM deliveries WHERE run_id=?1",
                [&cp.record.id],
                |r| r.get(0),
            )?;
            Ok(serde_json::from_slice(&bytes)?)
        })
        .await
        .unwrap();
    assert_eq!(receipt["status"], "Rejected");
    assert_eq!(receipt["cancel_requested"], true);
}

#[tokio::test]
async fn telegram_login_retains_user_identity_after_configured_owner_changes() {
    use crate::agent::inbox::{BusyMode, SubmissionRequest};
    use tower::ServiceExt;
    let data = tempfile::tempdir().unwrap();
    for user in [7, 8, 42] {
        AuthStore::new(data.path()).grant_test_user(user).unwrap();
    }
    let mut config = user_config();
    let previous_owner =
        crate::bootstrap::build_app_at(&config, &data.path().join("users/42"), false)
            .await
            .unwrap();
    let mut old_history = Session::new(SessionId::new("old-owner-history"));
    old_history.push(Message::User(UserMessage {
        content: "USER 42 HISTORY".into(),
        files: vec![],
        images: vec![],
    }));
    previous_owner
        .agent_runner
        .sqlite_store()
        .unwrap()
        .save(&old_history)
        .await
        .unwrap();
    drop(previous_owner);
    let auth = AuthStore::new(data.path());
    let now = chrono::Utc::now().timestamp();
    let token = auth
        .test_session(Identity::Telegram(42), now)
        .unwrap()
        .unwrap();
    config.owner.as_mut().unwrap().user_id = std::num::NonZeroU64::new(7).unwrap();
    let new_owner = crate::bootstrap::build_app_at(&config, &data.path().join("users/7"), false)
        .await
        .unwrap();
    let mut owner_history = Session::new(SessionId::new("new-owner-history"));
    owner_history.push(Message::User(UserMessage {
        content: "NEW OWNER SECRET".into(),
        files: vec![],
        images: vec![],
    }));
    new_owner
        .agent_runner
        .sqlite_store()
        .unwrap()
        .save(&owner_history)
        .await
        .unwrap();
    let users = Arc::new(crate::users::UserSpaces::new(
        &config,
        data.path(),
        new_owner.clone(),
    ));
    let chat = Chat::new(&new_owner.agent_runner, data.path())
        .await
        .unwrap()
        .with_users(users);
    let user_chat = chat.for_user(42).await.unwrap();
    let mut request = SubmissionRequest::new(
        old_history.id(),
        crate::gateway::IncomingMessage::new(
            crate::gateway::InterfaceId("web".into()),
            "old-owner-history",
            "permissions",
        ),
        BusyMode::FollowUp,
    );
    user_chat.runner.prepare_submission(&mut request);
    assert!(request.isolated);
    assert_eq!(
        auth.identity(&token, now).unwrap(),
        Some(Identity::Telegram(42))
    );
    assert!(!auth.authenticated(&token, now).unwrap());
    let settings = Settings {
        trusted_proxies: Vec::new(),
        listen: "127.0.0.1:0".parse().unwrap(),
        base_url: "http://localhost/".parse().unwrap(),
    };
    let cookie = session_cookie(&settings, &token, false)
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let app = crate::web::server::router_with_resources(settings, data.path(), Some(chat), None);
    let get = |path: &str| {
        Request::builder()
            .uri(path)
            .header("Cookie", &cookie)
            .body(axum::body::Body::empty())
            .unwrap()
    };
    let response = app
        .clone()
        .oneshot(get("/api/chat/sessions/old-owner-history"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    let history = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(history.contains("USER 42 HISTORY"));
    assert!(!history.contains("NEW OWNER SECRET"));
    assert_eq!(
        app.oneshot(get("/api/chat/sessions/new-owner-history"))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn restart_recovers_saved_user_web_input_without_browser_login() {
    use crate::{
        agent::inbox::{BusyMode, SubmissionRequest, SubmissionStatus},
        config::ProviderConfig,
    };
    use axum::response::IntoResponse;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let provider = axum::Router::new().route("/responses", axum::routing::post(|Json(request): Json<Value>| async move {
        let response = json!({"output":[{"type":"message","content":[{"type":"output_text","text":"recovered answer"}]}]});
        if request["stream"] == true {
            ([("content-type","text/event-stream")], format!("data: {}\n\ndata: [DONE]\n\n", json!({"type":"response.completed","response":response}))).into_response()
        } else { Json(response).into_response() }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });
    let data = tempfile::tempdir().unwrap();
    for user in [7, 8, 42] {
        AuthStore::new(data.path()).grant_test_user(user).unwrap();
    }
    let mut config = user_config();
    let Some(ProviderConfig::OpenAi { api_base, .. }) = config.provider.get_mut("primary") else {
        panic!()
    };
    *api_base = Some(endpoint);
    let owner = crate::bootstrap::build_app_at(&config, &data.path().join("users/42"), false)
        .await
        .unwrap();
    let old_registry = crate::users::UserSpaces::new(&config, data.path(), owner.clone());
    let app = old_registry.app(7).await.unwrap();
    let store = app.agent_runner.sqlite_store().unwrap();
    let id = SessionId::new(uuid::Uuid::new_v4().to_string());
    store.save(&Session::new(id.clone())).await.unwrap();
    let request = SubmissionRequest::new(
        &id,
        crate::gateway::IncomingMessage::new(
            crate::gateway::InterfaceId("web".into()),
            id.as_str(),
            "saved before restart",
        ),
        BusyMode::FollowUp,
    );
    let request_id = request.request_id.clone();
    app.agent_runner.submit(request).await.unwrap();
    drop(app);
    drop(old_registry);
    let registry = Arc::new(crate::users::UserSpaces::new(
        &config,
        data.path(),
        owner.clone(),
    ));
    let chat = Chat::new(&owner.agent_runner, data.path())
        .await
        .unwrap()
        .with_users(registry);
    assert!(chat.user_chats.lock().await.is_empty());
    chat.recover_users().await.unwrap();
    assert!(chat.user_chats.lock().await.contains_key(&7));
    assert!(!chat.user_chats.lock().await.contains_key(&42));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if store
                .submission(id.as_str(), &request_id)
                .await
                .unwrap()
                .is_some_and(|r| r.status == SubmissionStatus::Done)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("accepted user input waited for browser login after restart");
    assert!(
        store
            .load(&id)
            .await
            .unwrap()
            .unwrap()
            .messages()
            .iter()
            .any(|m| matches!(m,Message::Assistant(answer) if answer.content=="recovered answer"))
    );
    owner.agent_runner.suspend();
    server.abort();
}

pub(crate) fn user_config() -> crate::config::Config {
    toml::from_str("[gateway.main]\ntype='telegram'\ntoken='test'\n[owner]\ninterface='main'\nuser_id=42\n[companion]\nlearning_enabled=false\nproactive_enabled=false\n[sessions]\nauto_switch=false\n[provider.primary]\ntype='openai'\nmodel='test'\napi_key='test'\n").unwrap()
}

#[tokio::test]
async fn web_resume_explains_when_a_queue_requires_its_original_interface() {
    let data = tempfile::tempdir().unwrap();
    for user in [7, 8, 42] {
        AuthStore::new(data.path()).grant_test_user(user).unwrap();
    }
    let chat = crate::web::chat_tests::chat(data.path()).await;
    let session = Session::new(SessionId::new(uuid::Uuid::new_v4().to_string()));
    chat.sessions.save(&session).await.unwrap();
    let channel = SessionId::new("telegram:main:42");
    chat.sessions.select(&channel, session.id()).await.unwrap();
    chat.sessions
        .submit(crate::agent::inbox::SubmissionRequest::new(
            &channel,
            crate::gateway::IncomingMessage::new(
                crate::gateway::InterfaceId("telegram:main".into()),
                "42",
                "saved followup",
            ),
            crate::agent::inbox::BusyMode::FollowUp,
        ))
        .await
        .unwrap();
    chat.sessions
        .with_connection(|db| {
            db.execute(
                "INSERT INTO inbox_controls VALUES('telegram:main:42','Stopped by user')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let state = WebState {
        streams: Arc::new(Semaphore::new(64)),
        settings: Settings {
            trusted_proxies: Vec::new(),
            listen: "127.0.0.1:0".parse().unwrap(),
            base_url: "http://localhost/".parse().unwrap(),
        },
        auth: AuthStore::new(data.path()),
        chat: Some(chat.clone()),
    };
    let response = resume(Extension(state), Path(session.id().as_str().to_owned()))
        .await
        .unwrap_err();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(
        chat.sessions
            .inbox_blocked(channel.as_str())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        chat.sessions
            .conversation_view(session.id().as_str())
            .await
            .unwrap()
            .queued
            .len(),
        1
    );
}

#[tokio::test]
async fn web_continues_legacy_history_without_telegram_delivery_or_following_cli_selection() {
    let data = tempfile::tempdir().unwrap();
    for user in [7, 8, 42] {
        AuthStore::new(data.path()).grant_test_user(user).unwrap();
    }
    let chat = crate::web::chat_tests::chat(data.path()).await;
    let state = WebState {
        streams: Arc::new(Semaphore::new(64)),
        settings: Settings {
            trusted_proxies: Vec::new(),
            listen: "127.0.0.1:0".parse().unwrap(),
            base_url: "http://localhost/".parse().unwrap(),
        },
        auth: AuthStore::new(data.path()),
        chat: Some(chat.clone()),
    };
    for id in ["telegram:main:42", "cli:main"] {
        let mut original = Session::new(SessionId::new(id));
        original.push(Message::User(UserMessage {
            content: "Earlier device".into(),
            files: vec![],
            images: vec![],
        }));
        chat.sessions.save(&original).await.unwrap();
        let other = Session::new(SessionId::new(uuid::Uuid::new_v4().to_string()));
        chat.sessions.save(&other).await.unwrap();
        chat.sessions
            .select(original.id(), other.id())
            .await
            .unwrap();
        admit(
            state.clone(),
            Input {
                id: id.into(),
                request_id: new_request_id(),
                text: "Continue on web".into(),
            },
            vec![],
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let session = chat.sessions.load(original.id()).await.unwrap().unwrap();
                if session.messages().len() == 3 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            chat.sessions
                .load(other.id())
                .await
                .unwrap()
                .unwrap()
                .messages()
                .is_empty()
        );
        assert_eq!(
            chat.sessions.active_id(original.id()).await.unwrap(),
            *other.id()
        );
        assert_eq!(
            chat.sessions
                .with_connection(|db| Ok(db.query_row(
                    "SELECT count(*) FROM deliveries",
                    [],
                    |r| r.get::<_, i64>(0)
                )?))
                .await
                .unwrap(),
            0
        );
        let view = chat.sessions.conversation_view(id).await.unwrap();
        assert_eq!(view.run.unwrap().run.conversation_id, format!("web:{id}"));
        // The same owner runtime can select the web-updated history from CLI.
        while chat.sessions.session_busy(id).await.unwrap() {
            tokio::task::yield_now().await;
        }
        let cli = SessionId::new("cli:other-device");
        chat.runner
            .select_session(&cli, original.id())
            .await
            .unwrap();
        chat.runner
            .run(
                cli,
                crate::gateway::IncomingMessage::new(
                    crate::gateway::InterfaceId("cli".into()),
                    "other-device",
                    "Continue on CLI",
                ),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            chat.sessions
                .load(original.id())
                .await
                .unwrap()
                .unwrap()
                .messages()
                .len(),
            5
        );
    }
}

#[tokio::test]
async fn shared_owner_history_accepts_legacy_personal_session_ids() {
    let data = tempfile::tempdir().unwrap();
    for user in [7, 8, 42] {
        AuthStore::new(data.path()).grant_test_user(user).unwrap();
    }
    let chat = crate::web::chat_tests::chat(data.path()).await;
    let owner = SqliteSessionStore::open(data.path().join("owner-sessions")).unwrap();
    for id in [
        uuid::Uuid::new_v4().to_string(),
        "telegram:main:42".into(),
        "cli:main".into(),
    ] {
        let mut session = Session::new(SessionId::new(&id));
        session.push(Message::User(UserMessage {
            content: "Shared history".into(),
            images: vec![],
            files: vec![],
        }));
        owner.save(&session).await.unwrap();
        let shown = chat.session(&id).await.unwrap();
        assert_eq!(shown.messages().len(), 1);
    }
}

#[tokio::test]
async fn user_login_scopes_chat_storage_and_reads_telegram_history() {
    let data = tempfile::tempdir().unwrap();
    for user in [7, 8, 42] {
        AuthStore::new(data.path()).grant_test_user(user).unwrap();
    }
    let config = user_config();
    let app = crate::bootstrap::build_app_at(&config, &data.path().join("users/42"), false)
        .await
        .unwrap();
    let users = Arc::new(crate::users::UserSpaces::new(
        &config,
        data.path(),
        app.clone(),
    ));
    let owner_chat = Chat::new(&app.agent_runner, data.path())
        .await
        .unwrap()
        .with_users(users.clone());
    let auth = AuthStore::new(data.path());
    let now = chrono::Utc::now().timestamp();
    let mut tokens = vec![];
    let mut ids = vec![];
    for user in [7, 8] {
        let app = users.app(user).await.unwrap();
        let session = app
            .agent_runner
            .new_session(&SessionId::new(format!("telegram:main:{user}")))
            .await
            .unwrap();
        ids.push(session.id().as_str().to_owned());
        tokens.push(
            auth.test_session(Identity::Telegram(user), now)
                .unwrap()
                .unwrap(),
        );
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/", listener.local_addr().unwrap());
    let settings = Settings {
        trusted_proxies: Vec::new(),
        listen: listener.local_addr().unwrap(),
        base_url: base.parse().unwrap(),
    };
    let cookies: Vec<_> = tokens
        .iter()
        .map(|token| {
            session_cookie(&settings, token, false)
                .split(";")
                .next()
                .unwrap()
                .to_owned()
        })
        .collect();
    let router =
        crate::web::server::router_with_resources(settings, data.path(), Some(owner_chat), None);
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = reqwest::Client::new();
    for (index, cookie) in cookies.iter().enumerate() {
        let request = |path: &str| {
            client
                .get(format!("{base}api/chat/{path}"))
                .header("Cookie", cookie)
        };
        let response = request("sessions").send().await.unwrap();
        assert_eq!(response.status(), 200);
        let list: Value = response.json().await.unwrap();
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["id"], ids[index]);
        assert_eq!(
            request(&format!("sessions/{}", ids[1 - index]))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        assert_eq!(
            request(&format!("sessions/{}/run", ids[index]))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    server.abort();
}

async fn open_events(app: &axum::Router, id: &str, token: &str) -> axum::response::Response {
    use tower::ServiceExt;
    app.clone()
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/api/chat/sessions/{id}/events"))
                .header("cookie", format!("kodrik_web_session={token}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

fn login_token(auth: &AuthStore, identity: Identity) -> String {
    let now = chrono::Utc::now().timestamp();
    if let Identity::Telegram(user) = identity {
        auth.grant_test_user(user).unwrap();
    }
    auth.test_session(identity, now).unwrap().unwrap()
}

#[tokio::test]
async fn one_user_stream_limit_preserves_http_capacity() {
    use crate::agent::session::{Session, SessionId};
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let data = tempfile::tempdir().unwrap();
    for user in [7, 8, 42] {
        AuthStore::new(data.path()).grant_test_user(user).unwrap();
    }
    let config = user_config();
    let owner = crate::bootstrap::build_app_at(&config, &data.path().join("users/42"), false)
        .await
        .unwrap();
    let users = Arc::new(crate::users::UserSpaces::new(
        &config,
        data.path(),
        owner.clone(),
    ));
    let chat = Chat::new(&owner.agent_runner, data.path())
        .await
        .unwrap()
        .with_users(users);
    let user_chat = chat.for_user(7).await.unwrap();
    let ids = [
        uuid::Uuid::new_v4().to_string(),
        uuid::Uuid::new_v4().to_string(),
    ];
    for id in &ids {
        user_chat
            .sessions
            .save(&Session::new(SessionId::new(id)))
            .await
            .unwrap();
    }
    let owner_id = uuid::Uuid::new_v4().to_string();
    chat.sessions
        .save(&Session::new(SessionId::new(&owner_id)))
        .await
        .unwrap();
    let auth = AuthStore::new(data.path());
    let tokens = [
        login_token(&auth, Identity::Telegram(7)),
        login_token(&auth, Identity::Telegram(7)),
    ];
    let owner_tokens = [
        login_token(&auth, Identity::Owner),
        login_token(&auth, Identity::Telegram(42)),
    ];
    let settings = Settings {
        trusted_proxies: Vec::new(),
        listen: "127.0.0.1:0".parse().unwrap(),
        base_url: "http://127.0.0.1/".parse().unwrap(),
    };
    let router = crate::web::server::router_with_chat(settings, data.path(), Some(chat));
    let mut streams = Vec::new();
    for index in 0..4 {
        let response = open_events(&router, &ids[index % 2], &tokens[index % 2]).await;
        assert_eq!(response.status(), StatusCode::OK);
        streams.push(response);
    }
    assert_eq!(
        open_events(&router, &ids[1], &tokens[1]).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/auth/login")
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    for index in 0..4 {
        let response = open_events(&router, &owner_id, &owner_tokens[index % 2]).await;
        assert_eq!(response.status(), StatusCode::OK);
        streams.push(response);
    }
    for token in &owner_tokens {
        assert_eq!(
            open_events(&router, &owner_id, token).await.status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    drop(streams);
    assert_eq!(
        open_events(&router, &ids[0], &tokens[0]).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        open_events(&router, &owner_id, &owner_tokens[1])
            .await
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn full_stream_pool_does_not_block_login_and_releases_capacity() {
    use crate::agent::session::{Session, SessionId};
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let data = tempfile::tempdir().unwrap();
    for user in [7, 8, 42] {
        AuthStore::new(data.path()).grant_test_user(user).unwrap();
    }
    let config = user_config();
    let owner = crate::bootstrap::build_app_at(&config, &data.path().join("users/42"), false)
        .await
        .unwrap();
    let users = Arc::new(crate::users::UserSpaces::new(
        &config,
        data.path(),
        owner.clone(),
    ));
    let chat = Chat::new(&owner.agent_runner, data.path())
        .await
        .unwrap()
        .with_users(users);
    let auth = AuthStore::new(data.path());
    let mut clients = Vec::new();
    for user in 1..=17 {
        auth.grant_test_user(user).unwrap();
        let user_chat = chat.for_user(user).await.unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        user_chat
            .sessions
            .save(&Session::new(SessionId::new(&id)))
            .await
            .unwrap();
        clients.push((id, login_token(&auth, Identity::Telegram(user))));
    }
    let settings = Settings {
        trusted_proxies: Vec::new(),
        listen: "127.0.0.1:0".parse().unwrap(),
        base_url: "http://127.0.0.1/".parse().unwrap(),
    };
    let router = crate::web::server::router_with_chat(settings, data.path(), Some(chat));
    let mut streams = Vec::new();
    for (id, token) in &clients[..16] {
        for _ in 0..4 {
            let response = open_events(&router, id, token).await;
            assert_eq!(response.status(), StatusCode::OK);
            streams.push(response);
        }
    }
    let (id, token) = &clients[16];
    assert_eq!(
        open_events(&router, id, token).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/auth/login")
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    streams.pop();
    assert_eq!(
        open_events(&router, id, token).await.status(),
        StatusCode::OK
    );
}
