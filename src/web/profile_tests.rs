use super::*;
use crate::web::auth::Identity;
use axum::http::Request;
use serde_json::{Value, json};
use tower::ServiceExt;

struct Fixture {
    dir: tempfile::TempDir,
    app: Router,
    auth: AuthStore,
    owner: String,
    telegram_owner: String,
    user: String,
    other: String,
}

impl Fixture {
    fn new(password: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let auth = AuthStore::new(dir.path());
        if password {
            auth.set_password("preview-password").unwrap();
        }
        auth.configure_owner(Some(42)).unwrap();
        auth.grant_test_user(7).unwrap();
        auth.grant_test_user(8).unwrap();
        let now = chrono::Utc::now().timestamp();
        let owner = auth.test_session(Identity::Owner, now).unwrap().unwrap();
        let telegram_owner = auth
            .test_session(Identity::Telegram(42), now)
            .unwrap()
            .unwrap();
        let user = auth
            .test_session(Identity::Telegram(7), now)
            .unwrap()
            .unwrap();
        let other = auth
            .test_session(Identity::Telegram(8), now)
            .unwrap()
            .unwrap();
        let app = router(Self::settings(), dir.path());
        Self {
            dir,
            app,
            auth,
            owner,
            telegram_owner,
            user,
            other,
        }
    }

    fn settings() -> Settings {
        Settings {
            trusted_proxies: vec![],
            listen: "127.0.0.1:0".parse().unwrap(),
            base_url: "http://127.0.0.1/kodrik/".parse().unwrap(),
        }
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        token: &str,
        origin: bool,
        body: Value,
    ) -> (StatusCode, Value, HeaderMap) {
        let mut request = Request::builder()
            .method(method)
            .uri(format!("/kodrik/{path}"));
        if !token.is_empty() {
            request = request.header(header::COOKIE, format!("kodrik_web_session={token}"));
        }
        if origin {
            request = request.header(header::ORIGIN, "http://127.0.0.1");
        }
        let request = request
            .header(header::CONTENT_TYPE, "application/json")
            .body(if body.is_null() {
                Body::empty()
            } else {
                Body::from(body.to_string())
            })
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            headers,
        )
    }

    fn change(current: &str, new: &str, confirmation: &str) -> Value {
        json!({"current_password":current, "new_password":new, "confirm_password":confirmation})
    }
}

#[tokio::test]
async fn profiles_require_authentication_and_isolate_names_without_changing_identity() {
    let mut f = Fixture::new(true);
    assert_eq!(
        f.request("GET", "api/profile", "", false, Value::Null)
            .await
            .0,
        401
    );
    let (status, owner, _) = f
        .request("GET", "api/profile", &f.owner, false, Value::Null)
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        owner,
        json!({"name":"", "owner":true, "telegram_id":42, "password_set":true})
    );
    assert_eq!(
        f.request(
            "PATCH",
            "api/profile",
            &f.owner,
            false,
            json!({"name":"Owner"})
        )
        .await
        .0,
        403
    );
    for name in ["", "   ", "line\nbreak", &"я".repeat(101)] {
        assert_eq!(
            f.request("PATCH", "api/profile", &f.owner, true, json!({"name":name}))
                .await
                .0,
            400
        );
    }
    assert_eq!(
        f.request(
            "PATCH",
            "api/profile",
            &f.user,
            true,
            json!({"name":"Guest", "owner":true})
        )
        .await
        .0,
        400
    );
    assert_eq!(
        f.request(
            "PATCH",
            "api/profile",
            &f.telegram_owner,
            true,
            json!({"name":"  Алиса  "})
        )
        .await
        .0,
        200
    );
    let (_, owner, _) = f
        .request("GET", "api/profile", &f.owner, false, Value::Null)
        .await;
    assert_eq!(owner["name"], "Алиса");
    assert_eq!(
        f.request("PATCH", "api/profile", &f.user, true, json!({"name":"Bob"}))
            .await
            .0,
        200
    );
    f.app = router(Fixture::settings(), f.dir.path());
    let (_, user, _) = f
        .request("GET", "api/profile", &f.user, false, Value::Null)
        .await;
    assert_eq!(
        user,
        json!({"name":"Bob", "owner":false, "telegram_id":7, "password_set":false})
    );
    assert_eq!(
        f.request("GET", "api/profile", &f.owner, false, Value::Null)
            .await
            .1["name"],
        "Алиса"
    );
    assert_eq!(
        f.request("GET", "api/profile", &f.other, false, Value::Null)
            .await
            .1["name"],
        ""
    );
    f.auth.revoke_user(7).unwrap();
    assert_eq!(
        f.request(
            "PATCH",
            "api/profile",
            &f.user,
            true,
            json!({"name":"No access"})
        )
        .await
        .0,
        401
    );
    assert_eq!(
        f.request("GET", "api/profile", &f.user, false, Value::Null)
            .await
            .0,
        401
    );
}

