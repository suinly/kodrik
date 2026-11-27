use super::{WebState, index, internal};
use crate::{
    billing::{AccountUsage, account_usage, saved_account_usage},
    web::auth::AccountSummary,
};
use axum::{
    Extension, Json, Router,
    http::{StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Serialize;

pub(super) fn routes(state: WebState) -> Router<WebState> {
    Router::new()
        .route("/api/admin/users", get(list))
        .route("/app/users", get(index))
        .route("/app/users/", get(index))
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
        auth.active_accounts().map(|accounts| {
            accounts
                .into_iter()
                .map(|account| {
                    let usage = if account.owner {
                        None
                    } else {
                        let id = account.telegram_id.expect("Member always has an ID");
                        let directory = crate::spaces::personal(auth.data_directory(), id);
                        available(saved_account_usage(&directory), id)
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
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(UsersReport { users }),
    )
        .into_response())
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
