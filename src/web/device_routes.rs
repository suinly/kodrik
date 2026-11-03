use super::*;
use crate::web::auth::{Identity, devices::DeviceError};
use axum::Json;
use uuid::Uuid;

pub(super) fn routes() -> Router<ServerState> {
    Router::new()
        .route("/api/devices/pair/start", post(start))
        .route("/api/devices/pair/poll", post(poll))
        .route("/api/devices/pair/approve", post(approve))
        .route("/api/devices", get(list))
        .route("/api/devices/{id}", delete(revoke))
        .layer(DefaultBodyLimit::max(16 * 1024))
}

fn failure(error: anyhow::Error) -> Response {
    let status = match error.downcast_ref::<DeviceError>() {
        Some(DeviceError::Invalid) => StatusCode::BAD_REQUEST,
        Some(DeviceError::Forbidden) => StatusCode::FORBIDDEN,
        Some(DeviceError::RateLimited | DeviceError::Capacity) => StatusCode::TOO_MANY_REQUESTS,
        None => StatusCode::INTERNAL_SERVER_ERROR,
    };
    // Neither submitted codes/tokens nor underlying storage errors belong in responses/logs.
    (
        status,
        Json(serde_json::json!({"error":"Device request denied"})),
    )
        .into_response()
}

fn native_origin(state: &ServerState, headers: &HeaderMap) -> bool {
    !headers.contains_key(header::ORIGIN) || valid_origin(&state.settings, headers)
}

fn browser_owner(state: &ServerState, headers: &HeaderMap) -> Result<Identity, Response> {
    let token = request_token(headers).ok_or_else(|| StatusCode::UNAUTHORIZED.into_response())?;
    let now = chrono::Utc::now().timestamp();
    let identity = state
        .auth
        .identity(&token, now)
        .map_err(failure)?
        .ok_or_else(|| StatusCode::UNAUTHORIZED.into_response())?;
    state.auth.list_devices(&identity, now).map_err(failure)?;
    Ok(identity)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Start {
    label: String,
}

pub(super) async fn start(
    State(state): State<ServerState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    Json(input): Json<Start>,
) -> Response {
    if !native_origin(&state, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let client = match client_ip(
        &state.settings,
        &headers,
        peer.map(|Extension(ConnectInfo(peer))| peer),
    ) {
        Ok(client) => client,
        Err(status) => return status.into_response(),
    };
    if !state
        .device_limiter
        .lock()
        .is_ok_and(|mut l| l.allow(client, Instant::now()))
    {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    blocking(move || {
        Ok(
            match state
                .auth
                .start_device_pairing(&input.label, chrono::Utc::now().timestamp())
            {
                Ok(pairing) => Json(pairing).into_response(),
                Err(error) => failure(error),
            },
        )
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Poll {
    request_id: Uuid,
    polling_secret: String,
}

pub(super) async fn poll(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(input): Json<Poll>,
) -> Response {
    if !native_origin(&state, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    blocking(move || {
        Ok(
            match state.auth.poll_device_pairing(
                input.request_id,
                &input.polling_secret,
                chrono::Utc::now().timestamp(),
            ) {
                Ok(result) => Json(result).into_response(),
                Err(error) => failure(error),
            },
        )
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Approval {
    request_id: Uuid,
    code: String,
}

pub(super) async fn approve(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(input): Json<Approval>,
) -> Response {
    if !valid_origin(&state.settings, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    blocking(move || {
        let identity = match browser_owner(&state, &headers) {
            Ok(i) => i,
            Err(response) => return Ok(response),
        };
        Ok(
            match state.auth.approve_device_pairing(
                input.request_id,
                &input.code,
                &identity,
                chrono::Utc::now().timestamp(),
            ) {
                Ok(()) => StatusCode::OK.into_response(),
                Err(error) => failure(error),
            },
        )
    })
    .await
}

pub(super) async fn list(State(state): State<ServerState>, headers: HeaderMap) -> Response {
    blocking(move || {
        let identity = match browser_owner(&state, &headers) {
            Ok(i) => i,
            Err(response) => return Ok(response),
        };
        Ok(
            match state
                .auth
                .list_devices(&identity, chrono::Utc::now().timestamp())
            {
                Ok(devices) => Json(devices).into_response(),
                Err(error) => failure(error),
            },
        )
    })
    .await
}

pub(super) async fn revoke(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Response {
    if !valid_origin(&state.settings, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    blocking(move || {
        if let Err(response) = browser_owner(&state, &headers) {
            return Ok(response);
        }
        Ok(match state.auth.revoke_device(id) {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(error) => failure(error),
        })
    })
    .await
}

#[cfg(test)]
#[path = "device_routes_tests.rs"]
mod device_auth_tests;
