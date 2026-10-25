use super::{
    Settings,
    auth::{AuthStore, Identity, request_token},
    chat_stream::{SnapshotProjection, history},
    server::valid_origin,
};
use crate::agent::{
    AgentRunner,
    session::{Session, SessionId, SessionStore, SqliteSessionStore},
};
use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, OriginalUri, Path, Request, State},
    http::{HeaderMap, Method, StatusCode},
    middleware::{self, Next},
    response::{
        IntoResponse, Redirect, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    path::Path as FsPath,
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, Semaphore, mpsc};

#[path = "chat_management.rs"]
mod management;
#[path = "chat_subagents.rs"]
mod subagents;
#[path = "chat_titles.rs"]
mod titles;
#[cfg(test)]
#[path = "chat_user_tests.rs"]
pub(super) mod user_tests;
#[cfg(test)]
#[path = "webhook_tests.rs"]
mod webhook_tests;
#[path = "webhooks.rs"]
mod webhooks;

#[derive(Clone)]
pub(crate) struct Chat {
    runner: Arc<AgentRunner>,
    sessions: Arc<SqliteSessionStore>,
    runs: Arc<Mutex<HashSet<String>>>,
    title_jobs: Arc<Mutex<HashSet<String>>>,
    capacity: Arc<Semaphore>,
    streams: Arc<Semaphore>,
    attachment_directory: std::path::PathBuf,
    users: Option<Arc<crate::users::UserSpaces>>,
    user_chats: Arc<Mutex<HashMap<u64, Chat>>>,
}

impl Chat {
    pub(crate) async fn new(owner: &AgentRunner, data: &FsPath) -> anyhow::Result<Self> {
        let sessions =
            Arc::new(owner.sqlite_store().ok_or_else(|| {
                anyhow::anyhow!("Web chat requires a durable user session store")
            })?);
        titles::init(&sessions).await?;
        management::init(&sessions).await?;
        super::webhook_store::init(&sessions).await?;
        let chat = Self {
            runner: Arc::new(owner.for_web(sessions.clone())),
            sessions,
            runs: Default::default(),
            title_jobs: Default::default(),
            capacity: Arc::new(Semaphore::new(4)),
            streams: Arc::new(Semaphore::new(4)),
            attachment_directory: data.join("web/attachments"),
            users: None,
            user_chats: Default::default(),
        };
        // Reserve recovered conversations before accepting HTTP submissions.
        for id in chat.runner.unfinished_conversations().await? {
            if uuid::Uuid::parse_str(id.as_str()).is_ok() || id.as_str().starts_with("web:") {
                chat.runs
                    .lock()
                    .await
                    .insert(web_session(id.as_str()).into());
                let worker = chat.clone();
                tokio::spawn(async move {
                    worker.work(id, true).await;
                });
            }
        }
        Ok(chat)
    }

    pub(crate) fn with_users(mut self, users: Arc<crate::users::UserSpaces>) -> Self {
        self.users = Some(users);
        self
    }

    pub(crate) async fn recover_users(&self) -> anyhow::Result<()> {
        if let Some(users) = &self.users {
            for id in users.saved_users().await? {
                if !users.is_owner(id) {
                    self.for_user(id).await?;
                }
            }
        }
        Ok(())
    }

