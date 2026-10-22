use super::*;
use crate::web::auth::{LoginPoll, browser_login_cookie, browser_login_token};
use axum::Json;
use serde_json::json;
#[cfg(test)]
#[path = "telegram_login_tests.rs"]
mod tests;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StartLogin {
    return_to: Option<String>,
    invite: Option<String>,
}

pub(super) async fn start(
    State(state): State<ServerState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    Json(input): Json<StartLogin>,
) -> Response {
    if !valid_origin(&state.settings, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Err(response) = state.allow_login(&headers, peer) {
        return response;
    }
    blocking(move || {
        let destination = safe_return_path(&state.settings, input.return_to.as_deref().unwrap_or(state.settings.prefix()));
        let previous = browser_login_token(&headers);
        let login = match state.auth.begin_telegram_login_with_invite(previous.as_deref(), &destination, input.invite.as_deref(), chrono::Utc::now().timestamp()) {
            Ok(login) => login,
            Err(error) if error.is::<crate::web::auth::InvalidInvite>() => return Ok((StatusCode::BAD_REQUEST, Json(json!({"error":"Приглашение недействительно. Попросите владельца прислать новое."}))).into_response()),
            Err(error) => return Err(error),
        };
        let Some(login) = login else {
            return Ok((StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":"Бот Telegram ещё не готов. Попробуйте позже."}))).into_response());
        };
        let mut response = Json(json!({"url":format!("https://t.me/{}?start=login_{}",login.username,login.id),
            "verification":login.id[..6].to_ascii_uppercase(),"expiresIn":300})).into_response();
        response.headers_mut().insert(header::SET_COOKIE, browser_login_cookie(&state.settings, &login.cookie, false).parse()?);
        Ok(response)
    }).await
}

pub(super) async fn poll(State(state): State<ServerState>, headers: HeaderMap) -> Response {
    if !valid_origin(&state.settings, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    blocking(move || {
        let result = state.auth.poll_telegram_login(
            &browser_login_token(&headers).unwrap_or_default(),
            chrono::Utc::now().timestamp(),
        )?;
        let mut response = match &result {
            LoginPoll::Pending => Json(json!({"status":"pending"})).into_response(),
            LoginPoll::Expired => Json(json!({"status":"expired"})).into_response(),
            LoginPoll::Denied => Json(json!({"status":"denied"})).into_response(),
            LoginPoll::Approved { token, return_to } => {
                let destination = safe_return_path(&state.settings, return_to);
                let mut response =
                    Json(json!({"status":"approved","redirect":state.settings.url(&destination)?}))
                        .into_response();
                response.headers_mut().append(
                    header::SET_COOKIE,
                    session_cookie(&state.settings, token, false).parse()?,
                );
                response
            }
        };
        if result != LoginPoll::Pending {
            response.headers_mut().append(
                header::SET_COOKIE,
                browser_login_cookie(&state.settings, "", true).parse()?,
            );
        }
        Ok(response)
    })
    .await
}

pub(super) async fn cancel(State(state): State<ServerState>, headers: HeaderMap) -> Response {
    if !valid_origin(&state.settings, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    blocking(move || {
        if let Some(cookie) = browser_login_token(&headers) {
            state.auth.cancel_telegram_login(&cookie)?;
        }
        let mut response = StatusCode::NO_CONTENT.into_response();
        response.headers_mut().insert(
            header::SET_COOKIE,
            browser_login_cookie(&state.settings, "", true).parse()?,
        );
        Ok(response)
    })
    .await
}
