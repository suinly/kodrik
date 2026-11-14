use super::{
    Settings,
    auth::{AuthStore, request_token},
    server::valid_origin,
};
use crate::config::{Config, McpServerConfig, ProviderConfig};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};

// Serializes read/check/backup/replace across every router instance in this process.
static CONFIG_LOCK: Mutex<()> = Mutex::new(());

/// Validate offline; never return parser diagnostics containing submitted secrets.
pub(crate) fn validate_source(source: &str) -> anyhow::Result<Config> {
    let config: Config = toml::from_str(source).map_err(|error: toml::de::Error| {
        let line = error.span().map(|span| {
            source.as_bytes()[..span.start.min(source.len())]
                .iter()
                .filter(|&&b| b == b'\n')
                .count()
                + 1
        });
        match line {
            Some(line) => anyhow::anyhow!("Invalid TOML or configuration value at line {line}"),
            None => anyhow::anyhow!("Invalid TOML or configuration value"),
        }
    })?;
    config
        .validate_owner()
        .map_err(|_| anyhow::anyhow!("Invalid owner configuration"))?;
    Settings::from_config(&config.web).map_err(|_| anyhow::anyhow!("Invalid web configuration"))?;
    config
        .research
        .search_endpoint()
        .map_err(|_| anyhow::anyhow!("Invalid research configuration"))?;
    config
        .companion
        .validate()
        .and_then(|_| {
            config
                .companion
                .recipient(&config.gateway, config.owner.as_ref())
                .map(|_| ())
        })
        .map_err(|_| anyhow::anyhow!("Invalid companion configuration"))?;
    anyhow::ensure!(
        config.provider.contains_key("primary"),
        "Primary provider is required"
    );
    for (name, provider) in &config.provider {
        let ProviderConfig::OpenAi {
            api_key,
            model,
            api_base,
        } = provider;
        anyhow::ensure!(
            !name.trim().is_empty() && !api_key.trim().is_empty() && !model.trim().is_empty(),
            "Provider name, model and API key must not be empty"
        );
        if let Some(raw) = api_base {
            anyhow::ensure!(
                reqwest::Url::parse(raw).is_ok_and(|url| matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.fragment().is_none()),
                "Invalid provider API base URL"
            );
        }
    }
    for (name, server) in &config.mcp {
        let McpServerConfig::Http { url, headers } = server;
        anyhow::ensure!(
            !name.trim().is_empty()
                && reqwest::Url::parse(url).is_ok_and(|url| matches!(
                    url.scheme(),
                    "http" | "https"
                ) && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.fragment().is_none()),
            "Invalid MCP configuration"
        );
        let mut names = std::collections::HashSet::new();
        for (name, value) in headers {
            anyhow::ensure!(
                names.insert(name.to_ascii_lowercase()),
                "Duplicate MCP header name"
            );
            anyhow::ensure!(
                axum::http::HeaderName::try_from(name).is_ok()
                    && axum::http::HeaderValue::try_from(value).is_ok(),
                "Invalid MCP header configuration"
            );
        }
    }
    Ok(config)
}

#[derive(Clone)]
struct ConfigurationState {
    settings: Settings,
    auth: AuthStore,
    path: PathBuf,
}

pub(super) fn routes(settings: Settings, data: &Path, auth: AuthStore) -> Router {
    Router::new()
        .route("/api/config", get(read).put(update))
        .layer(DefaultBodyLimit::max(256 * 1024))
        .with_state(ConfigurationState {
            settings,
            auth,
            path: data.join("config.toml"),
        })
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    config: String,
    revision: String,
}

fn document(config: String) -> Document {
    let revision = format!("{:x}", Sha256::digest(config.as_bytes()));
    Document { config, revision }
}

fn failure(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({"error": message}))).into_response()
}

fn owner(state: &ConfigurationState, headers: &HeaderMap) -> Result<(), Response> {
    let token = request_token(headers)
        .ok_or_else(|| failure(StatusCode::UNAUTHORIZED, "Authentication required"))?;
    match state.auth.profile(&token, chrono::Utc::now().timestamp()) {
        Ok(Some(profile)) if profile.owner => Ok(()),
        Ok(Some(_)) => Err(failure(StatusCode::FORBIDDEN, "Owner access required")),
        Ok(None) => Err(failure(StatusCode::UNAUTHORIZED, "Authentication required")),
        Err(_) => Err(failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Cannot authenticate request",
        )),
    }
}

async fn read(State(state): State<ConfigurationState>, headers: HeaderMap) -> Response {
    if let Err(response) = owner(&state, &headers) {
        return response;
    }
    run(move || {
        let _lock = CONFIG_LOCK
            .lock()
            .map_err(|_| anyhow::anyhow!("Configuration lock unavailable"))?;
        Ok(Json(document(std::fs::read_to_string(&state.path)?)).into_response())
    })
    .await
}

