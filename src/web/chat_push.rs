use super::{Chat, WebState, internal};
use crate::agent::session::SqliteSessionStore;
use crate::web::{
    Settings,
    auth::{AuthStore, request_token, token_hash},
};
use anyhow::{Context, ensure};
use axum::{
    Extension, Json,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures::{StreamExt, stream};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use web_push_native::{
    Auth, WebPushBuilder,
    jwt_simple::algorithms::ES256KeyPair,
    p256::{PublicKey, elliptic_curve::sec1::ToEncodedPoint},
};

pub(super) struct Push {
    settings: Settings,
    auth: AuthStore,
    key: ES256KeyPair,
    public_key: String,
    store: SqliteSessionStore,
    client: reqwest::Client,
    subscription_updates: tokio::sync::Mutex<()>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Keys {
    p256dh: String,
    auth: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Subscription {
    endpoint: String,
    keys: Keys,
    #[serde(default, rename = "expirationTime")]
    expiration_time: Option<f64>,
}

impl Subscription {
    fn builder(&self) -> anyhow::Result<WebPushBuilder> {
        let key = URL_SAFE_NO_PAD.decode(&self.keys.p256dh)?;
        let auth = URL_SAFE_NO_PAD.decode(&self.keys.auth)?;
        ensure!(
            key.len() == 65 && key[0] == 4 && auth.len() == 16,
            "Invalid push keys"
        );
        Ok(WebPushBuilder::new(
            self.endpoint.parse()?,
            PublicKey::from_sec1_bytes(&key)?,
            Auth::clone_from_slice(&auth),
        ))
    }

    fn validate(&self) -> anyhow::Result<()> {
        ensure!(self.endpoint.len() <= 2048, "Invalid push endpoint");
        let url = reqwest::Url::parse(&self.endpoint)?;
        let host = url.host_str().unwrap_or("");
        let allowed = matches!(
            host,
            "fcm.googleapis.com" | "updates.push.services.mozilla.com"
        ) || host.ends_with(".push.apple.com")
            || host.ends_with(".notify.windows.com");
        ensure!(
            allowed
                && url.scheme() == "https"
                && url.port_or_known_default() == Some(443)
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none(),
            "Unsupported push endpoint"
        );
        self.builder()?;
        Ok(())
    }
}

impl Chat {
    pub(crate) async fn with_push(
        mut self,
        settings: Settings,
        auth: AuthStore,
    ) -> anyhow::Result<Self> {
        let key = self
            .sessions
            .with_connection(|db| {
                let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let saved: Option<String> = tx
                    .query_row(
                        "SELECT value FROM schema_meta WHERE key='web_push_vapid'",
                        [],
                        |r| r.get(0),
                    )
                    .optional()?;
                let bytes = match saved {
                    Some(saved) => URL_SAFE_NO_PAD.decode(saved)?,
                    None => {
                        let bytes = ES256KeyPair::generate().to_bytes();
                        tx.execute(
                            "INSERT INTO schema_meta(key,value) VALUES('web_push_vapid',?1)",
                            [URL_SAFE_NO_PAD.encode(&bytes)],
                        )?;
                        bytes
                    }
                };
                tx.commit()?;
                Ok(ES256KeyPair::from_bytes(&bytes)?)
            })
            .await?;
        self.push = Some(Arc::new(Push {
            public_key: URL_SAFE_NO_PAD.encode(
                PublicKey::from_sec1_bytes(&key.public_key().to_bytes())?
                    .to_encoded_point(false)
                    .as_bytes(),
            ),
            key,
            settings,
            auth,
            store: (*self.sessions).clone(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .build()?,
            subscription_updates: tokio::sync::Mutex::new(()),
        }));
        let chat = self.clone();
        tokio::spawn(async move {
            let mut shutdown = chat.runner.shutdown_receiver();
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.wait_for(|stop| *stop) => break,
                    _ = tick.tick() => {},
                }
                let mut chats = vec![chat.clone()];
                chats.extend(chat.user_chats.lock().await.values().cloned());
                let work = stream::iter(chats).for_each_concurrent(4, |chat| async move {
                    if chat
                        .push_tick(chrono::Utc::now().timestamp())
                        .await
                        .is_err()
                    {
                        log::warn!("Web push queue processing failed");
                    }
                });
                tokio::select! {
                    biased;
                    _ = shutdown.wait_for(|stop| *stop) => break,
                    _ = work => {},
                }
            }
        });
        Ok(self)
    }

    async fn prune_push(&self, now: i64) -> anyhow::Result<()> {
        let push = self.push.as_ref().context("Push is unavailable")?;
        let hashes = self
            .sessions
            .with_connection(|db| {
                Ok(db
                    .prepare("SELECT session_hash FROM web_push_subscriptions UNION SELECT session_hash FROM web_push_presence")?
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?)
            })
            .await?;
        let auth = push.auth.clone();
        let invalid = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<String>> {
            let mut invalid = vec![];
            for hash in hashes {
                if auth.identity_from_hash(&hash, now)?.is_none() {
                    invalid.push(hash);
                }
            }
            Ok(invalid)
        })
        .await??;
        let invalid_bindings = invalid.clone();
        push.store
            .with_connection(move |db| {
                for hash in invalid_bindings {
                    db.execute(
                        "DELETE FROM web_push_bindings WHERE session_hash=?1",
                        [hash],
                    )?;
                }
                Ok(())
            })
            .await?;
        self.sessions
            .with_connection(move |db| {
                for hash in invalid {
                    db.execute(
                        "DELETE FROM web_push_presence WHERE session_hash=?1",
                        [&hash],
                    )?;
                    db.execute(
                        "DELETE FROM web_push_subscriptions WHERE session_hash=?1",
                        [hash],
                    )?;
                }
                db.execute(
                    "DELETE FROM web_push_queue WHERE created_at<=?1",
                    [now - 86400],
                )?;
                db.execute(
                    "DELETE FROM web_push_presence WHERE updated_at<=?1",
                    [now - 45],
                )?;
                Ok(())
            })
            .await
    }

    async fn push_tick(&self, now: i64) -> anyhow::Result<()> {
        self.prune_push(now).await?;
        let rows = self.sessions.with_connection(move |db| {
            Ok(db.prepare("SELECT q.event_id,q.endpoint,q.session_id,q.attempts,s.session_hash,s.subscription,s.locale,q.created_at
                FROM web_push_queue q JOIN web_push_subscriptions s ON s.endpoint=q.endpoint
                WHERE q.next_attempt<=?1 ORDER BY q.next_attempt LIMIT 32")?
                .query_map([now], |r| Ok(Delivery {
                    event:r.get(0)?, endpoint:r.get(1)?, session:r.get(2)?, attempts:r.get(3)?,
                    hash:r.get(4)?, subscription:r.get(5)?, locale:r.get(6)?, created_at:r.get(7)?,
                }))?.collect::<Result<Vec<_>,_>>()?)
        }).await?;
        let results = stream::iter(rows)
            .map(|row| self.deliver_push(row, now))
            .buffer_unordered(4)
            .collect::<Vec<_>>()
            .await;
        for result in results {
            result?;
        }
        Ok(())
    }

    async fn deliver_push(&self, row: Delivery, now: i64) -> anyhow::Result<()> {
        let now = now.max(chrono::Utc::now().timestamp());
        let push = self.push.as_ref().context("Push is unavailable")?;
        let subscription_update = push.subscription_updates.lock().await;
        let (endpoint, hash) = (row.endpoint.clone(), row.hash.clone());
        let bound = push.store.with_connection(move |db| {
            Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM web_push_bindings WHERE endpoint=?1 AND session_hash=?2)",params![endpoint,hash],|r|r.get::<_,bool>(0))?)
        }).await?;
        let auth = push.auth.clone();
        let hash = row.hash.clone();
        let allowed = tokio::task::spawn_blocking(move || {
            auth.identity_from_hash(&hash, now.max(chrono::Utc::now().timestamp()))
        })
        .await??
        .is_some();
        if !bound || !allowed {
            return self.remove_push(&row.endpoint, &row.hash).await;
        }
        if let Some(id) = row.event.strip_prefix("approval:")
            && !self
                .sessions
                .pending_command_approvals(&row.session)
                .await?
                .iter()
                .any(|approval| approval.id == id)
        {
            return self.finish_push(row).await;
        }
        let (event, endpoint, hash) = (row.event.clone(), row.endpoint.clone(), row.hash.clone());
        let viewers = self.sessions.with_connection(move |db| {
            let root: Option<String> = db.query_row("SELECT coalesce((SELECT root_id FROM web_chat_branches WHERE session_id=q.session_id),q.session_id)
                FROM web_push_queue q JOIN web_push_subscriptions s ON s.endpoint=q.endpoint
                WHERE q.event_id=?1 AND q.endpoint=?2 AND s.session_hash=?3",
                params![event,endpoint,hash],|r|r.get(0)).optional()?;
            let Some(root) = root else { return Ok(None); };
            Ok(Some(db.prepare("SELECT DISTINCT session_hash FROM web_push_presence WHERE session_id=?1 AND updated_at>?2")?
                .query_map(params![root,now.max(chrono::Utc::now().timestamp())-45],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?))
        }).await?;
        let Some(viewers) = viewers else {
            return Ok(());
        };
        let auth = push.auth.clone();
        let visible = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
            for hash in viewers {
                if auth
                    .identity_from_hash(&hash, now.max(chrono::Utc::now().timestamp()))?
                    .is_some()
                {
                    return Ok(true);
                }
            }
            Ok(false)
        })
        .await??;
        let now = now.max(chrono::Utc::now().timestamp());
        if visible {
            return self
                .sessions
                .with_connection(move |db| {
                    db.execute("DELETE FROM web_push_queue WHERE event_id=?1", [row.event])?;
                    Ok(())
                })
                .await;
        }
        if row.created_at <= now - 86400 {
            return self.finish_push(row).await;
        }
        let subscription: Subscription = serde_json::from_slice(&row.subscription)?;
        let payload = serde_json::to_vec(
            &json!({"session_id":row.session,"event_id":row.event,"locale":row.locale}),
        )?;
        let request = subscription
            .builder()?
            .with_valid_duration(Duration::from_secs(
                (86400 - (now - row.created_at)).clamp(1, 86400) as u64,
            ))
            .with_vapid(
                &push.key,
                if push.settings.secure() {
                    push.settings.base_url.as_str()
                } else {
                    // HTTP loopback is valid for chat, but not as a VAPID contact.
                    env!("CARGO_PKG_REPOSITORY")
                },
            )
            .build(payload)?;
        let request: reqwest::Request = request.try_into()?;
        drop(subscription_update);
        let result = push.client.execute(request).await;
        if result.as_ref().is_ok_and(|r| r.status().is_success()) {
            return self.finish_push(row).await;
        }
        if result
            .as_ref()
            .is_ok_and(|r| matches!(r.status().as_u16(), 404 | 410))
        {
            return self.remove_push(&row.endpoint, &row.hash).await;
        }
        let retry_after = result
            .ok()
            .and_then(|r| {
                r.headers()
                    .get(header::RETRY_AFTER)?
                    .to_str()
                    .ok()?
                    .parse::<i64>()
                    .ok()
            })
            .unwrap_or(0)
            .clamp(0, 3600);
        let delay = (5 * 2_i64.pow(row.attempts.min(10)))
            .min(3600)
            .max(retry_after);
        self.sessions.with_connection(move |db| {
            db.execute("UPDATE web_push_queue SET attempts=attempts+1,next_attempt=?3 WHERE event_id=?1 AND endpoint=?2", params![row.event,row.endpoint,now+delay])?;
            Ok(())
        }).await
    }

    async fn finish_push(&self, row: Delivery) -> anyhow::Result<()> {
        self.sessions
            .with_connection(move |db| {
                db.execute(
                    "DELETE FROM web_push_queue WHERE event_id=?1 AND endpoint=?2",
                    params![row.event, row.endpoint],
                )?;
                Ok(())
            })
            .await
    }

    async fn remove_push(&self, endpoint: &str, hash: &str) -> anyhow::Result<()> {
        let (endpoint, hash) = (endpoint.to_owned(), hash.to_owned());
        let (registry_endpoint, registry_hash) = (endpoint.clone(), hash.clone());
        self.push
            .as_ref()
            .context("Push is unavailable")?
            .store
            .with_connection(move |db| {
                db.execute(
                    "DELETE FROM web_push_bindings WHERE endpoint=?1 AND session_hash=?2",
                    params![registry_endpoint, registry_hash],
                )?;
                Ok(())
            })
            .await?;
        self.sessions
            .with_connection(move |db| {
                db.execute(
                    "DELETE FROM web_push_subscriptions WHERE endpoint=?1 AND session_hash=?2",
                    params![endpoint, hash],
                )?;
                Ok(())
            })
            .await
    }
}