#[tokio::test]
async fn password_changes_require_owner_origin_current_password_and_confirmation_then_revoke_sessions()
 {
    let f = Fixture::new(true);
    let new = "a new secure passphrase";
    let valid = Fixture::change("preview-password", new, new);
    assert_eq!(
        f.request("POST", "api/profile/password", "", true, valid.clone())
            .await
            .0,
        401
    );
    assert_eq!(
        f.request("POST", "api/profile/password", &f.user, true, valid.clone())
            .await
            .0,
        403
    );
    assert_eq!(
        f.request(
            "POST",
            "api/profile/password",
            &f.owner,
            false,
            valid.clone()
        )
        .await
        .0,
        403
    );
    for body in [
        Fixture::change("wrong", new, new),
        Fixture::change("preview-password", "1234", "1234"),
        Fixture::change("preview-password", new, "does not match"),
    ] {
        assert_eq!(
            f.request("POST", "api/profile/password", &f.owner, true, body)
                .await
                .0,
            400
        );
        assert!(
            f.auth
                .authenticated(&f.owner, chrono::Utc::now().timestamp())
                .unwrap()
        );
    }
    f.auth.register_telegram_bot("main", "TestBot").unwrap();
    let pending = f
        .auth
        .begin_telegram_login(None, "/kodrik/app/profile", chrono::Utc::now().timestamp())
        .unwrap()
        .unwrap();
    let (status, _, headers) = f
        .request(
            "POST",
            "api/profile/password",
            &f.telegram_owner,
            true,
            valid,
        )
        .await;
    assert_eq!(status, 204);
    assert!(
        headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    let now = chrono::Utc::now().timestamp();
    for token in [&f.owner, &f.telegram_owner, &f.user, &f.other] {
        assert!(f.auth.identity(token, now).unwrap().is_none());
    }
    assert_eq!(
        f.auth.poll_telegram_login(&pending.cookie, now).unwrap(),
        crate::web::auth::LoginPoll::Expired
    );
    assert!(f.auth.login("preview-password", now).unwrap().is_none());
    assert!(f.auth.login(new, now).unwrap().is_some());
    let journal = std::fs::read_to_string(f.dir.path().join("web/auth.json")).unwrap();
    assert!(!journal.contains(new));
}

#[tokio::test]
async fn password_changes_share_the_login_attempt_budget() {
    let f = Fixture::new(true);
    for _ in 0..5 {
        assert_eq!(
            f.request(
                "POST",
                "api/profile/password",
                &f.owner,
                true,
                Fixture::change("wrong", "new secure passphrase", "new secure passphrase")
            )
            .await
            .0,
            400
        );
    }
    assert_eq!(
        f.request(
            "POST",
            "api/profile/password",
            &f.owner,
            true,
            Fixture::change(
                "preview-password",
                "new secure passphrase",
                "new secure passphrase"
            )
        )
        .await
        .0,
        429
    );
    let request = Request::post("/kodrik/auth/login")
        .header(header::ORIGIN, "http://127.0.0.1")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("password=preview-password"))
        .unwrap();
    assert_eq!(f.app.clone().oneshot(request).await.unwrap().status(), 429);
    assert!(
        f.auth
            .login("preview-password", chrono::Utc::now().timestamp())
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn telegram_owner_can_set_the_first_password_but_invited_users_cannot() {
    let f = Fixture::new(false);
    let new = "first secure passphrase";
    assert_eq!(
        f.request("GET", "api/profile", &f.telegram_owner, false, Value::Null)
            .await
            .1["password_set"],
        false
    );
    let body = Fixture::change("", new, new);
    assert_eq!(
        f.request("POST", "api/profile/password", &f.user, true, body.clone())
            .await
            .0,
        403
    );
    assert_eq!(
        f.request(
            "POST",
            "api/profile/password",
            &f.telegram_owner,
            true,
            body
        )
        .await
        .0,
        204
    );
    assert!(
        f.auth
            .login(new, chrono::Utc::now().timestamp())
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn concurrent_password_changes_cannot_reuse_a_revoked_session() {
    let f = Fixture::new(true);
    let first = "first replacement passphrase";
    let second = "second replacement passphrase";
    let (a, b) = tokio::join!(
        f.request(
            "POST",
            "api/profile/password",
            &f.owner,
            true,
            Fixture::change("preview-password", first, first)
        ),
        f.request(
            "POST",
            "api/profile/password",
            &f.owner,
            true,
            Fixture::change("preview-password", second, second)
        ),
    );
    let mut statuses = [a.0.as_u16(), b.0.as_u16()];
    statuses.sort();
    assert_eq!(statuses, [204, 401]);
    let now = chrono::Utc::now().timestamp();
    assert_ne!(
        f.auth.login(first, now).unwrap().is_some(),
        f.auth.login(second, now).unwrap().is_some()
    );
}

#[tokio::test]
async fn password_changes_accept_maximum_sized_secrets_in_json() {
    let f = Fixture::new(true);
    let new = format!("{}z", "\"".repeat(1023));
    let (status, _, _) = f
        .request(
            "POST",
            "api/profile/password",
            &f.owner,
            true,
            Fixture::change("preview-password", &new, &new),
        )
        .await;
    assert_eq!(status, 204);
    assert!(
        f.auth
            .login(&new, chrono::Utc::now().timestamp())
            .unwrap()
            .is_some()
    );
}
