use super::*;

pub(super) async fn stop(
    Extension(state): Extension<WebState>,
    Path((id, agent)): Path<(String, String)>,
) -> Result<Json<Value>, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let parent = chat.session(&id).await?;
    let manager = chat
        .runner
        .subagents()
        .cloned()
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Child agent not found"))?;
    // An accepted stop keeps running even if the browser disconnects.
    let child = tokio::spawn(async move { manager.stop(&parent, &agent).await })
        .await
        .map_err(|e| internal(e.into()))?
        .map_err(|e| {
            if e.is::<crate::agent::subagents::ChildNotFound>() {
                error(StatusCode::NOT_FOUND, "Child agent not found")
            } else {
                internal(e)
            }
        })?;
    Ok(Json(child.view()))
}
