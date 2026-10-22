use super::*;
use tower::ServiceExt;

async fn post(app: &Router, path: &str, cookie: &str, origin: &str, body: &str) -> Response {
    app.clone()
        .oneshot(
            axum::http::Request::post(format!("/kodrik/auth/telegram/{path}"))
                .header(header::ORIGIN, origin)
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap()
}

fn cookies(response: &Response) -> Vec<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap().to_owned())
        .collect()
}

async fn json_body(response: Response) -> serde_json::Value {
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn short_invitation_landing_preserves_prefix_and_defaults_to_chat() {
    let dir = tempfile::tempdir().unwrap();
    let settings = Settings {
        trusted_proxies: Vec::new(),
        listen: "127.0.0.1:0".parse().unwrap(),
        base_url: "https://example.com/kodrik/".parse().unwrap(),
    };
    let response = super::super::router(settings, dir.path())
        .oneshot(
            axum::http::Request::get("/kodrik/i")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let body = axum::body::to_bytes(response.into_body(), 2_000_000)
        .await
        .unwrap();
    let body = std::str::from_utf8(&body).unwrap();
    assert!(body.contains(r#""returnTo":"/kodrik/chat/""#));
}

#[tokio::test]
async fn invitation_http_redemption_is_single_use_and_rejects_invalid_links() {
    let dir = tempfile::tempdir().unwrap();
    let settings = Settings {
        trusted_proxies: Vec::new(),
        listen: "127.0.0.1:0".parse().unwrap(),
        base_url: "https://example.com/kodrik/".parse().unwrap(),
    };
    let auth = AuthStore::new(dir.path());
    auth.register_telegram_bot("main", "TestBot").unwrap();
    let now = chrono::Utc::now().timestamp();
    let invite = auth.create_invite("Friend", now + 3600, now).unwrap();
    let app = super::super::router(settings, dir.path());
    let body = json!({"return_to":"/kodrik/chat/", "invite":invite.token}).to_string();
    let started = post(&app, "start", "", "https://example.com", &body).await;
    assert_eq!(started.status(), StatusCode::OK);
    assert_eq!(started.headers()[header::CACHE_CONTROL], "no-store");
    let browser = cookies(&started)[0].split(';').next().unwrap().to_owned();
    let started = json_body(started).await;
    assert!(!started.to_string().contains(&invite.token));
    let id = started["url"]
        .as_str()
        .unwrap()
        .split("login_")
        .nth(1)
        .unwrap();
    auth.present_telegram_login(id, "main", 42, now)
        .unwrap()
        .unwrap();
    assert!(
        auth.decide_telegram_login(id, "main", 42, true, now)
            .unwrap()
    );
    let approved = post(&app, "poll", &browser, "https://example.com", "{}").await;
    let session = cookies(&approved)
        .into_iter()
        .find(|value| value.starts_with("kodrik_web_session="))
        .unwrap();
    let token = session
        .split(';')
        .next()
        .unwrap()
        .split_once('=')
        .unwrap()
        .1;
    assert_eq!(
        auth.identity(token, now).unwrap(),
        Some(crate::web::auth::Identity::Telegram(42))
    );
    assert_eq!(
        json_body(approved).await["redirect"],
        "https://example.com/kodrik/chat/"
    );

    let revoked = auth.create_invite("Revoked", now + 3600, now).unwrap();
    auth.revoke_invite(&revoked.id).unwrap();
    let expired = auth.create_invite("Expired", now - 10, now - 20).unwrap();
    for token in [&invite.token, &revoked.token, &expired.token, "invalid"] {
        let response = post(
            &app,
            "start",
            "",
            "https://example.com",
            &json!({"invite":token}).to_string(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(cookies(&response).is_empty());
        assert_eq!(
            json_body(response).await["error"],
            "Приглашение недействительно. Попросите владельца прислать новое."
        );
    }
}

#[tokio::test]
async fn invitation_revoked_during_confirmation_cannot_issue_session() {
    let dir = tempfile::tempdir().unwrap();
    let settings = Settings {
        trusted_proxies: Vec::new(),
        listen: "127.0.0.1:0".parse().unwrap(),
        base_url: "https://example.com/kodrik/".parse().unwrap(),
    };
    let auth = AuthStore::new(dir.path());
    auth.register_telegram_bot("main", "TestBot").unwrap();
    let now = chrono::Utc::now().timestamp();
    let invite = auth.create_invite("Friend", now + 3600, now).unwrap();
    let app = super::super::router(settings, dir.path());
    let started = post(
        &app,
        "start",
        "",
        "https://example.com",
        &json!({"invite":invite.token}).to_string(),
    )
    .await;
    assert_eq!(started.status(), StatusCode::OK);
    let browser = cookies(&started)[0].split(';').next().unwrap().to_owned();
    let started = json_body(started).await;
    let id = started["url"]
        .as_str()
        .unwrap()
        .split("login_")
        .nth(1)
        .unwrap();
    auth.present_telegram_login(id, "main", 43, now)
        .unwrap()
        .unwrap();
    auth.revoke_invite(&invite.id).unwrap();
    assert!(
        !auth
            .decide_telegram_login(id, "main", 43, true, now)
            .unwrap()
    );
    let denied = post(&app, "poll", &browser, "https://example.com", "{}").await;
    assert!(
        !cookies(&denied)
            .iter()
            .any(|value| value.starts_with("kodrik_web_session="))
    );
    assert_eq!(json_body(denied).await["status"], "denied");
}

#[tokio::test]
async fn telegram_login_http_requires_origin_and_browser_binding_before_cookie_issue() {
    let dir = tempfile::tempdir().unwrap();
    let settings = Settings {
        trusted_proxies: Vec::new(),
        listen: "127.0.0.1:0".parse().unwrap(),
        base_url: "https://example.com/kodrik/".parse().unwrap(),
    };
    let auth = AuthStore::new(dir.path());
    auth.grant_test_user(7).unwrap();
    auth.register_telegram_bot("main", "TestBot").unwrap();
    let app = super::super::router(settings.clone(), dir.path());
    for path in ["start", "poll", "cancel"] {
        assert_eq!(
            post(&app, path, "", "https://evil.example", "{}")
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    let started = post(
        &app,
        "start",
        "",
        "https://example.com",
        r#"{"return_to":"/kodrik/chat/"}"#,
    )
    .await;
    assert_eq!(started.status(), StatusCode::OK);
    let headers = cookies(&started);
    assert_eq!(headers.len(), 1);
    for flag in [
        "HttpOnly",
        "Secure",
        "SameSite=Strict",
        "Path=/kodrik/",
        "Max-Age=300",
    ] {
        assert!(headers[0].contains(flag));
    }
    let browser = headers[0].split(';').next().unwrap().to_owned();
    assert!(!browser.starts_with("kodrik_web_session="));
    let body = json_body(started).await;
    let id = body["url"]
        .as_str()
        .unwrap()
        .strip_prefix("https://t.me/TestBot?start=login_")
        .unwrap();
    assert_eq!(body["verification"], id[..6].to_ascii_uppercase());
    let poll = post(&app, "poll", &browser, "https://example.com", "{}").await;
    assert!(cookies(&poll).is_empty());
    assert_eq!(json_body(poll).await["status"], "pending");
    let stranger = post(&app, "poll", "", "https://example.com", "{}").await;
    assert!(
        !cookies(&stranger)
            .iter()
            .any(|value| value.starts_with("kodrik_web_session="))
    );
    assert_eq!(json_body(stranger).await["status"], "expired");
    let now = chrono::Utc::now().timestamp();
    auth.present_telegram_login(id, "main", 7, now)
        .unwrap()
        .unwrap();
    assert!(
        auth.decide_telegram_login(id, "main", 7, true, now)
            .unwrap()
    );
    let approved = post(&app, "poll", &browser, "https://example.com", "{}").await;
    let values = cookies(&approved);
    let session = values
        .iter()
        .find(|cookie| cookie.starts_with("kodrik_web_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .split_once('=')
        .unwrap()
        .1;
    assert_eq!(
        auth.identity(session, now).unwrap(),
        Some(crate::web::auth::Identity::Telegram(7))
    );
    assert!(!auth.authenticated(session, now).unwrap());
    assert!(values.iter().any(|value|value.starts_with("kodrik_telegram_login=") && value.contains("Max-Age=0")));
    let body = json_body(approved).await;
    assert_eq!(body["redirect"], "https://example.com/kodrik/chat/");
    assert!(!body.to_string().contains(session));
    let replay = post(&app, "poll", &browser, "https://example.com", "{}").await;
    assert!(
        !cookies(&replay)
            .iter()
            .any(|value| value.starts_with("kodrik_web_session="))
    );
    assert_eq!(json_body(replay).await["status"], "expired");
}

#[tokio::test]
async fn telegram_login_http_denial_cancel_expiry_redirects_and_rate_limit() {
    let dir = tempfile::tempdir().unwrap();
    let settings = Settings {
        trusted_proxies: Vec::new(),
        listen: "127.0.0.1:0".parse().unwrap(),
        base_url: "https://example.com/kodrik/".parse().unwrap(),
    };
    let auth = AuthStore::new(dir.path());
    auth.grant_test_user(7).unwrap();
    auth.register_telegram_bot("main", "TestBot").unwrap();
    let app = super::super::router(settings, dir.path());
    let started = post(
        &app,
        "start",
        "",
        "https://example.com",
        r#"{"return_to":"https://evil.example/"}"#,
    )
    .await;
    let browser = cookies(&started)[0].split(';').next().unwrap().to_owned();
    let body = json_body(started).await;
    let id = body["url"]
        .as_str()
        .unwrap()
        .split("login_")
        .nth(1)
        .unwrap();
    let now = chrono::Utc::now().timestamp();
    auth.present_telegram_login(id, "main", 7, now).unwrap();
    auth.decide_telegram_login(id, "main", 7, false, now)
        .unwrap();
    assert_eq!(
        json_body(post(&app, "poll", &browser, "https://example.com", "{}").await).await["status"],
        "denied"
    );
    let login = auth
        .begin_telegram_login(None, "/kodrik/", now)
        .unwrap()
        .unwrap();
    let browser = format!("kodrik_telegram_login={}", login.cookie);
    assert_eq!(
        post(&app, "cancel", &browser, "https://evil.example", "{}")
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        auth.poll_telegram_login(&login.cookie, now).unwrap(),
        LoginPoll::Pending
    );
    assert_eq!(
        post(&app, "cancel", &browser, "https://example.com", "{}")
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        auth.poll_telegram_login(&login.cookie, now).unwrap(),
        LoginPoll::Expired
    );
    let old = auth
        .begin_telegram_login(None, "/kodrik/", now - 300)
        .unwrap()
        .unwrap();
    assert_eq!(
        json_body(
            post(
                &app,
                "poll",
                &format!("kodrik_telegram_login={}", old.cookie),
                "https://example.com",
                "{}"
            )
            .await
        )
        .await["status"],
        "expired"
    );
    for _ in 0..4 {
        assert_eq!(
            post(&app, "start", "", "https://example.com", "{}")
                .await
                .status(),
            StatusCode::OK
        );
    }
    assert_eq!(
        post(&app, "start", "", "https://example.com", "{}")
            .await
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}