    async fn for_user(&self, id: u64) -> anyhow::Result<Chat> {
        let users = self
            .users
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("User login is not configured"))?;
        anyhow::ensure!(users.allowed_user(id)?, "Access requires an invitation");
        if users.is_owner(id) {
            return Ok(self.clone());
        }
        let mut chats = self.user_chats.lock().await;
        if let Some(chat) = chats.get(&id) {
            return Ok(chat.clone());
        }
        let app = users.app(id).await?;
        let chat = Chat::new(&app.agent_runner, &users.directory(id)).await?;
        chats.insert(id, chat.clone());
        Ok(chat)
    }

    async fn stop(&self, id: &str) -> anyhow::Result<()> {
        self.sessions.cancel_session_deliveries(id).await?;
        let session = id.to_owned();
        let conversation = self.sessions.with_connection(move |db| {
            use rusqlite::OptionalExtension;
            Ok(db.query_row("SELECT conversation_id FROM runs WHERE session_id=?1 AND terminal=0 UNION ALL SELECT s.conversation_id FROM submissions s LEFT JOIN active_sessions a ON a.conversation_id=s.conversation_id WHERE s.status='Queued' AND coalesce(s.session_id,a.session_id,s.conversation_id)=?1 LIMIT 1", [&session], |row|row.get::<_,String>(0)).optional()?)
        }).await?;
        let conversation = match conversation {
            Some(conversation) => SessionId::new(conversation),
            None => {
                let conversation = web_conversation(id);
                if conversation.as_str() != id {
                    self.sessions
                        .select(&conversation, &SessionId::new(id))
                        .await?;
                }
                conversation
            }
        };
        let command = crate::gateway::IncomingMessage::new(
            crate::gateway::InterfaceId("web".into()),
            id,
            "/stop",
        );
        // Persist a control submission, including the admission-before-checkpoint gap.
        let runner = self.runner.clone();
        let id = conversation;
        // Losing the command response must not abandon an already persisted Stop.
        tokio::spawn(async move { runner.apply_control(&id, &command).await }).await?
    }

    async fn work(&self, id: SessionId, recover: bool) {
        let session_id = SessionId::new(web_session(id.as_str()));
        let _permit = self
            .capacity
            .acquire()
            .await
            .expect("Web capacity stays open");
        let mut recover = recover;
        loop {
            let mut drain_queue = true;
            let mut failed_input = None;
            let result = async {
                if recover {
                    let report = self
                        .runner
                        .recover(crate::agent::recovery::RecoveryTarget::Conversation(
                            id.clone(),
                        ))
                        .await?;
                    for (run, reason) in report.blocked {
                        drain_queue = false;
                        log::warn!("Web recovery blocked for {run}: {reason}");
                    }
                    self.start_title(&session_id).await;
                } else {
                    // Keep streaming enabled, but never bind runtime lifetime to an observer.
                    let (progress, mut events) = mpsc::channel(32);
                    let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });
                    let result = async {
                        while let Some(next) = self.runner.next_input(&id).await? {
                            let request = next.request_id.clone();
                            if let Err(error) = self
                                .runner
                                .run_with_cancel(
                                    id.clone(),
                                    next,
                                    Some(progress.clone()),
                                    None,
                                    None,
                                )
                                .await
                            {
                                failed_input = Some(request);
                                return Err(error);
                            }
                            self.start_title(&session_id).await;
                        }
                        Ok::<_, anyhow::Error>(())
                    }
                    .await;
                    drop(progress);
                    let _ = drain.await;
                    result?;
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = result {
                log::warn!("Web worker failed for {}: {error:#}", id.as_str());
            }
            // Admission uses this lock too: retire only after an atomic inbox check.
            let mut runs = self.runs.lock().await;
            if drain_queue && !*self.runner.shutdown_receiver().borrow() {
                match self.runner.next_input(&id).await {
                    Ok(Some(next))
                        if failed_input
                            .as_ref()
                            .is_none_or(|failed| failed != &next.request_id) =>
                    {
                        recover = false;
                        drop(runs);
                        continue;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        log::warn!("Web inbox check failed for {}: {error:#}", id.as_str())
                    }
                }
            }
            runs.remove(session_id.as_str());
            break;
        }
    }

    async fn snapshot(&self, id: &str) -> anyhow::Result<Value> {
        let view = self.sessions.conversation_view(id).await?;
        let mut snapshot = if let Some(run) = &view.run {
            SnapshotProjection::new(run.run.id.clone())
                .snapshot(run)
                .expect("Fresh projection")
        } else {
            json!({"type":"snapshot","run_id":null,"session_id":id,"phase":null,
            "cancel_requested":false,"messages":history(&view.session),"plan":null,
            "live":{"current":null,"interrupted":null},"deliveries":[]})
        };
        if view
            .run
            .as_ref()
            .is_some_and(|run| run.run.phase.terminal())
        {
            snapshot["messages"] = json!(history(&view.session));
        }
        snapshot["revision"] = json!(view.revision);
        snapshot["busy"] = json!(view.busy);
        snapshot["blocked_reason"] = json!(view.blocked_reason);
        snapshot["queued"] = json!(view.queued);
        let (title, title_default) = self.display_title(&view.session).await?;
        snapshot["title"] = json!(title);
        snapshot["title_default"] = json!(title_default);
        snapshot["titles_revision"] = json!(self.titles_revision().await?);
        let root = self.root_id(id).await?;
        snapshot["title_pending"] = json!(self.title_jobs.lock().await.contains(&root));
        let mut children = Vec::new();
        if let Some(manager) = self.runner.subagents() {
            for child in manager.list(&view.session).await? {
                let mut value = child.view();
                value["task"] = json!(child.task);
                value["cancel_requested"] = json!(manager.cancellation_requested(&child.id).await?);
                children.push(value);
            }
        }
        snapshot["subagents"] = json!(children);
        self.decorate_versions(id, &mut snapshot).await?;
        Ok(snapshot)
    }

    async fn session(&self, id: &str) -> Result<Session, Response> {
        if id.is_empty()
            || id.len() > 256
            || !id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b':' | b'-' | b'_'))
            || uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() != id)
        {
            return Err(error(StatusCode::BAD_REQUEST, "Invalid conversation ID"));
        }
        self.sessions
            .load(&SessionId::new(id))
            .await
            .map_err(internal)?
            .filter(|s| s.id().as_str() == id && personal_session(s))
            .ok_or_else(|| error(StatusCode::NOT_FOUND, "Conversation not found"))
    }

    async fn display_title(&self, session: &Session) -> anyhow::Result<(String, bool)> {
        let title = self
            .generated_title(session.id().as_str())
            .await?
            .or_else(|| fallback_title(session));
        let title_default = title.is_none();
        Ok((
            title.unwrap_or_else(|| "Новый диалог".into()),
            title_default,
        ))
    }
}

