use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Selection {
    model: Option<String>,
}

pub(super) async fn list(Extension(state): Extension<WebState>) -> Result<Json<Value>, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    Ok(Json(chat.models().await?))
}

pub(super) async fn set(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
    Json(input): Json<Selection>,
) -> Result<StatusCode, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    chat.set_model(&id, input.model).await?;
    Ok(StatusCode::NO_CONTENT)
}

impl Chat {
    pub(super) async fn models(&self) -> Result<Value, Response> {
        let models = self.runner.available_models().await.map_err(|reason| {
            log::warn!("Cannot list provider models: {reason:#}");
            error(
                StatusCode::BAD_GATEWAY,
                "Не удалось загрузить модели провайдера",
            )
        })?;
        Ok(json!({"models":models,"default_model":self.runner.model_identity()}))
    }

    pub(super) async fn set_model(&self, id: &str, model: Option<String>) -> Result<(), Response> {
        self.session(id).await?;
        let model = model.filter(|model| model != "auto");
        if let Some(model) = &model {
            if model.is_empty() || model.len() > 512 || model.chars().any(char::is_control) {
                return Err(error(StatusCode::BAD_REQUEST, "Некорректная модель"));
            }
            let models = self.runner.available_models().await.map_err(|reason| {
                log::warn!("Cannot validate provider model: {reason:#}");
                error(
                    StatusCode::BAD_GATEWAY,
                    "Не удалось загрузить модели провайдера",
                )
            })?;
            if !models.contains(model) {
                return Err(error(
                    StatusCode::BAD_REQUEST,
                    "Модель отсутствует в списке провайдера",
                ));
            }
        }
        let _runs = self.runs.lock().await;
        self.session(id).await?;
        self.sessions
            .set_session_model(id, model)
            .await
            .map_err(internal)
    }
}
