use super::{
    Settings,
    auth::{AuthStore, request_token},
};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::{path::PathBuf, sync::Arc};

#[derive(Clone)]
pub(crate) struct GatewayControl {
    instance_id: Arc<str>,
    restart: tokio::sync::watch::Sender<bool>,
}

impl GatewayControl {
    pub(crate) fn new() -> Self {
        Self {
            instance_id: uuid::Uuid::new_v4().to_string().into(),
            restart: tokio::sync::watch::channel(false).0,
        }
    }

    fn requested(&self) -> bool {
        *self.restart.borrow()
    }

    pub(crate) async fn wait(&self) {
        let _ = self
            .restart
            .subscribe()
            .wait_for(|requested| *requested)
            .await;
    }
}

#[derive(Clone)]
struct ControlState {
    settings: Settings,
    auth: AuthStore,
    control: GatewayControl,
    config_path: PathBuf,
}

pub(crate) fn router(
    settings: Settings,
    auth: AuthStore,
    control: GatewayControl,
    config_path: PathBuf,
) -> Router {
    let prefix = settings.prefix().trim_end_matches('/').to_owned();
    let routes = Router::new()
        .route("/api/gateway", get(status))
        .route("/api/gateway/restart", post(restart))
        .with_state(ControlState {
            settings,
            auth,
            control,
            config_path,
        });
    let routes = if prefix.is_empty() {
        routes
    } else {
        Router::new().nest(&prefix, routes)
    };
    routes.layer(middleware::map_response(
        |mut response: Response| async move {
            for (name, value) in [
                (header::CACHE_CONTROL, "no-store"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                (header::REFERRER_POLICY, "same-origin"),
            ] {
                response.headers_mut().insert(name, value.parse().unwrap());
            }
            response
        },
    ))
}

fn owner(state: &ControlState, headers: &HeaderMap) -> Result<(), StatusCode> {
    let token = request_token(headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let now = chrono::Utc::now().timestamp();
    if state
        .auth
        .authenticated(&token, now)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        return Ok(());
    }
    match state
        .auth
        .identity(&token, now)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        Some(_) => Err(StatusCode::FORBIDDEN),
        None => Err(StatusCode::UNAUTHORIZED),
    }
}

async fn status(
    State(state): State<ControlState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    owner(&state, &headers)?;
    Ok(Json(serde_json::json!({
        "instance_id": &*state.control.instance_id,
        "pid": std::process::id(),
        "restart_available": cfg!(unix) && !state.control.requested(),
    })))
}

