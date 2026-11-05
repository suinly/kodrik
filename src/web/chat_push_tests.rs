use super::*;
use crate::{
    agent::{
        AssistantMessage,
        message::Message,
        session::{Session, SessionId, SessionStore},
    },
    web::auth::Identity,
};
use axum::{
    Router,
    body::{Body, Bytes},
    http::Request,
    routing::post,
};
use tower::ServiceExt;
use web_push_native::p256::{SecretKey, elliptic_curve::sec1::ToEncodedPoint};

fn subscription(endpoint: &str) -> Subscription {
    let secret = SecretKey::from_slice(&[7; 32]).unwrap();
    Subscription {
        endpoint: endpoint.into(),
        keys: Keys {
            p256dh: URL_SAFE_NO_PAD.encode(secret.public_key().to_encoded_point(false).as_bytes()),
            auth: URL_SAFE_NO_PAD.encode([8; 16]),
        },
        expiration_time: None,
    }
}

async fn fixture() -> (tempfile::TempDir, Chat, String) {
    let dir = tempfile::tempdir().unwrap();
    let settings = Settings {
        base_url: "https://example.com/kodrik/".parse().unwrap(),
        listen: "127.0.0.1:0".parse().unwrap(),
        trusted_proxies: vec![],
    };
    let auth = AuthStore::new(dir.path());
    let token = auth
        .test_session(Identity::Owner, chrono::Utc::now().timestamp())
        .unwrap()
        .unwrap();
    let chat = crate::web::chat_tests::chat(dir.path())
        .await
        .with_push(settings, auth)
        .await
        .unwrap();
    chat.runner.suspend();
    tokio::task::yield_now().await;
    (dir, chat, token)
}

async fn call(
    app: &Router,
    token: &str,
    method: &str,
    path: &str,
    body: serde_json::Value,
) -> Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(format!("/kodrik/api/chat/{path}"))
                .header(header::COOKIE, format!("kodrik_web_session={token}"))
                .header(header::ORIGIN, "https://example.com")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn approval_push_context_identifies_background_source_without_arguments() {
    use crate::agent::{
        message::ToolCall,
        run_state::{Checkpoint, RunPhase},
    };
    let (_dir, chat, _token) = fixture().await;
    let mut session = Session::new(SessionId::new("webhook-session"));
    session.webhook_instruction = Some(format!(
        "## Recorded delivery source\n{}\n\n## Owner instruction\nprivate instruction",
        json!({"webhook_name":"Incident monitor","webhook_id":"hook-id"})
    ));
    let call = ToolCall {
        call_id: "call".into(),
        name: "exec_command".into(),
        arguments: json!({"cmd":"secret command"}),
    };
    session.push(Message::ToolCall(call.clone()));
    let mut cp = Checkpoint::new(&chat.sessions, session.id(), &session);
    cp.record.pending_calls = vec![call.clone()];
    cp.save(
        &session,
        RunPhase::NeedTool {
            call_id: call.call_id.clone(),
        },
        None,
    )
    .await
    .unwrap();
    chat.sessions
        .record_command_audits(&cp.record, &[(call, "destructive".into())])
        .await
        .unwrap();
    let approval = chat
        .sessions
        .pending_command_approvals(session.id().as_str())
        .await
        .unwrap()
        .remove(0);
    let context = chat
        .approval_push_context(&approval, session.id().as_str())
        .await
        .unwrap();
    assert_eq!(
        context,
        json!({"source":"Webhook: Incident monitor (hook-id)","event":cp.record.id,"tool":"exec_command"})
    );
    assert!(!context.to_string().contains("secret"));
    assert!(!context.to_string().contains("private instruction"));
}