fn fallback_title(session: &Session) -> Option<String> {
    session.messages().iter().find_map(|message| {
        let crate::agent::message::Message::User(user) = message else {
            return None;
        };
        let title: String = user
            .content
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(80)
            .collect();
        (!title.is_empty()).then_some(title)
    })
}

// Legacy ids name Telegram/CLI selectors; web must use its own delivery channel.
fn web_conversation(id: &str) -> SessionId {
    SessionId::new(if uuid::Uuid::parse_str(id).is_ok() {
        id.to_owned()
    } else {
        format!("web:{id}")
    })
}

fn web_session(conversation: &str) -> &str {
    conversation.strip_prefix("web:").unwrap_or(conversation)
}

#[derive(Clone)]
struct WebState {
    settings: Settings,
    auth: AuthStore,
    chat: Option<Chat>,
    streams: Arc<Semaphore>,
}

impl WebState {
    fn chat(&self) -> Result<&Chat, StatusCode> {
        self.chat.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)
    }

    async fn authenticated(&self, headers: &HeaderMap) -> Result<bool, Response> {
        let Some(token) = request_token(headers) else {
            return Ok(false);
        };
        let auth = self.auth.clone();
        tokio::task::spawn_blocking(move || {
            auth.identity(&token, chrono::Utc::now().timestamp())
                .map(|identity| identity.is_some())
        })
        .await
        .map_err(|e| internal(e.into()))?
        .map_err(internal)
    }
}

pub(super) fn routes(settings: Settings, data: &FsPath, chat: Option<Chat>) -> Router {
    let state = WebState {
        settings,
        auth: AuthStore::new(data),
        chat,
        streams: Arc::new(Semaphore::new(64)),
    };
    let api = Router::new()
        .route("/api/chat/sessions", get(management::list).post(create))
        .route(
            "/api/chat/sessions/{id}",
            get(read)
                .patch(management::rename)
                .delete(management::delete),
        )
        .route("/api/chat/sessions/{id}/select", post(management::select))
        .route("/api/chat/sessions/{id}/pin", post(management::pin))
        .route(
            "/api/chat/sessions/{id}/messages/{message}/edit",
            post(management::edit),
        )
        .route("/api/chat/sessions/{id}/stop", post(stop))
        .route(
            "/api/chat/sessions/{id}/subagents/{agent}/stop",
            post(subagents::stop),
        )
        .route("/api/chat/sessions/{id}/resume", post(resume))
        .route("/api/chat/sessions/{id}/run", get(run_snapshot))
        .route("/api/chat/sessions/{id}/events", get(events))
        .route(
            "/api/chat/sessions/{id}/attachments/{message}/{kind}/{index}",
            get(attachment),
        )
        .route(
            "/api/chat/sessions/{id}/requests/{request}",
            get(request_status),
        )
        .layer(DefaultBodyLimit::max(64 * 1024))
        .route(
            "/api/chat/send",
            post(send).layer(DefaultBodyLimit::max(super::chat_upload::BODY_LIMIT)),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize));
    Router::new()
        .merge(api)
        .merge(webhooks::routes(state.clone()))
        .route("/chat", get(chat_redirect))
        .route("/chat/", get(index))
        .route("/chat/{*path}", get(asset))
        .with_state(state)
}

