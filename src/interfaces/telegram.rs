use anyhow::Context;
use futures::{
    StreamExt,
    future::{AbortHandle, Abortable},
    stream::FuturesUnordered,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashMap, time::Duration};
use teloxide::{
    Bot,
    requests::Requester,
    types::{ChatAction, ChatId, Update, UpdateKind},
};
use tokio::sync::mpsc;

use crate::{
    Interface, MessageId,
    agent::{
        FileAttachment, Image,
        file::{MAX_FILE_BYTES, file_type},
        image::MAX_IMAGE_BYTES,
        telegram_group::{Participant, TelegramGroup},
    },
    gateway::{Event, IncomingMessage, InterfaceId},
    i18n::Language,
};

#[path = "telegram_language.rs"]
mod language;
#[path = "telegram_outbound.rs"]
mod outbound;
#[path = "telegram_picker.rs"]
mod picker;

fn login_start(text: &str) -> bool {
    let mut words = text.split_whitespace();
    words
        .next()
        .is_some_and(|word| word.split('@').next() == Some("/start"))
        && words
            .next()
            .is_some_and(|payload| payload.starts_with("login_"))
}

fn user_participant(user: &teloxide::types::User, message_id: i32) -> Participant {
    Participant {
        name: Some(user.full_name()),
        username: user.mention(),
        is_bot: user.is_bot,
        message_id,
    }
}

fn group_sender(message: &teloxide::types::Message, group: &mut TelegramGroup) -> Value {
    let (key, id, profile) = if let Some(chat) = &message.sender_chat {
        (
            "chat_id",
            chat.id.0,
            Participant {
                name: chat.title().map(str::to_owned),
                username: chat.username().map(|name| format!("@{name}")),
                is_bot: false,
                message_id: message.id.0,
            },
        )
    } else if let Some(user) = &message.from {
        (
            "user_id",
            user.id.0 as i64,
            user_participant(user, message.id.0),
        )
    } else {
        return json!({"unknown":true});
    };
    let kind = if key == "user_id" { "user" } else { "chat" };
    group.participants.insert(format!("{kind}:{id}"), profile);
    json!({key:id})
}

// Before admission, dropping a queued download on /stop deliberately rejects it.
// After handoff, only the gateway may acknowledge a persisted message.
struct AdmissionGuard(Option<tokio::sync::oneshot::Sender<()>>);
impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}
type PendingUpdates = (
    mpsc::UnboundedSender<(IncomingUpdate, Option<AdmissionGuard>)>,
    AbortHandle,
);
enum IncomingUpdate {
    Tracked(Box<IncomingUpdate>, AdmissionGuard),
    Regular(Update),
    Guest(Value),
    Poll(Value),
}

impl From<Update> for IncomingUpdate {
    fn from(update: Update) -> Self {
        Self::Regular(update)
    }
}

pub struct TelegramInterface {
    bot: Bot,
    tag: String,
    attachment_dir: std::path::PathBuf,
    space_root: Option<std::path::PathBuf>,
    access_root: Option<std::path::PathBuf>,
    journal: Option<crate::audit::Journal>,
    stream: bool,
    languages: std::sync::Arc<std::sync::RwLock<HashMap<i64, (Language, bool)>>>,
}

impl TelegramInterface {
    fn poll_store(&self) -> anyhow::Result<super::polls::PollStore> {
        let root = self
            .space_root
            .as_ref()
            .context("Poll storage is unavailable")?;
        Ok(super::polls::PollStore::new(root, &self.tag))
    }
    pub fn new(
        tag: impl Into<String>,
        token: impl Into<String>,
        attachment_dir: impl Into<std::path::PathBuf>,
    ) -> Self {
        Self {
            bot: Bot::new(token),
            tag: tag.into(),
            attachment_dir: attachment_dir.into(),
            space_root: None,
            access_root: None,
            journal: None,
            stream: true,
            languages: Default::default(),
        }
    }

    pub(crate) fn with_journal(mut self, journal: crate::audit::Journal) -> Self {
        self.journal = Some(journal);
        self
    }

    pub(crate) fn with_space_root(mut self, root: std::path::PathBuf) -> Self {
        self.space_root = Some(root);
        self
    }

    pub(crate) fn with_access(mut self, root: std::path::PathBuf) -> Self {
        self.access_root = Some(root);
        self
    }

    fn admits_message(&self, message: &teloxide::types::Message) -> bool {
        self.access_root.is_none()
            || message.chat.is_private()
                && message.chat.id.0 > 0
                && message
                    .from
                    .as_ref()
                    .is_some_and(|user| !user.is_bot && user.id.0 == message.chat.id.0 as u64)
                && (message.photo().is_none()
                    && message.document().is_none()
                    && message.text().is_some_and(login_start)
                    || self.allows_recipient(&message.chat.id.to_string()))
    }

    pub(crate) fn with_stream(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }

    // teloxide 0.17 has no rich-message methods or response types yet.
    async fn rich_request(&self, method: &str, mut body: Value) -> anyhow::Result<MessageId> {
        let result = self.api_request(method, body.clone()).await;
        let body = match result {
            Err(error)
                if error
                    .downcast_ref::<super::SendRejection>()
                    .is_some_and(|rejection| {
                        rejection.error_code == 400
                            && rejection.description.contains("RICH_MESSAGE_EMPTY")
                    })
                    && body["rich_message"]["markdown"]
                        .as_str()
                        .is_some_and(|text| !text.trim().is_empty()) =>
            {
                // Nonempty Markdown such as "20." can parse into an empty list.
                body["rich_message"] = json!({"blocks":[{"type":"paragraph","text":body["rich_message"]["markdown"]}]});
                self.api_request(method, body).await?
            }
            result => result?,
        };
        let id = body["result"]["message_id"]
            .as_i64()
            .and_then(|id| i32::try_from(id).ok())
            .context("Telegram response is missing a valid message_id")?;
        Ok(MessageId(id.to_string()))
    }

    async fn api_request(&self, method: &str, body: Value) -> anyhow::Result<Value> {
        let context = crate::audit::Context::current_or(
            &format!("{}:{}", self.id(), body["chat_id"]),
            "delivery",
        );
        let span = crate::audit::Span::start(
            self.journal.as_ref(),
            &context,
            "delivery",
            None,
            method,
            json!({
                "chat_id": body["chat_id"].as_i64(),
                "message_id": body["message_id"].as_i64(),
                "request_bytes": serde_json::to_vec(&body)?.len(),
            }),
        )
        .await?;
        let result = self.api_request_inner(method, body).await;
        let status = match &result {
            Ok(body)
                if matches!(method, "deleteMessage" | "answerCallbackQuery")
                    && body["result"] == true =>
            {
                "sent"
            }
            Ok(body)
                if method == "answerGuestQuery"
                    && body["result"]["inline_message_id"]
                        .as_str()
                        .is_some_and(|id| !id.is_empty()) =>
            {
                "sent"
            }
            Ok(body)
                if body["result"]["message_id"]
                    .as_i64()
                    .is_some_and(|id| i32::try_from(id).is_ok()) =>
            {
                "sent"
            }
            Ok(_) => "unknown",
            Err(error)
                if method == "editMessageText"
                    && error
                        .to_string()
                        .contains("Bad Request: message is not modified") =>
            {
                "unchanged"
            }
            Err(error) if error.downcast_ref::<super::SendRejection>().is_some() => "refused",
            Err(_) => "unknown",
        };
        let message_id = result
            .as_ref()
            .ok()
            .and_then(|body| body["result"]["message_id"].as_i64());
        let rejection = result
            .as_ref()
            .err()
            .and_then(|error| error.downcast_ref::<super::SendRejection>());
        if let Err(error) = span
            .finish(
                status,
                json!({
                    "message_id": message_id,
                    "error_code": rejection.map(|rejection| rejection.error_code),
                    "retry_after": rejection.and_then(|rejection| rejection.retry_after),
                }),
            )
            .await
        {
            // The API result owns delivery/retry semantics. Never turn a confirmed
            // send into a retry because its journal outcome could not be written.
            log::error!(
                "Delivery outcome could not be journaled; further actions blocked: {error:#}"
            );
        }
        result
    }