async fn update(
    State(state): State<ConfigurationState>,
    headers: HeaderMap,
    input: Result<Json<Document>, JsonRejection>,
) -> Response {
    if let Err(response) = owner(&state, &headers) {
        return response;
    }
    if !valid_origin(&state.settings, &headers) {
        return failure(StatusCode::FORBIDDEN, "Invalid origin");
    }
    let input = match input {
        Ok(Json(input)) => input,
        Err(error) => {
            return failure(
                if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    StatusCode::PAYLOAD_TOO_LARGE
                } else {
                    StatusCode::BAD_REQUEST
                },
                "Invalid configuration request",
            );
        }
    };
    run(move || {
        if let Err(error) = validate_source(&input.config) {
            return Ok(failure(StatusCode::BAD_REQUEST, &error.to_string()));
        }
        let _lock = CONFIG_LOCK
            .lock()
            .map_err(|_| anyhow::anyhow!("Configuration lock unavailable"))?;
        if std::fs::symlink_metadata(&state.path)?.is_symlink() {
            return Ok(failure(
                StatusCode::BAD_REQUEST,
                "Configuration is a managed symlink; edit its target on the host",
            ));
        }
        let current = std::fs::read_to_string(&state.path)?;
        if document(current.clone()).revision != input.revision {
            return Ok(failure(
                StatusCode::CONFLICT,
                "Configuration changed; reload before saving",
            ));
        }
        persist(&state.path.with_extension("toml.bak"), current.as_bytes())?;
        persist(&state.path, input.config.as_bytes())?;
        Ok(Json(document(input.config)).into_response())
    })
    .await
}

fn persist(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Missing configuration directory"))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

async fn run(work: impl FnOnce() -> anyhow::Result<Response> + Send + 'static) -> Response {
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(response)) => response,
        _ => failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Cannot access configuration file",
        ),
    }
}

#[cfg(test)]
mod tests {
    use crate::web::{
        Settings,
        auth::{AuthStore, Identity},
        server::router,
    };
    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use serde_json::{Value, json};
    use tower::ServiceExt;

    const SOURCE: &str = "# keep this comment\n[gateway]\n[provider.primary]\ntype='openai'\nmodel='test'\napi_key='PRIVATE_KEY'\n";

