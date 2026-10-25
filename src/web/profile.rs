use super::*;
use crate::web::auth::ProfileError;
use axum::{Json, extract::rejection::JsonRejection};
use serde_json::json;

fn failure(error: anyhow::Error) -> Response {
    let status = match error.downcast_ref::<ProfileError>() {
        Some(ProfileError::Unauthorized) => StatusCode::UNAUTHORIZED,
        Some(ProfileError::Forbidden) => StatusCode::FORBIDDEN,
        Some(ProfileError::Invalid(_)) => StatusCode::BAD_REQUEST,
        None => {
            log::warn!("Profile request failed: {error:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    (status, Json(json!({"error":error.to_string()}))).into_response()
}

pub(super) async fn read(State(state): State<ServerState>, headers: HeaderMap) -> Response {
    blocking(move || {
        let Some(token) = request_token(&headers) else {
            return Ok(failure(ProfileError::Unauthorized.into()));
        };
        Ok(
            match state.auth.profile(&token, chrono::Utc::now().timestamp())? {
                Some(profile) => Json(profile).into_response(),
                None => failure(ProfileError::Unauthorized.into()),
            },
        )
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UpdateProfile {
    name: String,
}

pub(super) async fn update(
    State(state): State<ServerState>,
    headers: HeaderMap,
    input: Result<Json<UpdateProfile>, JsonRejection>,
) -> Response {
    if !valid_origin(&state.settings, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let input = match input {
        Ok(Json(input)) => input,
        Err(error) => {
            return if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
                error.into_response()
            } else {
                StatusCode::BAD_REQUEST.into_response()
            };
        }
    };
    blocking(move || {
        let Some(token) = request_token(&headers) else {
            return Ok(failure(ProfileError::Unauthorized.into()));
        };
        Ok(
            match state
                .auth
                .update_profile(&token, &input.name, chrono::Utc::now().timestamp())
            {
                Ok(profile) => Json(profile).into_response(),
                Err(error) => failure(error),
            },
        )
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PasswordChange {
    current_password: String,
    new_password: String,
    confirm_password: String,
}

pub(super) async fn password(
    State(state): State<ServerState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    input: Result<Json<PasswordChange>, JsonRejection>,
) -> Response {
    if !valid_origin(&state.settings, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let input = match input {
        Ok(Json(input)) => input,
        Err(error) => {
            return if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
                error.into_response()
            } else {
                StatusCode::BAD_REQUEST.into_response()
            };
        }
    };
    blocking(move || {
        let Some(token) = request_token(&headers) else {
            return Ok(failure(ProfileError::Unauthorized.into()));
        };
        let Some(profile) = state.auth.profile(&token, chrono::Utc::now().timestamp())? else {
            return Ok(failure(ProfileError::Unauthorized.into()));
        };
        if !profile.owner {
            return Ok(failure(ProfileError::Forbidden.into()));
        }
        if let Err(response) = state.allow_login(&headers, peer) {
            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                return Ok((
                    StatusCode::TOO_MANY_REQUESTS,
                    [(header::RETRY_AFTER, "60")],
                    Json(json!({"error":"Too many password attempts"})),
                )
                    .into_response());
            }
            return Ok(response);
        }
        if let Err(error) = state.auth.change_password(
            &token,
            &input.current_password,
            &input.new_password,
            &input.confirm_password,
        ) {
            return Ok(failure(error));
        }
        Ok((
            StatusCode::NO_CONTENT,
            [(
                header::SET_COOKIE,
                session_cookie(&state.settings, "", true),
            )],
        )
            .into_response())
    })
    .await
}

#[cfg(test)]
#[path = "profile_tests.rs"]
mod tests;
