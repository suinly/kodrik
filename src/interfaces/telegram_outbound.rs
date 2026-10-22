use super::TelegramInterface;
use crate::{
    Interface, MessageId,
    interfaces::{MediaSource, OutgoingMessage, SendRejection},
};
use anyhow::Context;
use serde_json::{Value, json};
use teloxide::{
    payloads::{SendAudioSetters, SendDocumentSetters, SendPhotoSetters, SendVideoSetters},
    requests::Requester,
    types::{ChatId, FileId, InputFile, InputMedia, InputMediaPhoto},
};

impl TelegramInterface {
    pub async fn send_outgoing_message(
        &self,
        chat: &str,
        message: &OutgoingMessage,
    ) -> anyhow::Result<MessageId> {
        message.validate()?;
        let chat_id = chat.parse::<i64>()?;
        anyhow::ensure!(chat_id != 0, "Invalid Telegram chat ID");
        match message {
            OutgoingMessage::Photo { caption, .. }
            | OutgoingMessage::PhotoAlbum { caption, .. }
            | OutgoingMessage::Video { caption, .. }
            | OutgoingMessage::Document { caption, .. }
            | OutgoingMessage::Audio { caption, .. } => {
                let files = match message {
                    OutgoingMessage::PhotoAlbum { media, .. } => {
                        media.iter().map(input_file).collect::<Result<_, _>>()?
                    }
                    OutgoingMessage::Photo { media, .. }
                    | OutgoingMessage::Video { media, .. }
                    | OutgoingMessage::Document { media, .. }
                    | OutgoingMessage::Audio { media, .. } => vec![input_file(media)?],
                    _ => unreachable!(),
                };
                let method = match message {
                    OutgoingMessage::Photo { .. } => "sendPhoto",
                    OutgoingMessage::PhotoAlbum { .. } => "sendMediaGroup",
                    OutgoingMessage::Video { .. } => "sendVideo",
                    OutgoingMessage::Document { .. } => "sendDocument",
                    _ => "sendAudio",
                };
                let context =
                    crate::audit::Context::current_or(&format!("{}:{chat}", self.id()), "delivery");
                let span = crate::audit::Span::start(
                    self.journal.as_ref(),
                    &context,
                    "delivery",
                    None,
                    method,
                    json!({"chat_id":chat_id}),
                )
                .await?;
                let caption = caption.clone().unwrap_or_default();
                let result = match message {
                    OutgoingMessage::Photo { .. } => {
                        self.bot
                            .send_photo(ChatId(chat_id), files[0].clone())
                            .caption(caption)
                            .await
                            .map(Some)
                    }
                    OutgoingMessage::PhotoAlbum { .. } => {
                        let media = files.into_iter().enumerate().map(|(index, file)| {
                            let mut photo = InputMediaPhoto::new(file);
                            if index == 0 && !caption.is_empty() {
                                photo = photo.caption(caption.clone());
                            }
                            InputMedia::Photo(photo)
                        });
                        self.bot.send_media_group(ChatId(chat_id), media).await
                            .map(|messages| messages.into_iter().next())
                    }
                    OutgoingMessage::Video { .. } => {
                        self.bot
                            .send_video(ChatId(chat_id), files[0].clone())
                            .caption(caption)
                            .await
                            .map(Some)
                    }
                    OutgoingMessage::Document { .. } => {
                        self.bot
                            .send_document(ChatId(chat_id), files[0].clone())
                            .caption(caption)
                            .await
                            .map(Some)
                    }
                    _ => {
                        self.bot
                            .send_audio(ChatId(chat_id), files[0].clone())
                            .caption(caption)
                            .await
                            .map(Some)
                    }
                }
                .map_err(|error| media_error(method, error))
                .and_then(|message| {
                    message
                        .map(|message| MessageId(message.id.0.to_string()))
                        .context("Telegram response is missing a sent message; delivery outcome is unknown")
                });
                let rejection = result
                    .as_ref()
                    .err()
                    .and_then(|error| error.downcast_ref::<SendRejection>());
                let status = if result.is_ok() {
                    "sent"
                } else if rejection.is_some() {
                    "refused"
                } else {
                    "unknown"
                };
                if let Err(error) = span.finish(status, json!({
                    "message_id":result.as_ref().ok().and_then(|id| id.0.parse::<i32>().ok()),
                    "error_code":rejection.map(|error| error.error_code),
                    "retry_after":rejection.and_then(|error| error.retry_after),
                })).await {
                    log::error!("Delivery outcome could not be journaled; further actions blocked: {error:#}");
                }
                result
            }
            OutgoingMessage::Poll {
                question,
                options,
                multiple_answers,
                anonymous,
            } => {
                let response = self.api_request("sendPoll", json!({
                    "chat_id":chat_id,
                    "question":question,"options":options.iter().map(|text|json!({"text":text})).collect::<Vec<_>>(),
                    "allows_multiple_answers":multiple_answers.unwrap_or(false),"is_anonymous":anonymous.unwrap_or(true),
                })).await?;
                let id = response["result"]["message_id"]
                    .as_i64()
                    .and_then(|id| i32::try_from(id).ok())
                    .context("Telegram response is missing a valid message_id")?;
                if self.space_root.is_some() {
                    let store = self.poll_store()?;
                    let poll = response["result"]["poll"].clone();
                    let conversation = chat.to_owned();
                    // Delivery is confirmed; a storage failure must not duplicate the poll.
                    if let Err(error) =
                        tokio::task::spawn_blocking(move || store.register(&conversation, id, poll))
                            .await
                            .map_err(anyhow::Error::from)
                            .and_then(|result| result)
                    {
                        log::error!("Sent poll results could not be registered: {error:#}");
                    }
                }
                Ok(MessageId(id.to_string()))
            }
            OutgoingMessage::Location {
                latitude,
                longitude,
            } => {
                self.rich_request(
                    "sendLocation",
                    json!({"chat_id":chat_id,"latitude":latitude,"longitude":longitude}),
                )
                .await
            }
            OutgoingMessage::Post { title, body } => self.send(chat, &post_text(title, body)).await,
        }
    }

