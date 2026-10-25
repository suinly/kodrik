use super::{
    Settings,
    auth::{
        AuthStore, LoginLimiter, origin_allowed, request_token, safe_return_path, session_cookie,
    },
    pages::{PageStore, Visibility, validate_slug},
};
use axum::{
    Extension, Router,
    body::Body,
    extract::{ConnectInfo, DefaultBodyLimit, Form, OriginalUri, Path, Query, Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{delete, get, post},
};
use futures::StreamExt;
use serde::Deserialize;
use std::{
    net::{IpAddr, SocketAddr},
    path::Path as FsPath,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
#[path = "telegram_login.rs"]
mod telegram_login;

#[derive(Clone)]
struct ServerState {
    settings: Settings,
    pages: PageStore,
    auth: AuthStore,
    limiter: Arc<Mutex<LoginLimiter>>,
    datasets: Option<crate::agent::datasets::DatasetStore>,
}

fn client_ip(
    settings: &Settings,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
) -> Result<IpAddr, StatusCode> {
    let peer = peer.ok_or(StatusCode::BAD_REQUEST)?.ip();
    if !settings.trusted_proxies.contains(&peer) {
        return Ok(peer);
    }
    let mut forwarded = headers.get_all("x-forwarded-for").iter();
    let value = forwarded.next().ok_or(StatusCode::BAD_REQUEST)?;
    if forwarded.next().is_some() {
        return Err(StatusCode::BAD_REQUEST);
    }
    value
        .to_str()
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or(StatusCode::BAD_REQUEST)
}

impl ServerState {
    fn allow_login(
        &self,
        headers: &HeaderMap,
        peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    ) -> Result<(), Response> {
        let client = client_ip(
            &self.settings,
            headers,
            peer.map(|Extension(ConnectInfo(peer))| peer),
        )
        .map_err(IntoResponse::into_response)?;
        if self
            .limiter
            .lock()
            .is_ok_and(|mut limiter| limiter.allow(client, Instant::now()))
        {
            Ok(())
        } else {
            Err((StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, "60")]).into_response())
        }
    }
}

#[cfg(test)]
pub(crate) fn router(settings: Settings, data: &FsPath) -> Router {
    router_with_chat(settings, data, None)
}

#[cfg(test)]
pub(crate) fn router_with_chat(
    settings: Settings,
    data: &FsPath,
    chat: Option<super::chat::Chat>,
) -> Router {
    router_with_resources(settings, data, chat, None).layer(middleware::from_fn(
        |mut request: Request, next: Next| async move {
            // In-process test requests have no socket; keep that choice explicit here.
            if request
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .is_none()
            {
                request
                    .extensions_mut()
                    .insert(ConnectInfo("127.0.0.1:0".parse::<SocketAddr>().unwrap()));
            }
            next.run(request).await
        },
    ))
}