    fn fixture() -> (tempfile::TempDir, Router, String, String) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), SOURCE).unwrap();
        let auth = AuthStore::new(dir.path());
        auth.configure_owner(Some(42)).unwrap();
        auth.grant_test_user(7).unwrap();
        let now = chrono::Utc::now().timestamp();
        let owner = auth
            .test_session(Identity::Telegram(42), now)
            .unwrap()
            .unwrap();
        let guest = auth
            .test_session(Identity::Telegram(7), now)
            .unwrap()
            .unwrap();
        let settings = Settings {
            trusted_proxies: vec![],
            listen: "127.0.0.1:0".parse().unwrap(),
            base_url: "http://127.0.0.1/".parse().unwrap(),
        };
        let app = router(settings, dir.path());
        (dir, app, owner, guest)
    }

    async fn request(
        app: &Router,
        token: &str,
        input: Option<Value>,
        origin: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .uri("/api/config")
            .method(if input.is_some() { "PUT" } else { "GET" });
        if !token.is_empty() {
            req = req.header("cookie", format!("kodrik_web_session={token}"));
        }
        if let Some(origin) = origin {
            req = req.header("origin", origin);
        }
        let response = app
            .clone()
            .oneshot(
                req.header("content-type", "application/json")
                    .body(Body::from(input.map(|v| v.to_string()).unwrap_or_default()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    #[tokio::test]
    async fn configuration_owner_origin_and_revision() {
        let (dir, app, owner, guest) = fixture();
        assert_eq!(
            request(&app, "", None, None).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request(&app, &guest, None, None).await.0,
            StatusCode::FORBIDDEN
        );
        let (status, original) = request(&app, &owner, None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(original["config"], SOURCE);
        assert_eq!(original["revision"].as_str().unwrap().len(), 64);
        assert_eq!(
            request(&app, &owner, Some(original.clone()), None).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(
                &app,
                &owner,
                Some(original.clone()),
                Some("https://evil.example")
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(
                &app,
                &guest,
                Some(original.clone()),
                Some("http://127.0.0.1")
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        let updated = format!("{SOURCE}# new comment\n");
        let payload = json!({"config": updated, "revision": original["revision"]});
        let (status, saved) = request(
            &app,
            &owner,
            Some(payload.clone()),
            Some("http://127.0.0.1"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(saved["config"], updated);
        assert_ne!(saved["revision"], original["revision"]);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.toml")).unwrap(),
            updated
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.toml.bak")).unwrap(),
            SOURCE
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for name in ["config.toml", "config.toml.bak"] {
                assert_eq!(
                    std::fs::metadata(dir.path().join(name))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600
                );
            }
        }
        assert_eq!(
            request(&app, &owner, Some(payload), Some("http://127.0.0.1"))
                .await
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.toml.bak")).unwrap(),
            SOURCE
        );
    }

    #[tokio::test]
    async fn configuration_rejects_invalid_without_leaking_or_changing_files() {
        let (dir, app, owner, _) = fixture();
        let (_, original) = request(&app, &owner, None, None).await;
        for source in [
            "api_key='PRIVATE_KEY'\nBROKEN",
            "[gateway]\n[provider]",
            "[gateway]\n[provider.primary]\ntype='openai'\nmodel=''\napi_key='PRIVATE_KEY'",
        ] {
            let (status, error) = request(
                &app,
                &owner,
                Some(json!({"config":source,"revision":original["revision"]})),
                Some("http://127.0.0.1"),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(!error.to_string().contains("PRIVATE_KEY"));
            assert_eq!(
                std::fs::read_to_string(dir.path().join("config.toml")).unwrap(),
                SOURCE
            );
            assert!(!dir.path().join("config.toml.bak").exists());
        }
    }
    #[test]
    fn configuration_validation_is_offline_and_sanitized() {
        for suffix in [
            "[owner]\ninterface='missing'\nuser_id=42",
            "[research]\nsearxng_url='file:///PRIVATE_KEY'",
            "[research]\nobscura_path=''",
            "[companion]\ntimezone='PRIVATE_KEY'",
            "[companion]\ninterface='missing'\nchat_id=42",
            "[web]\nenabled=true\nbase_url='https://PRIVATE_KEY@example.com'",
            "[sessions]\ncompact_threshold_tokens=1",
            "[mcp.private]\ntype='http'\nurl='file:///PRIVATE_KEY'",
            "[mcp.private]\ntype='http'\nurl='https://example.com'\nheaders={Authorization='[REDACTED:auth_header]',authorization='other'}",
        ] {
            let error = super::validate_source(&format!("{SOURCE}{suffix}\n"))
                .unwrap_err()
                .to_string();
            assert!(!error.contains("PRIVATE_KEY"));
        }
        for source in [
            SOURCE.replace("api_key='PRIVATE_KEY'", "api_key=' '"),
            SOURCE.replace("provider.primary", "provider.secondary"),
            SOURCE.replace("provider.primary", "provider.\"\""),
        ] {
            assert!(super::validate_source(&source).is_err());
        }
        assert!(super::validate_source(SOURCE).is_ok());
        let error = super::validate_source("[gateway]\nPRIVATE_KEY=")
            .unwrap_err()
            .to_string();
        assert!(error.contains("line 2"));
        assert!(!error.contains("PRIVATE_KEY"));
    }

    #[tokio::test]
    async fn configuration_limits_body_and_serializes_concurrent_saves() {
        let (dir, app, owner, _) = fixture();
        let (_, original) = request(&app, &owner, None, None).await;
        let oversized = json!({"config": "X".repeat(256*1024), "revision": original["revision"]});
        assert_eq!(
            request(&app, &owner, Some(oversized), Some("http://127.0.0.1"))
                .await
                .0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        let payload =
            json!({"config": format!("{SOURCE}# update\n"), "revision": original["revision"]});
        let (a, b) = tokio::join!(
            request(
                &app,
                &owner,
                Some(payload.clone()),
                Some("http://127.0.0.1")
            ),
            request(&app, &owner, Some(payload), Some("http://127.0.0.1"))
        );
        assert!([a.0, b.0].contains(&StatusCode::OK));
        assert!([a.0, b.0].contains(&StatusCode::CONFLICT));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.toml.bak")).unwrap(),
            SOURCE
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn configuration_save_preserves_managed_symlink() {
        let (dir, app, owner, _) = fixture();
        let path = dir.path().join("config.toml");
        let target = dir.path().join("managed.toml");
        std::fs::rename(&path, &target).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let (_, original) = request(&app, &owner, None, None).await;
        let (status, _) = request(
            &app,
            &owner,
            Some(serde_json::json!({
                "config":format!("{SOURCE}# changed\n"), "revision":original["revision"]
            })),
            Some("http://127.0.0.1"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(std::fs::symlink_metadata(path).unwrap().is_symlink());
        assert_eq!(std::fs::read_to_string(target).unwrap(), SOURCE);
    }
}
