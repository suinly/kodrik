use super::*;
use crate::agent::tools::{Tool, webhooks::WebhooksTool};
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use tower::ServiceExt;

impl Fixture {
    fn tool(&self) -> WebhooksTool {
        WebhooksTool::new(
            (*self.chat.sessions).clone(),
            "http://localhost/kodrik/".parse().unwrap(),
        )
    }
}

#[tokio::test]
async fn webhook_tool_and_http_share_settings_tokens_and_accepted_work() {
    let f = Fixture::new().await;
    f.chat.runner.suspend();
    let tool = f.tool();
    let mut hook = tool
        .execute(json!({"action":"create","name":"  Monitor  ","instruction":"Investigate errors"}))
        .await
        .unwrap()
        .output;
    let id = hook["id"].as_str().unwrap().to_owned();
    assert_eq!(hook["name"], "Monitor");
    assert_eq!(hook["enabled"], true);
    assert_eq!(
        hook["url"],
        format!("http://localhost/kodrik/api/webhooks/{id}")
    );
    let (status, listed) = f.manage("GET", "", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed[0]["id"], id);
    assert!(listed[0].get("token").is_none());
    let changed = tool
        .execute(json!({"action":"update","id":id,"enabled":false}))
        .await
        .unwrap()
        .output;
    assert_eq!(changed["enabled"], false);
    assert_eq!(changed["instruction"], "Investigate errors");
    assert_eq!(
        f.deliver(&hook, "disabled", "error").await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.manage(
            "PATCH",
            &format!("/{id}"),
            json!({"name":"Renamed","instruction":"New instruction","enabled":true})
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let saved = f
        .tool()
        .execute(json!({"action":"get","id":id}))
        .await
        .unwrap()
        .output;
    assert_eq!(saved["name"], "Renamed");
    assert!(saved.get("token").is_none());
    tool.execute(json!({"action":"update","id":id,"name":"  Agent edit  ","instruction":"Follow the owner","enabled":null})).await.unwrap();
    let (_, listed) = f.manage("GET", "", Value::Null).await;
    assert_eq!(listed[0]["name"], "Agent edit");
    assert_eq!(listed[0]["instruction"], "Follow the owner");
    assert_eq!(listed[0]["enabled"], true);
    let rotated = tool
        .execute(json!({"action":"rotate_token","id":id}))
        .await
        .unwrap()
        .output;
    assert_ne!(hook["token"], rotated["token"]);
    assert_eq!(
        f.deliver(&hook, "revoked", "error").await.0,
        StatusCode::UNAUTHORIZED
    );
    hook["token"] = rotated["token"].clone();
    let (status, accepted) = f.deliver(&hook, "new-token", "error").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let events = tool
        .execute(json!({"action":"events","id":id}))
        .await
        .unwrap()
        .output;
    assert_eq!(
        events["events"][0]["conversation_id"],
        accepted["conversation_id"]
    );
    let deleted = tool
        .execute(json!({"action":"delete","id":id}))
        .await
        .unwrap()
        .output;
    assert_eq!(deleted["deleted"], true);
    assert_eq!(
        f.deliver(&hook, "deleted", "error").await.0,
        StatusCode::UNAUTHORIZED
    );
    assert!(
        tool.execute(json!({"action":"list"})).await.unwrap().output["webhooks"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    use crate::agent::session::SessionStore;
    assert!(
        f.chat
            .sessions
            .load(&SessionId::new(
                accepted["conversation_id"].as_str().unwrap()
            ))
            .await
            .unwrap()
            .is_some()
    );
    for action in ["get", "update", "rotate_token", "delete"] {
        let error = tool.execute(json!({"action":action,"id":id,"name":if action=="update" {Some("Gone")} else {None}})).await.err().unwrap();
        assert!(error.to_string().contains("Webhook not found"), "{error:#}");
    }
}

#[tokio::test]
async fn webhook_tool_validates_arguments_and_shares_the_http_limit() {
    let f = Fixture::new().await;
    let tool = f.tool();
    for args in [
        json!({"action":"create","name":" ","instruction":"Do it"}),
        json!({"action":"create","name":"x".repeat(201),"instruction":"Do it"}),
        json!({"action":"create","name":"Monitor","instruction":" "}),
        json!({"action":"create","name":"Monitor","instruction":"x".repeat(16385)}),
        json!({"action":"create","name":"Monitor"}),
        json!({"action":"create","name":"Monitor","instruction":"Do it","id":"chosen-id"}),
        json!({"action":"list","enabled":false}),
        json!({"action":"delete"}),
        json!({"action":"update","id":"missing"}),
        json!({"action":"list","token":"secret"}),
    ] {
        assert!(tool.execute(args.clone()).await.is_err(), "{args}");
    }
    let hook = f.create().await;
    for changes in [
        json!({"name":" "}),
        json!({"instruction":"x".repeat(16385)}),
    ] {
        let mut args = changes;
        args["action"] = "update".into();
        args["id"] = hook["id"].clone();
        assert!(tool.execute(args).await.is_err());
    }
    for _ in 1..32 {
        tool.execute(json!({"action":"create","name":"Monitor","instruction":"Investigate"}))
            .await
            .unwrap();
    }
    assert!(
        tool.execute(json!({"action":"create","name":"Overflow","instruction":"Investigate"}))
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("Webhook limit reached")
    );
    assert_eq!(
        f.manage(
            "POST",
            "",
            json!({"name":"Overflow","instruction":"Investigate"})
        )
        .await
        .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    let reopened = SqliteSessionStore::open_async(f._data.path().join("owner-sessions"))
        .await
        .unwrap();
    let restored = WebhooksTool::new(reopened, "http://localhost/kodrik/".parse().unwrap());
    assert_eq!(
        restored
            .execute(json!({"action":"list"}))
            .await
            .unwrap()
            .output["webhooks"]
            .as_array()
            .unwrap()
            .len(),
        32
    );
}

#[tokio::test]
async fn webhook_tool_rejects_guests_and_groups_even_when_called_directly() {
    use crate::agent::tools::{
        ReplayPolicy, ToolRegistry,
        telegram_reply::{REPLY_CONTEXT, ReplyContext},
    };
    let f = Fixture::new().await;
    assert_eq!(f.tool().replay_policy(), ReplayPolicy::Never);
    let registry = ToolRegistry::default().register(Arc::new(f.tool()));
    for (chat, guest) in [("42", Some("query".into())), ("-10042", None)] {
        let interface = Arc::new(crate::TelegramInterface::new(
            "main",
            "unused",
            f._data.path().join("attachments"),
        ));
        REPLY_CONTEXT
            .scope(ReplyContext::new(interface, chat.into(), guest), async {
                assert!(registry.list().is_empty());
                assert!(
                    registry
                        .execute("webhooks", json!({"action":"list"}))
                        .await
                        .is_err()
                );
                assert!(
                    f.tool()
                        .execute(json!({"action":"create","name":"Forbidden","instruction":"No"}))
                        .await
                        .is_err()
                );
            })
            .await;
    }
    assert!(
        f.tool()
            .execute(json!({"action":"list"}))
            .await
            .unwrap()
            .output["webhooks"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn messages_web_notification_is_visible_after_completed_run() {
    use crate::agent::{
        AssistantMessage,
        message::Message,
        run_state::{Checkpoint, RunPhase},
    };
    let f = Fixture::new().await;
    let mut session = Session::new(SessionId::new(uuid::Uuid::new_v4().to_string()));
    session.push(Message::Assistant(AssistantMessage {
        content: "Original answer".into(),
    }));
    f.chat.sessions.save(&session).await.unwrap();
    let mut checkpoint = Checkpoint::new(&f.chat.sessions, session.id(), &session);
    checkpoint
        .save(&session, RunPhase::Completed, None)
        .await
        .unwrap();
    let receipt = f
        .chat
        .sessions
        .queue_notification(
            &checkpoint.record.id,
            "notification",
            session.id().as_str(),
            "web",
            session.id().as_str(),
            "New notification".into(),
        )
        .await
        .unwrap();
    f.chat
        .sessions
        .deliver_web_notification(&receipt)
        .await
        .unwrap()
        .unwrap();
    let snapshot = f.chat.snapshot(session.id().as_str()).await.unwrap();
    let texts: Vec<_> = snapshot["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["parts"].as_array().unwrap())
        .filter_map(|part| part["text"].as_str())
        .collect();
    assert_eq!(texts, vec!["Original answer", "New notification"]);
}

struct Fixture {
    _data: tempfile::TempDir,
    chat: Chat,
    router: Router,
    cookie: String,
    guest: String,
}

impl Fixture {
    async fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let chat = crate::web::chat_tests::chat(data.path()).await;
        let settings = Settings {
            trusted_proxies: vec![],
            listen: "127.0.0.1:0".parse().unwrap(),
            base_url: "http://localhost/kodrik/".parse().unwrap(),
        };
        let auth = AuthStore::new(data.path());
        auth.configure_owner(Some(42)).unwrap();
        auth.grant_test_user(7).unwrap();
        let owner = auth
            .test_session(Identity::Telegram(42), chrono::Utc::now().timestamp())
            .unwrap()
            .unwrap();
        let guest = auth
            .test_session(Identity::Telegram(7), chrono::Utc::now().timestamp())
            .unwrap()
            .unwrap();
        let router =
            crate::web::server::router_with_chat(settings, data.path(), Some(chat.clone()));
        Self {
            _data: data,
            chat,
            router,
            cookie: format!("kodrik_web_session={owner}"),
            guest: format!("kodrik_web_session={guest}"),
        }
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: impl Into<Body>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(format!("/kodrik/api/webhooks{path}"));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = self
            .router
            .clone()
            .oneshot(request.body(body.into()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn manage(&self, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        self.request(
            method,
            path,
            &[
                ("cookie", &self.cookie),
                ("origin", "http://localhost"),
                ("content-type", "application/json"),
            ],
            body.to_string(),
        )
        .await
    }

    async fn create(&self) -> Value {
        let (status, hook) = self.manage("POST", "", json!({"name":"Site errors", "instruction":"Diagnose the incident and open a PR after tests pass."})).await;
        assert_eq!(status, StatusCode::CREATED, "{hook}");
        assert!(hook["token"].as_str().unwrap().len() >= 32);
        assert_eq!(
            hook["url"],
            format!(
                "http://localhost/kodrik/api/webhooks/{}",
                hook["id"].as_str().unwrap()
            )
        );
        hook
    }

    async fn idle(&self, id: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.chat.sessions.conversation_view(id).await.unwrap().busy
                || self.chat.runs.lock().await.contains(id)
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn reopen(&mut self) {
        self.chat = crate::web::chat_tests::chat(self._data.path()).await;
        self.router = crate::web::server::router_with_chat(
            Settings {
                trusted_proxies: vec![],
                listen: "127.0.0.1:0".parse().unwrap(),
                base_url: "http://localhost/kodrik/".parse().unwrap(),
            },
            self._data.path(),
            Some(self.chat.clone()),
        );
    }

    async fn deliver(&self, hook: &Value, key: &str, body: impl Into<Body>) -> (StatusCode, Value) {
        self.request(
            "POST",
            &format!("/{}", hook["id"].as_str().unwrap()),
            &[
                (
                    "authorization",
                    &format!("Bearer {}", hook["token"].as_str().unwrap()),
                ),
                ("idempotency-key", key),
            ],
            body,
        )
        .await
    }
}

#[tokio::test]
async fn webhook_owner_migration_preserves_endpoint_and_retry_history() {
    let mut f = Fixture::new().await;
    let auth = AuthStore::new(f._data.path());
    auth.configure_owner(None).unwrap();
    auth.set_password("migration-password").unwrap();
    let token = auth
        .login("migration-password", chrono::Utc::now().timestamp())
        .unwrap()
        .unwrap();
    f.cookie = format!("kodrik_web_session={token}");
    f.chat.runner.suspend();
    let hook = f.create().await;
    let (status, accepted) = f
        .deliver(&hook, "before-migration", "Checkout failed")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    tokio::time::timeout(Duration::from_secs(3), async {
        while !f.chat.runs.lock().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let Fixture {
        _data: data,
        chat,
        router,
        cookie,
        guest,
    } = f;
    drop(router);
    drop(chat);
    // The chat fixture uses owner-sessions; migrate through the production sessions path.
    std::fs::rename(
        data.path().join("owner-sessions"),
        data.path().join("sessions"),
    )
    .unwrap();
    let root = data.path().to_owned();
    let destination = tokio::task::spawn_blocking(move || {
        let config = toml::from_str("[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='test'\n[provider]").unwrap();
        crate::spaces::owner_data(&root, &config)
    }).await.unwrap().unwrap();
    std::fs::rename(
        destination.join("sessions"),
        destination.join("owner-sessions"),
    )
    .unwrap();
    AuthStore::new(data.path())
        .configure_owner(Some(42))
        .unwrap();
    let chat = crate::web::chat_tests::chat(&destination).await;
    let settings = Settings {
        trusted_proxies: vec![],
        listen: "127.0.0.1:0".parse().unwrap(),
        base_url: "http://localhost/kodrik/".parse().unwrap(),
    };
    let router = crate::web::server::router_with_chat(settings, data.path(), Some(chat.clone()));
    let f = Fixture {
        _data: data,
        chat,
        router,
        cookie,
        guest,
    };
    let (status, duplicate) = f
        .deliver(&hook, "before-migration", "Checkout failed")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{duplicate}");
    assert_eq!(duplicate["duplicate"], true);
    assert_eq!(duplicate["conversation_id"], accepted["conversation_id"]);
    assert_eq!(
        f.deliver(&hook, "before-migration", "Changed body").await.0,
        StatusCode::CONFLICT
    );
    let (status, fresh) = f.deliver(&hook, "after-migration", "New incident").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(fresh["duplicate"], false);
    let (status, hooks) = f.manage("GET", "", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hooks[0]["name"], "Site errors");
    assert_eq!(
        hooks[0]["instruction"],
        "Diagnose the incident and open a PR after tests pass."
    );
}

#[tokio::test]
async fn webhook_management_requires_owner_and_same_origin() {
    let f = Fixture::new().await;
    assert_eq!(
        f.request("GET", "", &[], "").await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.request("GET", "", &[("cookie", &f.guest)], "").await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.request(
            "POST",
            "",
            &[("cookie", &f.cookie), ("content-type", "application/json")],
            r#"{"name":"test","instruction":"test"}"#
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let hook = f.create().await;
    let (status, listed) = f.manage("GET", "", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed[0]["id"], hook["id"]);
    assert!(listed[0].get("token").is_none());
    assert!(listed[0].get("token_hash").is_none());
}

#[tokio::test]
async fn webhook_bearer_is_required_before_reading_body_or_creating_work() {
    let f = Fixture::new().await;
    let hook = f.create().await;
    let path = format!("/{}", hook["id"].as_str().unwrap());
    for headers in [
        vec![],
        vec![("cookie", f.cookie.as_str())],
        vec![("authorization", "Bearer wrong")],
    ] {
        assert_eq!(
            f.request("POST", &path, &headers, "x".repeat(70 * 1024))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert!(f.chat.sessions.list().await.unwrap().is_empty());
    assert_eq!(
        f.deliver(&hook, "large", "x".repeat(64 * 1024 + 1)).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        f.deliver(&hook, "binary", vec![0xff, 0xfe]).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.deliver(&hook, "empty", " ").await.0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn webhook_conversations_are_filtered_before_paging_and_keep_their_source() {
    async fn get(f: &Fixture, path: &str) -> Value {
        let token = AuthStore::new(f._data.path())
            .test_session(Identity::Owner, chrono::Utc::now().timestamp())
            .unwrap()
            .unwrap();
        let response = f
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/kodrik/api/chat/{path}"))
                    .header("cookie", format!("kodrik_web_session={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap()
    }
    let f = Fixture::new().await;
    let mut normal = Session::new(SessionId::new(uuid::Uuid::new_v4().to_string()));
    normal.push(crate::agent::message::Message::User(
        crate::agent::UserMessage {
            content: "KOD-2-event ordinary conversation".into(),
            images: vec![],
            files: vec![],
        },
    ));
    f.chat.sessions.save(&normal).await.unwrap();
    let hook = f.create().await;
    let body = r#"{"event":"KOD-2-event","items":[1,2]}"#;
    let (status, delivery) = f.deliver(&hook, "source-test", body).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = delivery["conversation_id"].as_str().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while f.chat.sessions.conversation_view(id).await.unwrap().busy {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let page = get(&f, "sessions?limit=1").await;
    assert_eq!(page.as_array().unwrap().len(), 1);
    assert_eq!(page[0]["id"], normal.id().as_str());
    assert!(
        get(&f, "sessions?limit=1&offset=1")
            .await
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        get(&f, "sessions?q=items")
            .await
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        get(&f, "sessions?webhooks=true&limit=1&offset=1")
            .await
            .as_array()
            .unwrap()
            .is_empty()
    );
    let found = get(&f, "sessions?webhooks=true&q=KOD-2-event").await;
    assert_eq!(found.as_array().unwrap().len(), 1);
    assert_eq!(found[0]["id"], id);
    assert_eq!(
        found[0]["webhook"],
        json!({"id":hook["id"],"name":"Site errors"})
    );
    assert_eq!(
        f.manage(
            "DELETE",
            &format!("/{}", hook["id"].as_str().unwrap()),
            Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let shown = get(&f, &format!("sessions/{id}")).await;
    assert_eq!(shown["webhook"], found[0]["webhook"]);
    assert_eq!(shown["messages"][0]["parts"][0]["text"], body);
    assert_eq!(shown["messages"][0]["parts"][0]["format"], "code");
    let mut session = f.chat.sessions.conversation_view(id).await.unwrap().session;
    session.push(crate::agent::message::Message::User(
        crate::agent::UserMessage {
            content: "**follow up**".into(),
            files: vec![],
            images: vec![],
        },
    ));
    f.chat.sessions.save(&session).await.unwrap();
    let snapshot = f.chat.snapshot(id).await.unwrap();
    assert_eq!(snapshot["messages"][0]["parts"][0]["format"], "code");
    assert!(
        snapshot["messages"].as_array().unwrap().last().unwrap()["parts"][0]["format"].is_null()
    );
}

#[tokio::test]
async fn webhook_deliveries_share_fixed_window() {
    let f = Fixture::new().await;
    f.chat.runner.suspend();
    let hook = f.create().await;
    let ((first_status, first), (second_status, second)) = tokio::join!(
        f.deliver(&hook, "first", "first event"),
        f.deliver(&hook, "second", "second event")
    );
    assert_eq!(first_status, StatusCode::ACCEPTED, "{first}");
    assert_eq!(second_status, StatusCode::ACCEPTED, "{second}");
    assert_eq!(first["conversation_id"], second["conversation_id"]);
    assert_eq!(first["duplicate"], false);
    assert_eq!(second["duplicate"], false);
    assert_eq!(f.chat.sessions.list().await.unwrap().len(), 1);
    let (receipts, submissions): (i64, i64) = f
        .chat
        .sessions
        .with_connection(|db| {
            Ok((
                db.query_row("SELECT count(*) FROM webhook_receipts", [], |r| r.get(0))?,
                db.query_row("SELECT count(*) FROM submissions", [], |r| r.get(0))?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(receipts, 2);
    assert_eq!(submissions, 2);
}

#[tokio::test]
async fn webhook_window_boundary_is_fixed_at_the_first_receipt() {
    let f = Fixture::new().await;
    f.chat.runner.suspend();
    let hook = f.create().await;
    let (_, first) = f.deliver(&hook, "first", "first event").await;
    let hook_id = hook["id"].as_str().unwrap().to_owned();
    let instruction = hook["instruction"].as_str().unwrap().to_owned();
    let expected = first["conversation_id"].as_str().unwrap().to_owned();
    f.chat
        .sessions
        .with_connection(move |db| {
            let opened: String =
                db.query_row("SELECT accepted_at FROM webhook_receipts", [], |row| {
                    row.get(0)
                })?;
            let opened = chrono::DateTime::parse_from_rfc3339(&opened)?.with_timezone(&chrono::Utc);
            assert_eq!(
                super::webhooks::window_session(db, &hook_id, &instruction, opened)?,
                Some(expected.clone())
            );
            assert_eq!(
                super::webhooks::window_session(
                    db,
                    &hook_id,
                    &instruction,
                    opened + chrono::Duration::minutes(5) - chrono::Duration::nanoseconds(1)
                )?,
                Some(expected)
            );
            assert_eq!(
                super::webhooks::window_session(
                    db,
                    &hook_id,
                    &instruction,
                    opened + chrono::Duration::minutes(5)
                )?,
                None
            );
            assert_eq!(
                super::webhooks::window_session(
                    db,
                    &hook_id,
                    &instruction,
                    opened - chrono::Duration::nanoseconds(1)
                )?,
                None
            );
            db.execute(
                "UPDATE webhook_receipts SET accepted_at=?1",
                [(chrono::Utc::now() - chrono::Duration::minutes(4)).to_rfc3339()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (_, second) = f.deliver(&hook, "second", "second event").await;
    assert_eq!(first["conversation_id"], second["conversation_id"]);
    f.chat
        .sessions
        .with_connection(|db| {
            db.execute(
                "UPDATE webhook_receipts SET accepted_at=?1 WHERE request_key='first'",
                [(chrono::Utc::now() - chrono::Duration::minutes(6)).to_rfc3339()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (_, third) = f.deliver(&hook, "third", "third event").await;
    assert_ne!(first["conversation_id"], third["conversation_id"]);
    let (status, duplicate) = f.deliver(&hook, "first", "first event").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(duplicate["conversation_id"], first["conversation_id"]);
    assert_eq!(duplicate["duplicate"], true);
}

#[tokio::test]
async fn webhook_windows_separate_hooks_and_changed_instructions() {
    let f = Fixture::new().await;
    f.chat.runner.suspend();
    let first_hook = f.create().await;
    let second_hook = f.create().await;
    let (_, first) = f.deliver(&first_hook, "one", "event").await;
    let (_, other) = f.deliver(&second_hook, "one", "event").await;
    assert_ne!(first["conversation_id"], other["conversation_id"]);
    let (status, _) = f
        .manage(
            "PATCH",
            &format!("/{}", first_hook["id"].as_str().unwrap()),
            json!({"name":"Site errors","instruction":"Use the new instruction"}),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, changed) = f.deliver(&first_hook, "two", "event").await;
    assert_ne!(first["conversation_id"], changed["conversation_id"]);
    let (status, duplicate) = f.deliver(&first_hook, "one", "event").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(duplicate["conversation_id"], first["conversation_id"]);
}

#[tokio::test]
async fn webhook_grouped_events_recover_with_individual_statuses_and_raw_bodies() {
    let mut f = Fixture::new().await;
    let hook = f.create().await;
    let (status, first) = f.deliver(&hook, "first", "**First raw payload**").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = first["conversation_id"].as_str().unwrap().to_owned();
    f.idle(&id).await;
    f.chat.runner.suspend();
    let (status, second) = f.deliver(&hook, "second", "/stop").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(second["conversation_id"], first["conversation_id"]);
    let (_, events) = f
        .manage(
            "GET",
            &format!("/{}/events", hook["id"].as_str().unwrap()),
            Value::Null,
        )
        .await;
    assert_eq!(events[0]["status"], "Queued");
    assert_eq!(events[1]["status"], "Done");
    let queued = f.chat.snapshot(&id).await.unwrap();
    assert_eq!(queued["queued"][0]["webhook"], true);
    assert_eq!(queued["queued"][0]["text"], "/stop");
    f.reopen().await;
    f.idle(&id).await;
    let (_, third) = f.deliver(&hook, "third", "Third raw payload").await;
    assert_eq!(third["conversation_id"], first["conversation_id"]);
    f.idle(&id).await;
    f.chat
        .runner
        .run(
            SessionId::new(&id),
            crate::gateway::IncomingMessage::new(
                crate::gateway::InterfaceId("web".into()),
                &id,
                "**Owner follow-up**",
            ),
            None,
        )
        .await
        .unwrap();
    let view = f.chat.sessions.conversation_view(&id).await.unwrap();
    let builder = crate::agent::ContextBuilder::new("base");
    let context = builder.build(&view.session);
    let event_count = context.messages.iter().filter(|message| matches!(message,
        crate::agent::message::Message::User(input) if input.content.starts_with("[Webhook event] "))).count();
    assert_eq!(event_count, 3);
    assert!(
        matches!(context.messages.iter().rev().find(|message| matches!(message, crate::agent::message::Message::User(_))).unwrap(), crate::agent::message::Message::User(input) if input.content=="**Owner follow-up**")
    );
    let snapshot = f.chat.snapshot(&id).await.unwrap();
    let users: Vec<_> = snapshot["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .collect();
    assert_eq!(users.len(), 4);
    for (message, text) in
        users[..3]
            .iter()
            .zip(["**First raw payload**", "/stop", "Third raw payload"])
    {
        assert_eq!(message["parts"][0]["text"], text);
        assert_eq!(message["parts"][0]["format"], "code");
    }
    assert!(users[3]["parts"][0]["format"].is_null());
}

#[tokio::test]
async fn webhook_full_shared_inbox_returns_429_without_losing_retries() {
    use crate::agent::inbox::{BusyMode, MAX_QUEUED_SUBMISSIONS, SubmissionRequest};
    let f = Fixture::new().await;
    f.chat.runner.suspend();
    let hook = f.create().await;
    let (_, first) = f.deliver(&hook, "first", "event").await;
    let id = SessionId::new(first["conversation_id"].as_str().unwrap());
    for _ in 1..MAX_QUEUED_SUBMISSIONS {
        let message = crate::gateway::IncomingMessage::new(
            crate::gateway::InterfaceId("web".into()),
            id.as_str(),
            "queued owner follow-up",
        );
        f.chat
            .runner
            .submit(SubmissionRequest::new(&id, message, BusyMode::FollowUp))
            .await
            .unwrap();
    }
    assert_eq!(
        f.deliver(&hook, "full", "new event").await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    let (status, duplicate) = f.deliver(&hook, "first", "event").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(duplicate["duplicate"], true);
    let (_, events) = f
        .manage(
            "GET",
            &format!("/{}/events", hook["id"].as_str().unwrap()),
            Value::Null,
        )
        .await;
    assert_eq!(events.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn webhook_shared_session_busy_in_another_channel_returns_conflict() {
    use crate::agent::inbox::{BusyMode, SubmissionRequest};
    let f = Fixture::new().await;
    let hook = f.create().await;
    let (_, first) = f.deliver(&hook, "first", "event").await;
    let id = first["conversation_id"].as_str().unwrap();
    f.idle(id).await;
    let channel = SessionId::new("telegram:test:42");
    f.chat
        .sessions
        .select(&channel, &SessionId::new(id))
        .await
        .unwrap();
    let message = crate::gateway::IncomingMessage::new(
        crate::gateway::InterfaceId("telegram:test".into()),
        "42",
        "queued owner question",
    );
    f.chat
        .runner
        .submit(SubmissionRequest::new(
            &channel,
            message,
            BusyMode::FollowUp,
        ))
        .await
        .unwrap();
    assert_eq!(
        f.deliver(&hook, "busy", "new event").await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.deliver(&hook, "first", "event").await.0,
        StatusCode::ACCEPTED
    );
}

#[tokio::test]
async fn webhook_window_keeps_its_original_session_after_selection_changes() {
    let f = Fixture::new().await;
    let hook = f.create().await;
    let (_, first) = f.deliver(&hook, "first", "first event").await;
    let id = first["conversation_id"].as_str().unwrap();
    f.idle(id).await;
    let other =
        crate::agent::session::Session::new(SessionId::new(uuid::Uuid::new_v4().to_string()));
    f.chat.sessions.save(&other).await.unwrap();
    f.chat
        .sessions
        .select(&SessionId::new(id), other.id())
        .await
        .unwrap();
    let (status, next) = f.deliver(&hook, "next", "next event").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(next["conversation_id"], first["conversation_id"]);
    f.idle(id).await;
    let original = f
        .chat
        .sessions
        .load(&SessionId::new(id))
        .await
        .unwrap()
        .unwrap();
    assert!(original.messages().iter().any(|message| matches!(message,
        crate::agent::message::Message::User(input) if input.content=="next event")));
    assert!(
        f.chat
            .sessions
            .load(other.id())
            .await
            .unwrap()
            .unwrap()
            .messages()
            .is_empty()
    );
}

#[tokio::test]
async fn webhook_arbitrary_body_runs_once_and_changed_retry_conflicts() {
    let f = Fixture::new().await;
    let hook = f.create().await;
    let body = "checkout failed\nstack trace: arbitrary text, no JSON schema";
    let ((status, first), (other_status, second)) = tokio::join!(
        f.deliver(&hook, "incident-1", body),
        f.deliver(&hook, "incident-1", body)
    );
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    assert_eq!(other_status, StatusCode::ACCEPTED, "{second}");
    assert_eq!(first["conversation_id"], second["conversation_id"]);
    assert_ne!(first["duplicate"], second["duplicate"]);
    assert_eq!(
        f.deliver(&hook, "incident-1", "different error").await.0,
        StatusCode::CONFLICT
    );
    let id = first["conversation_id"].as_str().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let view = f.chat.sessions.conversation_view(id).await.unwrap();
            if !view.busy && view.session.messages().len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let view = f.chat.sessions.conversation_view(id).await.unwrap();
    let crate::agent::message::Message::User(input) = &view.session.messages()[0] else {
        panic!("missing raw event")
    };
    assert_eq!(input.content, body);
    let context = crate::agent::ContextBuilder::new("base")
        .build(&view.session)
        .instructions
        .into_owned();
    assert!(context.contains(r#""webhook_name":"Site errors""#));
    assert!(context.contains(&format!(
        r#""webhook_id":"{}""#,
        hook["id"].as_str().unwrap()
    )));
    assert!(context.contains(r#""received_at":""#));
    assert!(!context.contains(body));
    let (status, events) = f
        .manage(
            "GET",
            &format!("/{}/events", hook["id"].as_str().unwrap()),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(events.as_array().unwrap().len(), 1);
    assert_eq!(events[0]["conversation_id"], id);
    assert_eq!(events[0]["status"], "Done");
}

#[tokio::test]
async fn webhook_rotation_disable_edit_and_delete_apply_immediately() {
    let f = Fixture::new().await;
    let mut hook = f.create().await;
    let path = format!("/{}", hook["id"].as_str().unwrap());
    let (status, rotated) = f
        .manage("POST", &format!("{path}/token"), Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        f.deliver(&hook, "old", "error").await.0,
        StatusCode::UNAUTHORIZED
    );
    hook["token"] = rotated["token"].clone();
    assert_eq!(
        f.manage(
            "PATCH",
            &path,
            json!({"name":"Renamed", "instruction":"New instruction", "enabled":false})
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        f.deliver(&hook, "disabled", "error").await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.manage(
            "PATCH",
            &path,
            json!({"name":"Renamed", "instruction":"New instruction", "enabled":true})
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        f.deliver(&hook, "enabled", "error").await.0,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        f.manage("DELETE", &path, Value::Null).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        f.deliver(&hook, "deleted", "error").await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn webhook_rate_limit_and_queue_limit_do_not_reject_identical_retries() {
    let f = Fixture::new().await;
    f.chat.runner.suspend();
    let hook = f.create().await;
    for number in 0..10 {
        assert_eq!(
            f.deliver(&hook, &format!("event-{number}"), "error")
                .await
                .0,
            StatusCode::ACCEPTED
        );
    }
    assert_eq!(
        f.deliver(&hook, "too-many", "error").await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        f.deliver(&hook, "event-0", "error").await.0,
        StatusCode::ACCEPTED
    );
    let other = f.create().await;
    {
        let mut runs = f.chat.runs.lock().await;
        for number in 0..100 {
            runs.insert(format!("reserved-{number}"));
        }
    }
    assert_eq!(
        f.deliver(&other, "full", "error").await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        f.deliver(&hook, "event-0", "error").await.0,
        StatusCode::ACCEPTED
    );
}

#[tokio::test]
async fn webhook_accepted_work_recovers_after_settings_are_changed_and_deleted() {
    let f = Fixture::new().await;
    f.chat.runner.suspend();
    let hook = f.create().await;
    let (_, delivery) = f.deliver(&hook, "recover", "/stop").await;
    let id = delivery["conversation_id"].as_str().unwrap().to_owned();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !f.chat.runs.lock().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let path = format!("/{}", hook["id"].as_str().unwrap());
    f.manage(
        "PATCH",
        &path,
        json!({"name":"Changed", "instruction":"Different instruction", "enabled":false}),
    )
    .await;
    assert_eq!(
        f.manage("DELETE", &path, Value::Null).await.0,
        StatusCode::NO_CONTENT
    );
    let recovered = crate::web::chat_tests::chat(f._data.path()).await;
    let view = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let view = recovered.sessions.conversation_view(&id).await.unwrap();
            if !view.busy && view.session.messages().len() >= 2 {
                break view;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let crate::agent::message::Message::User(input) = &view.session.messages()[0] else {
        panic!("missing event")
    };
    assert_eq!(input.content, "/stop");
    let context = crate::agent::ContextBuilder::new("base")
        .build(&view.session)
        .instructions
        .into_owned();
    assert!(context.contains("Diagnose the incident and open a PR after tests pass."));
    assert!(!context.contains("Different instruction"));
    assert!(context.contains(r#""webhook_name":"Site errors""#));
    assert!(context.contains(&format!(
        r#""webhook_id":"{}""#,
        hook["id"].as_str().unwrap()
    )));
    let run = view.run.unwrap().run;
    assert_eq!(run.model_limit, None);
    assert_eq!(run.deadline_at, None);
}

#[tokio::test]
async fn webhook_without_idempotency_key_groups_distinct_events_and_stores_no_token() {
    let f = Fixture::new().await;
    f.chat.runner.suspend();
    let hook = f.create().await;
    let path = format!("/{}", hook["id"].as_str().unwrap());
    let token = hook["token"].as_str().unwrap();
    let headers = [("authorization", format!("Bearer {token}"))];
    let (status, first) = f
        .request(
            "POST",
            &path,
            &[(headers[0].0, &headers[0].1)],
            r#"{"any":{"nested":[1,true,"自由"]}}"#,
        )
        .await;
    let (other_status, second) = f
        .request(
            "POST",
            &path,
            &[(headers[0].0, &headers[0].1)],
            r#"{"any":{"nested":[1,true,"自由"]}}"#,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(other_status, StatusCode::ACCEPTED);
    assert_eq!(first["conversation_id"], second["conversation_id"]);
    assert_eq!(first["duplicate"], false);
    assert_eq!(second["duplicate"], false);
    let stored: String = f
        .chat
        .sessions
        .with_connection(|db| {
            Ok(db.query_row("SELECT token_hash FROM webhooks", [], |row| row.get(0))?)
        })
        .await
        .unwrap();
    assert_eq!(stored.len(), 64);
    assert_ne!(stored, token);
}

#[tokio::test]
async fn webhook_rotation_while_reading_body_prevents_admission() {
    let f = Fixture::new().await;
    let hook = f.create().await;
    let (started, reading) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let body = Body::from_stream(futures::stream::once(async move {
        started.send(()).unwrap();
        released.await.unwrap();
        Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"error"))
    }));
    let delivery = f.deliver(&hook, "revoked-during-read", body);
    tokio::pin!(delivery);
    tokio::select! {
        _ = &mut delivery => panic!("body should be waiting"),
        result = reading => result.unwrap(),
    }
    assert_eq!(
        f.manage(
            "POST",
            &format!("/{}/token", hook["id"].as_str().unwrap()),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    release.send(()).unwrap();
    assert_eq!(delivery.await.0, StatusCode::UNAUTHORIZED);
    assert!(f.chat.sessions.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn webhook_receipt_prevents_reexecution_after_owner_deletes_conversation() {
    let f = Fixture::new().await;
    let hook = f.create().await;
    let (_, first) = f.deliver(&hook, "delete-history", "error").await;
    let id = first["conversation_id"].as_str().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while f.chat.runs.lock().await.contains(id) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let token = AuthStore::new(f._data.path())
        .test_session(Identity::Owner, chrono::Utc::now().timestamp())
        .unwrap()
        .unwrap();
    let response = f
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/kodrik/api/chat/sessions/{id}"))
                .header("cookie", format!("kodrik_web_session={token}"))
                .header("origin", "http://localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let (status, repeated) = f.deliver(&hook, "delete-history", "error").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(repeated["duplicate"], true);
    assert_eq!(repeated["conversation_id"], Value::Null);
    assert!(f.chat.sessions.list().await.unwrap().is_empty());
    let (status, fresh) = f.deliver(&hook, "fresh-after-delete", "new event").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_ne!(fresh["conversation_id"], first["conversation_id"]);
    f.idle(fresh["conversation_id"].as_str().unwrap()).await;
}