struct Delivery {
    created_at: i64,
    event: String,
    endpoint: String,
    session: String,
    attempts: u32,
    hash: String,
    subscription: Vec<u8>,
    locale: String,
}

fn session_hash(headers: &HeaderMap) -> String {
    token_hash(&request_token(headers).unwrap_or_default())
}

pub(super) async fn config(Extension(state): Extension<WebState>, headers: HeaderMap) -> Response {
    let Ok(chat) = state.chat() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Some(push) = &chat.push else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let hash = session_hash(&headers);
    match chat
        .sessions
        .with_connection(move |db| {
            Ok(db
                .prepare("SELECT endpoint FROM web_push_subscriptions WHERE session_hash=?1")?
                .query_map([hash], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?)
        })
        .await
    {
        Ok(endpoints) => (
            [(header::CACHE_CONTROL, "no-store")],
            Json(json!({"public_key":push.public_key,"endpoints":endpoints})),
        )
            .into_response(),
        Err(error) => internal(error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Subscribe {
    subscription: Subscription,
    locale: String,
}

pub(super) async fn subscribe(
    Extension(state): Extension<WebState>,
    headers: HeaderMap,
    Json(input): Json<Subscribe>,
) -> Response {
    let Ok(chat) = state.chat() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Some(push) = &chat.push else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    if input.subscription.validate().is_err() || !matches!(input.locale.as_str(), "ru" | "en") {
        return StatusCode::BAD_REQUEST.into_response();
    }
    // Subscription ownership spans the user store and the shared device registry.
    let _subscription_update = push.subscription_updates.lock().await;
    let hash = session_hash(&headers);
    let result = async {
        chat.prune_push(chrono::Utc::now().timestamp()).await?;
        let (endpoint,binding) = (input.subscription.endpoint.clone(),hash.clone());
        let status = chat.sessions.with_connection(move |db| {
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let count: i64 = tx.query_row("SELECT count(*) FROM web_push_subscriptions WHERE endpoint!=?1",[&input.subscription.endpoint],|r|r.get(0))?;
            if count >= 8 { return Ok(StatusCode::TOO_MANY_REQUESTS); }
            // A new login must never inherit queued notifications or foreground state.
            tx.execute("DELETE FROM web_push_subscriptions WHERE endpoint=?1 AND session_hash!=?2",params![input.subscription.endpoint,hash])?;
            tx.execute("INSERT INTO web_push_subscriptions VALUES(?1,?2,?3,?4) ON CONFLICT(endpoint) DO UPDATE SET subscription=excluded.subscription,locale=excluded.locale",
                params![input.subscription.endpoint,hash,serde_json::to_vec(&input.subscription)?,input.locale])?;
            tx.commit()?;
            Ok(StatusCode::NO_CONTENT)
        }).await?;
        if status == StatusCode::NO_CONTENT {
            push.store.with_connection(move |db| {
                db.execute("INSERT INTO web_push_bindings VALUES(?1,?2) ON CONFLICT(endpoint) DO UPDATE SET session_hash=excluded.session_hash",params![endpoint,binding])?;
                Ok(())
            }).await?;
        }
        Ok::<_,anyhow::Error>(status)
    }.await;
    match result {
        Ok(status) => status.into_response(),
        Err(error) => internal(error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Unsubscribe {
    endpoint: String,
}

pub(super) async fn unsubscribe(
    Extension(state): Extension<WebState>,
    headers: HeaderMap,
    Json(input): Json<Unsubscribe>,
) -> Response {
    let Ok(chat) = state.chat() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match chat
        .remove_push(&input.endpoint, &session_hash(&headers))
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => internal(error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Presence {
    #[serde(default, rename = "endpoint")]
    _legacy_endpoint: Option<serde::de::IgnoredAny>,
    tab_id: String,
    session_id: Option<String>,
}

pub(super) async fn presence(
    Extension(state): Extension<WebState>,
    headers: HeaderMap,
    Json(mut input): Json<Presence>,
) -> Response {
    let Ok(chat) = state.chat() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    if uuid::Uuid::parse_str(&input.tab_id).is_err() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Some(id) = &input.session_id {
        if let Err(response) = chat.session(id).await {
            return response;
        }
        input.session_id = match chat.root_id(id).await {
            Ok(id) => Some(id),
            Err(error) => return internal(error),
        };
    }
    let hash = session_hash(&headers);
    match chat.sessions.with_connection(move |db| {
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM web_push_presence WHERE session_hash=?1 AND tab_id=?2",params![hash,input.tab_id])?;
        if let Some(id) = input.session_id {
            let count: i64 = tx.query_row("SELECT count(*) FROM web_push_presence WHERE session_hash=?1 AND updated_at>?2",params![hash,chrono::Utc::now().timestamp()-45],|r|r.get(0))?;
            if count>=16 { return Ok(StatusCode::TOO_MANY_REQUESTS); }
            tx.execute("INSERT INTO web_push_presence VALUES(?1,?2,?3,?4)",params![hash,input.tab_id,id,chrono::Utc::now().timestamp()])?;
        }
        tx.commit()?;
        Ok(StatusCode::NO_CONTENT)
    }).await { Ok(status) => status.into_response(), Err(error) => internal(error) }
}

pub(super) async fn worker() -> Response {
    let mut response = crate::web::chat_assets::response("chat/push-sw.js");
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

#[cfg(test)]
#[path = "chat_push_tests.rs"]
mod tests;
