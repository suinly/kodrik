use super::*;
use crate::agent::command_audit::CommandAuditMode;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Decision {
    approve: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AuditSetting {
    mode: CommandAuditMode,
}

pub(super) async fn set_mode(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
    Json(input): Json<AuditSetting>,
) -> Result<StatusCode, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    chat.set_audit_mode(id, input.mode).await?;
    Ok(StatusCode::NO_CONTENT)
}

impl Chat {
    pub(super) async fn set_audit_mode(
        &self,
        id: String,
        mode: CommandAuditMode,
    ) -> Result<(), Response> {
        let chat = self;
        chat.session(&id).await?;
        let sessions = chat.sessions.clone();
        // Persist the explicit setting even if its browser disconnects.
        tokio::spawn(async move { sessions.set_command_audit_mode(&id, mode).await })
            .await
            .map_err(|e| internal(e.into()))?
            .map_err(internal)?;
        Ok(())
    }
}

pub(super) async fn default_mode(
    Extension(state): Extension<WebState>,
) -> Result<Json<Value>, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let mode = chat
        .sessions
        .default_command_audit_mode()
        .await
        .map_err(internal)?;
    Ok(Json(json!({"mode":mode})))
}

pub(super) async fn set_default_mode(
    Extension(state): Extension<WebState>,
    Json(input): Json<AuditSetting>,
) -> Result<StatusCode, Response> {
    let sessions = state
        .chat()
        .map_err(IntoResponse::into_response)?
        .sessions
        .clone();
    tokio::spawn(async move { sessions.set_default_command_audit_mode(input.mode).await })
        .await
        .map_err(|e| internal(e.into()))?
        .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn decide(
    Extension(state): Extension<WebState>,
    Path((id, approval)): Path<(String, String)>,
    Json(input): Json<Decision>,
) -> Result<StatusCode, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    chat.decide_approval(id, approval, input.approve).await?;
    Ok(StatusCode::NO_CONTENT)
}

impl Chat {
    pub(super) async fn decide_approval(
        &self,
        id: String,
        approval: String,
        approve: bool,
    ) -> Result<(), Response> {
        let chat = self;
        chat.session(&id).await?;
        let sessions = chat.sessions.clone();
        // Persist an explicit decision even if its browser disconnects.
        tokio::spawn(async move {
            sessions
                .decide_command_approval(&id, &approval, approve)
                .await
        })
        .await
        .map_err(|e| internal(e.into()))?
        .map_err(|reason| {
            if reason.is::<crate::agent::command_audit::CommandApprovalUnavailable>() {
                error(
                    StatusCode::CONFLICT,
                    "Command approval is no longer pending",
                )
            } else {
                internal(reason)
            }
        })?;
        Ok(())
    }
}
