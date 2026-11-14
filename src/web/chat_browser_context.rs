use super::*;
use crate::agent::tools::browser_context::BROKER;
use axum::extract::Query;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Poll {
    id: String,
    client_id: uuid::Uuid,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Complete {
    id: String,
    client_id: uuid::Uuid,
    request_id: String,
    context: Option<crate::agent::BrowserContext>,
    error: Option<String>,
}
pub(super) async fn poll(
    Extension(state): Extension<WebState>,
    Query(query): Query<Poll>,
) -> Result<Json<Value>, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let session = chat.session(&query.id).await?;
    let key = (
        chat.sessions.directory().to_owned(),
        session.id().as_str().to_owned(),
    );
    Ok(Json(
        json!({"request_id":BROKER.poll(&key, &query.client_id.to_string())}),
    ))
}
pub(super) async fn complete(
    Extension(state): Extension<WebState>,
    Json(input): Json<Complete>,
) -> Result<Json<Value>, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let session = chat.session(&input.id).await?;
    let result = match (input.context, input.error) {
        (Some(context), None) => Ok(context),
        (None, Some(error)) if !error.trim().is_empty() && error.len() <= 4096 => Err(error),
        (None, None) => Err("Chrome sidebar did not return browser context".into()),
        _ => {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "Provide context or an error, not both",
            ));
        }
    };
    let key = (
        chat.sessions.directory().to_owned(),
        session.id().as_str().to_owned(),
    );
    BROKER
        .complete(
            &key,
            &input.client_id.to_string(),
            &input.request_id,
            result,
        )
        .map_err(|err| error(StatusCode::BAD_REQUEST, &err.to_string()))?;
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CommandComplete {
    id: String,
    client_id: uuid::Uuid,
    request_id: String,
    #[serde(default, deserialize_with = "command_result")]
    result: Option<Value>,
    error: Option<String>,
}
pub(super) async fn poll_command(
    Extension(state): Extension<WebState>,
    Query(query): Query<Poll>,
) -> Result<Json<Value>, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let session = chat.session(&query.id).await?;
    let key = (
        chat.sessions.directory().to_owned(),
        session.id().as_str().to_owned(),
    );
    let pending = BROKER.poll_command(&key, &query.client_id.to_string());
    Ok(Json(match pending {
        Some((id, command)) => json!({"request_id":id,"command":command}),
        None => json!({"request_id":null,"command":null}),
    }))
}
pub(super) async fn complete_command(
    Extension(state): Extension<WebState>,
    Json(input): Json<CommandComplete>,
) -> Result<Json<Value>, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let session = chat.session(&input.id).await?;
    let result = match (input.result, input.error) {
        (Some(value), None) => Ok(value),
        (None, Some(error)) => Err(error),
        _ => {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "Provide result or error, not both",
            ));
        }
    };
    let key = (
        chat.sessions.directory().to_owned(),
        session.id().as_str().to_owned(),
    );
    BROKER
        .complete_command(
            &key,
            &input.client_id.to_string(),
            &input.request_id,
            result,
        )
        .map_err(|err| error(StatusCode::BAD_REQUEST, &err.to_string()))?;
    Ok(Json(json!({"ok":true})))
}

fn command_result<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}
