use super::{WebState, error, index, internal};
use crate::{
    billing::{AccountUsage, account_report, account_usage, saved_account_usage},
    web::auth::{AccountChange, AccountSummary, ProfileError},
};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query},
    http::{HeaderMap, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

pub(super) fn routes(state: WebState) -> Router<WebState> {
    Router::new()
        .route("/api/admin/users", get(list))
        .route("/api/admin/users/{account}", post(update))
        .route("/api/admin/users/{account}/usage", get(usage))
        .route("/app/users", get(index))
        .route("/app/users/", get(index))
        .route("/app/users/{account}", get(index))
        .route_layer(middleware::from_fn_with_state(
            state,
            super::webhooks::owner_only,
        ))
}

#[derive(Serialize)]
struct UserWithUsage {
    #[serde(flatten)]
    account: AccountSummary,
    usage: Option<AccountUsage>,
}

#[derive(Serialize)]
struct UsersReport {
    users: Vec<UserWithUsage>,
}

async fn list(Extension(state): Extension<WebState>) -> Result<Response, Response> {
    let chat = state.chat().map_err(StatusCode::into_response)?;
    let auth = state.auth.clone();
    let mut users = tokio::task::spawn_blocking(move || {
        auth.accounts().map(|accounts| {
            accounts
                .into_iter()
                .map(|account| {
                    let usage = if account.owner {
                        None
                    } else {
                        let id = account.telegram_id.expect("Member always has an ID");
                        let directory = crate::spaces::personal(auth.data_directory(), id);
                        let usage = crate::web::pages::reject_symlinks(&directory)
                            .map_err(Into::into)
                            .and_then(|()| saved_account_usage(&directory));
                        available(usage, id)
                    };
                    UserWithUsage { account, usage }
                })
                .collect::<Vec<_>>()
        })
    })
    .await
    .map_err(|e| internal(e.into()))?
    .map_err(internal)?;
    users[0].usage = available(account_usage(&chat.sessions).await, "owner");
    Ok(private_json(UsersReport { users }))
}

fn private_json(value: impl Serialize) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Json(value)).into_response()
}

fn account_id(key: &str) -> Result<Option<u64>, Response> {
    if key == "owner" {
        return Ok(None);
    }
    key.parse::<u64>()
        .ok()
        .filter(|id| *id > 0 && *id <= i64::MAX as u64 && id.to_string() == key)
        .map(Some)
        .ok_or_else(not_found)
}

fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "Account not found")
}

async fn update(
    Extension(state): Extension<WebState>,
    Path(key): Path<String>,
    headers: HeaderMap,
    Json(change): Json<AccountChange>,
) -> Result<Response, Response> {
    let member = account_id(&key)?;
    let token = super::request_token(&headers)
        .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "Sign in to continue"))?;
    let account = tokio::task::spawn_blocking(move || {
        state
            .auth
            .update_account(&token, member, change, chrono::Utc::now().timestamp())
    })
    .await
    .map_err(|e| internal(e.into()))?
    .map_err(|e| match e.downcast_ref::<ProfileError>() {
        Some(ProfileError::Invalid(message)) => error(StatusCode::BAD_REQUEST, message),
        Some(ProfileError::Unauthorized) => error(StatusCode::UNAUTHORIZED, "Sign in to continue"),
        _ => internal(e),
    })?
    .ok_or_else(not_found)?;
    Ok(private_json(account))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UsagePeriod {
    from: Option<i64>,
    to: Option<i64>,
    before: Option<String>,
}

async fn usage(
    Extension(state): Extension<WebState>,
    Path(key): Path<String>,
    Query(period): Query<UsagePeriod>,
) -> Result<Response, Response> {
    let member = account_id(&key)?;
    if period
        .from
        .into_iter()
        .chain(period.to)
        .any(|time| chrono::DateTime::from_timestamp(time, 0).is_none())
        || period
            .from
            .zip(period.to)
            .is_some_and(|(from, to)| from >= to)
        || period
            .before
            .as_ref()
            .is_some_and(|cursor| cursor.len() > 2048)
    {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "Invalid usage period or cursor",
        ));
    }
    let directory = match member {
        None => state
            .chat()
            .map_err(StatusCode::into_response)?
            .sessions
            .directory()
            .to_owned(),
        Some(id) => crate::spaces::personal(state.auth.data_directory(), id).join("sessions"),
    };
    let report = tokio::task::spawn_blocking(move || {
        if state.auth.account(member)?.is_none() {
            return Ok(None);
        }
        // A member's saved workspace must remain under the canonical data directory.
        if member.is_some() {
            crate::web::pages::reject_symlinks(&directory)?;
        }
        account_report(&directory, period.from, period.to, period.before.as_deref()).map(Some)
    })
    .await
    .map_err(|e| internal(e.into()))?
    .map_err(usage_error)?
    .ok_or_else(not_found)?;
    Ok(private_json(report))
}

fn usage_error(e: anyhow::Error) -> Response {
    if e.is::<crate::billing::InvalidCursor>() {
        error(StatusCode::BAD_REQUEST, "Invalid usage cursor")
    } else {
        internal(e)
    }
}

fn available(
    usage: anyhow::Result<AccountUsage>,
    account: impl std::fmt::Display,
) -> Option<AccountUsage> {
    match usage {
        Ok(usage) => Some(usage),
        Err(error) => {
            log::warn!("Unable to read model usage for account {account}: {error:#}");
            None
        }
    }
}
