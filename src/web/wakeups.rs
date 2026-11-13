use super::{WebState, error, internal};
use crate::agent::reminders::Reminder;
use axum::{
    Extension, Json, Router,
    extract::Path,
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};

pub(super) fn routes(state: WebState) -> Router<WebState> {
    Router::new()
        .route("/api/wakeups", get(list))
        .route("/api/wakeups/{id}/events", get(events))
        .route(
            "/api/wakeups/{id}/attachments/{message}/{kind}/{index}",
            get(attachment),
        )
        .route_layer(middleware::from_fn_with_state(
            state,
            super::webhooks::owner_only,
        ))
}

async fn records(state: &WebState) -> Result<Vec<Reminder>, Response> {
    let store = state
        .chat()
        .map_err(IntoResponse::into_response)?
        .wakeups
        .clone();
    tokio::task::spawn_blocking(move || store.history())
        .await
        .map_err(|e| internal(e.into()))?
        .map_err(internal)
}

async fn list(Extension(state): Extension<WebState>) -> Result<Json<Value>, Response> {
    let mut tasks = records(&state).await?;
    tasks.sort_by_key(|task| std::cmp::Reverse(task.due_at));
    Ok(Json(json!(
        tasks
            .into_iter()
            .map(|task| json!({
                "id":task.id, "instruction":task.text, "due_at":task.due_at,
                "status":task.status, "retry_at":task.retry_at, "last_error":task.last_error,
                "source_session":task.source_session, "message":task.message,
            }))
            .collect::<Vec<_>>()
    )))
}

async fn events(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, Response> {
    if uuid::Uuid::parse_str(&id).is_err()
        || !records(&state).await?.iter().any(|task| task.id == id)
    {
        return Err(error(StatusCode::NOT_FOUND, "Wakeup not found"));
    }
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let internal_id = format!("companion:wakeup:{id}");
    use crate::agent::session::SessionStore;
    if chat
        .sessions
        .load(&crate::agent::session::SessionId::new(&internal_id))
        .await
        .map_err(internal)?
        .is_none()
    {
        return Ok(Json(
            json!({"messages":[],"busy":false,"phase":null,"blocked_reason":null}),
        ));
    }
    let view = chat
        .sessions
        .conversation_view(&internal_id)
        .await
        .map_err(internal)?;
    let mut snapshot = if let Some(run) = &view.run {
        crate::web::chat_stream::SnapshotProjection::new(run.run.id.clone())
            .snapshot(run)
            .expect("Fresh projection")
    } else {
        json!({"messages":crate::web::chat_stream::history(&view.session),"phase":null})
    };
    if view
        .run
        .as_ref()
        .is_some_and(|run| run.run.phase.terminal())
    {
        snapshot["messages"] = json!(crate::web::chat_stream::history(&view.session));
    }
    snapshot["busy"] = json!(view.busy);
    snapshot["blocked_reason"] = json!(view.blocked_reason);
    chat.decorate_origins(&internal_id, &view.session, &mut snapshot)
        .await
        .map_err(internal)?;
    rewrite_attachment_urls(&mut snapshot, &id, &internal_id);
    Ok(Json(snapshot))
}

fn rewrite_attachment_urls(snapshot: &mut Value, id: &str, internal_id: &str) {
    let old = format!("../api/chat/sessions/{internal_id}/attachments/");
    for message in snapshot["messages"].as_array_mut().into_iter().flatten() {
        for part in message["parts"].as_array_mut().into_iter().flatten() {
            if let Some(suffix) = part["url"].as_str().and_then(|url| url.strip_prefix(&old)) {
                part["url"] = json!(format!("../api/wakeups/{id}/attachments/{suffix}"));
            }
        }
    }
}

async fn attachment(
    Extension(state): Extension<WebState>,
    Path((id, message, kind, index)): Path<(String, usize, String, usize)>,
) -> Result<Response, Response> {
    if uuid::Uuid::parse_str(&id).is_err()
        || !records(&state).await?.iter().any(|task| task.id == id)
    {
        return Err(error(StatusCode::NOT_FOUND, "Wakeup not found"));
    }
    use crate::agent::session::{SessionId, SessionStore};
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let session = chat
        .sessions
        .load(&SessionId::new(format!("companion:wakeup:{id}")))
        .await
        .map_err(internal)?
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Attachment not found"))?;
    super::attachment_response(chat, session, message, kind, index).await
}