async fn authorize(
    State(mut state): State<WebState>,
    headers: HeaderMap,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(token) = request_token(&headers) else {
        return error(StatusCode::UNAUTHORIZED, "Sign in to continue");
    };
    let auth = state.auth.clone();
    let identity = match tokio::task::spawn_blocking(move || {
        auth.identity(&token, chrono::Utc::now().timestamp())
    })
    .await
    {
        Ok(Ok(Some(identity))) => identity,
        Ok(Ok(None)) => return error(StatusCode::UNAUTHORIZED, "Sign in to continue"),
        Ok(Err(error)) => return internal(error),
        Err(error) => return internal(error.into()),
    };
    if request.method() != Method::GET
        && request.method() != Method::HEAD
        && !valid_origin(&state.settings, &headers)
    {
        return error(StatusCode::FORBIDDEN, "Invalid request origin");
    }
    if let Identity::Telegram(user) = identity {
        let Some(chat) = state.chat.as_ref() else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        state.chat = match chat.for_user(user).await {
            Ok(chat) => Some(chat),
            Err(error) => return internal(error),
        };
    }
    request.extensions_mut().insert(state);
    next.run(request).await
}

async fn chat_redirect(State(state): State<WebState>, OriginalUri(uri): OriginalUri) -> Redirect {
    let query = uri
        .query()
        .map(|query| format!("?{query}"))
        .unwrap_or_default();
    Redirect::to(&format!("{}chat/{query}", state.settings.prefix()))
}

async fn index(
    State(state): State<WebState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    match state.authenticated(&headers).await {
        Ok(true) => super::chat_assets::index(state.settings.prefix()),
        Ok(false) => {
            let mut url = state.settings.base_url.join("auth/login").unwrap();
            url.query_pairs_mut()
                .append_pair("return_to", uri.path_and_query().unwrap().as_str());
            Redirect::to(url.as_str()).into_response()
        }
        Err(response) => response,
    }
}

async fn asset(Path(path): Path<String>) -> Response {
    super::chat_assets::response(&format!("chat/{path}"))
}

pub(super) fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error":message}))).into_response()
}

fn personal_session(session: &Session) -> bool {
    session.telegram_group().is_none() && personal_session_id(session.id().as_str())
}

fn webhook_source(session: &Session) -> Option<Value> {
    let recorded = session
        .webhook_instruction
        .as_deref()?
        .strip_prefix("## Recorded delivery source\n")?
        .lines()
        .next()?;
    let source: Value = serde_json::from_str(recorded).ok()?;
    Some(json!({"id":source["webhook_id"].as_str()?,"name":source["webhook_name"].as_str()?}))
}

fn personal_session_id(id: &str) -> bool {
    !id.starts_with("companion:")
        && !id.ends_with(":guest")
        && !(id.starts_with("telegram:")
            && id
                .rsplit(':')
                .next()
                .is_some_and(|chat| chat.starts_with('-')))
}

fn internal(error_value: anyhow::Error) -> Response {
    log::warn!("Web chat failed: {error_value:#}");
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Unable to complete the request. See server logs.",
    )
}

async fn create(
    Extension(state): Extension<WebState>,
) -> Result<(StatusCode, Json<Value>), Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let session = Session::new(SessionId::new(uuid::Uuid::new_v4().to_string()));
    chat.sessions.save(&session).await.map_err(internal)?;
    Ok((
        StatusCode::CREATED,
        Json(
            json!({"id":session.id().as_str(), "title":"Новый диалог", "title_default":true, "busy":false, "updated_at":null}),
        ),
    ))
}