    pub async fn answer_guest_outgoing_message(
        &self,
        query: &str,
        message: &OutgoingMessage,
    ) -> anyhow::Result<MessageId> {
        let result = message.guest_result()?;
        let response = self
            .api_request(
                "answerGuestQuery",
                json!({"guest_query_id":query,"result":result}),
            )
            .await?;
        Ok(MessageId(
            response["result"]["inline_message_id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .context("Missing guest inline message ID")?
                .to_owned(),
        ))
    }
}

impl OutgoingMessage {
    /// Preflight the complete inline result before reserving a guest delivery slot.
    pub(crate) fn guest_result(&self) -> anyhow::Result<Value> {
        self.validate()?;
        anyhow::ensure!(
            self.supports_guest(),
            "Guest replies do not support photo albums, polls or local uploads; use an HTTP(S) URL or Telegram file ID for single media"
        );
        guest_result(self)
    }
}

fn input_file(media: &MediaSource) -> anyhow::Result<InputFile> {
    Ok(match media {
        MediaSource::File { path } => InputFile::file(path.clone()),
        MediaSource::Url { url } => InputFile::url(url.parse()?),
        MediaSource::Telegram { file_id } => InputFile::file_id(FileId(file_id.clone())),
        MediaSource::Upload { filename, data } => {
            InputFile::memory(data.clone()).file_name(filename.clone())
        }
    })
}

fn post_text(title: &str, body: &str) -> String {
    format!("# {title}\n\n{body}")
}

fn media_error(method: &str, error: teloxide::RequestError) -> anyhow::Error {
    match error {
        teloxide::RequestError::Api(error) => {
            let description = error.to_string();
            // shortcut: teloxide drops numeric codes; use raw multipart if exact refusal codes become necessary.
            let error_code = if description.starts_with("Forbidden:") {
                403
            } else if description.starts_with("Unauthorized")
                || matches!(error, teloxide::ApiError::InvalidToken)
            {
                401
            } else {
                400
            };
            SendRejection {
                description: format!("Telegram {method} refused: {description}"),
                error_code,
                retry_after: None,
            }
            .into()
        }
        teloxide::RequestError::RetryAfter(seconds) => SendRejection {
            description: format!("Telegram {method} rate limited"),
            error_code: 429,
            retry_after: Some(u64::from(seconds.seconds())),
        }
        .into(),
        teloxide::RequestError::MigrateToChatId(_) => SendRejection {
            description: format!("Telegram {method} refused: chat migrated"),
            error_code: 400,
            retry_after: None,
        }
        .into(),
        teloxide::RequestError::Network(_) => {
            anyhow::anyhow!("Telegram {method} request failed; delivery outcome is unknown")
        }
        teloxide::RequestError::InvalidJson { .. } => {
            anyhow::anyhow!("Telegram {method} returned invalid JSON; delivery outcome is unknown")
        }
        teloxide::RequestError::Io(_) => {
            anyhow::anyhow!("Telegram {method} upload failed; delivery outcome is unknown")
        }
    }
}

fn guest_result(message: &OutgoingMessage) -> anyhow::Result<Value> {
    let (kind, media, caption) = match message {
        OutgoingMessage::Photo { media, caption } => ("photo", media, caption),
        OutgoingMessage::Video { media, caption, .. } => ("video", media, caption),
        OutgoingMessage::Document { media, caption } => ("document", media, caption),
        OutgoingMessage::Audio { media, caption } => ("audio", media, caption),
        OutgoingMessage::Location {
            latitude,
            longitude,
        } => {
            return Ok(
                json!({"type":"location","id":"kodrik","title":"Location","latitude":latitude,"longitude":longitude}),
            );
        }
        OutgoingMessage::Post { title, body } => {
            return Ok(
                json!({"type":"article","id":"kodrik","title":title,"input_message_content":{"rich_message":{"markdown":post_text(title,body)}}}),
            );
        }
        _ => anyhow::bail!("Unsupported guest reply"),
    };
    let mut result = json!({"type":kind,"id":"kodrik"});
    if let Some(caption) = caption {
        result["caption"] = json!(caption);
    }
    match media {
        MediaSource::Telegram { file_id } => {
            result[format!("{kind}_file_id")] = json!(file_id);
            if matches!(kind, "video" | "document") {
                result["title"] = json!(
                    caption
                        .as_deref()
                        .filter(|text| !text.trim().is_empty())
                        .unwrap_or(kind)
                );
            }
        }
        MediaSource::Url { url } => {
            result[format!("{kind}_url")] = json!(url);
            let parsed = reqwest::Url::parse(url)?;
            let filename = parsed
                .path_segments()
                .and_then(|mut parts| parts.next_back())
                .filter(|name| !name.is_empty())
                .unwrap_or(kind);
            match message {
                OutgoingMessage::Photo { .. } => result["thumbnail_url"] = json!(url),
                OutgoingMessage::Video { thumbnail_url, .. } => {
                    let thumbnail = thumbnail_url.as_deref().context("Guest video URLs require thumbnail_url pointing to a JPEG thumbnail; alternatively use a Telegram video file ID")?;
                    result["thumbnail_url"] = json!(thumbnail);
                    result["mime_type"] = json!("video/mp4");
                    result["title"] = json!(filename);
                }
                OutgoingMessage::Document { .. } => {
                    let mime = if parsed.path().to_ascii_lowercase().ends_with(".pdf") {
                        "application/pdf"
                    } else if parsed.path().to_ascii_lowercase().ends_with(".zip") {
                        "application/zip"
                    } else {
                        anyhow::bail!(
                            "Guest document URLs must identify a PDF or ZIP (.pdf/.zip); use an existing Telegram file ID for other documents"
                        )
                    };
                    result["mime_type"] = json!(mime);
                    result["title"] = json!(filename);
                }
                OutgoingMessage::Audio { .. } => result["title"] = json!(filename),
                _ => unreachable!(),
            }
        }
        _ => anyhow::bail!("Guest media require an HTTP(S) URL or Telegram file ID"),
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interfaces::MediaSource;
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    async fn server(
        response: Value,
    ) -> (
        TelegramInterface,
        tokio::task::JoinHandle<(String, Vec<u8>)>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut interface =
            TelegramInterface::new("test", "123:test-secret", std::path::PathBuf::new());
        interface.bot = interface.bot.set_api_url(
            format!("http://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
        );
        let task = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_secs(5), async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(socket);
                let mut path = String::new();
                reader.read_line(&mut path).await.unwrap();
                let mut length = 0;
                let mut chunked = false;
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" { break; }
                    headers.push_str(&line);
                    if let Some(value) = line.to_lowercase().strip_prefix("content-length:") { length = value.trim().parse().unwrap(); }
                    if line.to_lowercase().starts_with("transfer-encoding: chunked") { chunked = true; }
                }
                let mut body = Vec::new();
                if chunked {
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).await.unwrap();
                        let size = usize::from_str_radix(line.trim(),16).unwrap();
                        if size == 0 { break; }
                        let start = body.len();
                        body.resize(start + size,0);
                        reader.read_exact(&mut body[start..]).await.unwrap();
                        reader.read_exact(&mut [0;2]).await.unwrap();
                    }
                } else if length > 0 {
                    body.resize(length,0);
                    reader.read_exact(&mut body).await.unwrap();
                } else {
                    let boundary = headers.lines().find_map(|line|line.split_once("boundary=").map(|(_,value)|value)).expect("multipart boundary");
                    let end = format!("--{boundary}--\r\n");
                    loop {
                        assert!(reader.read_until(b'\n', &mut body).await.unwrap() > 0, "truncated multipart request");
                        if body.ends_with(end.as_bytes()) { break; }
                    }
                }
                let response = response.to_string();
                reader.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
                (path, body)
            }).await.unwrap()
        });
        (interface, task)
    }

    fn sent() -> Value {
        json!({"ok":true,"result":{"message_id":42,"date":0,"chat":{"id":7,"type":"private"}}})
    }

    #[tokio::test]
    async fn photo_album_sends_one_ordered_group_with_caption_on_first_photo() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("first.jpg");
        std::fs::write(&file, "first photo bytes").unwrap();
        let message: OutgoingMessage = serde_json::from_value(json!({
            "type":"photo_album",
            "media":[
                {"source":"file","path":file},
                {"source":"url","url":"https://example.com/second.jpg"},
                {"source":"telegram","file_id":"third-photo"}
            ],
            "caption":"Album caption"
        }))
        .unwrap();
        let (interface, request) = server(json!({"ok":true,"result":[
            sent()["result"].clone(),
            {"message_id":43,"date":0,"chat":{"id":7,"type":"private"}},
            {"message_id":44,"date":0,"chat":{"id":7,"type":"private"}}
        ]}))
        .await;
        assert_eq!(
            interface
                .send_outgoing_message("7", &message)
                .await
                .unwrap(),
            MessageId("42".into())
        );
        let (path, body) = request.await.unwrap();
        assert!(path.to_lowercase().contains("/sendmediagroup "), "{path}");
        let body = String::from_utf8(body).unwrap();
        assert!(body.contains("first photo bytes"));
        let media: Value = serde_json::from_str(
            body.split("\r\n")
                .find(|line| line.starts_with('['))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(media.as_array().unwrap().len(), 3);
        assert_eq!(media[0]["type"], "photo");
        assert!(media[0]["media"].as_str().unwrap().starts_with("attach://"));
        assert_eq!(media[0]["caption"], "Album caption");
        assert_eq!(
            media[1],
            json!({"type":"photo","media":"https://example.com/second.jpg"})
        );
        assert_eq!(media[2], json!({"type":"photo","media":"third-photo"}));
    }

    #[tokio::test]
    async fn photo_album_audits_success_refusal_and_unknown_outcomes() {
        for (response, status) in [
            (
                json!({"ok":true,"result":[sent()["result"].clone(),sent()["result"].clone()]}),
                "sent",
            ),
            (
                json!({"ok":false,"error_code":400,"description":"Bad Request: invalid photo"}),
                "refused",
            ),
            (
                json!({"ok":false,"error_code":429,"description":"Too Many Requests: retry after 9","parameters":{"retry_after":9}}),
                "refused",
            ),
            (json!({"ok":true,"result":[]}), "unknown"),
            (json!({"ok":true,"result":{}}), "unknown"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let (interface, request) = server(response).await;
            let interface = interface.with_journal(crate::audit::Journal::new(root.path()));
            let message = serde_json::from_value(json!({
                "type":"photo_album", "media":[
                    {"source":"telegram","file_id":"PRIVATE_FIRST"},
                    {"source":"telegram","file_id":"PRIVATE_SECOND"}
                ], "caption":"PRIVATE_CAPTION"
            }))
            .unwrap();
            let result = interface.send_outgoing_message("7", &message).await;
            assert_eq!(result.is_ok(), status == "sent");
            if let Err(error) = &result {
                assert_eq!(
                    error.downcast_ref::<SendRejection>().is_some(),
                    status == "refused"
                );
                assert!(!format!("{error:#}").contains("test-secret"));
            }
            request.await.unwrap();
            let log = std::fs::read_to_string(
                root.path()
                    .join(format!("{}.jsonl", chrono::Utc::now().format("%Y-%m-%d"))),
            )
            .unwrap();
            assert!(!log.contains("PRIVATE_"));
            let rows: Vec<Value> = log
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0]["name"], "sendMediaGroup");
            assert_eq!(rows[1]["status"], status);
        }
    }

    #[tokio::test]
    async fn sends_all_media_using_real_multipart_uploads() {
        for kind in ["photo", "video", "document", "audio"] {
            let (interface, request) = server(sent()).await;
            let mut message: OutgoingMessage = serde_json::from_value(json!({"type":kind,"media":{"source":"telegram","file_id":"unused"},"caption":"Caption"})).unwrap();
            *message.media_mut().unwrap() = MediaSource::Upload {
                filename: "asset.bin".into(),
                data: b"actual file bytes".to_vec(),
            };
            assert_eq!(
                interface
                    .send_outgoing_message("7", &message)
                    .await
                    .unwrap(),
                MessageId("42".into())
            );
            let (path, body) = request.await.unwrap();
            assert!(path.to_lowercase().contains(&format!("/send{kind} ")));
            let body = String::from_utf8(body).unwrap();
            assert!(body.contains("actual file bytes"), "{body}");
            assert!(body.contains("asset.bin"), "{body}");
            assert!(body.contains("Caption"), "{body}");
        }
    }

    #[tokio::test]
    async fn ordinary_media_accepts_local_file_url_and_telegram_file_id() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.jpg");
        std::fs::write(&path, "local image bytes").unwrap();
        for (media, expected) in [
            (
                MediaSource::File {
                    path: path.to_string_lossy().into_owned(),
                },
                "local image bytes",
            ),
            (
                MediaSource::Url {
                    url: "https://example.com/image.jpg".into(),
                },
                "https://example.com/image.jpg",
            ),
            (
                MediaSource::Telegram {
                    file_id: "existing-file".into(),
                },
                "existing-file",
            ),
        ] {
            let (interface, request) = server(sent()).await;
            interface
                .send_outgoing_message(
                    "7",
                    &OutgoingMessage::Photo {
                        media,
                        caption: None,
                    },
                )
                .await
                .unwrap();
            let (_, body) = request.await.unwrap();
            let body = String::from_utf8(body).unwrap();
            assert!(body.contains(expected), "expected {expected:?} in {body:?}");
        }
    }

    #[tokio::test]
    async fn outgoing_poll_registers_its_chat_and_anonymity_for_reading() {
        let root = tempfile::tempdir().unwrap();
        let mut response = sent();
        response["result"]["poll"] = json!({"id":"p1","question":"Choose?","options":[{"text":"A","voter_count":0}],"total_voter_count":0,"is_anonymous":false,"is_closed":false});
        let (interface, request) = server(response).await;
        let interface = interface.with_space_root(root.path().into());
        let message = serde_json::from_value(json!({"type":"poll","question":"Choose?","options":["A"],"multiple_answers":false,"anonymous":false})).unwrap();
        assert_eq!(
            interface
                .send_outgoing_message("7", &message)
                .await
                .unwrap(),
            MessageId("42".into())
        );
        let (_, body) = request.await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["is_anonymous"],
            false
        );
        let stored = crate::interfaces::polls::PollStore::new(root.path(), "test")
            .read("7", Some("p1"), 0)
            .unwrap();
        assert_eq!(stored["message_id"], 42);
        assert_eq!(stored["poll"]["is_anonymous"], false);
    }

    #[tokio::test]
    async fn sends_native_poll_location_and_formatted_post() {
        for (message, method, expected) in [
            (
                json!({"type":"poll","question":"Choose","options":["A","B"],"multiple_answers":true}),
                "sendPoll",
                json!({"chat_id":7,"question":"Choose","options":[{"text":"A"},{"text":"B"}],"allows_multiple_answers":true,"is_anonymous":true}),
            ),
            (
                json!({"type":"location","latitude":12.3,"longitude":45.6}),
                "sendLocation",
                json!({"chat_id":7,"latitude":12.3,"longitude":45.6}),
            ),
            (
                json!({"type":"post","title":"Title","body":"**Body**"}),
                "sendRichMessage",
                json!({"chat_id":7,"rich_message":{"markdown":"# Title\n\n**Body**"}}),
            ),
        ] {
            let (interface, request) = server(sent()).await;
            interface
                .send_outgoing_message("7", &serde_json::from_value(message).unwrap())
                .await
                .unwrap();
            let (path, body) = request.await.unwrap();
            assert!(path.contains(method), "{path}");
            assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn guest_uses_inline_results_and_never_uploads() {
        for (kind, source, field) in [
            ("photo", "url", "photo_url"),
            ("video", "telegram", "video_file_id"),
            ("document", "url", "document_url"),
            ("audio", "url", "audio_url"),
        ] {
            let media = if source == "url" {
                json!({"source":"url","url":"https://example.com/asset.pdf"})
            } else {
                json!({"source":"telegram","file_id":"file-123"})
            };
            let (interface, request) =
                server(json!({"ok":true,"result":{"inline_message_id":"inline-42"}})).await;
            let message =
                serde_json::from_value(json!({"type":kind,"media":media,"caption":"Caption"}))
                    .unwrap();
            assert_eq!(
                interface
                    .answer_guest_outgoing_message("query-1", &message)
                    .await
                    .unwrap(),
                MessageId("inline-42".into())
            );
            let (path, body) = request.await.unwrap();
            assert!(path.contains("/answerGuestQuery "), "{path}");
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["guest_query_id"], "query-1");
            assert_eq!(body["result"]["type"], kind);
            assert!(body["result"][field].is_string());
            if kind == "audio" {
                assert_eq!(body["result"]["title"], "asset.pdf");
            }
        }
        let interface =
            TelegramInterface::new("test", "123:test-secret", std::path::PathBuf::new());
        let message = OutgoingMessage::Photo {
            media: MediaSource::Upload {
                filename: "x".into(),
                data: vec![1],
            },
            caption: None,
        };
        assert!(
            interface
                .answer_guest_outgoing_message("query", &message)
                .await
                .unwrap_err()
                .to_string()
                .contains("Guest")
        );
    }

    #[tokio::test]
    async fn guest_video_requires_thumbnail_and_documents_require_supported_url_types() {
        let interface =
            TelegramInterface::new("test", "123:test-secret", std::path::PathBuf::new());
        for (message, expected) in [
            (
                json!({"type":"video","media":{"source":"url","url":"https://example.com/video.mp4"}}),
                "thumbnail_url",
            ),
            (
                json!({"type":"document","media":{"source":"url","url":"https://example.com/file.txt"}}),
                "PDF or ZIP",
            ),
            (
                json!({"type":"poll","question":"Choose","options":["A","B"]}),
                "Guest",
            ),
        ] {
            assert!(
                interface
                    .answer_guest_outgoing_message(
                        "query",
                        &serde_json::from_value(message).unwrap()
                    )
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains(expected)
            );
        }
        let (interface, request) =
            server(json!({"ok":true,"result":{"inline_message_id":"video-inline"}})).await;
        let message = serde_json::from_value(json!({"type":"video","media":{"source":"url","url":"https://example.com/video.mp4"},"thumbnail_url":"https://example.com/thumb.jpg"})).unwrap();
        interface
            .answer_guest_outgoing_message("query", &message)
            .await
            .unwrap();
        let (_, body) = request.await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body["result"]["thumbnail_url"],
            "https://example.com/thumb.jpg"
        );
        assert_eq!(body["result"]["mime_type"], "video/mp4");
    }

    #[tokio::test]
    async fn guest_posts_and_locations_use_single_inline_result() {
        for (message, expected) in [
            (
                json!({"type":"post","title":"Title","body":"**Body**"}),
                json!({"type":"article","id":"kodrik","title":"Title","input_message_content":{"rich_message":{"markdown":"# Title\n\n**Body**"}}}),
            ),
            (
                json!({"type":"location","latitude":12.3,"longitude":45.6}),
                json!({"type":"location","id":"kodrik","title":"Location","latitude":12.3,"longitude":45.6}),
            ),
        ] {
            let (interface, request) =
                server(json!({"ok":true,"result":{"inline_message_id":"inline"}})).await;
            interface
                .answer_guest_outgoing_message("query", &serde_json::from_value(message).unwrap())
                .await
                .unwrap();
            let (path, body) = request.await.unwrap();
            assert!(path.contains("/answerGuestQuery "));
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap(),
                json!({"guest_query_id":"query","result":expected})
            );
        }
    }

    #[tokio::test]
    async fn media_audits_success_refusal_and_uncertain_response_without_content() {
        for (response, status) in [
            (sent(), "sent"),
            (
                json!({"ok":false,"error_code":403,"description":"Forbidden: bot was blocked by the user"}),
                "refused",
            ),
            (
                json!({"ok":false,"error_code":429,"description":"Too Many Requests: retry after 9","parameters":{"retry_after":9}}),
                "refused",
            ),
            (json!({"ok":true,"result":{}}), "unknown"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let (interface, request) = server(response).await;
            let interface = interface.with_journal(crate::audit::Journal::new(root.path()));
            let message = OutgoingMessage::Photo {
                media: MediaSource::Telegram {
                    file_id: "PRIVATE_FILE_ID".into(),
                },
                caption: Some("PRIVATE_CAPTION".into()),
            };
            let result = interface.send_outgoing_message("7", &message).await;
            assert_eq!(result.is_ok(), status == "sent");
            if let Err(error) = &result {
                assert_eq!(
                    error.downcast_ref::<SendRejection>().is_some(),
                    status == "refused"
                );
                assert!(!format!("{error:#}").contains("test-secret"));
            }
            request.await.unwrap();
            let log = std::fs::read_to_string(
                root.path()
                    .join(format!("{}.jsonl", chrono::Utc::now().format("%Y-%m-%d"))),
            )
            .unwrap();
            assert!(!log.contains("PRIVATE_"));
            let rows: Vec<Value> = log
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[1]["status"], status);
        }
    }

    #[tokio::test]
    async fn media_network_errors_do_not_leak_token_or_claim_confirmed_refusal() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut interface =
            TelegramInterface::new("test", "123:test-secret", std::path::PathBuf::new());
        interface.bot = interface.bot.set_api_url(
            format!("http://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
        );
        let task = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            drop(socket);
        });
        let message = OutgoingMessage::Audio {
            media: MediaSource::Telegram {
                file_id: "file".into(),
            },
            caption: None,
        };
        let error = interface
            .send_outgoing_message("7", &message)
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<SendRejection>().is_none());
        assert!(!format!("{error:#}").contains("test-secret"));
        assert!(error.to_string().contains("request failed"));
        task.await.unwrap();
    }
}