async fn restart(State(state): State<ControlState>, headers: HeaderMap) -> Response {
    if let Err(status) = owner(&state, &headers) {
        return status.into_response();
    }
    if !super::server::valid_origin(&state.settings, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !cfg!(unix) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if state.control.requested() {
        return StatusCode::CONFLICT.into_response();
    }
    let validation = tokio::task::spawn_blocking(move || {
        let source = std::fs::read_to_string(&state.config_path)?;
        super::configuration::validate_source(&source).map(|_| ())
    })
    .await;
    match validation {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({"error": "Saved configuration is invalid or unavailable"})),
            )
                .into_response();
        }
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
    // send_if_modified serializes competing requests and preserves the queued signal.
    if !state.control.restart.send_if_modified(|requested| {
        if *requested {
            false
        } else {
            *requested = true;
            true
        }
    }) {
        return StatusCode::CONFLICT.into_response();
    }
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"instance_id": &*state.control.instance_id})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::auth::Identity;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    struct Fixture {
        _dir: tempfile::TempDir,
        app: Router,
        control: GatewayControl,
        owner: String,
        guest: String,
        telegram_owner: String,
        config: std::path::PathBuf,
    }

    fn fixture(prefix: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            trusted_proxies: vec![],
            listen: "127.0.0.1:0".parse().unwrap(),
            base_url: format!("http://127.0.0.1{prefix}").parse().unwrap(),
        };
        let auth = AuthStore::new(dir.path());
        auth.configure_owner(Some(42)).unwrap();
        auth.set_password("valid-owner-passphrase").unwrap();
        auth.grant_test_user(7).unwrap();
        let now = chrono::Utc::now().timestamp();
        let owner = auth.login("valid-owner-passphrase", now).unwrap().unwrap();
        let guest = auth
            .test_session(Identity::Telegram(7), now)
            .unwrap()
            .unwrap();
        let telegram_owner = auth
            .test_session(Identity::Telegram(42), now)
            .unwrap()
            .unwrap();
        let config = dir.path().join("config.toml");
        // No Telegram interfaces: standalone web gateways must be restartable too.
        std::fs::write(&config, "[gateway]\n[provider.primary]\ntype = 'openai'\nmodel = 'test'\napi_key = 'test'\n[web]\nenabled = true\nbase_url = 'http://127.0.0.1/'\n").unwrap();
        let control = GatewayControl::new();
        let app = router(settings, auth, control.clone(), config.clone());
        Fixture {
            _dir: dir,
            app,
            control,
            owner,
            guest,
            telegram_owner,
            config,
        }
    }

    async fn request(
        f: &Fixture,
        method: &str,
        path: &str,
        token: &str,
        origin: Option<&str>,
    ) -> Response {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::COOKIE, format!("kodrik_web_session={token}"));
        if let Some(origin) = origin {
            req = req.header(header::ORIGIN, origin);
        }
        let response = f
            .app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        response
    }

    #[tokio::test]
    async fn owner_only_status_has_unique_instance_id_and_supports_prefix() {
        let f = fixture("/kodrik/");
        for (token, expected) in [
            ("", StatusCode::UNAUTHORIZED),
            (f.guest.as_str(), StatusCode::FORBIDDEN),
            (f.owner.as_str(), StatusCode::OK),
            (f.telegram_owner.as_str(), StatusCode::OK),
        ] {
            let response = request(&f, "GET", "/kodrik/api/gateway", token, None).await;
            assert_eq!(response.status(), expected);
            if expected == StatusCode::OK {
                let bytes = axum::body::to_bytes(response.into_body(), 4096)
                    .await
                    .unwrap();
                let status: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(status["instance_id"], &*f.control.instance_id);
                assert_eq!(status["pid"], std::process::id());
                assert_eq!(status["restart_available"], cfg!(unix));
                assert_ne!(GatewayControl::new().instance_id, f.control.instance_id);
            }
        }
        assert!(!f.control.requested());
    }

    #[tokio::test]
    async fn restart_requires_owner_and_exact_origin_then_queues_only_once() {
        let f = fixture("/");
        for (token, origin, expected) in [
            ("", Some("http://127.0.0.1"), StatusCode::UNAUTHORIZED),
            (
                f.guest.as_str(),
                Some("http://127.0.0.1"),
                StatusCode::FORBIDDEN,
            ),
            (f.owner.as_str(), None, StatusCode::FORBIDDEN),
            (
                f.owner.as_str(),
                Some("https://evil.example"),
                StatusCode::FORBIDDEN,
            ),
        ] {
            assert_eq!(
                request(&f, "POST", "/api/gateway/restart", token, origin)
                    .await
                    .status(),
                expected
            );
            assert!(!f.control.requested());
        }
        let response = request(
            &f,
            "POST",
            "/api/gateway/restart",
            &f.telegram_owner,
            Some("http://127.0.0.1"),
        )
        .await;
        if cfg!(unix) {
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            let _ = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(1), f.control.wait())
                .await
                .unwrap();
            assert!(f.control.requested());
            assert_eq!(
                request(
                    &f,
                    "POST",
                    "/api/gateway/restart",
                    &f.owner,
                    Some("http://127.0.0.1")
                )
                .await
                .status(),
                StatusCode::CONFLICT
            );
        } else {
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert!(!f.control.requested());
        }
    }

    #[tokio::test]
    async fn invalid_persisted_config_does_not_stop_gateway_or_leak_source() {
        let f = fixture("/");
        std::fs::write(&f.config, "api_key = 'sensitive-secret'\ninvalid = [").unwrap();
        let response = request(
            &f,
            "POST",
            "/api/gateway/restart",
            &f.owner,
            Some("http://127.0.0.1"),
        )
        .await;
        assert_eq!(
            response.status(),
            if cfg!(unix) {
                StatusCode::UNPROCESSABLE_ENTITY
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            }
        );
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("sensitive-secret"));
        assert!(!f.control.requested());
    }
}