async fn read(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    chat.session(&id).await?;
    let view = chat
        .sessions
        .conversation_view(&id)
        .await
        .map_err(internal)?;
    let session = &view.session;
    let root = chat.root_id(&id).await.map_err(internal)?;
    let title_pending = chat.title_jobs.lock().await.contains(&root);
    let (title, title_default) = chat.display_title(session).await.map_err(internal)?;
    Ok(Json(
        json!({"id":id, "root_id":root, "title_pending":title_pending,
        "title":title, "title_default":title_default,"webhook":webhook_source(session),
        "updated_at":session.last_message_at(),
        "messages":history(session), "busy":view.busy, "blocked_reason":view.blocked_reason}),
    ))
}

async fn stop(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
) -> Result<StatusCode, Response> {
    tokio::spawn(async move {
        let chat = state.chat().map_err(IntoResponse::into_response)?;
        let _runs = chat.runs.lock().await;
        chat.session(&id).await?;
        chat.stop(&id).await.map_err(internal)?;
        Ok(StatusCode::ACCEPTED)
    })
    .await
    .map_err(|e| internal(e.into()))?
}

async fn resume(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
) -> Result<StatusCode, Response> {
    // A disconnected observer must not separate unblocking from worker startup.
    tokio::spawn(async move {
        let chat = state.chat().map_err(IntoResponse::into_response)?;
        let mut runs = chat.runs.lock().await;
        chat.session(&id).await?;
        if !runs.contains(&id) && runs.len() >= 4 {
            return Err(error(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many active conversations",
            ));
        }
        let conversation = web_conversation(&id);
        if chat
            .sessions
            .conversation_view(&id)
            .await
            .map_err(internal)?
            .blocked_reason
            .is_some()
            && chat
                .sessions
                .inbox_blocked(conversation.as_str())
                .await
                .map_err(internal)?
                .is_none()
        {
            return Err(error(
                StatusCode::CONFLICT,
                "Resume the queue in its original interface",
            ));
        }
        chat.sessions
            .resume_inbox(conversation.as_str())
            .await
            .map_err(|reason| {
                if reason.is::<crate::agent::inbox::AdmissionRejected>() {
                    error(StatusCode::CONFLICT, "Conversation is still running")
                } else {
                    internal(reason)
                }
            })?;
        if !runs.contains(&id)
            && chat
                .runner
                .next_input(&conversation)
                .await
                .map_err(internal)?
                .is_some()
        {
            runs.insert(id.clone());
            let worker = chat.clone();
            tokio::spawn(async move {
                worker.work(conversation, false).await;
            });
        }
        Ok(StatusCode::ACCEPTED)
    })
    .await
    .map_err(|error| internal(error.into()))?
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Input {
    id: String,
    #[serde(default = "new_request_id")]
    request_id: String,
    text: String,
}

fn new_request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

async fn request_status(
    Extension(state): Extension<WebState>,
    Path((id, request)): Path<(String, String)>,
) -> Result<Json<Value>, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    chat.session(&id).await?;
    let record = chat
        .sessions
        .submission(web_conversation(&id).as_str(), &request)
        .await
        .map_err(internal)?
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Request not found"))?;
    let blocked = chat
        .sessions
        .inbox_blocked(web_conversation(&id).as_str())
        .await
        .map_err(internal)?;
    Ok(Json(
        json!({"request_id":record.request.request_id,"status":record.status,"run_id":record.run_id,"blocked_reason":blocked}),
    ))
}

async fn send(
    Extension(state): Extension<WebState>,
    request: Request,
) -> Result<Response, Response> {
    let (input, uploads) = super::chat_upload::parse(request).await?;
    // A lost HTTP observer must not interrupt admission or its attachment ownership.
    tokio::spawn(admit(state, input, uploads))
        .await
        .map_err(|e| internal(e.into()))?
}

