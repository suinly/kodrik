use super::{Chat, WebState, error, internal};
use crate::agent::{
    inbox::{BusyMode, SubmissionRequest, admit_submission},
    session::{Session, SessionId},
};
use crate::web::webhook_store::{WebhookError, WebhookStore, hash};
use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, FromRequest, Path, Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const BODY_LIMIT: usize = 64 * 1024;

pub(super) fn routes(state: WebState) -> Router<WebState> {
    let management = Router::new()
        .route("/api/webhooks", get(list).post(create))
        .route("/api/webhooks/{id}", patch(update).delete(delete))
        .route("/api/webhooks/{id}/token", post(rotate))
        .route("/api/webhooks/{id}/events", get(events))
        .layer(DefaultBodyLimit::max(32 * 1024))
        .route_layer(middleware::from_fn_with_state(state, owner_only));
    management.route(
        "/api/webhooks/{id}",
        post(receive).layer(DefaultBodyLimit::max(BODY_LIMIT)),
    )
}

async fn owner_only(State(state): State<WebState>, mut request: Request, next: Next) -> Response {
    let Some(token) = super::request_token(request.headers()) else {
        return error(StatusCode::UNAUTHORIZED, "Sign in to continue");
    };
    let auth = state.auth.clone();
    let access = tokio::task::spawn_blocking(move || {
        let now = chrono::Utc::now().timestamp();
        Ok::<_, anyhow::Error>((
            auth.identity(&token, now)?.is_some(),
            auth.authenticated(&token, now)?,
        ))
    })
    .await;
    match access {
        Ok(Ok((false, _))) => return error(StatusCode::UNAUTHORIZED, "Sign in to continue"),
        Ok(Ok((_, false))) => return error(StatusCode::FORBIDDEN, "Owner access required"),
        Ok(Ok((true, true))) => {}
        Ok(Err(e)) => return internal(e),
        Err(e) => return internal(e.into()),
    }
    if request.method() != Method::GET && !super::valid_origin(&state.settings, request.headers()) {
        return error(StatusCode::FORBIDDEN, "Invalid request origin");
    }
    request.extensions_mut().insert(state);
    next.run(request).await
}

fn store(state: &WebState) -> Result<WebhookStore, StatusCode> {
    let chat = state.chat()?;
    Ok(WebhookStore::new(
        (*chat.sessions).clone(),
        state.settings.base_url.clone(),
    ))
}

fn management_error(error: anyhow::Error) -> Response {
    let status = match error.downcast_ref::<WebhookError>() {
        Some(WebhookError::InvalidInput) => StatusCode::BAD_REQUEST,
        Some(WebhookError::NotFound) => StatusCode::NOT_FOUND,
        Some(WebhookError::LimitReached) => StatusCode::TOO_MANY_REQUESTS,
        None => return internal(error),
    };
    super::error(status, &error.to_string())
}

