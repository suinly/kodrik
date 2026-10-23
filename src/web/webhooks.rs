use super::{Chat, WebState, error, internal};
use crate::agent::{
    inbox::{BusyMode, SubmissionRequest, admit_submission},
    session::{Session, SessionId, SqliteSessionStore},
};
use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, FromRequest, Path, Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const BODY_LIMIT: usize = 64 * 1024;

pub(super) async fn init(store: &SqliteSessionStore) -> anyhow::Result<()> {
    store.with_connection(|db| {
        db.execute_batch("CREATE TABLE IF NOT EXISTS webhooks (
            id TEXT PRIMARY KEY, name TEXT NOT NULL, instruction TEXT NOT NULL,
            token_hash TEXT NOT NULL, enabled INTEGER NOT NULL, created_at TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS webhook_receipts (
            webhook_id TEXT NOT NULL REFERENCES webhooks(id) ON DELETE CASCADE,
            request_key TEXT NOT NULL, body_hash TEXT NOT NULL,
            session_id TEXT REFERENCES sessions(id) ON DELETE SET NULL,
            accepted_at TEXT NOT NULL,
            PRIMARY KEY(webhook_id,request_key));
            CREATE INDEX IF NOT EXISTS webhook_receipts_time ON webhook_receipts(webhook_id,accepted_at);")?;
        Ok(())
    }).await
}

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

#[derive(Serialize)]
struct Webhook {
    id: String,
    name: String,
    instruction: String,
    enabled: bool,
    created_at: String,
}

fn hooks(db: &Connection) -> anyhow::Result<Vec<Webhook>> {
    Ok(db.prepare("SELECT id,name,instruction,enabled,created_at FROM webhooks ORDER BY created_at DESC,id")?
        .query_map([], |row| Ok(Webhook { id:row.get(0)?,name:row.get(1)?,instruction:row.get(2)?,enabled:row.get(3)?,created_at:row.get(4)? }))?
        .collect::<rusqlite::Result<_>>()?)
}

fn with_url(state: &WebState, hook: &Webhook) -> Value {
    let mut value = serde_json::to_value(hook).expect("Webhook serialization");
    value["url"] = json!(
        state
            .settings
            .base_url
            .join(&format!("api/webhooks/{}", hook.id))
            .expect("Webhook URL")
            .as_str()
    );
    value
}

async fn list(Extension(state): Extension<WebState>) -> Result<Json<Value>, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let hooks = chat
        .sessions
        .with_connection(|db| hooks(db))
        .await
        .map_err(internal)?;
    Ok(Json(json!(
        hooks
            .iter()
            .map(|hook| with_url(&state, hook))
            .collect::<Vec<_>>()
    )))
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

fn validate(input: &Input) -> Result<(), Response> {
    if input.name.trim().is_empty()
        || input.name.len() > 200
        || input.instruction.trim().is_empty()
        || input.instruction.len() > 16 * 1024
    {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "Name must contain 1–200 bytes; instruction 1–16384 bytes",
        ));
    }
    Ok(())
}

fn new_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}
fn hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

async fn create(
    Extension(state): Extension<WebState>,
    Json(input): Json<Input>,
) -> Result<(StatusCode, Json<Value>), Response> {
    validate(&input)?;
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let hook = Webhook {
        id: uuid::Uuid::new_v4().to_string(),
        name: input.name.trim().into(),
        instruction: input.instruction,
        enabled: input.enabled,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let mut result = with_url(&state, &hook);
    let token = new_token();
    let digest = hash(&token);
    let created = chat
        .sessions
        .with_connection(move |db| {
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if tx.query_row("SELECT count(*) FROM webhooks", [], |row| {
                row.get::<_, i64>(0)
            })? >= 32
            {
                return Ok(false);
            }
            tx.execute(
                "INSERT INTO webhooks VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    hook.id,
                    hook.name,
                    hook.instruction,
                    digest,
                    hook.enabled,
                    hook.created_at
                ],
            )?;
            tx.commit()?;
            Ok(true)
        })
        .await
        .map_err(internal)?;
    if !created {
        return Err(error(
            StatusCode::TOO_MANY_REQUESTS,
            "Webhook limit reached",
        ));
    }
    result["token"] = json!(token);
    Ok((StatusCode::CREATED, Json(result)))
}

async fn update(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
    Json(input): Json<Input>,
) -> Result<StatusCode, Response> {
    validate(&input)?;
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let changed = chat
        .sessions
        .with_connection(move |db| {
            Ok(db.execute(
                "UPDATE webhooks SET name=?2,instruction=?3,enabled=?4 WHERE id=?1",
                params![id, input.name.trim(), input.instruction, input.enabled],
            )?)
        })
        .await
        .map_err(internal)?;
    if changed == 0 {
        return Err(error(StatusCode::NOT_FOUND, "Webhook not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn rotate(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let token = new_token();
    let digest = hash(&token);
    let changed = chat
        .sessions
        .with_connection(move |db| {
            Ok(db.execute(
                "UPDATE webhooks SET token_hash=?2 WHERE id=?1",
                params![id, digest],
            )?)
        })
        .await
        .map_err(internal)?;
    if changed == 0 {
        return Err(error(StatusCode::NOT_FOUND, "Webhook not found"));
    }
    Ok(Json(json!({"token":token})))
}

async fn delete(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
) -> Result<StatusCode, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let changed = chat
        .sessions
        .with_connection(move |db| Ok(db.execute("DELETE FROM webhooks WHERE id=?1", [id])?))
        .await
        .map_err(internal)?;
    if changed == 0 {
        return Err(error(StatusCode::NOT_FOUND, "Webhook not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn events(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let events = chat.sessions.with_connection(move |db| {
        Ok(db.prepare("SELECT w.session_id,w.accepted_at,coalesce(s.status,'Deleted') FROM webhook_receipts w
            LEFT JOIN submissions s ON s.id=(SELECT id FROM submissions WHERE conversation_id=w.session_id ORDER BY sequence LIMIT 1)
            WHERE w.webhook_id=?1 ORDER BY w.accepted_at DESC LIMIT 30")?
            .query_map([id], |row| Ok(json!({"conversation_id":row.get::<_,Option<String>>(0)?,"accepted_at":row.get::<_,String>(1)?,"status":row.get::<_,String>(2)?})))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }).await.map_err(internal)?;
    Ok(Json(json!(events)))
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