async fn admit(
    state: WebState,
    input: Input,
    uploads: Vec<super::chat_upload::Upload>,
) -> Result<Response, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?.clone();
    if (input.text.trim().is_empty() && uploads.is_empty()) || input.text.len() > 32 * 1024 {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "Message must contain text or attachments; text limit is 32768 bytes",
        ));
    }
    if !uuid::Uuid::parse_str(&input.request_id).is_ok_and(|id| id.to_string() == input.request_id)
    {
        return Err(error(StatusCode::BAD_REQUEST, "Invalid request ID"));
    }
    let mut runs = chat.runs.lock().await;
    chat.session(&input.id).await?;
    let conversation = web_conversation(&input.id);
    if conversation.as_str() != input.id {
        chat.sessions
            .select(&conversation, &SessionId::new(&input.id))
            .await
            .map_err(internal)?;
    }
    let existing = chat
        .sessions
        .submission(conversation.as_str(), &input.request_id)
        .await
        .map_err(internal)?;
    if existing.is_none() {
        if !runs.contains(&input.id) && runs.len() >= 4 {
            return Err(error(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many active conversations",
            ));
        }
    }
    let mut message = crate::gateway::IncomingMessage::new(
        crate::gateway::InterfaceId("web".into()),
        &input.id,
        &input.text,
    );
    message.request_id = input.request_id.clone();
    let directory = chat.attachment_directory.join(&input.id);
    let mut attachments = tokio::task::spawn_blocking(move || {
        super::chat_upload::Attachments::save(&directory, uploads)
    })
    .await
    .map_err(|e| internal(e.into()))?
    .map_err(|e| error(StatusCode::BAD_REQUEST, &e.to_string()))?;
    message.files = attachments.files.clone();
    message.images = attachments.images.clone();
    attachments.start_admission();
    let record = chat
        .runner
        .submit(crate::agent::inbox::SubmissionRequest::new(
            &conversation,
            message,
            existing
                .as_ref()
                .map(|record| record.request.mode)
                .unwrap_or(crate::agent::inbox::BusyMode::Steer),
        ))
        .await;
    let record = match record {
        Ok(record) => record,
        Err(e) => {
            if let Ok(saved) = chat
                .sessions
                .submission(conversation.as_str(), &input.request_id)
                .await
            {
                attachments.retain_accepted(
                    saved
                        .as_ref()
                        .map(|record| record.request.message.files.as_slice())
                        .unwrap_or_default(),
                );
            }
            return Err(
                if e.downcast_ref::<crate::agent::inbox::AdmissionRejected>()
                    .is_some_and(|reason| reason.0 == "Inbox queue is full")
                {
                    error(StatusCode::TOO_MANY_REQUESTS, "Inbox queue is full")
                } else if e.is::<crate::agent::inbox::AdmissionRejected>()
                    || e.to_string().contains("conflict")
                    || e.to_string().contains("busy")
                {
                    error(StatusCode::CONFLICT, &e.to_string())
                } else {
                    internal(e)
                },
            );
        }
    };
    attachments.retain_accepted(&record.request.message.files);
    if record.status == crate::agent::inbox::SubmissionStatus::Queued && !runs.contains(&input.id) {
        runs.insert(input.id.clone());
        let worker = chat.clone();
        let id = conversation;
        tokio::spawn(async move {
            worker.work(id, false).await;
        });
    }
    Ok(Json(
        json!({"request_id":record.request.request_id,"submission_id":record.id,
        "run_id":record.run_id,"status":record.status}),
    )
    .into_response())
}

async fn attachment(
    Extension(state): Extension<WebState>,
    Path((id, message, kind, index)): Path<(String, usize, String, usize)>,
) -> Result<Response, Response> {
    use axum::http::header;
    use base64::{Engine, engine::general_purpose::STANDARD};
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    let session = chat.session(&id).await?;
    let Some(crate::agent::message::Message::User(user)) = session.messages().get(message) else {
        return Err(error(StatusCode::NOT_FOUND, "Attachment not found"));
    };
    let (mime, filename, bytes) = match kind.as_str() {
        "files" => {
            let file = user
                .files
                .get(index)
                .ok_or_else(|| error(StatusCode::NOT_FOUND, "Attachment not found"))?;
            (
                file.mime_type.clone(),
                file.filename.clone(),
                file.bytes().await.map_err(internal)?,
            )
        }
        "images" => {
            let image = user
                .images
                .get(index)
                .ok_or_else(|| error(StatusCode::NOT_FOUND, "Attachment not found"))?;
            let (prefix, data) = image
                .data_url
                .split_once(";base64,")
                .ok_or_else(|| internal(anyhow::anyhow!("Invalid stored image")))?;
            let mime = prefix
                .strip_prefix("data:")
                .unwrap_or("application/octet-stream")
                .to_owned();
            let extension = mime.strip_prefix("image/").unwrap_or("bin");
            (
                mime.clone(),
                format!("photo-{}.{}", index + 1, extension),
                STANDARD.decode(data).map_err(|e| internal(e.into()))?,
            )
        }
        _ => return Err(error(StatusCode::NOT_FOUND, "Attachment not found")),
    };
    let encoded: String = filename
        .as_bytes()
        .iter()
        .map(|byte| format!("%{byte:02X}"))
        .collect();
    Ok((
        [
            (header::CONTENT_TYPE, mime),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename*=UTF-8''{encoded}"),
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; sandbox".into(),
            ),
        ],
        bytes,
    )
        .into_response())
}

