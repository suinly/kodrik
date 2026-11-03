use super::*;
use crate::remote::protocol::ChatCommand;
use axum::{extract::Query, http::header};

pub(super) fn routes(state: WebState) -> Router<WebState> {
    // Bearer authority exists only on these concrete routes, never on the browser API.
    Router::new()
        .route("/api/remote/chat/sessions", get(sessions))
        .route("/api/remote/chat/sessions/{id}", get(snapshot))
        .route("/api/remote/chat/sessions/{id}/commands", post(command))
        .route("/api/remote/chat/sessions/{id}/events", get(events))
        .route(
            "/api/remote/chat/sessions/{id}/history/{index}",
            get(full_group),
        )
        .route(
            "/api/remote/chat/sessions/{id}/attachments/{message}/{kind}/{index}",
            get(native_attachment),
        )
        .layer(DefaultBodyLimit::max(64 * 1024))
        .route_layer(middleware::from_fn_with_state(state, authorize))
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    if headers.get_all(header::AUTHORIZATION).iter().count() != 1 {
        return None;
    }
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::to_owned)
}

async fn device_valid(auth: AuthStore, token: String) -> bool {
    tokio::task::spawn_blocking(move || {
        auth.device_identity(&token, chrono::Utc::now().timestamp())
    })
    .await
    .is_ok_and(|result| result.is_ok_and(|identity| identity.is_some()))
}

async fn authorize(
    State(state): State<WebState>,
    headers: HeaderMap,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(token) = bearer(&headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if !device_valid(state.auth.clone(), token.clone()).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if headers.contains_key(header::ORIGIN) && !valid_origin(&state.settings, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    request.extensions_mut().insert(state);
    next.run(request).await
}

async fn sessions(Extension(state): Extension<WebState>) -> Result<Json<Value>, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    Ok(Json(chat.remote_sessions().await?))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    cursor: Option<String>,
}

async fn snapshot(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Result<Response, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let id = SessionId::new(id);
    let snapshot = match query.cursor {
        Some(cursor) => chat.remote_history(&id, &cursor).await?,
        None => chat.remote_snapshot(&id).await?,
    };
    Ok(Json(snapshot).into_response())
}

async fn command(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
    Json(command): Json<ChatCommand>,
) -> Result<Json<Value>, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    Ok(Json(
        chat.remote_command(&SessionId::new(id), command).await?,
    ))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChunkQuery {
    #[serde(default)]
    offset: usize,
}

async fn full_group(
    Extension(state): Extension<WebState>,
    Path((id, index)): Path<(String, usize)>,
    Query(query): Query<ChunkQuery>,
) -> Result<Json<Value>, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let session = chat.session(&id).await?;
    if session.webhook_instruction.is_some() {
        return Err(StatusCode::NOT_FOUND.into_response());
    }
    let starts = service::group_starts(&session);
    let position = starts
        .iter()
        .position(|start| *start == index)
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Message not found"))?;
    let end = starts
        .get(position + 1)
        .copied()
        .unwrap_or(session.messages().len());
    let row = service::history_group(&session, index, end);
    // A full group may be huge; stream its JSON as explicit UTF-8-safe bounded chunks.
    let text = serde_json::to_string(&row).map_err(|e| internal(e.into()))?;
    if query.offset > text.len() || !text.is_char_boundary(query.offset) {
        return Err(error(StatusCode::BAD_REQUEST, "Invalid chunk offset"));
    }
    let mut end = text.len().min(query.offset.saturating_add(64 * 1024));
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Ok(Json(
        json!({"data":&text[query.offset..end],"next_offset":(end<text.len()).then_some(end),"total_bytes":text.len()}),
    ))
}

async fn native_attachment(
    Extension(state): Extension<WebState>,
    Path((id, message, kind, index)): Path<(String, usize, String, usize)>,
) -> Result<Response, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    if chat.session(&id).await?.webhook_instruction.is_some() {
        return Err(StatusCode::NOT_FOUND.into_response());
    }
    attachment(Extension(state), Path((id, message, kind, index))).await
}

async fn events(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?.clone();
    chat.remote_snapshot(&SessionId::new(&id)).await?;
    let token = bearer(&headers).ok_or_else(|| StatusCode::UNAUTHORIZED.into_response())?;
    let user_permit = chat
        .streams
        .clone()
        .try_acquire_owned()
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS.into_response())?;
    let global_permit = state
        .streams
        .clone()
        .try_acquire_owned()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE.into_response())?;
    let mut shutdown = chat.runner.shutdown_receiver();
    let stream = futures::stream::unfold(
        (chat, state.auth, token, id, None),
        |(chat, auth, token, id, mut revision)| async move {
            loop {
                if !device_valid(auth.clone(), token.clone()).await {
                    return None;
                }
                let current = chat.sessions.view_revision().await.ok()?;
                if revision != Some(current) {
                    let snapshot = chat.remote_snapshot(&SessionId::new(&id)).await.ok()?;
                    revision = Some(snapshot.revision);
                    let data = serde_json::to_string(&snapshot).ok()?;
                    return Some((
                        Ok::<_, Infallible>(Event::default().data(data)),
                        (chat, auth, token, id, revision),
                    ));
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        },
    )
    .take_until(async move {
        let _ = shutdown.wait_for(|stopped| *stopped).await;
    });
    let stream = stream.map(move |event| {
        let _ = (&user_permit, &global_permit);
        event
    });
    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(10)))
        .into_response();
    response
        .extensions_mut()
        .insert(super::super::server::EventStream);
    response
        .headers_mut()
        .insert("x-accel-buffering", "no".parse().unwrap());
    Ok(response)
}
