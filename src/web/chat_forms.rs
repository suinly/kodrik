use super::*;
use crate::{
    agent::message::Message,
    web::documents::{FormRequest, UiAction},
};

async fn declared_action(
    state: &WebState,
    id: &str,
    call_id: &str,
    action_id: &str,
) -> Result<(UiAction, crate::agent::datasets::DatasetStore), Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let session = chat.session(id).await?;
    let messages = session.messages();
    let action = messages
        .iter()
        .enumerate()
        .find_map(|(index, message)| {
            let Message::ToolCall(call) = message else {
                return None;
            };
            if call.call_id != call_id || call.name != "show_ui" {
                return None;
            }
            let successful = messages[index + 1..].iter().any(|message| {
                matches!(message, Message::ToolResult(result)
                if result.call_id == call_id
                    && result.content.output["kind"] == "ui_document"
                    && result.content.output.get("error").is_none_or(Value::is_null))
            });
            if !successful {
                return None;
            }
            call.arguments["actions"]
                .as_array()?
                .iter()
                .find(|action| action["id"].as_str() == Some(action_id))
                .cloned()
        })
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Form action does not exist"))?;
    let action: UiAction = serde_json::from_value(action).map_err(|e| internal(e.into()))?;
    let datasets = chat
        .datasets
        .clone()
        .ok_or_else(|| error(StatusCode::SERVICE_UNAVAILABLE, "Datasets unavailable"))?;
    Ok((action, datasets))
}

pub(super) async fn read(
    Extension(state): Extension<WebState>,
    Path((id, call_id, action_id)): Path<(String, String, String)>,
) -> Result<Json<Value>, Response> {
    let (action, datasets) = declared_action(&state, &id, &call_id, &action_id).await?;
    crate::agent::run_state::spawn_owned_blocking(move || action.snapshot(&datasets))
        .await
        .map_err(|e| internal(e.into()))?
        .map(Json)
        .map_err(|e| error(StatusCode::BAD_REQUEST, &e.to_string()))
}

pub(super) async fn submit(
    Extension(state): Extension<WebState>,
    Path((id, call_id, action_id)): Path<(String, String, String)>,
    Json(input): Json<FormRequest>,
) -> Result<Json<Value>, Response> {
    let (action, datasets) = declared_action(&state, &id, &call_id, &action_id).await?;
    let namespace =
        serde_json::to_string(&[id, call_id, action_id]).map_err(|e| internal(e.into()))?;
    crate::agent::run_state::spawn_owned_blocking(move || {
        if action.is_response() {
            action.submit_response(&datasets, input, &namespace)
        } else {
            action.submit(&datasets, input)
        }
    })
    .await
    .map_err(|e| internal(e.into()))?
    .map(Json)
    .map_err(|reason| {
        let message = reason.to_string();
        let status =
            if message.contains("Revision conflict") || message.contains("different payload") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            };
        error(status, &message)
    })
}