#[tokio::test]
async fn subscriptions_validate_endpoints_keys_and_session_ownership() {
    let (dir, chat, token) = fixture().await;
    let push = chat.push.as_ref().unwrap();
    let other = push
        .auth
        .test_session(Identity::Owner, chrono::Utc::now().timestamp())
        .unwrap()
        .unwrap();
    let app =
        crate::web::server::router_with_chat(push.settings.clone(), dir.path(), Some(chat.clone()));
    for endpoint in [
        "http://127.0.0.1/push",
        "https://localhost/push",
        "https://example.com/push",
        "https://evil.push.apple.com.evil.example/push",
        "https://fcm.googleapis.com:444/push",
        "https://user@fcm.googleapis.com/push",
    ] {
        assert_eq!(
            call(
                &app,
                &token,
                "POST",
                "push",
                json!({"subscription":subscription(endpoint),"locale":"ru"})
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    let mut invalid = subscription("https://fcm.googleapis.com/push/device");
    invalid.keys.auth = "bad".into();
    assert_eq!(
        call(
            &app,
            &token,
            "POST",
            "push",
            json!({"subscription":invalid,"locale":"ru"})
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    let endpoint = "https://fcm.googleapis.com/push/device";
    assert_eq!(
        call(
            &app,
            &token,
            "POST",
            "push",
            json!({"subscription":subscription(endpoint),"locale":"ru"})
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&app, &other, "DELETE", "push", json!({"endpoint":endpoint}))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(
            &app,
            &other,
            "POST",
            "push/presence",
            json!({"endpoint":endpoint,"tab_id":uuid::Uuid::new_v4().to_string(),"session_id":null})
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    let response = call(&app, &token, "GET", "push", json!(null)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let value: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 16384)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(value["endpoints"], json!([endpoint]));
    assert_eq!(
        URL_SAFE_NO_PAD
            .decode(value["public_key"].as_str().unwrap())
            .unwrap()
            .len(),
        65
    );
    assert_eq!(
        call(&app, &token, "DELETE", "push", json!({"endpoint":endpoint}))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    let count = chat
        .sessions
        .with_connection(|db| {
            Ok(
                db.query_row("SELECT count(*) FROM web_push_subscriptions", [], |r| {
                    r.get::<_, i64>(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(count, 0);
}

async fn seed(chat: &Chat, token: &str, endpoint: &str, event: &str, now: i64) {
    let sub = subscription(endpoint);
    let hash = token_hash(token);
    let binding = hash.clone();
    let registry_endpoint = endpoint.to_owned();
    chat.push
        .as_ref()
        .unwrap()
        .store
        .with_connection(move |db| {
            db.execute(
                "INSERT OR REPLACE INTO web_push_bindings VALUES(?1,?2)",
                params![registry_endpoint, binding],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let mut session = Session::new(SessionId::new("personal"));
    session.push(Message::Assistant(AssistantMessage {
        content: "Secret answer".into(),
    }));
    chat.sessions.save(&session).await.unwrap();
    let event = event.to_owned();
    chat.sessions.with_connection(move |db| {
        db.execute("INSERT INTO web_push_subscriptions VALUES(?1,?2,?3,'ru') ON CONFLICT(endpoint) DO UPDATE SET subscription=excluded.subscription",params![sub.endpoint,hash,serde_json::to_vec(&sub)?])?;
        db.execute("INSERT INTO web_push_queue(event_id,endpoint,session_id,next_attempt,created_at) VALUES(?1,?2,'personal',?3,?3)",params![event,sub.endpoint,now])?;
        Ok(())
    }).await.unwrap();
}

#[tokio::test]
async fn sender_uses_a_valid_vapid_subject_for_loopback_http_and_preserves_https() {
    use web_push_native::jwt_simple::prelude::{ECDSAP256PublicKeyLike, NoCustomClaims};

    let received = Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));
    let headers = received.clone();
    let app = Router::new().route(
        "/push",
        post(move |input: HeaderMap| {
            let headers = headers.clone();
            async move {
                headers
                    .lock()
                    .await
                    .push(input[header::AUTHORIZATION].to_str().unwrap().to_owned());
                StatusCode::CREATED
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/push", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    for (base_url, expected_subject) in [
        ("http://127.0.0.1:8080/", "https://github.com/suinly/kodrik"),
        (
            "http://localhost:8080/kodrik/",
            "https://github.com/suinly/kodrik",
        ),
        ("http://[::1]:8080/", "https://github.com/suinly/kodrik"),
        ("https://example.com/kodrik/", "https://example.com/kodrik/"),
    ] {
        let (_dir, chat, token) = fixture().await;
        let push = chat.push.as_ref().unwrap();
        let mut settings = push.settings.clone();
        settings.base_url = base_url.parse().unwrap();
        let auth = push.auth.clone();
        let chat = chat.with_push(settings, auth).await.unwrap();
        let now = chrono::Utc::now().timestamp();
        seed(&chat, &token, &endpoint, "reply", now).await;
        chat.push_tick(now).await.unwrap();
        let authorization = received.lock().await.pop().expect("Push was not sent");
        let jwt = authorization
            .strip_prefix("vapid t=")
            .unwrap()
            .split(", k=")
            .next()
            .unwrap();
        let claims = chat
            .push
            .as_ref()
            .unwrap()
            .key
            .public_key()
            .verify_token::<NoCustomClaims>(jwt, None)
            .unwrap();
        assert_eq!(
            claims.subject.as_deref(),
            Some(expected_subject),
            "base URL: {base_url}"
        );
    }
    server.abort();
}

#[tokio::test]
async fn sender_encrypts_generic_payload_retries_temporarily_and_removes_expired_subscriptions() {
    let (_dir, chat, token) = fixture().await;
    let received = Arc::new(tokio::sync::Mutex::new(vec![]));
    let status = Arc::new(std::sync::atomic::AtomicU16::new(503));
    let app = Router::new().route(
        "/push",
        post({
            let received = received.clone();
            let status = status.clone();
            move |headers: HeaderMap, body: Bytes| {
                let received = received.clone();
                let status = status.clone();
                async move {
                    received.lock().await.push((headers, body));
                    (
                        StatusCode::from_u16(status.load(std::sync::atomic::Ordering::Relaxed))
                            .unwrap(),
                        [(header::RETRY_AFTER, "60")],
                    )
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/push", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let now = chrono::Utc::now().timestamp();
    seed(&chat, &token, &endpoint, "one", now).await;
    chat.push_tick(now).await.unwrap();
    let next = chat
        .sessions
        .with_connection(|db| {
            Ok(db.query_row(
                "SELECT attempts,next_attempt FROM web_push_queue",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(next, (1, now + 60));
    chat.push_tick(now + 59).await.unwrap();
    assert_eq!(received.lock().await.len(), 1);
    status.store(201, std::sync::atomic::Ordering::Relaxed);
    chat.push_tick(now + 60).await.unwrap();
    assert_eq!(
        chat.sessions
            .with_connection(|db| Ok(db.query_row(
                "SELECT count(*) FROM web_push_queue",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
        0
    );
    let received_guard = received.lock().await;
    assert_eq!(received_guard[1].0["ttl"], "86340");
    let (headers, body) = &received_guard[0];
    assert_eq!(headers[header::CONTENT_ENCODING], "aes128gcm");
    assert!(
        headers[header::AUTHORIZATION]
            .to_str()
            .unwrap()
            .starts_with("vapid t=")
    );
    let secret = SecretKey::from_slice(&[7; 32]).unwrap();
    let payload =
        web_push_native::decrypt(body.to_vec(), &secret, &Auth::clone_from_slice(&[8; 16]))
            .unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(
        payload,
        json!({"session_id":"personal","event_id":"one","locale":"ru"})
    );
    drop(received_guard);
    seed(&chat, &token, &endpoint, "two", now + 61).await;
    status.store(410, std::sync::atomic::Ordering::Relaxed);
    chat.push_tick(now + 61).await.unwrap();
    assert_eq!(
        chat.sessions
            .with_connection(|db| Ok(db.query_row(
                "SELECT count(*) FROM web_push_subscriptions",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
        0
    );
    server.abort();
}

#[tokio::test]
async fn approval_push_retries_only_while_the_request_is_pending() {
    use crate::agent::{
        command_audit::CommandAuditMode,
        message::ToolCall,
        run_state::{Checkpoint, RunPhase},
    };
    for resolution in ["approve", "reject", "stop", "trusted"] {
        let (_dir, chat, token) = fixture().await;
        let now = chrono::Utc::now().timestamp();
        seed(&chat, &token, "http://127.0.0.1:1/push", "initial", now).await;
        chat.sessions
            .with_connection(|db| {
                db.execute("DELETE FROM web_push_queue", [])?;
                Ok(())
            })
            .await
            .unwrap();
        let mut session = chat
            .sessions
            .load(&SessionId::new("personal"))
            .await
            .unwrap()
            .unwrap();
        let call = ToolCall {
            call_id: "approval".into(),
            name: "exec_command".into(),
            arguments: json!({"cmd":"secret command"}),
        };
        session.push(Message::ToolCall(call.clone()));
        let mut cp = Checkpoint::new(&chat.sessions, session.id(), &session);
        cp.record.pending_calls = vec![call.clone()];
        cp.save(
            &session,
            RunPhase::NeedTool {
                call_id: call.call_id.clone(),
            },
            None,
        )
        .await
        .unwrap();
        chat.sessions
            .set_command_audit_mode("personal", CommandAuditMode::Ask)
            .await
            .unwrap();
        chat.sessions
            .record_command_audits(&cp.record, &[(call, "ask".into())])
            .await
            .unwrap();
        let approval = chat
            .sessions
            .pending_command_approvals("personal")
            .await
            .unwrap()
            .remove(0);
        chat.push_tick(now).await.unwrap();
        assert_eq!(
            chat.sessions
                .with_connection(|db| Ok(db.query_row(
                    "SELECT attempts FROM web_push_queue",
                    [],
                    |r| r.get::<_, i64>(0)
                )?))
                .await
                .unwrap(),
            1
        );
        match resolution {
            "stop" => cp.save(&session, RunPhase::Cancelled, None).await.unwrap(),
            "trusted" => chat
                .sessions
                .set_command_audit_mode("personal", CommandAuditMode::Trusted)
                .await
                .unwrap(),
            _ => chat
                .sessions
                .decide_command_approval("personal", &approval.id, resolution == "approve")
                .await
                .unwrap(),
        }
        chat.push_tick(now + 6).await.unwrap();
        assert_eq!(
            chat.sessions
                .with_connection(|db| Ok(db.query_row(
                    "SELECT count(*) FROM web_push_queue",
                    [],
                    |r| r.get::<_, i64>(0)
                )?))
                .await
                .unwrap(),
            0,
            "{resolution}"
        );
    }
}

#[tokio::test]
async fn visible_conversation_is_suppressed_but_presence_expires_and_logout_clears_queue() {
    let (_dir, chat, token) = fixture().await;
    let now = chrono::Utc::now().timestamp();
    let endpoint = "http://127.0.0.1:1/push";
    seed(&chat, &token, endpoint, "visible", now).await;
    let presence_hash = token_hash(&token);
    chat.sessions
        .with_connection(move |db| {
            db.execute(
                "INSERT INTO web_push_presence VALUES(?1,'tab','personal',?2)",
                params![presence_hash, now],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    chat.push_tick(now).await.unwrap();
    assert_eq!(
        chat.sessions
            .with_connection(|db| Ok(db.query_row(
                "SELECT count(*) FROM web_push_queue",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
        0
    );
    seed(&chat, &token, endpoint, "expired-presence", now + 46).await;
    chat.push_tick(now + 46).await.unwrap();
    assert_eq!(
        chat.sessions
            .with_connection(|db| Ok(db.query_row(
                "SELECT attempts FROM web_push_queue",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
        1
    );
    chat.push.as_ref().unwrap().auth.logout(&token).unwrap();
    chat.push_tick(now + 47).await.unwrap();
    assert_eq!(
        chat.sessions
            .with_connection(|db| Ok(db.query_row(
                "SELECT count(*) FROM web_push_queue",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn foreground_presence_suppresses_push_on_all_devices() {
    let (_dir, chat, token) = fixture().await;
    let now = chrono::Utc::now().timestamp();
    let foreground = "http://127.0.0.1:1/push";
    let background = "http://127.0.0.1:2/push";
    seed(&chat, &token, foreground, "reply", now).await;
    seed(&chat, &token, background, "reply", now).await;
    let presence_hash = token_hash(&token);
    chat.sessions
        .with_connection(move |db| {
            db.execute(
                "INSERT INTO web_push_presence VALUES(?1,'tab','personal',?2)",
                params![presence_hash, now],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    chat.push_tick(now).await.unwrap();
    let queued = chat
        .sessions
        .with_connection(|db| {
            Ok(db
                .prepare("SELECT endpoint,attempts FROM web_push_queue")?
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap();
    assert!(
        queued.is_empty(),
        "Push remained queued for another device: {queued:?}"
    );
}

#[tokio::test]
async fn a_viewing_device_without_push_suppresses_other_devices_until_logout() {
    let (dir, chat, token) = fixture().await;
    let push = chat.push.as_ref().unwrap();
    let viewer = push
        .auth
        .test_session(Identity::Owner, chrono::Utc::now().timestamp())
        .unwrap()
        .unwrap();
    let app =
        crate::web::server::router_with_chat(push.settings.clone(), dir.path(), Some(chat.clone()));
    let now = chrono::Utc::now().timestamp();
    let endpoint = "http://127.0.0.1:1/push";
    seed(&chat, &token, endpoint, "reply", now).await;
    assert_eq!(
        call(
            &app,
            &viewer,
            "POST",
            "push/presence",
            json!({"tab_id":uuid::Uuid::new_v4().to_string(),"session_id":"personal"})
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    chat.push_tick(now).await.unwrap();
    assert_eq!(
        chat.sessions
            .with_connection(|db| Ok(db.query_row(
                "SELECT count(*) FROM web_push_queue",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
        0
    );
    push.auth.logout(&viewer).unwrap();
    seed(&chat, &token, endpoint, "after-logout", now).await;
    chat.push_tick(now).await.unwrap();
    assert_eq!(
        chat.sessions
            .with_connection(|db| Ok(db.query_row(
                "SELECT attempts FROM web_push_queue",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn viewing_another_conversation_does_not_suppress_push() {
    let (dir, chat, token) = fixture().await;
    let push = chat.push.as_ref().unwrap();
    let viewer = push
        .auth
        .test_session(Identity::Owner, chrono::Utc::now().timestamp())
        .unwrap()
        .unwrap();
    let app =
        crate::web::server::router_with_chat(push.settings.clone(), dir.path(), Some(chat.clone()));
    chat.sessions
        .save(&Session::new(SessionId::new("other")))
        .await
        .unwrap();
    let now = chrono::Utc::now().timestamp();
    seed(&chat, &token, "http://127.0.0.1:1/push", "reply", now).await;
    assert_eq!(
        call(
            &app,
            &viewer,
            "POST",
            "push/presence",
            json!({"tab_id":uuid::Uuid::new_v4().to_string(),"session_id":"other"})
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    chat.push_tick(now).await.unwrap();
    assert_eq!(
        chat.sessions
            .with_connection(|db| Ok(db.query_row(
                "SELECT attempts FROM web_push_queue",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn expired_events_and_rebound_devices_do_not_receive_old_notifications() {
    let (_dir, chat, token) = fixture().await;
    let now = chrono::Utc::now().timestamp();
    let endpoint = "http://127.0.0.1:1/push";
    seed(&chat, &token, endpoint, "old", now - 86400).await;
    chat.push_tick(now).await.unwrap();
    assert_eq!(
        chat.sessions
            .with_connection(|db| Ok(db.query_row(
                "SELECT count(*) FROM web_push_queue",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
        0
    );
    seed(&chat, &token, endpoint, "rebound", now).await;
    chat.push
        .as_ref()
        .unwrap()
        .store
        .with_connection(move |db| {
            db.execute(
                "UPDATE web_push_bindings SET session_hash='another-user' WHERE endpoint=?1",
                [endpoint],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    chat.push_tick(now).await.unwrap();
    assert_eq!(
        chat.sessions
            .with_connection(|db| Ok(db.query_row(
                "SELECT count(*) FROM web_push_queue",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn a_rejected_subscription_does_not_claim_the_device_binding() {
    let (dir, chat, token) = fixture().await;
    let push = chat.push.as_ref().unwrap();
    let app =
        crate::web::server::router_with_chat(push.settings.clone(), dir.path(), Some(chat.clone()));
    for index in 0..8 {
        let endpoint = format!("https://fcm.googleapis.com/push/{index}");
        assert_eq!(
            call(
                &app,
                &token,
                "POST",
                "push",
                json!({"subscription":subscription(&endpoint),"locale":"ru"})
            )
            .await
            .status(),
            StatusCode::NO_CONTENT
        );
    }
    let endpoint = "https://fcm.googleapis.com/push/rejected";
    assert_eq!(
        call(
            &app,
            &token,
            "POST",
            "push",
            json!({"subscription":subscription(endpoint),"locale":"ru"})
        )
        .await
        .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert!(
        !push
            .store
            .with_connection(move |db| Ok(db.query_row(
                "SELECT EXISTS(SELECT 1 FROM web_push_bindings WHERE endpoint=?1)",
                [endpoint],
                |r| r.get::<_, bool>(0)
            )?))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn delivery_rechecks_foreground_presence_after_a_batch_was_loaded() {
    let (_dir, chat, token) = fixture().await;
    let now = chrono::Utc::now().timestamp();
    let endpoint = "http://127.0.0.1:1/push";
    seed(&chat, &token, endpoint, "delayed", now).await;
    let presence_hash = token_hash(&token);
    chat.sessions
        .with_connection(move |db| {
            db.execute(
                "INSERT INTO web_push_presence VALUES(?1,'tab','personal',?2)",
                params![presence_hash, now],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    chat.deliver_push(
        Delivery {
            created_at: now,
            event: "delayed".into(),
            endpoint: endpoint.into(),
            session: "personal".into(),
            attempts: 0,
            hash: token_hash(&token),
            subscription: serde_json::to_vec(&subscription(endpoint)).unwrap(),
            locale: "ru".into(),
        },
        now,
    )
    .await
    .unwrap();
    assert_eq!(
        chat.sessions
            .with_connection(|db| Ok(db.query_row(
                "SELECT count(*) FROM web_push_queue",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn delivery_rechecks_session_expiry_after_a_batch_was_loaded() {
    let (_dir, chat, _) = fixture().await;
    let now = chrono::Utc::now().timestamp();
    let token = chat
        .push
        .as_ref()
        .unwrap()
        .auth
        .test_session(Identity::Owner, now - 7 * 86400 - 1)
        .unwrap()
        .unwrap();
    let endpoint = "http://127.0.0.1:1/push";
    seed(&chat, &token, endpoint, "expired-session", now - 10).await;
    chat.deliver_push(
        Delivery {
            created_at: now - 10,
            event: "expired-session".into(),
            endpoint: endpoint.into(),
            session: "personal".into(),
            attempts: 0,
            hash: token_hash(&token),
            subscription: serde_json::to_vec(&subscription(endpoint)).unwrap(),
            locale: "ru".into(),
        },
        now - 10,
    )
    .await
    .unwrap();
    assert_eq!(
        chat.sessions
            .with_connection(|db| Ok(db.query_row(
                "SELECT count(*) FROM web_push_queue",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn delivery_does_not_send_a_loaded_event_after_its_chat_is_deleted() {
    let (_dir, chat, token) = fixture().await;
    let received = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = received.clone();
    let app = Router::new().route(
        "/push",
        post(move || {
            let count = count.clone();
            async move {
                count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                StatusCode::CREATED
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/push", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let now = chrono::Utc::now().timestamp();
    seed(&chat, &token, &endpoint, "deleted", now).await;
    chat.sessions
        .delete(&SessionId::new("personal"))
        .await
        .unwrap();
    chat.deliver_push(
        Delivery {
            created_at: now,
            event: "deleted".into(),
            endpoint: endpoint.clone(),
            session: "personal".into(),
            attempts: 0,
            hash: token_hash(&token),
            subscription: serde_json::to_vec(&subscription(&endpoint)).unwrap(),
            locale: "ru".into(),
        },
        now,
    )
    .await
    .unwrap();
    assert_eq!(received.load(std::sync::atomic::Ordering::Relaxed), 0);
    server.abort();
}