async fn list(Extension(state): Extension<WebState>) -> Result<Json<Value>, Response> {
    Ok(Json(
        store(&state)
            .map_err(IntoResponse::into_response)?
            .list()
            .await
            .map_err(management_error)?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    name: String,
    instruction: String,
    #[serde(default = "enabled")]
    enabled: bool,
}
fn enabled() -> bool {
    true
}

async fn create(
    Extension(state): Extension<WebState>,
    Json(input): Json<Input>,
) -> Result<(StatusCode, Json<Value>), Response> {
    let result = store(&state)
        .map_err(IntoResponse::into_response)?
        .create(input.name, input.instruction, input.enabled)
        .await
        .map_err(management_error)?;
    Ok((StatusCode::CREATED, Json(result)))
}

async fn update(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
    Json(input): Json<Input>,
) -> Result<StatusCode, Response> {
    store(&state)
        .map_err(IntoResponse::into_response)?
        .update(
            id,
            Some(input.name),
            Some(input.instruction),
            Some(input.enabled),
        )
        .await
        .map_err(management_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn rotate(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, Response> {
    Ok(Json(
        store(&state)
            .map_err(IntoResponse::into_response)?
            .rotate(id)
            .await
            .map_err(management_error)?,
    ))
}

async fn delete(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
) -> Result<StatusCode, Response> {
    store(&state)
        .map_err(IntoResponse::into_response)?
        .delete(id)
        .await
        .map_err(management_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn events(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, Response> {
    Ok(Json(
        store(&state)
            .map_err(IntoResponse::into_response)?
            .events(id)
            .await
            .map_err(management_error)?,
    ))
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    if headers.get_all(header::AUTHORIZATION).iter().count() != 1 {
        return None;
    }
    let (scheme, token) = headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .split_once(' ')?;
    (scheme.eq_ignore_ascii_case("Bearer")
        && token.len() == 64
        && token.bytes().all(|c| c.is_ascii_hexdigit()))
    .then(|| hash(token))
}

async fn receive(
    State(state): State<WebState>,
    Path(id): Path<String>,
    request: Request,
) -> Result<(StatusCode, Json<Value>), Response> {
    let digest = bearer(request.headers())
        .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "Valid Bearer token required"))?;
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let (check_id, check_digest) = (id.clone(), digest.clone());
    let allowed = chat
        .sessions
        .with_connection(move |db| {
            Ok(db.query_row(
                "SELECT EXISTS(SELECT 1 FROM webhooks WHERE id=?1 AND token_hash=?2 AND enabled=1)",
                params![check_id, check_digest],
                |row| row.get::<_, bool>(0),
            )?)
        })
        .await
        .map_err(internal)?;
    if !allowed {
        return Err(error(
            StatusCode::UNAUTHORIZED,
            "Valid Bearer token required",
        ));
    }
    let keys = request.headers().get_all("idempotency-key");
    if keys.iter().count() > 1 {
        return Err(error(StatusCode::BAD_REQUEST, "Invalid Idempotency-Key"));
    }
    let key = match keys.iter().next() {
        Some(key) => key
            .to_str()
            .ok()
            .filter(|key| !key.trim().is_empty() && key.len() <= 128 && key.is_ascii())
            .ok_or_else(|| error(StatusCode::BAD_REQUEST, "Invalid Idempotency-Key"))?
            .to_owned(),
        None => uuid::Uuid::new_v4().to_string(),
    };
    let body = String::from_request(request, &())
        .await
        .map_err(IntoResponse::into_response)?;
    if body.trim().is_empty() {
        return Err(error(StatusCode::BAD_REQUEST, "Webhook body is empty"));
    }
    // Admission and worker startup outlive a disconnected sender.
    tokio::spawn(admit(state, id, digest, key, body))
        .await
        .map_err(|e| internal(e.into()))?
}

#[derive(Serialize)]
struct Delivery {
    conversation_id: Option<String>,
    duplicate: bool,
}

async fn admit(
    state: WebState,
    id: String,
    digest: String,
    key: String,
    body: String,
) -> Result<(StatusCode, Json<Value>), Response> {
    let chat: Chat = state.chat().map_err(IntoResponse::into_response)?.clone();
    let mut runs = chat.runs.lock().await;
    // shortcut: queue capacity is per server process; use DB accounting before running multiple web workers.
    let queue_full = runs.len() >= 100;
    let result = chat.sessions.with_connection(move |db| {
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Recheck revocation at the same boundary as persisting work.
        let webhook: Option<(String,String)> = tx.query_row("SELECT name,instruction FROM webhooks WHERE id=?1 AND token_hash=?2 AND enabled=1", params![id,digest], |row| Ok((row.get(0)?,row.get(1)?))).optional()?;
        let Some((name,instruction)) = webhook else { return Ok(Err(StatusCode::UNAUTHORIZED)); };
        let old: Option<(String, Option<String>)> = tx.query_row("SELECT body_hash,session_id FROM webhook_receipts WHERE webhook_id=?1 AND request_key=?2", params![id,key], |row| Ok((row.get(0)?,row.get(1)?))).optional()?;
        let body_hash = hash(&body);
        if let Some((old_hash, session)) = old {
            return Ok(if old_hash == body_hash { Ok(Delivery { conversation_id:session,duplicate:true }) } else { Err(StatusCode::CONFLICT) });
        }
        let now = chrono::Utc::now();
        let cutoff = (now - chrono::Duration::minutes(1)).to_rfc3339();
        let recent: i64 = tx.query_row("SELECT count(*) FROM webhook_receipts WHERE webhook_id=?1 AND accepted_at>?2", params![id,cutoff], |row| row.get(0))?;
        if queue_full || recent >= 10 { return Ok(Err(StatusCode::TOO_MANY_REQUESTS)); }
        let session_id = uuid::Uuid::new_v4().to_string();
        let mut session = Session::new(SessionId::new(&session_id));
        let source = json!({"transport":"http_webhook","webhook_id":id,"webhook_name":name,"received_at":now.to_rfc3339()});
        session.webhook_instruction = Some(format!("## Recorded delivery source\n{source}\n\n## Owner instruction\n{instruction}"));
        tx.execute("INSERT INTO sessions(id,payload,updated_at) VALUES(?1,?2,?3)", params![session_id,serde_json::to_vec(&session)?,now.timestamp_nanos_opt().ok_or_else(|| anyhow::anyhow!("Session timestamp overflow"))?])?;
        let mut message = crate::gateway::IncomingMessage::new(crate::gateway::InterfaceId("webhook".into()), &session_id, body);
        message.request_id = format!("webhook:{id}:{key}");
        let mut request = SubmissionRequest::new(session.id(), message, BusyMode::FollowUp);
        // Free-form event bodies such as /stop are data, never chat commands.
        request.control = false;
        let hash = request.hash()?;
        admit_submission(&tx, request, hash)?;
        tx.execute("INSERT INTO webhook_receipts VALUES(?1,?2,?3,?4,?5)", params![id,key,body_hash,session_id,now.to_rfc3339()])?;
        tx.commit()?;
        Ok(Ok(Delivery {conversation_id:Some(session_id),duplicate:false}))
    }).await.map_err(internal)?;
    let delivery = result.map_err(|status| {
        error(
            status,
            match status {
                StatusCode::UNAUTHORIZED => "Valid Bearer token required",
                StatusCode::CONFLICT => "Idempotency-Key was already used with a different body",
                _ => "Webhook rate or queue limit reached",
            },
        )
    })?;
    if !delivery.duplicate
        && let Some(id) = &delivery.conversation_id
    {
        runs.insert(id.clone());
        let worker = chat.clone();
        let conversation = SessionId::new(id);
        tokio::spawn(async move {
            worker.work(conversation, false).await;
        });
    }
    Ok((StatusCode::ACCEPTED, Json(json!(delivery))))
}