async fn run_snapshot(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?;
    chat.session(&id).await?;
    Ok(Json(chat.snapshot(&id).await.map_err(internal)?))
}

async fn events(
    Extension(state): Extension<WebState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let chat = state.chat().map_err(IntoResponse::into_response)?.clone();
    chat.session(&id).await?;
    let user_permit = chat
        .streams
        .clone()
        .try_acquire_owned()
        .map_err(|_| error(StatusCode::TOO_MANY_REQUESTS, "Too many event streams"))?;
    let stream_permit = state.streams.clone().try_acquire_owned().map_err(|_| {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Event stream capacity reached",
        )
    })?;
    let mut shutdown = chat.runner.shutdown_receiver();
    // Pull one latest snapshot at a time: a slow browser cannot accumulate a backlog.
    let stream = futures::stream::unfold(
        (state, chat, id, headers, None),
        |(state, chat, id, headers, mut revision)| async move {
            loop {
                match state.authenticated(&headers).await {
                    Ok(true) => {}
                    _ => return None,
                }
                match chat.sessions.view_revision().await {
                    Ok(current) if revision == Some(current) => {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        continue;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        log::warn!("Web revision read failed: {error:#}");
                        return None;
                    }
                }
                match chat.snapshot(&id).await {
                    Ok(snapshot) => {
                        let next = snapshot["revision"].as_i64();
                        if revision != next {
                            revision = next;
                            let event = Event::default().data(snapshot.to_string());
                            return Some((
                                Ok::<_, Infallible>(event),
                                (state, chat, id, headers, revision),
                            ));
                        }
                    }
                    Err(error) => {
                        log::warn!("Web watch failed: {error:#}");
                        return None;
                    }
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        },
    )
    .take_until(async move {
        // Graceful HTTP shutdown must not wait for the browser to disconnect.
        let _ = shutdown.wait_for(|stopped| *stopped).await;
    });
    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(10)))
        .into_response();
    response
        .headers_mut()
        .insert("x-accel-buffering", "no".parse().unwrap());
    response = super::server::hold_permit(response, user_permit);
    response = super::server::hold_permit(response, stream_permit);
    response.extensions_mut().insert(super::server::EventStream);
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_finishes_with_chat_observer_still_connected() {
        let data = tempfile::tempdir().unwrap();
        let chat = super::super::chat_tests::chat(data.path()).await;
        let id = uuid::Uuid::new_v4().to_string();
        chat.sessions
            .save(&Session::new(SessionId::new(&id)))
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let settings = Settings {
            trusted_proxies: Vec::new(),
            listen: listener.local_addr().unwrap(),
            base_url: base.parse().unwrap(),
        };
        let auth = AuthStore::new(data.path());
        auth.set_password("test-password").unwrap();
        let token = auth
            .login("test-password", chrono::Utc::now().timestamp())
            .unwrap()
            .unwrap();
        let cookie = super::super::auth::session_cookie(&settings, &token, false)
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let router =
            super::super::server::router_with_chat(settings, data.path(), Some(chat.clone()));
        let mut server = tokio::spawn(super::super::server::serve_with_shutdown(
            listener,
            router,
            chat.runner.shutdown_receiver(),
        ));
        let mut observer = reqwest::Client::new()
            .get(format!("{base}api/chat/sessions/{id}/events"))
            .header("Cookie", cookie)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let first = observer.chunk().await.unwrap().unwrap();
        assert!(String::from_utf8_lossy(&first).contains("snapshot"));

        chat.runner.suspend();
        let stopped = tokio::time::timeout(Duration::from_secs(2), &mut server).await;
        if stopped.is_err() {
            server.abort();
        }
        stopped
            .expect("graceful shutdown waited for the browser to close its SSE connection")
            .unwrap()
            .unwrap();
        assert!(observer.chunk().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn cancelled_resume_handler_still_drains_saved_queue() {
        let data = tempfile::tempdir().unwrap();
        let chat = super::super::chat_tests::chat(data.path()).await;
        let id = uuid::Uuid::new_v4().to_string();
        chat.sessions
            .save(&Session::new(SessionId::new(&id)))
            .await
            .unwrap();
        let request = crate::agent::inbox::SubmissionRequest::new(
            &SessionId::new(&id),
            crate::gateway::IncomingMessage::new(
                crate::gateway::InterfaceId("web".into()),
                &id,
                "resume saved input",
            ),
            crate::agent::inbox::BusyMode::FollowUp,
        );
        let request_id = request.request_id.clone();
        chat.sessions.submit(request).await.unwrap();
        let db =
            rusqlite::Connection::open(data.path().join("owner-sessions/state.sqlite3")).unwrap();
        db.execute(
            "INSERT INTO inbox_controls VALUES(?1,'Stopped by user')",
            [&id],
        )
        .unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let state = WebState {
            streams: Arc::new(Semaphore::new(64)),
            settings: Settings {
                trusted_proxies: Vec::new(),
                listen: "127.0.0.1:0".parse().unwrap(),
                base_url: "http://127.0.0.1/".parse().unwrap(),
            },
            auth: AuthStore::new(data.path()),
            chat: Some(chat.clone()),
        };
        let handler = tokio::spawn(resume(Extension(state), Path(id.clone())));
        tokio::time::timeout(Duration::from_secs(3), async {
            while chat.runs.try_lock().is_ok() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        handler.abort();
        let _ = handler.await;
        db.execute_batch("COMMIT").unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if chat
                    .sessions
                    .submission(&id, &request_id)
                    .await
                    .unwrap()
                    .is_some_and(|record| {
                        record.status == crate::agent::inbox::SubmissionStatus::Done
                    })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Disconnected resume handler stranded an unblocked queue");
    }

    #[tokio::test]
    async fn cancelled_http_handler_keeps_admitted_files_and_starts_worker() {
        let data = tempfile::tempdir().unwrap();
        let chat = super::super::chat_tests::chat(data.path()).await;
        let id = uuid::Uuid::new_v4().to_string();
        chat.sessions
            .save(&Session::new(SessionId::new(&id)))
            .await
            .unwrap();
        let state = WebState {
            streams: Arc::new(Semaphore::new(64)),
            settings: Settings {
                trusted_proxies: Vec::new(),
                listen: "127.0.0.1:0".parse().unwrap(),
                base_url: "http://127.0.0.1/".parse().unwrap(),
            },
            auth: AuthStore::new(data.path()),
            chat: Some(chat.clone()),
        };
        let request_id = uuid::Uuid::new_v4().to_string();
        let request = Request::builder()
            .header("Content-Type", "multipart/form-data; boundary=chat-test")
            .body(axum::body::Body::from(super::super::chat_tests::multipart(
                &id,
                &request_id,
                "",
                &[("notes.txt", b"keep this file")],
            )))
            .unwrap();
        let db =
            rusqlite::Connection::open(data.path().join("owner-sessions/state.sqlite3")).unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let handler = tokio::spawn(send(Extension(state), request));
        let directory = chat.attachment_directory.join(&id);
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if std::fs::read_dir(&directory).is_ok_and(|mut entries| entries.next().is_some()) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        handler.abort();
        let _ = handler.await;
        db.execute_batch("COMMIT").unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(record) = chat.sessions.submission(&id, &request_id).await.unwrap()
                    && record.status == crate::agent::inbox::SubmissionStatus::Done
                {
                    assert_eq!(
                        record.request.message.files[0].bytes().await.unwrap(),
                        b"keep this file"
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("accepted upload lost its files or worker when the HTTP handler was cancelled");
    }
}