    async fn api_request_inner(&self, method: &str, body: Value) -> anyhow::Result<Value> {
        let mut url = self.bot.api_url();
        url.set_path(&format!("/bot{}/{method}", self.bot.token()));
        let response = self
            .bot
            .client()
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|error| error.without_url())
            .with_context(|| format!("Telegram {method} request failed"))?;
        let status = response.status();
        let body: Value = response
            .json()
            .await
            .map_err(|error| error.without_url())
            .with_context(|| format!("Telegram {method} returned invalid JSON (HTTP {status})"))?;
        if body["ok"] == false
            && let Some(code) = body["error_code"]
                .as_u64()
                .filter(|code| (400..600).contains(code))
        {
            return Err(super::SendRejection {
                error_code: code as u16,
                description: format!(
                    "Telegram {method} failed (HTTP {status}): {}",
                    body["description"]
                        .as_str()
                        .unwrap_or("unsuccessful API response")
                ),
                retry_after: (code == 429 || code >= 500).then(|| {
                    body["parameters"]["retry_after"]
                        .as_u64()
                        .unwrap_or(60)
                        .max(1)
                }),
            }
            .into());
        }
        anyhow::ensure!(
            status.is_success() && body["ok"] == true,
            "Telegram {method} failed (HTTP {status}): {}",
            body["description"]
                .as_str()
                .unwrap_or("unsuccessful API response")
        );
        Ok(body)
    }

    async fn map_update(
        &self,
        update: Update,
        bot_username: &str,
    ) -> anyhow::Result<Option<Event>> {
        let message = match update.kind {
            UpdateKind::Message(message) => message,
            UpdateKind::CallbackQuery(query) => {
                if self.access_root.is_some()
                    && (!query
                        .regular_message()
                        .is_some_and(|message| message.chat.is_private())
                        || !query
                            .data
                            .as_deref()
                            .is_some_and(|data| data.starts_with("wa:"))
                            && !self.allows_recipient(&query.from.id.0.to_string()))
                {
                    return Ok(None);
                }
                return self.map_callback(query, bot_username).await;
            }
            _ => return Ok(None),
        };

        if !self.admits_message(&message) {
            return Ok(None);
        }
        self.detect_language(
            message.chat.id.0,
            message
                .from
                .as_ref()
                .and_then(|user| user.language_code.as_deref()),
        );

        let photo = message.photo().and_then(|sizes| {
            sizes
                .iter()
                .max_by_key(|photo| u64::from(photo.width) * u64::from(photo.height))
        });
        let document = message
            .document()
            .map(|document| {
                file_type(
                    document.file_name.as_deref().unwrap_or("attachment"),
                    document.mime_type.as_ref().map(|mime| mime.as_ref()),
                )
                .map(|kind| (document, kind))
            })
            .transpose()?;
        let Some(text) = message
            .text()
            .or_else(|| photo.map(|_| message.caption().unwrap_or_default()))
            .or_else(|| {
                document.map(|(_, kind)| {
                    message
                        .caption()
                        .filter(|text| !text.trim().is_empty())
                        .unwrap_or(if kind.starts_with("image/") {
                            ""
                        } else {
                            self.language(&message.chat.id.to_string()).pick(
                                "Analyze the attached file.",
                                "Проанализируй прикреплённый файл.",
                            )
                        })
                })
            })
        else {
            return Ok(None);
        };
        let mut text = text.to_owned();
        if (photo.is_none() && document.is_none() || login_start(&text))
            && let Some(word) = text.split_whitespace().next()
            && let Some((
                command @ ("/new" | "/stop" | "/sessions" | "/session" | "/start" | "/language"),
                target,
            )) = word.split_once('@')
        {
            if !target.eq_ignore_ascii_case(bot_username) {
                return Ok(None);
            }
            text = format!("{command}{}", &text.trim()[word.len()..]);
        }

        let control =
            photo.is_none() && document.is_none() && matches!(text.trim(), "/new" | "/stop");
        let personal_command = login_start(&text)
            || photo.is_none()
                && document.is_none()
                && matches!(
                    text.split_whitespace().next(),
                    Some("/sessions" | "/session")
                );
        let is_group = message.chat.is_group() || message.chat.is_supergroup();
        let mut telegram_group = None;
        if is_group && !control && !personal_command {
            let mut group = TelegramGroup {
                chat_id: message.chat.id.0,
                name: message.chat.title().map(str::to_owned),
                bot_username: format!("@{bot_username}"),
                participants: Default::default(),
            };
            let reply_to = message.reply_to_message().map(|reply| {
                let mut metadata = group_sender(reply, &mut group);
                metadata["message_id"] = json!(reply.id.0);
                if let Some(text) = reply.text().or_else(|| reply.caption()) {
                    metadata["text"] = json!(text);
                }
                metadata
            });
            let mut sender = group_sender(&message, &mut group);
            sender["message_id"] = json!(message.id.0);
            let entities = message
                .entities()
                .or_else(|| message.caption_entities())
                .unwrap_or_default();
            if !entities.is_empty() {
                let mut compact = Vec::with_capacity(entities.len());
                for entity in entities {
                    let mut metadata = json!(entity);
                    if let teloxide::types::MessageEntityKind::TextMention { user } = &entity.kind {
                        group
                            .participants
                            .entry(format!("user:{}", user.id.0))
                            .or_insert_with(|| user_participant(user, 0));
                        metadata.as_object_mut().unwrap().remove("user");
                        metadata["user_id"] = json!(user.id.0);
                    }
                    compact.push(metadata);
                }
                sender["entities"] = json!(compact);
            }
            if let Some(reply_to) = reply_to {
                sender["reply_to"] = reply_to;
            }
            text = format!("[Telegram group sender] {sender}\n{text}");
            telegram_group = Some(group);
        }

        let mut incoming = IncomingMessage::new(self.id(), message.chat.id.to_string(), text);
        incoming.request_id = format!("{}:{}:message:{}", self.id(), message.chat.id, message.id.0);
        incoming.is_group = is_group;
        incoming.telegram_group = telegram_group;
        if let Some(photo) = photo {
            let image = tokio::time::timeout(
                Duration::from_secs(30),
                self.download_photo(photo.file.id.clone()),
            )
            .await
            .context("Telegram photo download timed out")??;
            incoming.images.push(image);
        }
        if let Some((document, kind)) = document {
            let name = document.file_name.as_deref().unwrap_or("attachment");
            let limit = if kind.starts_with("image/") {
                MAX_IMAGE_BYTES
            } else {
                MAX_FILE_BYTES
            };
            anyhow::ensure!(document.file.size != 0, "File is empty");
            anyhow::ensure!(
                document.file.size == u32::MAX || document.file.size as usize <= limit,
                "Attachment exceeds {} bytes (images: 10 MiB; documents: 20 MB)",
                limit
            );
            let bytes = tokio::time::timeout(
                Duration::from_secs(30),
                self.download_bytes(document.file.id.clone(), limit),
            )
            .await
            .context("Telegram attachment download timed out")??;
            if kind.starts_with("image/") {
                let image =
                    tokio::task::spawn_blocking(move || Image::from_bytes(&bytes)).await??;
                incoming.images.push(image);
            } else {
                let directory = match &self.space_root {
                    Some(root) => crate::spaces::telegram(root, &self.tag, message.chat.id.0)
                        .join("attachments"),
                    None => self.attachment_dir.clone(),
                };
                let name = name.to_owned();
                let file = tokio::task::spawn_blocking(move || {
                    FileAttachment::save(&directory, &name, Some(kind), &bytes)
                })
                .await??;
                incoming.files.push(file);
            }
        }
        Ok(Some(Event::Message(incoming)))
    }

    fn map_guest(&self, guest: Value, bot_username: &str) -> anyhow::Result<Option<Event>> {
        if self.access_root.is_some() {
            return Ok(None);
        }
        let chat = guest["chat"]["id"]
            .as_i64()
            .context("Guest chat ID missing")?;
        self.detect_language(chat, guest["from"]["language_code"].as_str());
        let query = guest["guest_query_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .context("Guest query ID missing")?;
        let is_group = matches!(guest["chat"]["type"].as_str(), Some("group" | "supergroup"));
        let mut text = guest["text"]
            .as_str()
            .or_else(|| guest["caption"].as_str())
            .unwrap_or_default()
            .to_owned();
        if text.is_empty() {
            text =
                "[Guest message has no readable text; media is unavailable in guest mode]".into();
        }
        if login_start(&text)
            && let Some(word) = text.split_whitespace().next()
            && let Some((command, target)) = word.split_once('@')
        {
            if !target.eq_ignore_ascii_case(bot_username.trim_start_matches('@')) {
                return Ok(None);
            }
            text = format!("{command}{}", &text.trim()[word.len()..]);
        }
        let mut incoming = IncomingMessage::new(self.id(), chat.to_string(), "");
        incoming.request_id = format!("{}:{chat}:guest:{query}", self.id());
        incoming.guest_query_id = Some(query.to_owned());
        incoming.is_group = is_group;
        if is_group
            && !login_start(&text)
            && !matches!(
                text.split_whitespace().next(),
                Some("/sessions" | "/session")
            )
        {
            let mut group = TelegramGroup {
                chat_id: chat,
                name: guest["chat"]["title"].as_str().map(str::to_owned),
                bot_username: bot_username.to_owned(),
                participants: Default::default(),
            };
            let mut sender = json!({"unknown":true});
            if let Some(id) = guest["sender_chat"]["id"].as_i64() {
                sender = json!({"chat_id":id});
                group.participants.insert(
                    format!("chat:{id}"),
                    Participant {
                        name: guest["sender_chat"]["title"].as_str().map(str::to_owned),
                        username: guest["sender_chat"]["username"]
                            .as_str()
                            .map(|u| format!("@{u}")),
                        is_bot: false,
                        message_id: guest["message_id"].as_i64().unwrap_or(0) as i32,
                    },
                );
            } else if let Some(user) = guest["from"].as_object()
                && let Some(id) = user.get("id").and_then(Value::as_i64)
            {
                sender = json!({"user_id":id});
                group.participants.insert(
                    format!("user:{id}"),
                    Participant {
                        name: user
                            .get("first_name")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        username: user
                            .get("username")
                            .and_then(Value::as_str)
                            .map(|u| format!("@{u}")),
                        is_bot: user.get("is_bot").and_then(Value::as_bool).unwrap_or(false),
                        message_id: guest["message_id"].as_i64().unwrap_or(0) as i32,
                    },
                );
            }
            sender["message_id"] = guest["message_id"].clone();
            if !guest["reply_to_message"].is_null() {
                sender["reply_to"] = json!({"text": guest["reply_to_message"]["text"]});
            }
            text = format!("[Telegram group sender] {sender}\n{text}");
            incoming.telegram_group = Some(group);
        }
        incoming.text = text;
        Ok(Some(Event::Message(incoming)))
    }

    async fn process_updates<U: Into<IncomingUpdate>>(
        &self,
        stream: impl futures::Stream<Item = Result<U, teloxide::RequestError>>,
        tx: mpsc::Sender<Event>,
        bot_username: &str,
    ) -> anyhow::Result<()> {
        tokio::pin!(stream);
        let mut chats: HashMap<(ChatId, bool), PendingUpdates> = HashMap::new();
        let mut workers = FuturesUnordered::new();
        let mut receiving = true;
        while receiving || !workers.is_empty() {
            tokio::select! {
                result = workers.next(), if !workers.is_empty() => {
                    if let Some(Ok(result)) = result { result?; }
                }
                update = stream.next(), if receiving => {
                    let update = match update {
                        Some(Ok(update)) => update.into(),
                        Some(Err(error)) => {
                            log::warn!("Telegram polling error: {error}");
                            continue;
                        }
                        None => {
                            receiving = false;
                            chats.clear(); // Close queues and drain admitted updates.
                            continue;
                        }
                    };
                    let (update,receipt)=match update {IncomingUpdate::Tracked(update,receipt)=>(*update,Some(receipt)),update=>(update,None)};
                    if let IncomingUpdate::Poll(raw) = &update {
                        let result = async {
                            let store = self.poll_store()?;
                            let raw = raw.clone();
                            tokio::task::spawn_blocking(move || store.update(&raw)).await?
                        }.await;
                        if let Err(error) = result { log::warn!("Could not save Telegram poll update: {error:#}"); }
                        continue;
                    }
                    if matches!(&update, IncomingUpdate::Regular(Update { kind: UpdateKind::CallbackQuery(_), .. })) {
                        self.forward_update(update, &tx, bot_username, receipt).await?;
                        continue;
                    }
                    let message = match &update {
                        IncomingUpdate::Regular(Update { kind: UpdateKind::Message(message), .. }) => Some(message),
                        IncomingUpdate::Guest(_) => None,
                        _ => continue,
                    };
                    if self.access_root.is_some() && message.is_none_or(|message| !self.admits_message(message)) {
                        continue;
                    }
                    let chat = if let Some(message) = message { message.chat.id } else {
                        let IncomingUpdate::Guest(guest) = &update else { unreachable!() };
                        let Some(id) = guest["chat"]["id"].as_i64() else { continue };
                        ChatId(id)
                    };
                    if let Some(message) = message {
                        self.detect_language(chat.0, message.from.as_ref().and_then(|user| user.language_code.as_deref()));
                    } else if let IncomingUpdate::Guest(guest) = &update {
                        self.detect_language(chat.0, guest["from"]["language_code"].as_str());
                    }
                    let text = message.and_then(|m| m.text()).or_else(|| match &update { IncomingUpdate::Guest(guest) => guest["text"].as_str(), _ => None }).unwrap_or_default().trim();
                    let language_text = if text.is_empty() { message.and_then(|message| message.caption()).or_else(|| match &update { IncomingUpdate::Guest(guest) => guest["caption"].as_str(), _ => None }).unwrap_or_default().trim() } else { text };
                    let word = language_text.split_whitespace().next().unwrap_or_default();
                    if word.split('@').next() == Some("/language") {
                        if word.split_once('@').is_some_and(|(_, target)| !target.eq_ignore_ascii_case(bot_username)) { continue; }
                        // Local commands never enter the agent queue, including invalid arguments.
                        let guest = match &update { IncomingUpdate::Guest(guest) => {
                            let Some(query) = guest["guest_query_id"].as_str().filter(|query| !query.is_empty()) else { continue; };
                            Some(query)
                        }, _ => None };
                        if let Err(error) = tokio::time::timeout(Duration::from_secs(15), self.language_command(chat.0, &language_text[word.len()..], guest)).await.map_err(anyhow::Error::from).and_then(|result| result) {
                            log::warn!("Telegram language reply failed: {error:#}");
                        }
                        continue;
                    }
                    let command = match text.split_once('@') {
                        Some((command, target)) if target.eq_ignore_ascii_case(bot_username) => command,
                        Some(("/chatid", _)) if message.is_some() => continue,
                        _ => text,
                    };
                    if message.is_some() && command == "/chatid" {
                        let result = tokio::time::timeout(Duration::from_secs(15), self.api_request(
                            "sendMessage", json!({"chat_id":chat.0,"text":format!("{}: {}",self.language(&chat.0.to_string()).pick("Chat ID", "ID чата"), chat.0)}),
                        )).await;
                        if let Err(error) = result.map_err(anyhow::Error::from).and_then(|result| result) {
                            log::warn!("Telegram chat ID reply failed: {error:#}");
                        }
                        continue;
                    }
                    let key = (chat, message.is_none());
                    if message.is_some() && matches!(command, "/stop" | "/new") {
                        if let Some((_, abort)) = chats.remove(&key) {
                            abort.abort(); // Drop this chat's download and queued pre-command messages.
                        }
                        self.forward_update(update, &tx, bot_username,receipt).await?;
                        continue;
                    }
                    let (queue, _) = chats.entry(key).or_insert_with(|| {
                        // ponytail: queue update metadata without backpressure so control messages
                        // remain reachable; add per-chat admission limits if sustained floods matter.
                        let (queue, mut pending) = mpsc::unbounded_channel();
                        let (abort, registration) = AbortHandle::new_pair();
                        let tx = tx.clone();
                        workers.push(Abortable::new(async move {
                            while let Some((update,receipt)) = pending.recv().await {
                                self.forward_update(update, &tx, bot_username,receipt).await?;
                            }
                            Ok::<_, anyhow::Error>(())
                        }, registration));
                        (queue, abort)
                    });
                    queue.send((update,receipt)).map_err(|_|anyhow::anyhow!("Telegram chat worker stopped"))?;
                }
            }
        }
        Ok(())
    }

    async fn forward_update(
        &self,
        update: IncomingUpdate,
        tx: &mpsc::Sender<Event>,
        bot_username: &str,
        mut receipt: Option<AdmissionGuard>,
    ) -> anyhow::Result<()> {
        let regular = matches!(
            update,
            IncomingUpdate::Regular(Update {
                kind: UpdateKind::Message(_),
                ..
            })
        );
        let chat = match &update {
            IncomingUpdate::Regular(Update {
                kind: UpdateKind::Message(message),
                ..
            }) => message.chat.id,
            IncomingUpdate::Regular(Update {
                kind: UpdateKind::CallbackQuery(query),
                ..
            }) => ChatId(query.from.id.0.try_into()?),
            IncomingUpdate::Guest(guest) => ChatId(
                guest["chat"]["id"]
                    .as_i64()
                    .context("Guest chat ID missing")?,
            ),
            _ => return Ok(()),
        };
        let result = match update {
            IncomingUpdate::Regular(update) => self.map_update(update, bot_username).await,
            IncomingUpdate::Guest(guest) => self.map_guest(guest, bot_username),
            IncomingUpdate::Poll(_) | IncomingUpdate::Tracked(..) => Ok(None),
        };
        match result {
            Ok(Some(Event::Message(mut message))) => {
                if let Some(sender) = receipt.as_mut().and_then(|r| r.0.take()) {
                    message.admission =
                        Some(std::sync::Arc::new(std::sync::Mutex::new(Some(sender))));
                }
                tx.send(Event::Message(message)).await?;
            }
            Ok(None) => {}
            Err(error) if regular => {
                log::warn!("Telegram attachment rejected: {error:#}");
                let _ = tokio::time::timeout(
                    Duration::from_secs(30),
                    self.bot
                        .send_message(chat, self.language(&chat.0.to_string()).pick("Could not read the attachment. Check its format and size, then try again.", "Не удалось прочитать вложение. Проверьте формат и размер и попробуйте ещё раз.")),
                )
                .await;
            }
            Err(error) => log::warn!("Telegram update rejected: {error:#}"),
        }
        Ok(())
    }

    async fn download_photo(&self, id: teloxide::types::FileId) -> anyhow::Result<Image> {
        let bytes = self.download_bytes(id, MAX_IMAGE_BYTES).await?;
        tokio::task::spawn_blocking(move || Image::from_bytes(&bytes)).await?
    }

    async fn download_bytes(
        &self,
        id: teloxide::types::FileId,
        limit: usize,
    ) -> anyhow::Result<Vec<u8>> {
        // getFile errors can contain the token-bearing URL; do not propagate them.
        let file = self
            .bot
            .get_file(id)
            .await
            .map_err(|_| anyhow::anyhow!("Telegram getFile failed"))?;
        anyhow::ensure!(
            file.size == u32::MAX || file.size as usize <= limit,
            "Attachment exceeds {limit} bytes"
        );
        let mut url = self.bot.api_url();
        url.set_path(&format!("/file/bot{}/{}", self.bot.token(), file.path));
        let mut response = self
            .bot
            .client()
            .get(url)
            .send()
            .await
            .map_err(|error| error.without_url())
            .context("Telegram attachment request failed")?
            .error_for_status()
            .map_err(|error| error.without_url())?;
        anyhow::ensure!(
            response
                .content_length()
                .is_none_or(|size| size <= limit as u64),
            "Attachment exceeds {limit} bytes"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| error.without_url())?
        {
            anyhow::ensure!(
                bytes.len() + chunk.len() <= limit,
                "Attachment exceeds {limit} bytes"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

#[async_trait::async_trait]
impl Interface for TelegramInterface {
    fn language(&self, conversation_id: &str) -> Language {
        self.chat_language(conversation_id)
    }
    async fn read_results(
        &self,
        conversation_id: &str,
        kind: &str,
        id: Option<&str>,
        offset: usize,
    ) -> anyhow::Result<Value> {
        anyhow::ensure!(kind == "poll", "Unknown Telegram result kind");
        anyhow::ensure!(
            offset == 0 || id.is_some(),
            "Vote offset requires a poll ID"
        );
        anyhow::ensure!(
            self.allows_recipient(conversation_id),
            "Invalid Telegram chat ID"
        );
        let store = self.poll_store()?;
        let conversation = conversation_id.to_owned();
        let id = id.map(str::to_owned);
        tokio::task::spawn_blocking(move || store.read(conversation, id.as_deref(), offset)).await?
    }
    fn stream_replies(&self) -> bool {
        self.stream
    }
    fn id(&self) -> InterfaceId {
        InterfaceId(format!("telegram:{}", self.tag))
    }

    fn allows_recipient(&self, conversation_id: &str) -> bool {
        conversation_id.parse::<i64>().is_ok_and(|chat| {
            chat != 0
                && self.access_root.as_ref().is_none_or(|root| {
                    chat > 0
                        && match crate::web::auth::AuthStore::new(root).allowed_user(chat as u64) {
                            Ok(allowed) => allowed,
                            Err(error) => {
                                log::warn!("Telegram access check failed: {error:#}");
                                false
                            }
                        }
                })
        })
    }

    async fn run(&self, tx: mpsc::Sender<Event>) -> anyhow::Result<()> {
        let me = self.bot.get_me().await?;
        if let Some(root) = &self.space_root {
            let auth = crate::web::auth::AuthStore::new(root);
            let tag = self.tag.clone();
            let username = me.username().to_owned();
            if let Err(error) =
                tokio::task::spawn_blocking(move || auth.register_telegram_bot(&tag, &username))
                    .await?
            {
                log::warn!("Telegram web login unavailable: {error:#}");
            }
        }
        for language in [None, Some(Language::En), Some(Language::Ru)] {
            let mut body =
                json!({"commands": language::commands(language.unwrap_or(Language::En))});
            if let Some(language) = language {
                body["language_code"] = json!(language.code());
            }
            self.api_request_inner("setMyCommands", body).await?;
        }
        self.api_request_inner("deleteWebhook", json!({})).await?;
        let (updates, rx) = mpsc::channel(100);
        let processing = self.process_updates(
            futures::stream::unfold(rx, |mut rx| async {
                rx.recv().await.map(|update| (Ok(update), rx))
            }),
            tx,
            me.username(),
        );
        let polling = async {
            let mut offset = 0_u64;
            loop {
                let result = self
                    .api_request_inner(
                        "getUpdates",
                        json!({
                            "offset": offset, "timeout": 10,
                            "allowed_updates": ["message", "guest_message", "poll", "poll_answer", "callback_query"]
                        }),
                    )
                    .await;
                match result {
                    Ok(response) => {
                        let mut receipts = Vec::new();
                        let mut next_offset = offset;
                        for raw in response["result"]
                            .as_array()
                            .context("Invalid getUpdates result")?
                        {
                            let id = raw["update_id"].as_u64().context("Missing update ID")?;
                            next_offset = id + 1;
                            let update = if let Some(guest) = raw.get("guest_message") {
                                IncomingUpdate::Guest(guest.clone())
                            } else if raw.get("poll").is_some() || raw.get("poll_answer").is_some()
                            {
                                IncomingUpdate::Poll(raw.clone())
                            } else {
                                // teloxide's flattened update decoder needs borrowed JSON keys.
                                IncomingUpdate::Regular(Update::deserialize(raw)?)
                            };
                            let (admitted, receipt) = tokio::sync::oneshot::channel();
                            updates
                                .send(IncomingUpdate::Tracked(
                                    Box::new(update),
                                    AdmissionGuard(Some(admitted)),
                                ))
                                .await?;
                            receipts.push(receipt);
                        }
                        for receipt in receipts {
                            receipt
                                .await
                                .context("Telegram input was not durably admitted")?;
                        }
                        offset = next_offset;
                    }
                    Err(error) => {
                        log::warn!("Telegram polling error: {error:#}");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
            #[allow(unreachable_code)]
            Ok::<_, anyhow::Error>(())
        };
        tokio::try_join!(processing, polling)?;
        Ok(())
    }

    async fn answer_guest(&self, query_id: &str, message: &str) -> anyhow::Result<MessageId> {
        anyhow::ensure!(
            self.access_root.is_none(),
            "Telegram guest access is disabled"
        );
        let body = self
            .api_request(
                "answerGuestQuery",
                json!({
                    "guest_query_id": query_id,
                    "result": {
                        "type": "article", "id": "kodrik", "title": "Kodrik",
                        "input_message_content": {"message_text": message}
                    }
                }),
            )
            .await?;
        Ok(MessageId(
            body["result"]["inline_message_id"]
                .as_str()
                .context("Missing guest inline message ID")?
                .to_owned(),
        ))
    }

    async fn send_content(
        &self,
        conversation_id: &str,
        message: &super::OutgoingMessage,
    ) -> anyhow::Result<MessageId> {
        anyhow::ensure!(
            self.allows_recipient(conversation_id),
            "Telegram access denied"
        );
        self.send_outgoing_message(conversation_id, message).await
    }

    async fn answer_guest_content(
        &self,
        query_id: &str,
        message: &super::OutgoingMessage,
    ) -> anyhow::Result<MessageId> {
        anyhow::ensure!(
            self.access_root.is_none(),
            "Telegram guest access is disabled"
        );
        self.answer_guest_outgoing_message(query_id, message).await
    }

    async fn send(&self, conversation_id: &str, message: &str) -> anyhow::Result<MessageId> {
        anyhow::ensure!(
            self.allows_recipient(conversation_id),
            "Telegram access denied"
        );
        self.rich_request(
            "sendRichMessage",
            json!({
                "chat_id": conversation_id.parse::<i64>()?,
                "rich_message": { "markdown": message },
            }),
        )
        .await
    }

    async fn send_buttons(
        &self,
        conversation_id: &str,
        text: &str,
        buttons: &[Vec<super::InlineButton>],
    ) -> anyhow::Result<MessageId> {
        self.send_picker(conversation_id, None, text, buttons).await
    }

    async fn edit_buttons(
        &self,
        conversation_id: &str,
        id: &MessageId,
        text: &str,
        buttons: &[Vec<super::InlineButton>],
    ) -> anyhow::Result<()> {
        self.send_picker(conversation_id, Some(id), text, buttons)
            .await?;
        Ok(())
    }

    async fn answer_callback(&self, id: &str, text: &str) -> anyhow::Result<()> {
        let result = self
            .api_request(
                "answerCallbackQuery",
                json!({"callback_query_id":id,"text":text}),
            )
            .await?;
        anyhow::ensure!(
            result["result"] == true,
            "Telegram callback was not acknowledged"
        );
        Ok(())
    }

    async fn send_status(&self, conversation_id: &str, message: &str) -> anyhow::Result<MessageId> {
        anyhow::ensure!(
            self.allows_recipient(conversation_id),
            "Telegram access denied"
        );
        self.rich_request(
            "sendRichMessage",
            json!({
                "chat_id": conversation_id.parse::<i64>()?,
                "rich_message": { "markdown": message },
                "disable_notification": true,
            }),
        )
        .await
    }

    async fn edit(
        &self,
        conversation_id: &str,
        id: &MessageId,
        message: &str,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.allows_recipient(conversation_id),
            "Telegram access denied"
        );
        let result = self
            .rich_request(
                "editMessageText",
                json!({
                    "chat_id": conversation_id.parse::<i64>()?,
                    "message_id": id.0.parse::<i32>()?,
                    "rich_message": { "markdown": message },
                }),
            )
            .await;

        if let Err(error) = result
            && !error
                .to_string()
                .contains("Bad Request: message is not modified")
        {
            return Err(error);
        }

        Ok(())
    }

    async fn delete(&self, conversation_id: &str, id: &MessageId) -> anyhow::Result<()> {
        let body = self
            .api_request(
                "deleteMessage",
                json!({
                    "chat_id": conversation_id.parse::<i64>()?, "message_id": id.0.parse::<i32>()?,
                }),
            )
            .await?;
        anyhow::ensure!(
            body["result"] == true,
            "Telegram deleteMessage did not confirm deletion"
        );

        Ok(())
    }

    async fn typing(&self, conversation_id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.allows_recipient(conversation_id),
            "Telegram access denied"
        );
        self.bot
            .send_chat_action(ChatId(conversation_id.parse()?), ChatAction::Typing)
            .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[tokio::test]
    async fn invite_access_blocks_delivery_but_allows_login_prompts() {
        let root = tempfile::tempdir().unwrap();
        let (interface, request) =
            server("200 OK", json!({"ok":true,"result":{"message_id":1}})).await;
        let interface = interface.with_access(root.path().into());
        assert!(interface.answer_guest("guest", "output").await.is_err());
        assert!(interface.send("7", "assistant output").await.is_err());
        assert!(interface.send_status("7", "status").await.is_err());
        assert!(
            interface
                .edit("7", &MessageId("1".into()), "output")
                .await
                .is_err()
        );
        interface
            .send_buttons("7", "Confirm login", &[])
            .await
            .unwrap();
        request.await.unwrap();
    }

    #[tokio::test]
    async fn invite_access_blocks_attachments_and_groups_but_preserves_private_login() {
        let root = tempfile::tempdir().unwrap();
        let auth = crate::web::auth::AuthStore::new(root.path());
        auth.configure_owner(Some(42)).unwrap();
        let mut interface =
            TelegramInterface::new("test", "123:test", root.path().join("attachments"))
                .with_access(root.path().into());
        interface.bot = interface
            .bot
            .set_api_url("http://127.0.0.1:1".parse().unwrap());
        let from = json!({"id":7,"is_bot":false,"first_name":"Guest"});
        for content in [
            json!({"text":"hello"}),
            json!({"caption":"/start login_request","photo":[{"file_id":"photo","file_unique_id":"p","width":1,"height":1}]}),
            json!({"document":{"file_id":"file","file_unique_id":"f","file_name":"file.txt","file_size":1}}),
        ] {
            let mut message =
                json!({"message_id":1,"date":1,"chat":{"id":7,"type":"private"},"from":from});
            message
                .as_object_mut()
                .unwrap()
                .extend(content.as_object().unwrap().clone());
            let update = Update::deserialize(&json!({"update_id":1,"message":message})).unwrap();
            assert!(
                interface
                    .map_update(update, "TestBot")
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        for data in ["cs:n", "wa:y:request"] {
            let event = interface
                .map_update(picker_callback(7, 7, data), "TestBot")
                .await
                .unwrap();
            assert_eq!(event.is_some(), data.starts_with("wa:"));
        }
        for (chat, kind, allowed) in [(7, "private", true), (-7, "group", false)] {
            let update = Update::deserialize(&json!({"update_id":1,"message":{"message_id":1,"date":1,"chat":{"id":chat,"type":kind},"from":from,"text":"/start@TestBot login_request"}})).unwrap();
            assert_eq!(
                interface
                    .map_update(update, "TestBot")
                    .await
                    .unwrap()
                    .is_some(),
                allowed
            );
        }
        assert!(interface.map_guest(json!({}), "TestBot").unwrap().is_none());
        assert!(!root.path().join("attachments").exists());
        assert!(!root.path().join("spaces").exists());
        assert!(interface.allows_recipient("42"));
        assert!(!interface.allows_recipient("7"));
        assert!(!interface.allows_recipient("-42"));
        auth.grant_test_user(7).unwrap();
        assert!(interface.allows_recipient("7"));
        auth.revoke_user(7).unwrap();
        assert!(!interface.allows_recipient("7"));
    }

    #[tokio::test]
    async fn telegram_login_start_keeps_group_context_and_normalizes_guest_targets() {
        let interface = TelegramInterface::new("test", "123:test", "");
        for (text, personal) in [
            ("/start", false),
            ("/start something", false),
            ("/start@TestBot login_invalid", true),
        ] {
            let message: teloxide::types::Message =
                serde_json::from_value(json!({"message_id":1,"date":1,
                "chat":{"id":-42,"type":"group","title":"Group"},"text":text}))
                .unwrap();
            let Some(Event::Message(message)) = interface
                .map_update(
                    Update {
                        id: teloxide::types::UpdateId(1),
                        kind: UpdateKind::Message(message),
                    },
                    "TestBot",
                )
                .await
                .unwrap()
            else {
                panic!()
            };
            assert_eq!(message.telegram_group.is_none(), personal);
            if personal {
                assert_eq!(message.text, "/start login_invalid");
            }
        }
        for target in ["TestBot", "OtherBot"] {
            let event = interface.map_guest(json!({"guest_query_id":"guest","chat":{"id":-42,"type":"group"},"text":format!("/start@{target} login_invalid")}),"TestBot").unwrap();
            if target == "OtherBot" {
                assert!(event.is_none());
            } else {
                let Some(Event::Message(message)) = event else {
                    panic!()
                };
                assert_eq!(message.text, "/start login_invalid");
                assert!(message.guest_query_id.is_some());
            }
        }
    }

    pub(super) fn picker_callback(chat: i64, sender: u64, data: &str) -> Update {
        let raw = json!({
            "update_id":10,
            "callback_query":{
                "id":"picker-query", "from":{"id":sender,"is_bot":false,"first_name":"Ada"},
                "chat_instance":"chat", "data":data,
                "message":{"message_id":20,"date":1,
                    "from":{"id":123,"is_bot":true,"first_name":"Bot","username":"TestBot"},
                    "chat":{"id":chat,"type":if chat > 0 {"private"} else {"group"}},
                    "text":"Диалоги"
                }
            }
        });
        Update::deserialize(&raw).unwrap()
    }

    #[tokio::test]
    async fn session_picker_callback_is_forwarded_without_model_text() {
        let interface = TelegramInterface::new("test", "token", "");
        let Some(Event::Message(message)) = interface
            .map_update(picker_callback(42, 42, "cs:p:1"), "TestBot")
            .await
            .unwrap()
        else {
            panic!("picker callback was dropped");
        };
        assert_eq!(message.source.conversation_id, "42");
        assert!(message.text.is_empty());
        assert!(message.files.is_empty() && message.images.is_empty());
    }

    #[tokio::test]
    async fn failed_poll_storage_does_not_block_private_commands() {
        let root = tempfile::tempdir().unwrap();
        let poll = json!({"id":"p1","question":"Choose?","options":[{"text":"A","voter_count":0}],"total_voter_count":0,"is_anonymous":true,"is_closed":false});
        let store = super::super::polls::PollStore::new(root.path(), "test");
        store.register(42, 7, poll.clone()).unwrap();
        let path = crate::spaces::telegram_interface(root.path(), "test").join("polls/p1.json");
        std::fs::write(&path, "broken").unwrap();
        let interface =
            TelegramInterface::new("test", "token", "").with_space_root(root.path().into());
        let (tx, mut rx) = mpsc::channel(4);
        interface
            .process_updates(
                futures::stream::iter([
                    Ok::<_, teloxide::RequestError>(IncomingUpdate::Poll(
                        json!({"update_id":1,"poll":poll}),
                    )),
                    Ok(IncomingUpdate::Poll(json!({"update_id":2,"poll":{}}))),
                    Ok(text_update(42, "/new").into()),
                ]),
                tx,
                "TestBot",
            )
            .await
            .unwrap();
        let Some(Event::Message(message)) = rx.recv().await else {
            panic!("missing private command")
        };
        assert_eq!(message.text, "/new");
        assert!(rx.recv().await.is_none());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "broken");
    }

    #[tokio::test]
    async fn polling_preserves_private_commands_alongside_guest_updates() {
        let root = tempfile::tempdir().unwrap();
        let polls = super::super::polls::PollStore::new(root.path(), "test");
        polls.register(42,7,json!({"id":"p1","question":"Choose?","options":[{"text":"A","voter_count":0}],"total_voter_count":0,"is_anonymous":false,"is_closed":false})).unwrap();
        let router = axum::Router::new().fallback(axum::routing::post(
            move |uri: axum::http::Uri, axum::Json(body): axum::Json<Value>| {
                async move {
                if uri.path().to_ascii_lowercase().ends_with("/getupdates") {
                    assert_eq!(body["allowed_updates"], json!(["message", "guest_message", "poll", "poll_answer", "callback_query"]));
                }
                let result = match uri.path().rsplit('/').next().unwrap().to_ascii_lowercase().as_str() {
                    "getme" => json!({"id":123,"is_bot":true,"first_name":"Test","username":"TestBot","can_join_groups":true,"can_read_all_group_messages":false,"supports_inline_queries":false,"has_main_web_app":false}),
                    "getupdates" => json!([
                        {"update_id":1,"guest_message":{
                            "guest_query_id":"guest-1","chat":{"id":-42,"type":"group"},"text":"hello"
                        }},
                        {"update_id":3,"poll":{"id":"p1","question":"Choose?","options":[{"text":"A","persistent_id":"option-stable","voter_count":1}],"total_voter_count":1,"is_anonymous":false,"is_closed":false}},
                        {"update_id":4,"poll_answer":{"poll_id":"p1","user":{"id":99,"is_bot":false,"first_name":"Ada"},"option_ids":[0],"option_persistent_ids":["option-stable"]}},
                        {"update_id":5,"message":{
                            "message_id":2,"date":0,
                            "chat":{"id":42,"type":"private","first_name":"Test"},"text":"/new"
                        }}
                    ]),
                    _ => json!(true),
                };
                axum::Json(json!({"ok":true,"result":result}))
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut interface =
            TelegramInterface::new("test", "123:test", "").with_space_root(root.path().into());
        interface.bot = interface.bot.set_api_url(endpoint.parse().unwrap());
        let (tx, mut events) = mpsc::channel(10);
        let worker = tokio::spawn(async move { interface.run(tx).await });
        let received = tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(Event::Message(message)) = events.recv().await {
                message.acknowledge();
                if message.guest_query_id.is_none() {
                    return message;
                }
            }
            panic!("polling stopped without a private command");
        })
        .await;
        worker.abort();
        server.abort();
        let message = received.expect("private /new was silently discarded");
        assert_eq!(message.source.conversation_id, "42");
        assert_eq!(message.text, "/new");
        let results = polls.read(42, Some("p1"), 0).unwrap();
        assert_eq!(results["poll"]["total_voter_count"], 1);
        assert_eq!(
            results["poll"]["options"][0]["persistent_id"],
            "option-stable"
        );
        assert_eq!(
            results["votes"][0]["option_persistent_ids"],
            json!(["option-stable"])
        );
    }

    #[tokio::test]
    async fn guest_updates_keep_chat_context_and_do_not_send_regular_messages() {
        let interface = TelegramInterface::new("test", "123:test", std::path::PathBuf::new());
        let (tx, mut events) = mpsc::channel(2);
        interface
            .process_updates(
                futures::stream::iter([Ok::<_, teloxide::RequestError>(IncomingUpdate::Guest(
                    json!({
                        "message_id": 8, "guest_query_id": "query-1",
                        "chat": {"id": -42, "type": "group", "title": "Other group"},
                        "from": {"id": 19, "first_name": "Ada"},
                        "text": "@TestBot hello"
                    }),
                ))]),
                tx,
                "TestBot",
            )
            .await
            .unwrap();
        let Some(Event::Message(message)) = events.recv().await else {
            panic!("guest missing")
        };
        assert_eq!(message.source.conversation_id, "-42");
        assert_eq!(message.guest_query_id.as_deref(), Some("query-1"));
        assert!(message.is_group);
        assert!(message.text.contains("@TestBot hello"));
        assert!(
            message
                .telegram_group
                .unwrap()
                .participants
                .contains_key("user:19")
        );
    }
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn compact_group_metadata_does_not_repeat_profiles_or_empty_fields() {
        let interface = TelegramInterface::new("test", "token", "/unused");
        let message = serde_json::from_value(json!({
            "message_id":9,"date":0,
            "chat":{"id":-42,"type":"supergroup","title":"Test"},
            "from":{"id":11,"is_bot":false,"first_name":"Alice","username":"alice"},
            "text":"Hello"
        }))
        .unwrap();
        let update = Update {
            id: teloxide::types::UpdateId(1),
            kind: UpdateKind::Message(message),
        };
        let Some(Event::Message(message)) = interface.map_update(update, "TestBot").await.unwrap()
        else {
            panic!("group message missing")
        };
        let (metadata, body) = message.text.split_once('\n').unwrap();
        let metadata: Value =
            serde_json::from_str(metadata.strip_prefix("[Telegram group sender] ").unwrap())
                .unwrap();
        assert_eq!(metadata, json!({"user_id":11,"message_id":9}));
        assert_eq!(body, "Hello");
        let group = serde_json::to_value(message.telegram_group.unwrap()).unwrap();
        assert_eq!(group["chat_id"], -42);
        assert_eq!(group["name"], "Test");
        assert_eq!(group["bot_username"], "@TestBot");
        assert_eq!(group["participants"]["user:11"]["name"], "Alice");
        assert_eq!(group["participants"]["user:11"]["username"], "@alice");
    }

    #[tokio::test]
    async fn group_reply_metadata_identifies_bot_and_other_participant() {
        let interface = TelegramInterface::new("test", "token", "/unused");
        let message = serde_json::from_value(json!({
            "message_id":9,"date":0,
            "chat":{"id":-42,"type":"supergroup","title":"Test"},
            "from":{"id":1,"is_bot":false,"first_name":"Alice"},
            "text":"😀 @Bob посмотри",
            "entities":[{"type":"mention","offset":3,"length":4}],
            "reply_to_message":{
                "message_id":8,"date":0,
                "chat":{"id":-42,"type":"supergroup","title":"Test"},
                "from":{"id":2,"is_bot":false,"first_name":"Bob","username":"Bob"},
                "text":"Вопрос к Alice"
            }
        }))
        .unwrap();
        let update = Update {
            id: teloxide::types::UpdateId(1),
            kind: UpdateKind::Message(message),
        };
        let Some(Event::Message(message)) = interface.map_update(update, "TestBot").await.unwrap()
        else {
            panic!()
        };
        let (metadata, text) = message.text.split_once('\n').unwrap();
        let metadata: Value =
            serde_json::from_str(metadata.strip_prefix("[Telegram group sender] ").unwrap())
                .unwrap();
        assert!(metadata.get("bot_username").is_none());
        assert_eq!(metadata["reply_to"]["user_id"], 2);
        assert!(metadata["reply_to"].get("from").is_none());
        assert_eq!(metadata["reply_to"]["text"], "Вопрос к Alice");
        assert_eq!(metadata["entities"][0]["offset"], 3);
        assert_eq!(text, "😀 @Bob посмотри");
        assert!(message.is_group);
        let group = serde_json::to_value(message.telegram_group.unwrap()).unwrap();
        assert_eq!(group["participants"]["user:2"]["username"], "@Bob");
    }

    fn photo_update(chat: i64, caption: Option<&str>) -> Update {
        let update: Update = serde_json::from_str(
            &json!({"update_id":1,"message":{
                "message_id":1,"date":0,
                "chat":{"id":chat,"type":"private","first_name":"Test"},
                "caption":caption,
                "photo":[
                    {"file_id":"large","file_unique_id":"l","width":2,"height":3},
                    {"file_id":"small","file_unique_id":"s","width":1,"height":1}
                ]
            }})
            .to_string(),
        )
        .unwrap();
        assert!(
            matches!(&update.kind, UpdateKind::Message(message) if message.photo().is_some()),
            "{update:?}"
        );
        update
    }

    #[tokio::test]
    async fn telegram_inbox_waits_for_durable_admission_and_preserves_identity() {
        let dir = tempfile::tempdir().unwrap();
        let interface = TelegramInterface::new("test", "test-secret", dir.path());
        let (accepted, mut receipt) = tokio::sync::oneshot::channel();
        let update = IncomingUpdate::Tracked(
            Box::new(text_update(42, "work").into()),
            AdmissionGuard(Some(accepted)),
        );
        let (tx, mut rx) = mpsc::channel(2);
        interface
            .process_updates(futures::stream::iter(vec![Ok(update)]), tx, "TestBot")
            .await
            .unwrap();
        let Event::Message(message) = rx.recv().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut receipt)
                .await
                .is_err()
        );
        message.acknowledge();
        receipt.await.unwrap();
        let Some(Event::Message(again)) = interface
            .map_update(text_update(42, "work"), "TestBot")
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(message.request_id, again.request_id);
        let Some(Event::Message(other)) = interface
            .map_update(text_update(43, "work"), "TestBot")
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_ne!(message.request_id, other.request_id);
    }

    pub(super) fn text_update(chat: i64, text: &str) -> Update {
        serde_json::from_str(
            &json!({"update_id":2,"message":{
                "message_id":2,"date":0,
                "chat":{"id":chat,"type":"private","first_name":"Test"},"text":text
            }})
            .to_string(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn group_messages_identify_senders_without_changing_control_commands() {
        let interface = TelegramInterface::new("test", "123:test", "");
        for kind in ["group", "supergroup", "private"] {
            for (from, sender_chat, expected) in [
                (
                    json!({"id":11,"is_bot":false,"first_name":"Alex","last_name":"One","username":"alex"}),
                    Value::Null,
                    json!({"user_id":11,"name":"Alex One","username":"@alex"}),
                ),
                (
                    json!({"id":12,"is_bot":false,"first_name":"Alex","last_name":"One"}),
                    Value::Null,
                    json!({"user_id":12,"name":"Alex One","username":null}),
                ),
                (
                    json!({"id":1087968824_u64,"is_bot":true,"first_name":"Group"}),
                    json!({"id":-42,"type":"supergroup","title":"Anonymous admins"}),
                    json!({"chat_id":-42,"name":"Anonymous admins","username":null}),
                ),
                (Value::Null, Value::Null, json!({"unknown":true})),
            ] {
                for text in ["Привет", "/new", " /stop@tEsTbOt ", "/new@OtherBot"] {
                    let chat = if kind == "private" {
                        json!({"id":42,"type":kind,"first_name":"Test"})
                    } else {
                        json!({"id":-42,"type":kind,"title":"Test"})
                    };
                    let message: teloxide::types::Message = serde_json::from_value(json!({
                        "message_id":1,"date":0,"chat":chat,"from":from,
                        "sender_chat":sender_chat,"text":text
                    }))
                    .unwrap();
                    let update = Update {
                        id: teloxide::types::UpdateId(1),
                        kind: UpdateKind::Message(message),
                    };
                    let event = interface.map_update(update, "TestBot").await.unwrap();
                    if text == "/new@OtherBot" {
                        assert!(event.is_none());
                        continue;
                    }
                    let Some(Event::Message(message)) = event else {
                        panic!("message ignored")
                    };
                    let normalized = if text.contains("@tEsTbOt") {
                        "/stop"
                    } else {
                        text
                    };
                    if kind == "private" || normalized.starts_with('/') {
                        assert_eq!(message.text, normalized);
                        assert!(message.telegram_group.is_none());
                    } else {
                        let (metadata, body) = message
                            .text
                            .split_once('\n')
                            .expect("sender metadata missing");
                        let sender: Value = serde_json::from_str(
                            metadata.strip_prefix("[Telegram group sender] ").unwrap(),
                        )
                        .unwrap();
                        let group = serde_json::to_value(message.telegram_group.unwrap()).unwrap();
                        let id = if expected.get("user_id").is_some() {
                            format!("user:{}", expected["user_id"])
                        } else {
                            format!("chat:{}", expected["chat_id"])
                        };
                        for (key, value) in expected.as_object().unwrap() {
                            if matches!(key.as_str(), "name" | "username") {
                                assert_eq!(&group["participants"][&id][key], value);
                                assert!(sender.get(key).is_none());
                            } else {
                                assert_eq!(&sender[key], value);
                            }
                        }
                        assert_eq!(group["bot_username"], "@TestBot");
                        assert!(sender.get("reply_to").is_none());
                        assert_eq!(body, text);
                        assert_eq!(message.source.conversation_id, "-42");
                    }
                    assert_eq!(message.is_group, kind != "private");
                }
            }
        }
    }

    #[tokio::test]
    async fn group_text_mentions_use_ids_and_keep_utf16_offsets() {
        let interface = TelegramInterface::new("test", "token", "/unused");
        let message = serde_json::from_value(json!({
            "message_id":9,"date":0,
            "chat":{"id":-42,"type":"supergroup","title":"Test"},
            "from":{"id":11,"is_bot":false,"first_name":"Alice"},
            "text":"😀 Bob",
            "entities":[{"type":"text_mention","offset":3,"length":3,
                "user":{"id":12,"is_bot":false,"first_name":"Bob"}}]
        }))
        .unwrap();
        let update = Update {
            id: teloxide::types::UpdateId(1),
            kind: UpdateKind::Message(message),
        };
        let Some(Event::Message(message)) = interface.map_update(update, "TestBot").await.unwrap()
        else {
            panic!()
        };
        let metadata: Value = serde_json::from_str(
            message
                .text
                .split_once('\n')
                .unwrap()
                .0
                .strip_prefix("[Telegram group sender] ")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            metadata["entities"],
            json!([{"type":"text_mention","offset":3,"length":3,"user_id":12}])
        );
        let group = serde_json::to_value(message.telegram_group.unwrap()).unwrap();
        assert_eq!(group["participants"]["user:12"]["name"], "Bob");
    }

    fn document_update(
        chat: i64,
        name: &str,
        mime: &str,
        size: usize,
        caption: Option<&str>,
    ) -> Update {
        let update: Update = serde_json::from_str(
            &json!({"update_id":3,"message":{
                "message_id":3,"date":0,
                "chat":{"id":chat,"type":"private","first_name":"Test"},
                "caption":caption,
                "document":{"file_id":"document","file_unique_id":"doc",
                    "file_name":name,"mime_type":mime,"file_size":size}
            }})
            .to_string(),
        )
        .unwrap();
        assert!(
            matches!(&update.kind, UpdateKind::Message(message) if message.document().is_some()),
            "{update:?}"
        );
        update
    }

    #[tokio::test]
    async fn rejected_documents_notify_user_and_do_not_drop_following_text() {
        let (interface, request) = server("200 OK", json!({"ok":true,"result":{
            "message_id":7,"date":0,"chat":{"id":42,"type":"private","first_name":"Test"},"text":"error"
        }})).await;

        let (tx, mut events) = mpsc::channel(16);
        interface
            .process_updates(
                futures::stream::iter([
                    Ok(document_update(
                        42,
                        "archive.zip",
                        "application/zip",
                        13,
                        None,
                    )),
                    Ok(text_update(42, "still here")),
                ]),
                tx,
                "TestBot",
            )
            .await
            .unwrap();
        let Event::Message(message) = events.recv().await.unwrap();
        assert_eq!(message.text, "still here");
        assert!(events.recv().await.is_none());
        let (path, response) = request.await.unwrap();
        assert!(path.to_lowercase().contains("/sendmessage "), "{path}");
        assert!(
            response["text"]
                .as_str()
                .unwrap()
                .contains("Check its format and size")
        );
    }

    #[tokio::test]
    async fn documents_enforce_format_and_size_before_download() {
        let dir = tempfile::tempdir().unwrap();
        let interface = TelegramInterface::new("test", "123:test-secret", dir.path());
        for update in [
            document_update(42, "test.zip", "application/zip", 13, None),
            document_update(42, "test.pdf", "application/pdf", 20_000_001, None),
            document_update(42, "test.txt", "text/plain", 0, None),
        ] {
            let error = interface
                .map_update(update, "TestBot")
                .await
                .err()
                .expect("document must be rejected");
            assert!(!format!("{error:#}").contains("test-secret"));
        }
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn downloads_documents_with_caption_or_default_without_exposing_token() {
        for (name, mime, bytes, locale) in [
            (
                "report.pdf",
                "application/pdf",
                b"%PDF-1.7\ntest".to_vec(),
                "en",
            ),
            ("notes.txt", "text/plain", b"hello".to_vec(), "en"),
            ("notes.txt", "text/plain", b"hello".to_vec(), "ru"),
            (
                "photo.png",
                "image/png",
                crate::agent::image::tests::sample(image::ImageFormat::Png),
                "en",
            ),
        ] {
            for caption in [Some("/stop"), None, Some("   ")] {
                let dir = tempfile::tempdir().unwrap();
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let mut interface = TelegramInterface::new("test", "123:test-secret", dir.path())
                    .with_space_root(dir.path().to_owned());
                interface.bot = interface.bot.set_api_url(
                    format!("http://{}", listener.local_addr().unwrap())
                        .parse()
                        .unwrap(),
                );
                let chat = if caption.is_none() { 7 } else { 42 };
                let mut update = document_update(chat, name, mime, bytes.len(), caption);
                if let UpdateKind::Message(message) = &mut update.kind {
                    message.from = Some(serde_json::from_value(json!({"id":chat,"is_bot":false,"first_name":"Test","language_code":locale})).unwrap());
                }
                let original = bytes.clone();
                let bytes = bytes.clone();
                let server = tokio::spawn(async move {
                    for body in [json!({"ok":true,"result":{"file_id":"document","file_unique_id":"doc","file_size":bytes.len(),"file_path":"documents/test"}}).to_string().into_bytes(), bytes] {
                        let (socket, _) = listener.accept().await.unwrap();
                        let mut reader = BufReader::new(socket);
                        let mut length = 0;
                        loop {
                            let mut line = String::new();
                            reader.read_line(&mut line).await.unwrap();
                            if line == "\r\n" { break; }
                            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") { length = value.trim().parse().unwrap(); }
                        }
                        let mut request = vec![0; length];
                        reader.read_exact(&mut request).await.unwrap();
                        reader.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
                        reader.get_mut().write_all(&body).await.unwrap();
                    }
                });
                let event = tokio::time::timeout(
                    Duration::from_secs(5),
                    interface.map_update(update, "TestBot"),
                )
                .await
                .unwrap()
                .unwrap();
                if event.is_none() {
                    server.abort();
                }
                let Some(Event::Message(message)) = event else {
                    panic!("document ignored");
                };
                assert_eq!(
                    message.text,
                    caption.filter(|text| !text.trim().is_empty()).unwrap_or(
                        if mime.starts_with("image/") {
                            ""
                        } else {
                            Language::from_locale(locale).pick(
                                "Analyze the attached file.",
                                "Проанализируй прикреплённый файл.",
                            )
                        }
                    )
                );
                if mime.starts_with("image/") {
                    assert_eq!(message.images.len(), 1);
                } else {
                    assert_eq!(message.files.len(), 1);
                    assert_eq!(message.files[0].filename, name);
                    assert!(message.files[0].path.starts_with(
                        crate::spaces::telegram(dir.path(), "test", chat).join("attachments")
                    ));
                    assert_eq!(std::fs::read(&message.files[0].path).unwrap(), original);
                    assert!(
                        !serde_json::to_string(&message.files)
                            .unwrap()
                            .contains("test-secret")
                    );
                }
                server.await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn stalled_photos_preserve_chat_order_but_do_not_block_commands_or_other_chats() {
        for command in ["/stop@TestBot", "/new@TestBot"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut interface =
                TelegramInterface::new("test", "123:test", std::path::PathBuf::new());
            interface.bot = interface.bot.set_api_url(
                format!("http://{}", listener.local_addr().unwrap())
                    .parse()
                    .unwrap(),
            );
            let (started, waiting) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                started.send(()).unwrap();
                // Hold getFile open until the test cancels this server.
                let _socket = socket;
                std::future::pending::<()>().await;
            });
            let (updates, input) = futures::channel::mpsc::unbounded();
            let (tx, mut events) = mpsc::channel(16);
            let worker =
                tokio::spawn(async move { interface.process_updates(input, tx, "TestBot").await });
            updates
                .unbounded_send(Ok(photo_update(42, Some("photo"))))
                .unwrap();
            waiting.await.unwrap();
            updates
                .unbounded_send(Ok(text_update(42, "after photo")))
                .unwrap();
            updates
                .unbounded_send(Ok(text_update(43, "other chat")))
                .unwrap();
            let event = tokio::time::timeout(Duration::from_secs(2), events.recv()).await;
            if event.is_err() {
                worker.abort();
                server.abort();
            }
            let Event::Message(message) = event.expect("photo blocked another chat").unwrap();
            assert_eq!(
                message.text, "other chat",
                "same-chat text overtook its photo"
            );
            updates
                .unbounded_send(Ok(text_update(42, command)))
                .unwrap();
            let event = tokio::time::timeout(Duration::from_secs(2), events.recv()).await;
            if event.is_err() {
                worker.abort();
                server.abort();
            }
            let Event::Message(message) = event.expect("photo blocked cancellation").unwrap();
            assert_eq!(message.text, command.split('@').next().unwrap());
            updates
                .unbounded_send(Ok(text_update(42, "fresh task")))
                .unwrap();
            let Event::Message(message) =
                tokio::time::timeout(Duration::from_secs(2), events.recv())
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(message.text, "fresh task");
            drop(updates);
            tokio::time::timeout(Duration::from_secs(2), worker)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(
                events.recv().await.is_none(),
                "cancelled photo or queued text escaped"
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn downloads_largest_photo_without_inventing_a_prompt() {
        for (group, caption) in [
            (false, Some("Что изображено?")),
            (false, None),
            (false, Some("/stop@OtherBot")),
            (true, Some("/stop")),
            (true, None),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut interface =
                TelegramInterface::new("test", "123:test-secret", std::path::PathBuf::new());
            interface.bot = interface.bot.set_api_url(
                format!("http://{}", listener.local_addr().unwrap())
                    .parse()
                    .unwrap(),
            );
            let bytes = crate::agent::image::tests::sample(image::ImageFormat::Jpeg);
            let expected = Image::from_bytes(&bytes).unwrap().data_url;
            let server = tokio::spawn(async move {
                for (index, body) in [
                    json!({"ok":true,"result":{"file_id":"large","file_unique_id":"l","file_size":bytes.len(),"file_path":"photos/test.jpg"}}).to_string().into_bytes(),
                    bytes,
                ].into_iter().enumerate() {
                    let (socket, _) = listener.accept().await.unwrap();
                    let mut reader = BufReader::new(socket);
                    let mut path = String::new();
                    reader.read_line(&mut path).await.unwrap();
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).await.unwrap();
                        if line == "\r\n" { break; }
                        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                            length = value.trim().parse().unwrap();
                        }
                    }
                    let mut request = vec![0; length];
                    reader.read_exact(&mut request).await.unwrap();
                    if index == 0 {
                        assert!(path.to_ascii_lowercase().contains("/getfile "), "{path}");
                        assert_eq!(serde_json::from_slice::<Value>(&request).unwrap()["file_id"], "large");
                    } else {
                        assert!(path.contains("/file/bot123:test-secret/photos/test.jpg "));
                    }
                    reader.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
                    reader.get_mut().write_all(&body).await.unwrap();
                }
            });
            tokio::time::timeout(Duration::from_secs(5), async {
                let (tx, mut events) = mpsc::channel(16);
                let mut update = photo_update(42, caption);
                let chat = if group { -42 } else { 42 };
                if group && let UpdateKind::Message(message) = &mut update.kind {
                    message.chat = serde_json::from_value(
                        json!({"id":-42,"type":"supergroup","title":"Test"}),
                    )
                    .unwrap();
                    message.from = Some(
                        serde_json::from_value(json!({"id":11,"is_bot":false,"first_name":"Alex"}))
                            .unwrap(),
                    );
                }
                interface
                    .process_updates(
                        futures::stream::iter([Ok(update), Ok(text_update(chat, "after photo"))]),
                        tx,
                        "TestBot",
                    )
                    .await
                    .unwrap();
                let Some(Event::Message(message)) = events.recv().await else {
                    panic!("photo ignored")
                };
                if group {
                    let (metadata, body) =
                        message.text.split_once('\n').expect("photo sender missing");
                    let sender: Value = serde_json::from_str(
                        metadata.strip_prefix("[Telegram group sender] ").unwrap(),
                    )
                    .unwrap();
                    assert_eq!(sender["user_id"], 11);
                    assert!(sender.get("name").is_none());
                    let group =
                        serde_json::to_value(message.telegram_group.as_ref().unwrap()).unwrap();
                    assert_eq!(group["participants"]["user:11"]["name"], "Alex");
                    assert_eq!(group["participants"]["user:11"]["username"], Value::Null);
                    assert_eq!(group["bot_username"], "@TestBot");
                    assert!(message.is_group);
                    assert_eq!(body, caption.unwrap_or_default());
                } else {
                    assert_eq!(message.text, caption.unwrap_or_default());
                }
                assert_eq!(message.images.len(), 1);
                assert_eq!(message.images[0].data_url, expected);
                assert!(!message.images[0].data_url.contains("test-secret"));
                let Some(Event::Message(message)) = events.recv().await else {
                    panic!("text lost")
                };
                assert_eq!(message.text, "after photo");
                assert!(events.recv().await.is_none());
                server.await.unwrap();
            })
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn photo_download_errors_hide_token() {
        for body in [
            json!({"ok":false,"error_code":400,"description":"error with test-secret"}),
            json!({"ok":true,"result":{"file_id":"large","file_unique_id":"l","file_size":MAX_IMAGE_BYTES + 1,"file_path":"photos/test.jpg"}}),
        ] {
            let (interface, request) = server("200 OK", body).await;
            let error = interface.download_photo("large".into()).await.unwrap_err();
            assert!(!format!("{error:#}").contains("test-secret"));
            request.await.unwrap();
        }
    }

    #[tokio::test]
    async fn identical_edit_is_idempotent() {
        for (description, succeeds) in [
            ("Bad Request: message is not modified", true),
            (
                "Bad Request: message is not modified: specified new message content and reply markup are exactly the same as a current content and reply markup of the message",
                true,
            ),
            ("Bad Request: message to edit not found", false),
        ] {
            let (interface, request) = server(
                "400 Bad Request",
                json!({"ok":false,"error_code":400,"description":description}),
            )
            .await;
            let result = interface
                .edit("-42", &MessageId("7".into()), "unchanged")
                .await;
            assert_eq!(result.is_ok(), succeeds, "{description}: {result:?}");
            request.await.unwrap();
        }
    }

    #[tokio::test]
    async fn reminders_classify_only_explicit_api_refusals() {
        for (status, body, retry) in [
            (
                "429 Too Many Requests",
                json!({"ok":false,"error_code":429,"description":"rate limited","parameters":{"retry_after":1}}),
                Some(Some(1)),
            ),
            (
                "429 Too Many Requests",
                json!({"ok":false,"error_code":429,"description":"rate limited","parameters":{"retry_after":120}}),
                Some(Some(120)),
            ),
            (
                "503 Service Unavailable",
                json!({"ok":false,"error_code":503,"description":"unavailable"}),
                Some(Some(60)),
            ),
            (
                "403 Forbidden",
                json!({"ok":false,"error_code":403,"description":"blocked"}),
                Some(None),
            ),
            (
                "502 Bad Gateway",
                json!({"description":"proxy error"}),
                None,
            ),
            ("200 OK", json!({"ok":true,"result":{}}), None),
        ] {
            let (interface, request) = server(status, body).await;
            let error = interface.send("42", "Reminder").await.unwrap_err();
            assert_eq!(
                error
                    .downcast_ref::<super::super::SendRejection>()
                    .map(|rejection| rejection.retry_after),
                retry
            );
            request.await.unwrap();
        }
        let interface = TelegramInterface::new("test", "123:test", std::path::PathBuf::new());
        assert!(interface.allows_recipient("42"));
        assert!(interface.allows_recipient("7"));
        assert!(interface.allows_recipient("-42"));
        assert!(!interface.allows_recipient("0"));
        assert!(!interface.allows_recipient("invalid"));
    }

    #[tokio::test]
    async fn personal_commands_in_groups_reach_private_chat_rejection_unchanged() {
        let interface = TelegramInterface::new("test", "123:test", std::path::PathBuf::new());
        for text in ["/sessions", "/session cli:main"] {
            let update = Update {
                id: teloxide::types::UpdateId(1),
                kind: UpdateKind::Message(serde_json::from_value(json!({
                    "message_id":1,"date":0,"chat":{"id":-42,"type":"group","title":"Test"},"text":text
                })).unwrap()),
            };
            let Event::Message(message) = interface
                .map_update(update, "TestBot")
                .await
                .unwrap()
                .unwrap();
            assert!(message.is_group);
            assert_eq!(message.text, text);
            let Event::Message(guest) = interface.map_guest(json!({
                "guest_query_id":"query","chat":{"id":-42,"type":"group","title":"Test"},"text":text,
            }), "TestBot").unwrap().unwrap();
            assert!(guest.is_group);
            assert_eq!(guest.text, text);
        }
    }

    #[tokio::test]
    async fn removed_login_command_keeps_normal_group_message_metadata() {
        let interface = TelegramInterface::new("test", "123:test", "");
        for text in ["/login", "/login@TestBot", "/login@OtherBot"] {
            let update = Update {
                id: teloxide::types::UpdateId(1),
                kind: UpdateKind::Message(serde_json::from_value(json!({
                    "message_id":1,"date":0,"chat":{"id":-42,"type":"group","title":"Test"},"text":text
                })).unwrap()),
            };
            let Event::Message(message) = interface
                .map_update(update, "TestBot")
                .await
                .unwrap()
                .unwrap();
            assert!(message.telegram_group.is_some());
            assert!(message.text.ends_with(&format!("\n{text}")));
            let Event::Message(guest) = interface.map_guest(json!({"guest_query_id":"query","chat":{"id":-42,"type":"group","title":"Test"},"text":text}), "TestBot").unwrap().unwrap();
            assert!(guest.telegram_group.is_some());
            assert!(guest.text.ends_with(&format!("\n{text}")));
        }
    }

    #[tokio::test]
    async fn commands_accept_any_chat_and_respect_bot_username() {
        let interface = TelegramInterface::new("test", "123:test", std::path::PathBuf::new());
        for (chat, text, expected) in [
            (42, "/new", Some("/new")),
            (42, "/stop", Some("/stop")),
            (42, "/new@TestBot", Some("/new")),
            (42, " /stop@testbot ", Some("/stop")),
            (42, "/sessions@TestBot 2", Some("/sessions 2")),
            (42, "/session@TestBot cli:main", Some("/session cli:main")),
            (42, "/login@TestBot", Some("/login@TestBot")),
            (42, "/sessions@OtherBot", None),
            (42, "/session@OtherBot cli:main", None),
            (42, "/login@OtherBot", Some("/login@OtherBot")),
            (42, "/new@OtherBot", None),
            (42, "/stop@OtherBot", None),
            (7, "/new", Some("/new")),
            (7, "/stop", Some("/stop")),
            (7, "hello", Some("hello")),
            (42, "hello", Some("hello")),
        ] {
            let update: Update = serde_json::from_str(
                &json!({
                    "update_id": 1,
                    "message": {
                        "message_id": 1, "date": 0,
                        "chat": {"id": chat, "type": "private", "first_name": "Test"},
                        "text": text,
                    }
                })
                .to_string(),
            )
            .unwrap();
            assert!(matches!(update.kind, UpdateKind::Message(_)), "{update:?}");
            let actual = interface
                .map_update(update, "TestBot")
                .await
                .unwrap()
                .map(|event| {
                    let Event::Message(message) = event;
                    message.text
                });
            assert_eq!(actual.as_deref(), expected, "{chat}: {text}");
        }
    }

    #[tokio::test]
    async fn guest_audits_record_confirmed_outcomes() {
        for (method, result, expected) in [
            (
                "answerGuestQuery",
                json!({"inline_message_id":"inline"}),
                "sent",
            ),
            (
                "answerGuestQuery",
                json!({"inline_message_id":""}),
                "unknown",
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let (interface, request) = server("200 OK", json!({"ok":true,"result":result})).await;
            let interface = interface.with_journal(crate::audit::Journal::new(directory.path()));
            interface.api_request(method, json!({})).await.unwrap();
            request.await.unwrap();
            let journal = std::fs::read_to_string(
                directory
                    .path()
                    .join(format!("{}.jsonl", chrono::Utc::now().format("%Y-%m-%d"))),
            )
            .unwrap();
            let outcome: Value = serde_json::from_str(journal.lines().last().unwrap()).unwrap();
            assert_eq!(outcome["status"], expected);
        }
    }

    #[tokio::test]
    async fn audit_delivery_records_confirmed_refused_and_unknown_outcomes() {
        for (status, body, expected) in [
            (
                "200 OK",
                json!({"ok":true,"result":{"message_id":7}}),
                "sent",
            ),
            (
                "403 Forbidden",
                json!({"ok":false,"error_code":403,"description":"PRIVATE_ERROR"}),
                "refused",
            ),
            (
                "502 Bad Gateway",
                json!({"description":"PRIVATE_ERROR"}),
                "unknown",
            ),
            ("200 OK", json!({"ok":true,"result":{}}), "unknown"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (interface, request) = server(status, body).await;
            let interface = interface.with_journal(crate::audit::Journal::new(dir.path()));
            let context = crate::audit::Context::new("selected-session", "message");
            let result =
                crate::audit::scope(context, interface.send("42", "PRIVATE_MESSAGE")).await;
            assert_eq!(result.is_ok(), expected == "sent");
            request.await.unwrap();
            let file = dir
                .path()
                .join(format!("{}.jsonl", chrono::Utc::now().format("%Y-%m-%d")));
            let text = std::fs::read_to_string(file).unwrap();
            assert!(!text.contains("PRIVATE_MESSAGE"));
            assert!(!text.contains("PRIVATE_ERROR"));
            assert!(!text.contains("test-secret"));
            let rows: Vec<Value> = text
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0]["event"], "delivery_started");
            assert_eq!(rows[1]["event"], "delivery_finished");
            assert_eq!(rows[1]["status"], expected);
            assert_eq!(rows[1]["session_id"], "selected-session");
            assert_eq!(rows[0]["run_id"], rows[1]["run_id"]);
        }
    }

    #[tokio::test]
    async fn audit_failure_after_send_preserves_delivery_and_blocks_next_send() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir
            .path()
            .join(format!("{}.jsonl", chrono::Utc::now().format("%Y-%m-%d")));
        let (interface, request) = server_with_hook(
            "200 OK",
            json!({"ok":true,"result":{"message_id":7}}),
            Some(file),
        )
        .await;
        let interface = interface.with_journal(crate::audit::Journal::new(dir.path()));
        assert_eq!(
            interface.send("42", "first").await.unwrap(),
            MessageId("7".into())
        );
        request.await.unwrap();
        let error = interface.send("42", "second").await.unwrap_err();
        assert!(error.to_string().contains("journal"));
    }

    pub(super) async fn server(
        status: &str,
        response: Value,
    ) -> (TelegramInterface, tokio::task::JoinHandle<(String, Value)>) {
        server_with_hook(status, response, None).await
    }

    async fn server_with_hook(
        status: &str,
        response: Value,
        block_journal: Option<std::path::PathBuf>,
    ) -> (TelegramInterface, tokio::task::JoinHandle<(String, Value)>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut interface =
            TelegramInterface::new("test", "123:test-secret", std::path::PathBuf::new());
        interface.bot = interface.bot.set_api_url(
            format!("http://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
        );
        let response = response.to_string();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
            response.len()
        );
        let task = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_secs(5), async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(socket);
                let mut path = String::new();
                reader.read_line(&mut path).await.unwrap();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).await.unwrap();
                if let Some(path) = block_journal {
                    std::fs::rename(&path, path.with_extension("saved")).unwrap();
                    std::fs::create_dir(path).unwrap();
                }
                reader
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
                (path, serde_json::from_slice(&body).unwrap())
            })
            .await
            .unwrap()
        });
        (interface, task)
    }

    #[tokio::test]
    async fn chatid_answers_in_groups_without_forwarding_command_to_agent() {
        for (kind, command) in [("group", "/chatid"), ("supergroup", "/chatid@tEsTbOt")] {
            let chat = -1001234567890_i64;
            let (interface, request) = server("200 OK", json!({"ok":true,"result":{"message_id":7,"date":0,"chat":{"id":chat,"type":kind,"title":"Test"},"text":"ID"}})).await;
            let update: Update = serde_json::from_str(&json!({"update_id":1,"message":{"message_id":3,"date":0,"chat":{"id":chat,"type":kind,"title":"Test"},"text":command}}).to_string()).unwrap();
            let (tx, mut events) = mpsc::channel(16);
            assert!(
                matches!(&update.kind, UpdateKind::Message(message) if message.text() == Some(command)),
                "{update:?}"
            );
            interface
                .process_updates(
                    futures::stream::iter([Ok(update), Ok(text_update(chat, "ordinary message"))]),
                    tx,
                    "TestBot",
                )
                .await
                .unwrap();
            let Some(Event::Message(message)) = events.recv().await else {
                panic!("ordinary message missing")
            };
            assert_eq!(message.text, "ordinary message");
            assert!(events.recv().await.is_none());
            let (path, body) = request.await.unwrap();
            assert!(path.to_lowercase().contains("/sendmessage "), "{path}");
            assert_eq!(body["chat_id"], chat);
            assert_eq!(body["text"], "Chat ID: -1001234567890");
        }
    }

    #[tokio::test]
    async fn chatid_ignores_foreign_bot_and_keeps_processing_after_send_failure() {
        let (interface, request) = server(
            "403 Forbidden",
            json!({"ok":false,"error_code":403,"description":"Cannot send messages"}),
        )
        .await;

        let (tx, mut events) = mpsc::channel(16);
        interface
            .process_updates(
                futures::stream::iter([
                    Ok(text_update(42, "/chatid@OtherBot")),
                    Ok(text_update(7, "/chatid")),
                    Ok(text_update(42, "still here")),
                ]),
                tx,
                "TestBot",
            )
            .await
            .unwrap();
        let Event::Message(message) = events.recv().await.unwrap();
        assert_eq!(message.text, "still here");
        assert!(events.recv().await.is_none());
        let (_, body) = request.await.unwrap();
        assert_eq!(body["chat_id"], 7);
        assert_eq!(body["text"], "Chat ID: 7");
    }

    #[tokio::test]
    async fn empty_rich_markdown_retries_as_literal_text() {
        for action in ["send", "status", "edit"] {
            let (tx, mut requests) = mpsc::unbounded_channel();
            let app = axum::Router::new().fallback(axum::routing::post(
                move |uri: axum::http::Uri, axum::Json(body): axum::Json<Value>| {
                    tx.send((uri.path().to_owned(), body.clone())).unwrap();
                    async move {
                        if body["rich_message"].get("markdown").is_some() {
                            (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"ok":false,"error_code":400,"description":"Bad Request: RICH_MESSAGE_EMPTY"})))
                        } else {
                            (axum::http::StatusCode::OK, axum::Json(json!({"ok":true,"result":{"message_id":42}})))
                        }
                    }
                },
            ));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut interface =
                TelegramInterface::new("test", "123:test-secret", std::path::PathBuf::new());
            interface.bot = interface.bot.set_api_url(
                format!("http://{}", listener.local_addr().unwrap())
                    .parse()
                    .unwrap(),
            );
            let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            match action {
                "send" => {
                    assert_eq!(
                        interface.send("7", "20.").await.unwrap(),
                        MessageId("42".into())
                    );
                }
                "status" => {
                    assert_eq!(
                        interface.send_status("7", "20.").await.unwrap(),
                        MessageId("42".into())
                    );
                }
                _ => interface
                    .edit("7", &MessageId("42".into()), "20.")
                    .await
                    .unwrap(),
            }
            let (path, mut original) = requests.try_recv().unwrap();
            let (retry_path, retry) = requests.try_recv().unwrap();
            assert_eq!(path, retry_path);
            assert_eq!(original["rich_message"], json!({"markdown":"20."}));
            original["rich_message"] = json!({"blocks":[{"type":"paragraph","text":"20."}]});
            assert_eq!(
                retry, original,
                "{action}: preserve edit id / silent status"
            );
            assert!(requests.try_recv().is_err());
            task.abort();
        }
    }

    #[tokio::test]
    async fn literal_retry_requires_definite_empty_markdown_rejection() {
        for (code, description, text) in [
            (429, "RICH_MESSAGE_EMPTY", "20."),
            (500, "RICH_MESSAGE_EMPTY", "20."),
            (400, "Bad Request: invalid rich message", "20."),
            (400, "Bad Request: RICH_MESSAGE_EMPTY", " \n\t"),
        ] {
            let (interface, request) = server(
                "400 Bad Request",
                json!({"ok":false,"error_code":code,"description":description}),
            )
            .await;
            let error = interface.send("7", text).await.unwrap_err();
            let rejection = error
                .downcast_ref::<super::super::SendRejection>()
                .expect("preserve the original rejection");
            assert_eq!(rejection.error_code, code);
            assert!(rejection.description.contains(description));
            request.await.unwrap();
        }
    }

    #[tokio::test]
    async fn sends_and_edits_rich_markdown() {
        let markdown = "# Привет 🦀\n**bold** _italic_ [link](https://example.com)\n```rust\nlet x = 1;\n```\n| A | B |\n|---|---|\n| 1 | 2 |";
        for edit in [false, true] {
            let (interface, request) = server("200 OK", json!({"ok":true,"result":{"message_id":42,"date":0,"chat":{"id":-123,"type":"supergroup","title":"Test"},"rich_message":{"blocks":[]}}})).await;
            if edit {
                interface
                    .edit("-123", &MessageId("42".into()), markdown)
                    .await
                    .unwrap();
            } else {
                assert_eq!(
                    interface.send("-123", markdown).await.unwrap(),
                    MessageId("42".into())
                );
            }
            let (path, body) = request.await.unwrap();
            let method = if edit {
                "editMessageText"
            } else {
                "sendRichMessage"
            };
            assert_eq!(
                path,
                format!("POST /bot123:test-secret/{method} HTTP/1.1\r\n")
            );
            let mut expected = json!({"chat_id":-123,"rich_message":{"markdown":markdown}});
            if edit {
                expected["message_id"] = json!(42);
            }
            assert_eq!(body, expected);
        }
    }

    #[tokio::test]
    async fn sends_status_without_notification() {
        let (interface, request) =
            server("200 OK", json!({"ok":true,"result":{"message_id":42}})).await;
        assert_eq!(
            interface.send_status("-123", "Working").await.unwrap(),
            MessageId("42".into())
        );
        let (path, body) = request.await.unwrap();
        assert_eq!(
            path,
            "POST /bot123:test-secret/sendRichMessage HTTP/1.1\r\n"
        );
        assert_eq!(
            body,
            json!({"chat_id":-123,"rich_message":{"markdown":"Working"},"disable_notification":true})
        );
    }

    #[tokio::test]
    async fn rejects_api_errors_and_malformed_success() {
        for (status, response, expected) in [
            (
                "400 Bad Request",
                json!({"ok":false,"error_code":400,"description":"Bad Request: invalid rich message"}),
                "invalid rich message",
            ),
            (
                "200 OK",
                json!({"ok":false,"error_code":400,"description":"Bad Request: invalid rich message"}),
                "invalid rich message",
            ),
            ("200 OK", json!({"ok":true,"result":{}}), "message_id"),
            (
                "500 Internal Server Error",
                json!({"ok":true,"result":{"message_id":42}}),
                "500",
            ),
            (
                "200 OK",
                json!({"result":{"message_id":42}}),
                "unsuccessful API response",
            ),
        ] {
            let (interface, request) = server(status, response).await;
            let error = interface.send("-123", "**hello**").await.unwrap_err();
            let error = format!("{error:#}");
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("test-secret"), "{error}");
            request.await.unwrap();
        }
    }

    #[tokio::test]
    async fn transport_errors_do_not_expose_bot_token() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut interface =
            TelegramInterface::new("test", "123:test-secret", std::path::PathBuf::new());
        interface.bot = interface.bot.set_api_url(
            format!("http://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
        );
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            drop(socket);
        });
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            interface.send("-123", "hello"),
        )
        .await
        .unwrap()
        .unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("request failed"), "{error}");
        assert!(!error.contains("test-secret"), "{error}");
        server.await.unwrap();
    }
}