pub(crate) fn router_with_resources(
    settings: Settings,
    data: &FsPath,
    chat: Option<super::chat::Chat>,
    datasets: Option<crate::agent::datasets::DatasetStore>,
) -> Router {
    let state = ServerState {
        settings: settings.clone(),
        pages: PageStore::new(data),
        auth: AuthStore::new(data),
        limiter: Arc::new(Mutex::new(LoginLimiter::new())),
        datasets,
    };
    let routes = Router::new()
        .route("/", get(home))
        .route("/i", get(invite_get))
        .route("/auth/login", get(login_get).post(login_post))
        .route("/auth/logout", post(logout_post))
        .route("/auth/telegram/start", post(telegram_login::start))
        .route("/auth/telegram/poll", post(telegram_login::poll))
        .route("/auth/telegram/cancel", post(telegram_login::cancel))
        .route("/pages/{slug}", get(page_root_redirect))
        .route("/pages/{slug}/", get(page_index))
        .route("/pages/{slug}/{*path}", get(page))
        .route("/api/pages", get(page_catalog))
        .route("/api/pages/{slug}", delete(page_delete))
        .route(
            "/api/documents/{slug}/sources/{source}",
            get(document_source),
        )
        .route(
            "/api/documents/{slug}/actions/{action}",
            post(document_action),
        )
        .layer(DefaultBodyLimit::max(4096))
        .with_state(state.clone())
        .merge(super::chat::routes(settings.clone(), data, chat));
    let app = if settings.prefix() == "/" {
        routes
    } else {
        Router::new()
            .nest(settings.prefix().trim_end_matches('/'), routes)
            .route(settings.prefix(), get(home).with_state(state))
    };
    let requests = Arc::new(tokio::sync::Semaphore::new(64));
    let upload_path = format!("{}api/chat/send", settings.prefix());
    app.layer(middleware::from_fn(move |request: Request, next: Next| {
        let requests = requests.clone();
        let timeout = Duration::from_secs(
            if request.method() == axum::http::Method::POST
                && request.uri().path() == upload_path
                && super::chat_upload::is_multipart(&request)
            {
                300
            } else {
                10
            },
        );
        async move {
            let mut response = match requests.try_acquire_owned() {
                Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
                Ok(permit) => match tokio::time::timeout(timeout, next.run(request)).await {
                    Ok(mut response) => {
                        if response.extensions_mut().remove::<EventStream>().is_some() {
                            response
                        } else {
                            hold_permit(response, permit)
                        }
                    }
                    Err(_) => StatusCode::REQUEST_TIMEOUT.into_response(),
                },
            };
            for (name, value) in [
                (header::CACHE_CONTROL, "no-store"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                (header::REFERRER_POLICY, "same-origin"),
            ] {
                response.headers_mut().insert(name, value.parse().unwrap());
            }
            response
        }
    }))
}

#[derive(Clone)]
pub(super) struct EventStream;

struct LimitedChunk {
    bytes: axum::body::Bytes,
    _permit: Arc<tokio::sync::OwnedSemaphorePermit>,
}

impl AsRef<[u8]> for LimitedChunk {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

pub(super) fn hold_permit(
    response: Response,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Response {
    let (parts, body) = response.into_parts();
    let permit = Arc::new(permit);
    let stream = body.into_data_stream().map(move |chunk| {
        chunk.map(|bytes| {
            axum::body::Bytes::from_owner(LimitedChunk {
                bytes,
                _permit: permit.clone(),
            })
        })
    });
    Response::from_parts(parts, Body::from_stream(stream))
}

pub(crate) async fn serve_with_shutdown(
    listener: tokio::net::TcpListener,
    app: Router,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut stopping = shutdown.clone();
    let server = async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = shutdown.wait_for(|stopped| *stopped).await;
        })
        .await
    };
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result?,
        _ = async { let _ = stopping.wait_for(|stopped| *stopped).await; } => {
            // A partial request or stalled reader must not hold up a process restart.
            match tokio::time::timeout(Duration::from_secs(5), server).await {
                Ok(result) => result?,
                Err(_) => log::warn!("HTTP shutdown exceeded 5 seconds; continuing process shutdown"),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) async fn serve(listener: tokio::net::TcpListener, app: Router) -> anyhow::Result<()> {
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

async fn blocking(work: impl FnOnce() -> anyhow::Result<Response> + Send + 'static) -> Response {
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            log::warn!("Web request failed: {error:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(error) => {
            log::warn!("Web request task failed: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Default, Deserialize)]
struct LoginQuery {
    return_to: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginForm {
    password: String,
    return_to: Option<String>,
}

fn login_form(settings: &Settings, return_to: &str, error: &str) -> Response {
    super::chat_assets::login(settings.prefix(), return_to, error)
}

async fn home(State(state): State<ServerState>, headers: HeaderMap) -> Response {
    blocking(move || {
        let logged_in = match request_token(&headers) {
            Some(token) => state
                .auth
                .identity(&token, chrono::Utc::now().timestamp())?
                .is_some(),
            None => false,
        };
        Ok(super::chat_assets::home(state.settings.prefix(), logged_in))
    })
    .await
}

async fn login_get(State(state): State<ServerState>, Query(query): Query<LoginQuery>) -> Response {
    let return_to = safe_return_path(
        &state.settings,
        query
            .return_to
            .as_deref()
            .unwrap_or(state.settings.prefix()),
    );
    login_form(&state.settings, &return_to, "")
}

async fn invite_get(State(state): State<ServerState>) -> Response {
    login_form(
        &state.settings,
        &format!("{}app/chat", state.settings.prefix()),
        "",
    )
}

pub(super) fn valid_origin(settings: &Settings, headers: &HeaderMap) -> bool {
    headers.get_all(header::ORIGIN).iter().count() == 1
        && origin_allowed(
            settings,
            headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()),
        )
}

async fn login_post(
    State(state): State<ServerState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    form: Result<Form<LoginForm>, axum::extract::rejection::FormRejection>,
) -> Response {
    if !valid_origin(&state.settings, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let form = match form {
        Ok(Form(form)) => form,
        Err(error) if error.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    if let Err(response) = state.allow_login(&headers, peer) {
        return response;
    }
    blocking(move || {
        let return_to = safe_return_path(
            &state.settings,
            form.return_to.as_deref().unwrap_or(state.settings.prefix()),
        );
        let now = chrono::Utc::now().timestamp();
        let token = state.auth.login(&form.password, now)?;
        let Some(token) = token else {
            let mut response = login_form(
                &state.settings,
                &return_to,
                "Не удалось войти. Проверьте пароль.",
            );
            *response.status_mut() = StatusCode::UNAUTHORIZED;
            return Ok(response);
        };
        let mut response = Redirect::to(&state.settings.url(&return_to)?).into_response();
        response.headers_mut().insert(
            header::SET_COOKIE,
            session_cookie(&state.settings, &token, false).parse()?,
        );
        Ok(response)
    })
    .await
}

async fn logout_post(State(state): State<ServerState>, headers: HeaderMap) -> Response {
    if !valid_origin(&state.settings, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    blocking(move || {
        if let Some(token) = request_token(&headers) {
            state.auth.logout(&token)?;
        }
        let mut response = Redirect::to(state.settings.base_url.as_str()).into_response();
        response.headers_mut().insert(
            header::SET_COOKIE,
            session_cookie(&state.settings, "", true).parse()?,
        );
        Ok(response)
    })
    .await
}

async fn page_index(
    State(state): State<ServerState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    method: Method,
    OriginalUri(uri): OriginalUri,
) -> Response {
    page_response(
        state,
        slug,
        "index.html".into(),
        headers,
        method,
        uri.to_string(),
        false,
    )
    .await
}

async fn page_delete(
    State(state): State<ServerState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !valid_origin(&state.settings, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if validate_slug(&slug).is_err() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    blocking(move || {
        let authenticated = match request_token(&headers) {
            Some(token) => state
                .auth
                .authenticated(&token, chrono::Utc::now().timestamp())?,
            None => false,
        };
        if !authenticated {
            return Ok(StatusCode::UNAUTHORIZED.into_response());
        }
        state.pages.delete(&slug)?;
        Ok(StatusCode::NO_CONTENT.into_response())
    })
    .await
}

async fn page_catalog(State(state): State<ServerState>, headers: HeaderMap) -> Response {
    blocking(move || {
        let Some(token) = request_token(&headers) else {
            return Ok(StatusCode::UNAUTHORIZED.into_response());
        };
        let Some(_) = state
            .auth
            .identity(&token, chrono::Utc::now().timestamp())?
        else {
            return Ok(StatusCode::UNAUTHORIZED.into_response());
        };
        let mut pages = Vec::new();
        if state
            .auth
            .authenticated(&token, chrono::Utc::now().timestamp())?
        {
            for page in state.pages.list()? {
                pages.push(serde_json::json!({
                    "url": state.settings.url(&format!("pages/{}/", page.slug))?,
                    "slug": page.slug,
                    "title": page.title,
                    "visibility": page.visibility,
                    "document": page.document,
                }));
            }
        }
        Ok(axum::Json(pages).into_response())
    })
    .await
}
async fn page_root_redirect(
    State(state): State<ServerState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    method: Method,
    OriginalUri(uri): OriginalUri,
) -> Response {
    page_response(
        state,
        slug,
        "index.html".into(),
        headers,
        method,
        uri.to_string(),
        true,
    )
    .await
}
async fn page(
    State(state): State<ServerState>,
    Path((slug, path)): Path<(String, String)>,
    headers: HeaderMap,
    method: Method,
    OriginalUri(uri): OriginalUri,
) -> Response {
    page_response(state, slug, path, headers, method, uri.to_string(), false).await
}

async fn page_response(
    state: ServerState,
    slug: String,
    path: String,
    headers: HeaderMap,
    method: Method,
    return_to: String,
    redirect: bool,
) -> Response {
    if validate_slug(&slug).is_err()
        || path.is_empty()
        || path.contains(['\\', '\0'])
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    blocking(move || {
        let Some(manifest) = state.pages.manifest(&slug)? else {
            return Ok(StatusCode::NOT_FOUND.into_response());
        };
        if manifest.visibility == Visibility::Private {
            let authenticated = match request_token(&headers) {
                Some(token) => state
                    .auth
                    .authenticated(&token, chrono::Utc::now().timestamp())?,
                None => false,
            };
            if !authenticated {
                if path.ends_with(".html") {
                    let mut url = state.settings.base_url.join("auth/login")?;
                    url.query_pairs_mut()
                        .append_pair("return_to", &safe_return_path(&state.settings, &return_to));
                    return Ok(Redirect::to(url.as_str()).into_response());
                }
                return Ok(StatusCode::UNAUTHORIZED.into_response());
            }
        }
        if redirect {
            return Ok(
                Redirect::to(&state.settings.url(&format!("pages/{slug}/"))?).into_response(),
            );
        }
        if manifest.document {
            if path == "index.html" {
                return super::chat_assets::document(&state.settings, &slug);
            }
            if path != "document.json" {
                return Ok(StatusCode::NOT_FOUND.into_response());
            }
            let published = load_document(&state.pages, &slug, &manifest)?;
            let mut snapshot = if manifest.visibility == Visibility::Private {
                if let Some(datasets) = &state.datasets {
                    published.document.snapshot(datasets)?
                } else {
                    published.snapshot.clone()
                }
            } else {
                published.snapshot.clone()
            };
            if manifest.visibility == Visibility::Private && state.datasets.is_some() {
                for (index, source) in published.document.sources.iter().enumerate() {
                    if source.query.is_some() {
                        snapshot["sources"][index]["url"] =
                            serde_json::json!(state.settings.url(&format!(
                                "api/documents/{slug}/sources/{}?release={}",
                                source.id, manifest.release
                            ))?);
                    }
                }
            }
            // Frozen publications never carry write capabilities.
            if let Some(actions) = snapshot["actions"].as_array_mut() {
                for action in actions.iter_mut() {
                    if let Some(object) = action.as_object_mut() {
                        object.remove("url");
                    }
                }
                if manifest.visibility == Visibility::Private && state.datasets.is_some() {
                    for (index, action) in published.document.actions.iter().enumerate() {
                        actions[index]["url"] = serde_json::json!(state.settings.url(&format!(
                            "api/documents/{slug}/actions/{}?release={}",
                            action.id, manifest.release
                        ))?);
                    }
                }
            }
            return Ok(axum::Json(snapshot).into_response());
        }
        let Some(file) = state.pages.read(&slug, &manifest, &path)? else {
            return Ok(StatusCode::NOT_FOUND.into_response());
        };
        let mut response = Response::builder()
            .header(header::CONTENT_TYPE, file.mime)
            .header(header::CONTENT_LENGTH, file.bytes.len());
        if file.download {
            response = response.header(header::CONTENT_DISPOSITION, "attachment");
        }
        Ok(response.body(if method == Method::HEAD {
            Body::empty()
        } else {
            Body::from(file.bytes)
        })?)
    })
    .await
}

fn load_document(
    pages: &PageStore,
    slug: &str,
    manifest: &super::pages::Manifest,
) -> anyhow::Result<super::documents::PublishedDocument> {
    let file = pages
        .read(slug, manifest, "document.json")?
        .ok_or_else(|| anyhow::anyhow!("Document release is missing"))?;
    Ok(serde_json::from_slice(&file.bytes)?)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourcePage {
    cursor: Option<String>,
    release: Option<uuid::Uuid>,
}

async fn document_source(
    State(state): State<ServerState>,
    Path((slug, source)): Path<(String, String)>,
    Query(query): Query<SourcePage>,
    headers: HeaderMap,
) -> Response {
    if validate_slug(&slug).is_err()
        || source.len() > 64
        || query.cursor.as_ref().is_some_and(|s| s.len() > 1024)
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    blocking(move || {
        let Some(manifest) = state.pages.manifest(&slug)? else {
            return Ok(StatusCode::NOT_FOUND.into_response());
        };
        if !manifest.document {
            return Ok(StatusCode::NOT_FOUND.into_response());
        }
        if manifest.visibility == Visibility::Public {
            return Ok(StatusCode::FORBIDDEN.into_response());
        }
        let authenticated = match request_token(&headers) {
            Some(token) => state
                .auth
                .authenticated(&token, chrono::Utc::now().timestamp())?,
            None => false,
        };
        if !authenticated {
            return Ok(StatusCode::UNAUTHORIZED.into_response());
        }
        if query.release.is_some_and(|r| r != manifest.release) {
            return Ok((
                StatusCode::CONFLICT,
                axum::Json(serde_json::json!({"error":"Страница обновлена. Загрузите её заново."})),
            )
                .into_response());
        }
        let published = load_document(&state.pages, &slug, &manifest)?;
        let Some(source) = published.document.sources.iter().find(|s| s.id == source) else {
            return Ok(StatusCode::NOT_FOUND.into_response());
        };
        if source.query.is_none() {
            return Ok(StatusCode::NOT_FOUND.into_response());
        }
        let Some(store) = state.datasets else {
            return Ok(StatusCode::SERVICE_UNAVAILABLE.into_response());
        };
        match source.read(&store, query.cursor.as_deref()) {
            Ok(value) => Ok(axum::Json(value).into_response()),
            Err(error) => Ok((
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error":error.to_string()})),
            )
                .into_response()),
        }
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionRelease {
    release: Option<uuid::Uuid>,
}

fn action_error(status: StatusCode, message: impl ToString) -> Response {
    (
        status,
        axum::Json(serde_json::json!({"error":message.to_string()})),
    )
        .into_response()
}

async fn document_action(
    State(state): State<ServerState>,
    Path((slug, action)): Path<(String, String)>,
    query: Result<Query<ActionRelease>, axum::extract::rejection::QueryRejection>,
    request: Request,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(error) => return action_error(StatusCode::BAD_REQUEST, error),
    };
    if validate_slug(&slug).is_err() || action.len() > 64 {
        return action_error(StatusCode::BAD_REQUEST, "Invalid document or action id");
    }
    let (parts, body) = request.into_parts();
    if !origin_allowed(
        &state.settings,
        parts
            .headers
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok()),
    ) {
        return action_error(StatusCode::FORBIDDEN, "A same-origin request is required");
    }
    if !parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .is_some_and(|v| v.trim() == "application/json")
        })
    {
        return action_error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Use application/json");
    }
    let bytes = match axum::body::to_bytes(body, 16 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return action_error(StatusCode::PAYLOAD_TOO_LARGE, "Form request exceeds 16 KiB");
        }
    };
    let input: super::documents::FormRequest = match serde_json::from_slice(&bytes) {
        Ok(input) => input,
        Err(error) => return action_error(StatusCode::BAD_REQUEST, error),
    };
    blocking(move || {
        let authenticated = match request_token(&parts.headers) {
            Some(token) => state
                .auth
                .authenticated(&token, chrono::Utc::now().timestamp())?,
            None => false,
        };
        if !authenticated {
            return Ok(action_error(
                StatusCode::UNAUTHORIZED,
                "Owner login is required",
            ));
        }
        let Some(manifest) = state.pages.manifest(&slug)? else {
            return Ok(action_error(
                StatusCode::NOT_FOUND,
                "Document does not exist",
            ));
        };
        if !manifest.document {
            return Ok(action_error(
                StatusCode::NOT_FOUND,
                "Document does not exist",
            ));
        }
        if manifest.visibility != Visibility::Private {
            return Ok(action_error(
                StatusCode::FORBIDDEN,
                "Public documents cannot write datasets",
            ));
        }
        if query.release != Some(manifest.release) {
            return Ok(action_error(
                StatusCode::CONFLICT,
                "Page has changed. Reload before submitting.",
            ));
        }
        let published = load_document(&state.pages, &slug, &manifest)?;
        let Some(action) = published
            .document
            .actions
            .iter()
            .find(|candidate| candidate.id == action)
        else {
            return Ok(action_error(StatusCode::NOT_FOUND, "Action does not exist"));
        };
        let Some(store) = state.datasets else {
            return Ok(action_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Datasets unavailable",
            ));
        };
        match action.submit(&store, input) {
            Ok(value) => Ok(axum::Json(value).into_response()),
            Err(error) => {
                let message = error.to_string();
                let status = if message.contains("Revision conflict")
                    || message.contains("different payload")
                {
                    StatusCode::CONFLICT
                } else {
                    StatusCode::BAD_REQUEST
                };
                Ok(action_error(status, message))
            }
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::WebConfig,
        web::{
            auth::AuthStore,
            pages::{PageStore, Visibility},
        },
    };

    #[tokio::test]
    async fn dashboard_routes_preserve_login_destinations_with_and_without_prefix() {
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        for prefix in ["/", "/kodrik/"] {
            let settings = Settings {
                trusted_proxies: vec![],
                listen: "127.0.0.1:0".parse().unwrap(),
                base_url: format!("http://localhost{prefix}").parse().unwrap(),
            };
            let app = router(settings, dir.path());
            for section in ["chat", "pages", "webhooks"] {
                for suffix in ["", "/", "/telegram%3Amain%3A7"] {
                    let path = format!("{prefix}app/{section}{suffix}");
                    let response = app
                        .clone()
                        .oneshot(Request::builder().uri(&path).body(Body::empty()).unwrap())
                        .await
                        .unwrap();
                    assert_eq!(response.status(), StatusCode::SEE_OTHER);
                    let location =
                        reqwest::Url::parse(response.headers()[header::LOCATION].to_str().unwrap())
                            .unwrap();
                    assert_eq!(location.path(), format!("{prefix}auth/login"));
                    assert_eq!(
                        location
                            .query_pairs()
                            .find(|(key, _)| key == "return_to")
                            .unwrap()
                            .1,
                        path
                    );
                }
            }
            for suffix in ["app", "app/"] {
                let response = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .uri(format!("{prefix}{suffix}"))
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::SEE_OTHER);
                assert_eq!(
                    response.headers()[header::LOCATION],
                    format!("{prefix}app/chat")
                );
            }
        }
    }

    #[tokio::test]
    async fn page_deletion_requires_owner_and_origin_and_revokes_published_urls() {
        use crate::web::auth::Identity;
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            trusted_proxies: vec![],
            listen: "127.0.0.1:0".parse().unwrap(),
            base_url: "http://127.0.0.1/kodrik/".parse().unwrap(),
        };
        let auth = AuthStore::new(dir.path());
        let now = chrono::Utc::now().timestamp();
        auth.set_password("password").unwrap();
        auth.configure_owner(Some(42)).unwrap();
        auth.grant_test_user(7).unwrap();
        let owner = auth.login("password", now).unwrap().unwrap();
        let guest = auth
            .test_session(Identity::Telegram(7), now)
            .unwrap()
            .unwrap();
        let telegram_owner = auth
            .test_session(Identity::Telegram(42), now)
            .unwrap()
            .unwrap();
        let app = router(settings, dir.path());
        let pages = PageStore::new(dir.path());
        let draft = pages.drafts().join("report");
        std::fs::create_dir_all(&draft).unwrap();
        std::fs::write(draft.join("index.html"), "report").unwrap();
        std::fs::write(draft.join("data.json"), "{}").unwrap();
        pages.publish("report", Some(Visibility::Public)).unwrap();
        async fn remove(app: &Router, slug: &str, token: &str, origin: Option<&str>) -> StatusCode {
            let mut request = Request::builder()
                .method(Method::DELETE)
                .uri(format!("/kodrik/api/pages/{slug}"))
                .header(header::COOKIE, format!("kodrik_web_session={token}"));
            if let Some(origin) = origin {
                request = request.header(header::ORIGIN, origin);
            }
            app.clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status()
        }
        for (token, origin, expected) in [
            ("", Some("http://127.0.0.1"), StatusCode::UNAUTHORIZED),
            (
                guest.as_str(),
                Some("http://127.0.0.1"),
                StatusCode::UNAUTHORIZED,
            ),
            (owner.as_str(), None, StatusCode::FORBIDDEN),
            (
                owner.as_str(),
                Some("https://evil.example"),
                StatusCode::FORBIDDEN,
            ),
        ] {
            assert_eq!(remove(&app, "report", token, origin).await, expected);
            assert!(pages.manifest("report").unwrap().is_some());
        }
        assert_eq!(
            remove(&app, "INVALID", &owner, Some("http://127.0.0.1")).await,
            StatusCode::BAD_REQUEST
        );
        for token in [&owner, &telegram_owner] {
            assert_eq!(
                remove(&app, "report", token, Some("http://127.0.0.1")).await,
                StatusCode::NO_CONTENT
            );
            assert!(pages.list().unwrap().is_empty());
            for path in ["pages/report/", "pages/report/data.json"] {
                let response = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .uri(format!("/kodrik/{path}"))
                            .header(header::COOKIE, format!("kodrik_web_session={owner}"))
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::NOT_FOUND);
                assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            }
            assert_eq!(
                remove(&app, "report", token, Some("http://127.0.0.1")).await,
                StatusCode::NO_CONTENT
            );
            pages.publish("report", Some(Visibility::Public)).unwrap();
        }
    }

    #[tokio::test]
    async fn page_catalog_lists_current_publications_only_for_owner_sessions() {
        use crate::web::{
            auth::Identity,
            documents::{Document, PublishedDocument},
        };
        use serde_json::{Value, json};
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            trusted_proxies: vec![],
            listen: "127.0.0.1:0".parse().unwrap(),
            base_url: "http://127.0.0.1/kodrik/".parse().unwrap(),
        };
        let auth = AuthStore::new(dir.path());
        let now = chrono::Utc::now().timestamp();
        auth.set_password("password").unwrap();
        auth.configure_owner(Some(42)).unwrap();
        auth.grant_test_user(7).unwrap();
        let telegram_owner = auth
            .test_session(Identity::Telegram(42), now)
            .unwrap()
            .unwrap();
        let owner = auth.login("password", now).unwrap().unwrap();
        let guest = auth
            .test_session(Identity::Telegram(7), now)
            .unwrap()
            .unwrap();
        let app = router(settings, dir.path());
        async fn catalog(app: &Router, token: &str, status: StatusCode) -> Value {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/kodrik/api/pages")
                        .header(header::COOKIE, format!("kodrik_web_session={token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap();
            if status == StatusCode::OK {
                serde_json::from_slice(&bytes).unwrap()
            } else {
                Value::Null
            }
        }
        catalog(&app, "", StatusCode::UNAUTHORIZED).await;
        assert_eq!(catalog(&app, &owner, StatusCode::OK).await, json!([]));
        let pages = PageStore::new(dir.path());
        let document = |title: &str| PublishedDocument {
            document: Document {
                title: title.into(),
                body: "Report".into(),
                sources: vec![],
                views: vec![],
                actions: vec![],
            },
            snapshot: json!({}),
        };
        pages
            .publish_document("nutrition", &document("Old title"), None)
            .unwrap();
        pages
            .publish_document("nutrition", &document("Meals"), Some(Visibility::Public))
            .unwrap();
        let draft = pages.drafts().join("legacy");
        std::fs::create_dir_all(&draft).unwrap();
        std::fs::write(draft.join("index.html"), "Legacy page").unwrap();
        pages.publish("legacy", None).unwrap();
        std::fs::create_dir_all(pages.drafts().join("unpublished")).unwrap();
        let expected = json!([
            {"slug":"legacy","title":"legacy","document":false,"visibility":"private","url":"http://127.0.0.1/kodrik/pages/legacy/"},
            {"slug":"nutrition","title":"Meals","document":true,"visibility":"public","url":"http://127.0.0.1/kodrik/pages/nutrition/"}
        ]);
        assert_eq!(catalog(&app, &owner, StatusCode::OK).await, expected);
        assert_eq!(
            catalog(&app, &telegram_owner, StatusCode::OK).await,
            expected
        );
        for path in [
            "pages/legacy/",
            "pages/nutrition/",
            "pages/nutrition/document.json",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/kodrik/{path}"))
                        .header(
                            header::COOKIE,
                            format!("kodrik_web_session={telegram_owner}"),
                        )
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
        }
        assert_eq!(catalog(&app, &guest, StatusCode::OK).await, json!([]));
        auth.logout(&owner).unwrap();
        catalog(&app, &owner, StatusCode::UNAUTHORIZED).await;
    }

    #[test]
    fn forwarded_client_ip_requires_explicit_trusted_peer_and_one_valid_address() {
        let mut settings = Settings {
            trusted_proxies: Vec::new(),
            listen: "127.0.0.1:0".parse().unwrap(),
            base_url: "http://127.0.0.1/".parse().unwrap(),
        };
        let peer: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "192.0.2.1".parse().unwrap());
        assert_eq!(client_ip(&settings, &headers, Some(peer)), Ok(peer.ip()));
        assert_eq!(
            client_ip(&settings, &headers, None),
            Err(StatusCode::BAD_REQUEST)
        );
        settings.trusted_proxies.push(peer.ip());
        assert_eq!(
            client_ip(&settings, &headers, Some(peer)),
            Ok("192.0.2.1".parse().unwrap())
        );
        for invalid in [
            "",
            "unknown",
            "192.0.2.1, 192.0.2.2",
            "192.0.2.1:80",
            " 192.0.2.1",
        ] {
            headers.insert("x-forwarded-for", invalid.parse().unwrap());
            assert_eq!(
                client_ip(&settings, &headers, Some(peer)),
                Err(StatusCode::BAD_REQUEST)
            );
        }
        headers.insert("x-forwarded-for", "2001:db8::1".parse().unwrap());
        assert_eq!(
            client_ip(&settings, &headers, Some(peer)),
            Ok("2001:db8::1".parse().unwrap())
        );
        headers.append("x-forwarded-for", "192.0.2.2".parse().unwrap());
        assert_eq!(
            client_ip(&settings, &headers, Some(peer)),
            Err(StatusCode::BAD_REQUEST)
        );
        headers.clear();
        assert_eq!(
            client_ip(&settings, &headers, Some(peer)),
            Err(StatusCode::BAD_REQUEST)
        );
    }

    #[tokio::test]
    async fn trusted_proxy_limits_use_forwarded_client_and_share_login_routes() {
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            trusted_proxies: vec!["127.0.0.1".parse().unwrap()],
            listen: "127.0.0.1:0".parse().unwrap(),
            base_url: "http://127.0.0.1/".parse().unwrap(),
        };
        let app = router(settings, dir.path());
        for (path, ip, expected) in [
            ("login", "192.0.2.1", 401),
            ("login", "192.0.2.1", 401),
            ("login", "192.0.2.1", 401),
            ("login", "192.0.2.1", 401),
            ("login", "192.0.2.1", 401),
            ("telegram/start", "192.0.2.1", 429),
            ("login", "192.0.2.2", 401),
            ("login", "192.0.2.2, 192.0.2.3", 400),
        ] {
            let (content_type, body) = if path == "login" {
                ("application/x-www-form-urlencoded", "password=wrong")
            } else {
                ("application/json", "{}")
            };
            let request = axum::http::Request::post(format!("/auth/{path}"))
                .header("origin", "http://127.0.0.1")
                .header("content-type", content_type)
                .header("x-forwarded-for", ip)
                .body(Body::from(body))
                .unwrap();
            assert_eq!(
                app.clone()
                    .oneshot(request)
                    .await
                    .unwrap()
                    .status()
                    .as_u16(),
                expected
            );
        }
        let app = router_with_resources(
            Settings {
                trusted_proxies: vec![],
                listen: "127.0.0.1:0".parse().unwrap(),
                base_url: "http://127.0.0.1/".parse().unwrap(),
            },
            dir.path(),
            None,
            None,
        );
        let request = axum::http::Request::post("/auth/login")
            .header("origin", "http://127.0.0.1")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("password=wrong"))
            .unwrap();
        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn unauthenticated_event_paths_keep_ordinary_response_limits() {
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let app = router(
            Settings {
                trusted_proxies: Vec::new(),
                listen: "127.0.0.1:0".parse().unwrap(),
                base_url: "http://127.0.0.1/".parse().unwrap(),
            },
            dir.path(),
        );
        let request = |path: &str| axum::http::Request::get(path).body(Body::empty()).unwrap();
        let mut responses = Vec::new();
        for _ in 0..64 {
            let response = app
                .clone()
                .oneshot(request("/api/chat/sessions/fake/events"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            responses.push(response);
        }
        assert_eq!(
            app.clone()
                .oneshot(request("/auth/login"))
                .await
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        drop(responses);
        assert_eq!(
            app.oneshot(request("/auth/login")).await.unwrap().status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn real_socket_peer_reaches_login_without_test_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let origin = format!("http://{address}");
        let settings = Settings {
            trusted_proxies: Vec::new(),
            listen: address,
            base_url: format!("{origin}/").parse().unwrap(),
        };
        let app = router_with_resources(settings, dir.path(), None, None);
        let (shutdown, receiver) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(serve_with_shutdown(listener, app, receiver));
        let response = reqwest::Client::new()
            .post(format!("{origin}/auth/login"))
            .header("origin", &origin)
            .header("content-type", "application/x-www-form-urlencoded")
            .body("password=wrong")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        shutdown.send(true).unwrap();
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn login_limits_are_separate_for_socket_clients() {
        use axum::{body::Body, extract::ConnectInfo, http::Request};
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            trusted_proxies: Vec::new(),
            listen: "127.0.0.1:0".parse().unwrap(),
            base_url: "http://127.0.0.1/".parse().unwrap(),
        };
        let router = router(settings, dir.path());
        for (ip, expected) in [("192.0.2.1:1", 401); 5]
            .into_iter()
            .chain([("192.0.2.1:1", 429), ("192.0.2.2:1", 401)])
        {
            let mut request = Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("origin", "http://127.0.0.1")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("x-forwarded-for", uuid::Uuid::new_v4().to_string())
                .body(Body::from("password=wrong"))
                .unwrap();
            request
                .extensions_mut()
                .insert(ConnectInfo(ip.parse::<std::net::SocketAddr>().unwrap()));
            assert_eq!(
                router
                    .clone()
                    .oneshot(request)
                    .await
                    .unwrap()
                    .status()
                    .as_u16(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn shutdown_does_not_wait_for_incomplete_http_request() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, receiver) = tokio::sync::watch::channel(false);
        let mut server = tokio::spawn(serve_with_shutdown(listener, Router::new(), receiver));
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        shutdown.send_replace(true);
        let stopped = tokio::time::timeout(Duration::from_secs(8), &mut server).await;
        if stopped.is_err() {
            server.abort();
        }
        stopped
            .expect("shutdown waited for the HTTP client")
            .unwrap()
            .unwrap();
        drop(client);
    }

    #[tokio::test]
    async fn shutdown_allows_in_flight_http_response_to_finish() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let app = Router::new().route(
            "/",
            get({
                let entered = entered.clone();
                let release = release.clone();
                move || async move {
                    entered.notify_one();
                    release.notified().await;
                    "finished"
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, receiver) = tokio::sync::watch::channel(false);
        let mut server = tokio::spawn(serve_with_shutdown(listener, app, receiver));
        let request = tokio::spawn(async move {
            reqwest::get(format!("http://{address}/"))
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        });
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();

        shutdown.send_replace(true);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut server)
                .await
                .is_err()
        );
        release.notify_one();
        assert_eq!(request.await.unwrap(), "finished");
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn login_accepts_only_password_and_keeps_invited_users_out_of_owner_pages() {
        use crate::web::auth::Identity;
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            trusted_proxies: Vec::new(),
            listen: "127.0.0.1:0".parse().unwrap(),
            base_url: "http://127.0.0.1/kodrik/".parse().unwrap(),
        };
        let auth = AuthStore::new(dir.path());
        let now = chrono::Utc::now().timestamp();
        auth.set_password("password").unwrap();
        auth.grant_test_user(42).unwrap();
        let user_token = auth
            .test_session(Identity::Telegram(42), now)
            .unwrap()
            .unwrap();
        let draft = dir.path().join("pages/drafts/private");
        std::fs::create_dir_all(&draft).unwrap();
        std::fs::write(draft.join("index.html"), "secret").unwrap();
        std::fs::write(draft.join("data.json"), "secret").unwrap();
        PageStore::new(dir.path()).publish("private", None).unwrap();
        let app = router(settings, dir.path());
        let post = |body: String, origin: &str| {
            Request::builder()
                .method("POST")
                .uri("/kodrik/auth/login")
                .header(header::ORIGIN, origin)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap()
        };
        assert_eq!(
            app.clone()
                .oneshot(post("password=password".into(), "https://evil.example"))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        for body in [
            String::new(),
            format!("code={}", "a".repeat(64)),
            format!("code={}&password=password", "a".repeat(64)),
            "password=password&code=".into(),
            "password=password&unexpected=true".into(),
        ] {
            let response = app
                .clone()
                .oneshot(post(body, "http://127.0.0.1"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert!(!response.headers().contains_key(header::SET_COOKIE));
        }
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/kodrik/pages/private/data.json")
                    .header(header::COOKIE, format!("kodrik_web_session={user_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            auth.identity(&user_token, now).unwrap(),
            Some(Identity::Telegram(42))
        );
        assert_eq!(
            app.clone()
                .oneshot(post("password=wrong".into(), "http://127.0.0.1"))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let response = app
            .clone()
            .oneshot(post(
                "password=password&return_to=%2Fkodrik%2Fapp%2Fchat".into(),
                "http://127.0.0.1",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers()[header::LOCATION],
            "http://127.0.0.1/kodrik/app/chat"
        );
        let cookie = response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let token = cookie.split_once('=').unwrap().1.to_owned();
        assert_eq!(auth.identity(&token, now).unwrap(), Some(Identity::Owner));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/kodrik/pages/private/data.json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/kodrik/auth/logout")
                    .header(header::ORIGIN, "http://127.0.0.1")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(auth.identity(&token, now).unwrap(), None);
    }

    #[tokio::test]
    async fn password_login_uses_the_existing_attempt_limit() {
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            trusted_proxies: Vec::new(),
            listen: "127.0.0.1:0".parse().unwrap(),
            base_url: "http://127.0.0.1/kodrik/".parse().unwrap(),
        };
        let app = router(settings, dir.path());
        for attempt in 0..6 {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/kodrik/auth/login")
                        .header(header::ORIGIN, "http://127.0.0.1")
                        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                        .body(Body::from("password=wrong"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if attempt < 5 {
                    StatusCode::UNAUTHORIZED
                } else {
                    StatusCode::TOO_MANY_REQUESTS
                }
            );
        }
    }

    #[tokio::test]
    async fn document_forms_enforce_write_boundaries() {
        use crate::agent::{
            datasets::DatasetStore,
            tools::{ShowUiTool, Tool},
        };
        use serde_json::json;
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let datasets = DatasetStore::open(dir.path().join("datasets")).unwrap();
        datasets.execute(serde_json::from_value(json!({"action":"create","dataset":"items","title":"Items","fields":[{"name":"name","type":"string","required":true}]})).unwrap()).unwrap();
        let settings = Settings {
            trusted_proxies: Vec::new(),
            listen: "127.0.0.1:0".parse().unwrap(),
            base_url: "http://127.0.0.1/kodrik/".parse().unwrap(),
        };
        let tool = ShowUiTool::new(
            datasets.clone(),
            Some((dir.path().to_owned(), settings.clone())),
        );
        for (slug, visibility) in [("private", "private"), ("public", "public")] {
            tool.execute(json!({"title":"Items","body":"Form","sources":[],"actions":[{"id":"add","dataset":"items","operation":"insert","fields":["name"],"record_id":null}],"slug":slug,"visibility":visibility})).await.unwrap();
        }
        let auth = AuthStore::new(dir.path());
        auth.set_password("password").unwrap();
        let token = auth
            .login("password", chrono::Utc::now().timestamp())
            .unwrap()
            .unwrap();
        let cookie = format!("kodrik_web_session={token}");
        auth.configure_owner(Some(42)).unwrap();
        auth.grant_test_user(7).unwrap();
        let now = chrono::Utc::now().timestamp();
        let telegram = auth
            .test_session(super::super::auth::Identity::Telegram(42), now)
            .unwrap()
            .unwrap();
        let guest = auth
            .test_session(super::super::auth::Identity::Telegram(7), now)
            .unwrap()
            .unwrap();
        let telegram_cookie = format!("kodrik_web_session={telegram}");
        let guest_cookie = format!("kodrik_web_session={guest}");
        let release = PageStore::new(dir.path())
            .manifest("private")
            .unwrap()
            .unwrap()
            .release;
        let app = router_with_resources(settings, dir.path(), None, Some(datasets.clone()));
        for (slug, session, enabled) in [
            ("private", cookie.as_str(), true),
            ("private", telegram_cookie.as_str(), true),
            ("public", "", false),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/kodrik/pages/{slug}/document.json"))
                        .header(header::COOKIE, session)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let bytes = axum::body::to_bytes(response.into_body(), 65536)
                .await
                .unwrap();
            let snapshot: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(snapshot["actions"][0].get("url").is_some(), enabled);
        }
        let payload = json!({"request_id":uuid::Uuid::new_v4(),"values":{"name":"one"},"expected_revision":null,"clear_fields":[]});
        for (slug, query, session, origin, status) in [
            (
                "private",
                format!("?release={release}"),
                guest_cookie.as_str(),
                "http://127.0.0.1",
                401,
            ),
            (
                "private",
                format!("?release={release}"),
                "",
                "http://127.0.0.1",
                401,
            ),
            (
                "private",
                format!("?release={release}"),
                cookie.as_str(),
                "https://evil.example",
                403,
            ),
            (
                "public",
                format!("?release={release}"),
                cookie.as_str(),
                "http://127.0.0.1",
                403,
            ),
            (
                "private",
                String::new(),
                cookie.as_str(),
                "http://127.0.0.1",
                409,
            ),
            (
                "private",
                format!("?release={}", uuid::Uuid::new_v4()),
                cookie.as_str(),
                "http://127.0.0.1",
                409,
            ),
            (
                "private",
                format!("?release={release}"),
                telegram_cookie.as_str(),
                "http://127.0.0.1",
                200,
            ),
            (
                "private",
                format!("?release={release}"),
                cookie.as_str(),
                "http://127.0.0.1",
                200,
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(format!("/kodrik/api/documents/{slug}/actions/add{query}"))
                        .header(header::COOKIE, session)
                        .header(header::ORIGIN, origin)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(payload.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), status);
            let bytes = axum::body::to_bytes(response.into_body(), 65536)
                .await
                .unwrap();
            assert!(serde_json::from_slice::<serde_json::Value>(&bytes).is_ok());
        }
        for bad in [
            json!({"request_id":uuid::Uuid::new_v4(),"values":{"name":"bad"},"clear_fields":["name"],"expected_revision":null}),
            json!({"request_id":uuid::Uuid::new_v4(),"values":{"name":"bad"},"clear_fields":["extra"],"expected_revision":null}),
            json!({"request_id":uuid::Uuid::new_v4(),"values":{"name":"bad","extra":"no"},"expected_revision":null}),
            json!({"request_id":uuid::Uuid::new_v4(),"values":{"name":5},"expected_revision":null}),
            json!({"request_id":uuid::Uuid::new_v4(),"values":{"name":{}},"expected_revision":null}),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(format!(
                            "/kodrik/api/documents/private/actions/add?release={release}"
                        ))
                        .header(header::COOKIE, &cookie)
                        .header(header::ORIGIN, "http://127.0.0.1")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(bad.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        assert_eq!(
            datasets
                .execute(
                    serde_json::from_value(json!({"action":"query","dataset":"items"})).unwrap()
                )
                .unwrap()["records"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn native_documents_keep_public_snapshots_and_private_live_reads_separate() {
        use crate::agent::{
            datasets::DatasetStore,
            tools::{ShowUiTool, Tool},
        };
        use serde_json::{Value, json};
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let datasets = DatasetStore::open(dir.path().join("datasets")).unwrap();
        for request in [
            json!({"action":"create","dataset":"meals","title":"Meals","fields":[{"name":"kcal","type":"number","required":true}]}),
            json!({"action":"upsert","dataset":"meals","request_id":uuid::Uuid::new_v4().to_string(),"records":[{"id":"a","expected_revision":null,"values":[{"field":"kcal","value":350}]}]}),
        ] {
            datasets
                .execute(serde_json::from_value(request).unwrap())
                .unwrap();
        }
        let settings = Settings {
            trusted_proxies: Vec::new(),
            listen: "127.0.0.1:0".parse().unwrap(),
            base_url: "http://127.0.0.1/kodrik/".parse().unwrap(),
        };
        let tool = ShowUiTool::new(
            datasets.clone(),
            Some((dir.path().to_owned(), settings.clone())),
        );
        for (slug, visibility) in [("private", None), ("public", Some("public"))] {
            tool.execute(json!({"title":"Meals","body":"::data-table{source=\"meals\"}\n::","views":[{"id":"total","source":"meals","jq":"map(.kcal) | add"}],"sources":[{"id":"meals","data_json":null,"query":{
                "mode":"query","dataset":"meals","date_from":null,"date_to":null,"filters":null,"metrics":null,"group_by":null,"limit":1}}],"slug":slug,"visibility":visibility})).await.unwrap();
        }
        datasets.execute(serde_json::from_value(json!({"action":"upsert","dataset":"meals","request_id":uuid::Uuid::new_v4().to_string(),"records":[{"id":"a","expected_revision":1,"values":[{"field":"kcal","value":450}]}]})).unwrap()).unwrap();
        let auth = AuthStore::new(dir.path());
        auth.set_password("password").unwrap();
        let token = auth
            .login("password", chrono::Utc::now().timestamp())
            .unwrap()
            .unwrap();
        let cookie = session_cookie(&settings, &token, false)
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let router = router_with_resources(settings, dir.path(), None, Some(datasets));
        async fn get(router: &Router, path: &str, cookie: &str) -> (StatusCode, String) {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header(header::COOKIE, cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap();
            (status, String::from_utf8(bytes.to_vec()).unwrap())
        }
        let (status, html) = get(&router, "/kodrik/pages/public/", "").await;
        assert_eq!(status, 200);
        assert!(html.contains("kodrik-document"));
        assert!(html.contains("/kodrik/chat/_nuxt/"));
        let (status, body) = get(&router, "/kodrik/pages/public/document.json", "").await;
        assert_eq!(status, 200);
        let public: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            public["sources"][0]["data"]["records"][0]["values"]["kcal"],
            350.0
        );
        assert!(public["sources"][0].get("url").is_none());
        assert_eq!(public["views"]["total"], 350.0);
        assert_eq!(
            get(&router, "/kodrik/pages/private/document.json", "")
                .await
                .0,
            401
        );
        let (status, body) = get(&router, "/kodrik/pages/private/document.json", &cookie).await;
        assert_eq!(status, 200);
        let private: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(private["views"]["total"], 450.0);
        assert_eq!(
            private["sources"][0]["data"]["records"][0]["values"]["kcal"],
            450.0
        );
        assert!(
            private["sources"][0]["url"]
                .as_str()
                .unwrap()
                .contains("/kodrik/api/documents/private/sources/meals?release=")
        );
        assert_eq!(
            get(
                &router,
                "/kodrik/api/documents/public/sources/meals",
                &cookie
            )
            .await
            .0,
            403
        );
        assert_eq!(
            get(&router, "/kodrik/api/documents/private/sources/meals", "")
                .await
                .0,
            401
        );
        assert_eq!(
            get(
                &router,
                "/kodrik/api/documents/private/sources/meals",
                &cookie
            )
            .await
            .0,
            200
        );
        assert_eq!(
            get(
                &router,
                "/kodrik/api/documents/private/sources/unknown",
                &cookie
            )
            .await
            .0,
            404
        );
    }

    #[tokio::test]
    async fn home_uses_shared_ui_and_preserves_auth_state_under_prefix() {
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            trusted_proxies: Vec::new(),
            listen: "127.0.0.1:0".parse().unwrap(),
            base_url: "http://127.0.0.1/kodrik/".parse().unwrap(),
        };
        let auth = AuthStore::new(dir.path());
        auth.set_password("test-password").unwrap();
        let token = auth
            .login("test-password", chrono::Utc::now().timestamp())
            .unwrap()
            .unwrap();
        let cookie = session_cookie(&settings, &token, false);
        let cookie = cookie.split(';').next().unwrap();
        let app = router(settings, dir.path());
        for (cookie, logged_in) in [("", false), (cookie, true)] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/kodrik/")
                        .header(header::COOKIE, cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(
                response.headers()[header::CONTENT_SECURITY_POLICY]
                    .to_str()
                    .unwrap()
                    .contains("'nonce-")
            );
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let html = String::from_utf8(body.to_vec()).unwrap();
            assert!(html.contains("/kodrik/chat/_nuxt/"));
            let data = html
                .split("id=\"kodrik-home\" type=\"application/json\">")
                .nth(1)
                .expect("home must use the shared Nuxt UI shell")
                .split("</script>")
                .next()
                .unwrap();
            let data: serde_json::Value = serde_json::from_str(data).unwrap();
            assert_eq!(
                data,
                serde_json::json!({"prefix":"/kodrik/","loggedIn":logged_in})
            );
        }
    }

    #[tokio::test]
    async fn login_form_embeds_ui_with_inert_page_data() {
        let settings = Settings {
            trusted_proxies: Vec::new(),
            listen: "127.0.0.1:0".parse().unwrap(),
            base_url: "http://127.0.0.1/kodrik/".parse().unwrap(),
        };
        let return_to = "/kodrik/chat/?value=</script><img>";
        let error = "</script><script>alert(1)</script>";
        let response = login_form(&settings, return_to, error);
        assert!(
            response.headers()[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .contains("'nonce-")
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("/kodrik/chat/_nuxt/"));
        let data = html
            .split("id=\"kodrik-login\" type=\"application/json\">")
            .nth(1)
            .expect("login must use the Nuxt UI shell with page data")
            .split("</script>")
            .next()
            .unwrap();
        assert!(!data.contains('<'));
        let data: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(data["returnTo"], return_to);
        assert_eq!(data["error"], error);
        assert!(!html.contains("<script>alert(1)</script>"));
    }

    #[tokio::test]
    async fn chat_api_requires_login() {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/kodrik/", listener.local_addr().unwrap());
        let settings = Settings::from_config(&WebConfig {
            trusted_proxies: Vec::new(),
            enabled: true,
            listen: "127.0.0.1:0".into(),
            base_url: Some(base.clone()),
        })
        .unwrap()
        .unwrap();
        let server = tokio::spawn(serve(listener, router(settings, dir.path())));
        let response = reqwest::get(format!("{base}api/chat/sessions"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        server.abort();
    }

    #[tokio::test]
    #[ignore = "Manual browser preview; stop with Ctrl+C"]
    async fn manual_browser_preview() {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let settings = Settings::from_config(&WebConfig {
            trusted_proxies: Vec::new(),
            enabled: true,
            listen: address.to_string(),
            base_url: Some(format!("http://{address}/kodrik/")),
        })
        .unwrap()
        .unwrap();
        let draft = dir.path().join("pages/drafts/nutrition");
        std::fs::create_dir_all(&draft).unwrap();
        std::fs::write(draft.join("index.html"), "<!doctype html><html lang=\"en\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Demo nutrition</title><style>body{font:18px system-ui;max-width:640px;margin:48px auto;padding:24px;background:#f5f4f0;color:#242824}h1{letter-spacing:-.04em}strong{font-size:48px}</style><h1>Demo nutrition</h1><p>Today’s energy</p><strong id=\"total\">Loading…</strong><p>Fictional preview records.</p><script>fetch('./data.json').then(r=>r.json()).then(d=>document.getElementById('total').textContent=d.kcal+' kcal')</script></html>").unwrap();
        std::fs::write(draft.join("data.json"), "{\"kcal\":1050}").unwrap();
        PageStore::new(dir.path())
            .publish("nutrition", None)
            .unwrap();
        AuthStore::new(dir.path())
            .set_password("preview-password")
            .unwrap();
        eprintln!("PREVIEW_URL=http://{address}/kodrik/pages/nutrition/");
        serve(listener, router(settings, dir.path())).await.unwrap();
    }

    #[tokio::test]
    async fn response_chunks_retain_request_permit_until_released() {
        use futures::StreamExt;
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = semaphore.clone().try_acquire_owned().unwrap();
        let response = hold_permit(Response::new(Body::from("response")), permit);
        let mut stream = response.into_body().into_data_stream();
        let bytes = stream.next().await.unwrap().unwrap();
        drop(stream);
        assert_eq!(&bytes[..], b"response");
        assert!(semaphore.clone().try_acquire_owned().is_err());
        drop(bytes);
        assert!(semaphore.try_acquire_owned().is_ok());
    }

    #[tokio::test]
    async fn http_pages_enforce_login_proxy_prefix_and_visibility_changes() {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let origin = format!("http://{address}");
        let base = format!("{origin}/kodrik/");
        let settings = Settings::from_config(&WebConfig {
            trusted_proxies: Vec::new(),
            enabled: true,
            listen: "127.0.0.1:0".into(),
            base_url: Some(base.clone()),
        })
        .unwrap()
        .unwrap();
        let pages = PageStore::new(dir.path());
        for (slug, visibility) in [
            ("public", Visibility::Public),
            ("private", Visibility::Private),
        ] {
            let draft = dir.path().join(format!("pages/drafts/{slug}"));
            std::fs::create_dir_all(&draft).unwrap();
            std::fs::write(draft.join("index.html"), "<h1>Nutrition</h1>").unwrap();
            std::fs::write(draft.join("data.json"), "{\"kcal\":500}").unwrap();
            std::fs::write(draft.join("download.unknownext"), "binary").unwrap();
            pages.publish(slug, Some(visibility)).unwrap();
        }
        let auth = AuthStore::new(dir.path());
        auth.set_password("test-password").unwrap();
        let server = tokio::spawn(serve(listener, router(settings, dir.path())));
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let public = format!("{base}pages/public/");
        let private = format!("{base}pages/private/");
        let json = format!("{private}data.json");
        let response = client.get(&public).send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let response = client
            .get(&private)
            .header("Host", "evil.example")
            .header("X-Forwarded-Host", "evil.example")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 303);
        assert!(
            response.headers()["location"]
                .to_str()
                .unwrap()
                .starts_with(&base)
        );
        assert_eq!(client.get(&json).send().await.unwrap().status(), 401);
        assert_eq!(
            client
                .get(format!("{origin}/pages/public/"))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        let login = format!("{base}auth/login");
        // Native same-origin form POSTs send Origin: null under no-referrer
        // (Fetch Standard, append a request Origin header), unlike reqwest.
        let login_page = client.get(&login).send().await.unwrap();
        let form_origin = if login_page
            .headers()
            .get("referrer-policy")
            .is_some_and(|policy| policy == "no-referrer")
        {
            "null"
        } else {
            &origin
        };
        assert_eq!(
            client
                .post(&login)
                .header("Origin", &origin)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body("return_to=%2Fkodrik%2F")
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        assert_eq!(
            client
                .post(&login)
                .header("Origin", &origin)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body("x".repeat(4097))
                .send()
                .await
                .unwrap()
                .status(),
            413
        );
        assert_eq!(
            client
                .post(&login)
                .header("Origin", "https://evil.example")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body("password=test-password")
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        let response = client
            .post(&login)
            .header("Origin", form_origin)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body("password=test-password&return_to=%2Fkodrik%2Fpages%2Fprivate%2F")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 303);
        assert_eq!(
            response.headers()["location"],
            format!("{base}pages/private/")
        );
        let cookie = response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        assert_eq!(
            client
                .get(&json)
                .header("Cookie", &cookie)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "{\"kcal\":500}"
        );
        std::fs::write(
            dir.path().join("pages/drafts/private/data.json"),
            "{\"kcal\":750}",
        )
        .unwrap();
        pages.publish("private", None).unwrap();
        assert_eq!(
            client
                .get(&json)
                .header("Cookie", &cookie)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "{\"kcal\":750}"
        );
        assert!(
            client
                .head(&public)
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            client
                .get(format!("{public}download.unknownext"))
                .send()
                .await
                .unwrap()
                .headers()["content-disposition"],
            "attachment"
        );
        for path in ["%2e%2e%2findex.html", "%2fetc%2fpasswd", "a%5cb"] {
            assert!(
                client
                    .get(format!("{public}{path}"))
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .is_client_error()
            );
        }
        pages.publish("public", Some(Visibility::Private)).unwrap();
        assert_eq!(
            client
                .get(format!("{public}data.json"))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(
            client
                .post(format!("{base}auth/logout"))
                .header("Origin", &origin)
                .header("Cookie", &cookie)
                .send()
                .await
                .unwrap()
                .status(),
            303
        );
        assert_eq!(
            client
                .get(&json)
                .header("Cookie", &cookie)
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        let token = auth
            .login("test-password", chrono::Utc::now().timestamp())
            .unwrap()
            .unwrap();
        let cookie = format!("kodrik_web_session={token}");
        auth.set_password("changed-password").unwrap();
        assert_eq!(
            client
                .get(&json)
                .header("Cookie", &cookie)
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        server.abort();
        let _ = server.await;
    }
}
