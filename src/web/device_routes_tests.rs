use super::*;
use crate::web::auth::Identity;
use axum::http::Request;
use serde_json::{Value, json};
use tower::ServiceExt;

fn settings() -> Settings {
    Settings {
        trusted_proxies: vec![],
        listen: "127.0.0.1:0".parse().unwrap(),
        base_url: "http://127.0.0.1/kodrik/".parse().unwrap(),
    }
}

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    origin: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(format!("/kodrik/{path}"))
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = cookie {
        req = req.header(header::COOKIE, format!("kodrik_web_session={token}"));
    }
    if let Some(origin) = origin {
        req = req.header(header::ORIGIN, origin);
    }
    let response = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 32768)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn device_routes_require_owner_cookie_origin_and_never_grant_browser_scope() {
    let dir = tempfile::tempdir().unwrap();
    let auth = AuthStore::new(dir.path());
    auth.configure_owner(Some(42)).unwrap();
    auth.grant_test_user(7).unwrap();
    let now = chrono::Utc::now().timestamp();
    let owner = auth
        .test_session(Identity::Telegram(42), now)
        .unwrap()
        .unwrap();
    let user = auth
        .test_session(Identity::Telegram(7), now)
        .unwrap()
        .unwrap();
    let app = router(settings(), dir.path());
    let (status, start) = request(
        &app,
        "POST",
        "api/devices/pair/start",
        None,
        None,
        json!({"label":"Laptop"}),
    )
    .await;
    assert_eq!(status, 200);
    let approve = json!({"request_id":start["request_id"],"code":start["code"]});
    for (cookie, origin, expected) in [
        (None, Some("http://127.0.0.1"), 401),
        (Some(user.as_str()), Some("http://127.0.0.1"), 403),
        (Some(owner.as_str()), None, 403),
        (Some(owner.as_str()), Some("https://evil.example"), 403),
    ] {
        assert_eq!(
            request(
                &app,
                "POST",
                "api/devices/pair/approve",
                cookie,
                origin,
                approve.clone()
            )
            .await
            .0,
            expected
        );
    }
    assert_eq!(
        request(
            &app,
            "POST",
            "api/devices/pair/approve",
            Some(&owner),
            Some("http://127.0.0.1"),
            approve
        )
        .await
        .0,
        200
    );
    let (status, poll) = request(
        &app,
        "POST",
        "api/devices/pair/poll",
        None,
        None,
        json!({"request_id":start["request_id"],"polling_secret":start["polling_secret"]}),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(poll["status"], "authorized");
    let token = poll["token"].as_str().unwrap();
    assert_eq!(
        request(&app, "GET", "api/profile", Some(token), None, Value::Null)
            .await
            .0,
        401
    );
    assert_eq!(
        request(&app, "GET", "api/devices", Some(token), None, Value::Null)
            .await
            .0,
        401
    );
    assert_eq!(
        request(&app, "GET", "api/devices", Some(&user), None, Value::Null)
            .await
            .0,
        403
    );
    let (status, devices) =
        request(&app, "GET", "api/devices", Some(&owner), None, Value::Null).await;
    assert_eq!(status, 200);
    assert_eq!(devices.as_array().unwrap().len(), 1);
    assert!(!devices.to_string().contains(token));
    let path = format!("api/devices/{}", poll["device_id"].as_str().unwrap());
    assert_eq!(
        request(
            &app,
            "DELETE",
            &path,
            Some(&owner),
            Some("http://127.0.0.1"),
            Value::Null
        )
        .await
        .0,
        204
    );
    assert!(auth.device_identity(token, now).unwrap().is_none());
}

#[tokio::test]
async fn native_pairing_routes_reject_cross_origin_oversized_and_rate_limited_requests() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(settings(), dir.path());
    assert_eq!(
        request(
            &app,
            "POST",
            "api/devices/pair/start",
            None,
            Some("https://evil.example"),
            json!({"label":"Laptop"})
        )
        .await
        .0,
        403
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "api/devices/pair/start",
            None,
            None,
            json!({"label":"x".repeat(17000)})
        )
        .await
        .0,
        413
    );
    for _ in 0..5 {
        assert_eq!(
            request(
                &app,
                "POST",
                "api/devices/pair/start",
                None,
                None,
                json!({"label":"Laptop"})
            )
            .await
            .0,
            200
        );
    }
    assert_eq!(
        request(
            &app,
            "POST",
            "api/devices/pair/start",
            None,
            None,
            json!({"label":"Laptop"})
        )
        .await
        .0,
        429
    );
}
