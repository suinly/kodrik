use std::sync::Arc;

use serde::Deserialize;
use tokio::sync::Mutex;

use super::{Tool, ToolOutput};
use crate::{
    Interface, MessageId,
    interfaces::{MediaSource, OutgoingMessage, SendRejection},
};

tokio::task_local! {
    pub(crate) static REPLY_CONTEXT: ReplyContext;
}

#[derive(Clone)]
pub(crate) struct ReplyContext {
    pub(super) interface: Arc<dyn Interface>,
    pub(super) chat: String,
    pub(super) guest_query: Option<String>,
    state: Arc<Mutex<ReplyState>>,
}

#[derive(Default)]
struct ReplyState {
    delivered: Option<MessageId>,
    uncertain: bool,
    failed: bool,
    final_delivery: Option<Arc<crate::agent::session::ExecutionOwnership>>,
}

impl ReplyContext {
    pub(crate) fn new(
        interface: Arc<dyn Interface>,
        chat: String,
        guest_query: Option<String>,
    ) -> Self {
        Self {
            interface,
            chat,
            guest_query,
            state: Arc::new(Mutex::new(ReplyState::default())),
        }
    }

    pub(crate) async fn hold_final_delivery(
        &self,
        store: &crate::agent::session::SqliteSessionStore,
        run: &str,
    ) -> anyhow::Result<()> {
        if self.guest_query.is_none() && self.interface.id().0.starts_with("telegram:") {
            self.state.lock().await.final_delivery = Some(
                store
                    .execution_lock("delivery", run)
                    .await
                    .map_err(crate::agent::delivery::storage_failure)?,
            );
        }
        Ok(())
    }
    pub(crate) async fn take_final_delivery(
        &self,
    ) -> Option<Arc<crate::agent::session::ExecutionOwnership>> {
        self.state.lock().await.final_delivery.take()
    }
    pub(crate) async fn delivered(&self) -> Option<MessageId> {
        let state = self.state.lock().await;
        if state.failed {
            None
        } else {
            state.delivered.clone()
        }
    }
    pub(crate) async fn guest_consumed(&self) -> bool {
        let state = self.state.lock().await;
        state.delivered.is_some() || state.uncertain
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReplyArgs {
    message: OutgoingMessage,
}

pub struct TelegramReplyTool {
    sandbox: Option<Arc<crate::agent::sandbox::Sandbox>>,
}

impl TelegramReplyTool {
    pub(crate) fn new(sandbox: Option<Arc<crate::agent::sandbox::Sandbox>>) -> Self {
        Self { sandbox }
    }
}

#[async_trait::async_trait]
impl Tool for TelegramReplyTool {
    fn name(&self) -> String {
        "telegram_reply".into()
    }
    fn description(&self) -> String {
        "Send a photo, photo album, video, document, audio, poll, location, or formatted post as your reply in the CURRENT Telegram chat. The application binds the recipient; you cannot choose another chat. Use photo_album to send 2–10 photos together: media is an ordered array of sources, and caption is shared on the first photo only. Media sources: file (local or this conversation's virtual filesystem), url (HTTP/HTTPS), telegram (existing file_id). Photo uploads are limited to 10 MB each; captions to 1024 characters. This tool sends existing media; it does not generate it. Use document for other file formats. Poll anonymous defaults true; set false only when individual votes should be visible. Read saved poll results using telegram_results. Post body uses the existing rich Markdown format. For nullable optional fields, pass null when unused. Video thumbnail_url is for guest URL video results only. Guest queries accept one reply: photo/video/document/audio via public URL or existing file_id, location or post; no file uploads, photo albums or polls. URL documents in guest mode support PDF/ZIP only. A successful tool result IS the reply: do not repeat it in your final text. An uncertain delivery is not safe to retry.".into()
    }
    fn parameters(&self) -> serde_json::Value {
        let mut schema = schemars::schema_for!(ReplyArgs).to_value();
        if REPLY_CONTEXT
            .try_with(|context| context.guest_query.is_some())
            .unwrap_or(false)
        {
            restrict_variants(&mut schema, &["poll", "file", "photo_album"]);
        }
        strict_schema(&mut schema);
        schema
    }
    fn available(&self) -> bool {
        REPLY_CONTEXT
            .try_with(|context| context.interface.id().0.starts_with("telegram:"))
            .unwrap_or(false)
    }
    async fn execute(&self, properties: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let context = REPLY_CONTEXT.try_with(Clone::clone).map_err(|_| {
            anyhow::anyhow!("Telegram replies require an active Telegram conversation")
        })?;
        anyhow::ensure!(self.available(), "Telegram replies are unavailable here");
        context.state.lock().await.failed = true;
        let mut args: ReplyArgs = serde_json::from_value(properties)?;
        args.message.validate()?;
        if context.guest_query.is_some() {
            args.message.guest_result()?;
        } else {
            let upload_limit = if matches!(
                args.message,
                OutgoingMessage::Photo { .. } | OutgoingMessage::PhotoAlbum { .. }
            ) {
                10_000_000
            } else {
                50_000_000
            };
            for media in args.message.media_sources_mut() {
                let MediaSource::File { path } = media else {
                    continue;
                };
                let filename = std::path::Path::new(path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("attachment")
                    .to_owned();
                let data = if let Some(sandbox) = &self.sandbox {
                    sandbox.read_file(path).await?
                } else {
                    let path = path.clone();
                    tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
                        use std::io::Read;
                        let metadata = std::fs::metadata(&path)?;
                        anyhow::ensure!(metadata.is_file(), "Attachment must be a regular file");
                        anyhow::ensure!(
                            metadata.len() <= upload_limit,
                            "Attachment exceeds {upload_limit} bytes"
                        );
                        let mut bytes = Vec::new();
                        std::fs::File::open(path)?
                            .take(upload_limit + 1)
                            .read_to_end(&mut bytes)?;
                        Ok(bytes)
                    })
                    .await??
                };
                anyhow::ensure!(
                    !data.is_empty() && data.len() as u64 <= upload_limit,
                    "Upload must contain 1–{upload_limit} bytes"
                );
                *media = MediaSource::Upload { filename, data };
            }
            args.message.validate()?;
        }
        let mut state = context.state.lock().await;
        anyhow::ensure!(
            !state.uncertain,
            "Earlier delivery outcome is unknown; do not retry"
        );
        anyhow::ensure!(
            context.guest_query.is_none() || state.delivered.is_none(),
            "Guest query already answered"
        );
        // Cancellation during the HTTP request must also consume an uncertain guest slot.
        state.uncertain = true;
        state.failed = true;
        let result = if let Some(query) = &context.guest_query {
            context
                .interface
                .answer_guest_content(query, &args.message)
                .await
        } else if let Some((store, run, conversation)) = crate::agent::delivery::CURRENT_DELIVERY
            .try_with(Clone::clone)
            .ok()
            .flatten()
        {
            let (_, call) = crate::agent::run_state::CURRENT_TOOL
                .try_with(Clone::clone)
                .ok()
                .flatten()
                .ok_or_else(|| anyhow::anyhow!("Missing durable tool identity"))?;
            let id = store
                .queue_explicit(
                    &run,
                    &call,
                    &conversation,
                    &context.interface.id().0,
                    &context.chat,
                    &args.message,
                )
                .await?;
            store
                .deliver_receipt(context.interface.as_ref(), &id, chrono::Utc::now(), None)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Reply delivery pending or outcome uncertain; do not send it again"
                    )
                })
        } else {
            context
                .interface
                .send_content(&context.chat, &args.message)
                .await
        };
        match result {
            Ok(id) => {
                state.delivered = Some(id.clone());
                state.uncertain = false;
                state.failed = false;
                Ok(serde_json::json!({"status":"sent","message_id":id.0}).into())
            }
            Err(error) => {
                if error.downcast_ref::<SendRejection>().is_some() {
                    state.uncertain = false;
                }
                Err(error)
            }
        }
    }
}

fn strict_schema(schema: &mut serde_json::Value) {
    match schema {
        serde_json::Value::Object(object) => {
            if let Some(variants) = object.remove("oneOf") {
                object.insert("anyOf".into(), variants);
            }
            if let Some(serde_json::Value::Object(properties)) = object.get("properties") {
                let required: Vec<_> = properties.keys().cloned().collect();
                object.insert("required".into(), serde_json::json!(required));
                object.insert("additionalProperties".into(), serde_json::json!(false));
            }
            for value in object.values_mut() {
                strict_schema(value);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                strict_schema(value);
            }
        }
        _ => {}
    }
}

fn restrict_variants(schema: &mut serde_json::Value, excluded: &[&str]) {
    match schema {
        serde_json::Value::Object(object) => {
            for key in ["oneOf", "anyOf"] {
                if let Some(serde_json::Value::Array(variants)) = object.get_mut(key) {
                    variants.retain(|variant| {
                        let tag = variant["properties"]["type"]["const"]
                            .as_str()
                            .or_else(|| variant["properties"]["source"]["const"].as_str());
                        !tag.is_some_and(|tag| excluded.contains(&tag))
                    });
                }
            }
            for value in object.values_mut() {
                restrict_variants(value, excluded);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                restrict_variants(value, excluded);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        MessageId,
        gateway::{Event, InterfaceId},
        interfaces::{MediaSource, SendRejection},
    };
    use serde_json::json;
    use tokio::sync::mpsc;

    #[derive(Default)]
    struct Recorder {
        seen: Mutex<Vec<(String, String)>>,
        failure: Option<bool>,
        pending: bool,
    }

    #[async_trait::async_trait]
    impl Interface for Recorder {
        fn id(&self) -> InterfaceId {
            InterfaceId("telegram:test".into())
        }
        async fn run(&self, _: mpsc::Sender<Event>) -> anyhow::Result<()> {
            Ok(())
        }
        async fn send(&self, _: &str, _: &str) -> anyhow::Result<MessageId> {
            unreachable!()
        }
        async fn send_content(
            &self,
            chat: &str,
            message: &OutgoingMessage,
        ) -> anyhow::Result<MessageId> {
            let content = match message {
                OutgoingMessage::Post { body, .. } => body.clone(),
                OutgoingMessage::Document {
                    media: MediaSource::Upload { data, .. },
                    ..
                } => String::from_utf8(data.clone()).unwrap(),
                _ => serde_json::to_string(message).unwrap(),
            };
            self.seen.lock().await.push((chat.into(), content));
            if self.pending {
                std::future::pending::<()>().await;
            }
            match self.failure {
                Some(true) => Err(SendRejection {
                    description: "rejected".into(),
                    error_code: 400,
                    retry_after: None,
                }
                .into()),
                Some(false) => anyhow::bail!("connection lost"),
                None => Ok(MessageId("1".into())),
            }
        }
        async fn answer_guest_content(
            &self,
            query: &str,
            message: &OutgoingMessage,
        ) -> anyhow::Result<MessageId> {
            self.send_content(query, message).await
        }
    }

    fn post() -> serde_json::Value {
        json!({"message":{"type":"post","title":"Report","body":"hello"}})
    }

    #[tokio::test]
    async fn schema_is_strict_nullable_and_limits_guest_variants() {
        fn check(value: &serde_json::Value) {
            match value {
                serde_json::Value::Object(object) => {
                    assert!(!object.contains_key("oneOf"));
                    if let Some(properties) =
                        object.get("properties").and_then(|value| value.as_object())
                    {
                        assert_eq!(object["additionalProperties"], false);
                        let required = object["required"].as_array().unwrap();
                        assert_eq!(required.len(), properties.len());
                        for key in properties.keys() {
                            assert!(required.contains(&json!(key)));
                        }
                        if let Some(caption) = properties.get("caption") {
                            assert_eq!(caption["type"], json!(["string", "null"]));
                        }
                    }
                    for value in object.values() {
                        check(value);
                    }
                }
                serde_json::Value::Array(values) => {
                    for value in values {
                        check(value);
                    }
                }
                _ => {}
            }
        }
        let tool = TelegramReplyTool::new(None);
        let normal = tool.parameters();
        check(&normal);
        let normal = normal.to_string();
        for tag in ["poll", "file", "photo_album"] {
            assert!(normal.contains(&format!("\"const\":\"{tag}\"")));
        }
        assert!(!normal.contains("\"const\":\"checklist\""));
        let context = ReplyContext::new(
            Arc::new(Recorder::default()),
            "42".into(),
            Some("query".into()),
        );
        REPLY_CONTEXT
            .scope(context, async {
                let guest = tool.parameters();
                check(&guest);
                let guest = guest.to_string();
                for tag in ["poll", "checklist", "file", "upload", "photo_album"] {
                    assert!(!guest.contains(&format!("\"const\":\"{tag}\"")));
                }
                for tag in [
                    "post", "location", "photo", "video", "document", "audio", "url", "telegram",
                ] {
                    assert!(guest.contains(&format!("\"const\":\"{tag}\"")));
                }
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_an_inflight_guest_send_consumes_its_slot() {
        let interface = Arc::new(Recorder {
            pending: true,
            ..Default::default()
        });
        let context = ReplyContext::new(interface.clone(), "42".into(), Some("query".into()));
        REPLY_CONTEXT
            .scope(context.clone(), async {
                let tool = TelegramReplyTool::new(None);
                assert!(
                    tokio::time::timeout(std::time::Duration::from_secs(1), tool.execute(post()))
                        .await
                        .is_err()
                );
                assert!(context.guest_consumed().await);
                assert!(tool.execute(post()).await.is_err());
                assert_eq!(interface.seen.lock().await.len(), 1);
            })
            .await;
    }

    #[tokio::test]
    async fn replies_bind_concurrent_recipients_and_reject_overrides() {
        let interface = Arc::new(Recorder::default());
        let tool = Arc::new(TelegramReplyTool::new(None));
        assert!(!tool.available());
        assert!(tool.execute(post()).await.is_err());
        let mut tasks = Vec::new();
        for chat in ["42", "7"] {
            let context = ReplyContext::new(interface.clone(), chat.into(), None);
            let tool = tool.clone();
            tasks.push(tokio::spawn(REPLY_CONTEXT.scope(context.clone(), async move {
                assert!(tool.available());
                assert!(tool.execute(json!({"message":{"type":"post","title":"Report","body":"hello"},"chat":"999"})).await.is_err());
                tool.execute(post()).await.unwrap();
                assert!(context.delivered().await.is_some());
            })));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let mut seen = interface.seen.lock().await.clone();
        seen.sort();
        assert_eq!(
            seen,
            vec![("42".into(), "hello".into()), ("7".into(), "hello".into())]
        );
    }

    #[tokio::test]
    async fn guests_have_one_slot_and_reject_unsupported_content_before_sending() {
        let interface = Arc::new(Recorder::default());
        let context = ReplyContext::new(interface.clone(), "42".into(), Some("guest-query".into()));
        REPLY_CONTEXT.scope(context.clone(), async {
            let tool = TelegramReplyTool::new(None);
            assert!(tool.execute(json!({"message":{"type":"photo_album","media":[{"source":"telegram","file_id":"first"},{"source":"telegram","file_id":"second"}],"caption":null}})).await.is_err());
            assert!(tool.execute(json!({"message":{"type":"poll","question":"Choose?","options":["a","b"],"multiple_answers":null}})).await.is_err());
            assert!(tool.execute(json!({"message":{"type":"document","media":{"source":"file","path":"/private/secret"},"caption":null}})).await.is_err());
            assert!(!context.guest_consumed().await);
            tool.execute(post()).await.unwrap();
            assert!(tool.execute(post()).await.is_err());
            assert_eq!(interface.seen.lock().await.as_slice(), &[("guest-query".into(), "hello".into())]);
        }).await;
    }

    #[tokio::test]
    async fn photo_album_prepares_all_files_before_sending() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.jpg");
        let second = directory.path().join("second.jpg");
        std::fs::write(&first, "first photo").unwrap();
        let interface = Arc::new(Recorder::default());
        let context = ReplyContext::new(interface.clone(), "42".into(), None);
        let args = json!({"message":{"type":"photo_album","media":[
            {"source":"file","path":first}, {"source":"file","path":second},
            {"source":"url","url":"https://example.com/third.jpg"}
        ],"caption":"Photos"}});
        REPLY_CONTEXT
            .scope(context.clone(), async {
                let tool = TelegramReplyTool::new(None);
                assert!(tool.execute(args.clone()).await.is_err());
                assert!(interface.seen.lock().await.is_empty());
                std::fs::write(&second, "second photo").unwrap();
                tool.execute(args).await.unwrap();
                assert!(context.delivered().await.is_some());
            })
            .await;
        let seen = interface.seen.lock().await;
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "42");
        let sent: serde_json::Value = serde_json::from_str(&seen[0].1).unwrap();
        assert_eq!(sent["media"][0]["source"], "upload");
        assert_eq!(sent["media"][0]["filename"], "first.jpg");
        assert_eq!(sent["media"][0]["data"], json!(b"first photo".as_slice()));
        assert_eq!(sent["media"][1]["data"], json!(b"second photo".as_slice()));
        assert_eq!(
            sent["media"][2],
            json!({"source":"url","url":"https://example.com/third.jpg"})
        );
    }

    #[tokio::test]
    async fn photo_album_rejects_oversized_file_before_loading_the_next_photo() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("oversized.jpg");
        std::fs::File::create(&first)
            .unwrap()
            .set_len(10_000_001)
            .unwrap();
        let interface = Arc::new(Recorder::default());
        let context = ReplyContext::new(interface.clone(), "42".into(), None);
        REPLY_CONTEXT
            .scope(context, async {
                let error = TelegramReplyTool::new(None)
                    .execute(json!({"message":{
                        "type":"photo_album", "media":[
                            {"source":"file","path":first},
                            {"source":"file","path":directory.path().join("missing.jpg")}
                        ], "caption":null
                    }}))
                    .await
                    .unwrap_err();
                assert!(error.to_string().contains("10000000"), "{error:#}");
            })
            .await;
        assert!(interface.seen.lock().await.is_empty());
    }

    #[tokio::test]
    async fn photo_album_respects_sandbox_filesystem_for_every_photo() {
        let directory = tempfile::tempdir().unwrap();
        let sandbox = Arc::new(crate::agent::sandbox::Sandbox::new(
            directory.path().to_owned(),
        ));
        sandbox.tool("exec_command").execute(json!({"cmd":"printf 'first virtual photo' > /workspace/first.jpg; printf 'second virtual photo' > /workspace/second.jpg"})).await.unwrap();
        let interface = Arc::new(Recorder::default());
        let context = ReplyContext::new(interface.clone(), "42".into(), None);
        REPLY_CONTEXT
            .scope(context, async {
                let tool = TelegramReplyTool::new(Some(sandbox));
                let args = |second| {
                    json!({"message":{"type":"photo_album","media":[
                {"source":"file","path":"/workspace/first.jpg"}, {"source":"file","path":second}
            ],"caption":null}})
                };
                assert!(tool.execute(args("/etc/passwd")).await.is_err());
                assert!(interface.seen.lock().await.is_empty());
                tool.execute(args("/workspace/second.jpg")).await.unwrap();
            })
            .await;
        let seen = interface.seen.lock().await;
        let sent: serde_json::Value = serde_json::from_str(&seen[0].1).unwrap();
        assert_eq!(
            sent["media"][0]["data"],
            json!(b"first virtual photo".as_slice())
        );
        assert_eq!(
            sent["media"][1]["data"],
            json!(b"second virtual photo".as_slice())
        );
    }

    #[tokio::test]
    async fn failed_preflight_after_a_success_keeps_the_final_explanation() {
        let context = ReplyContext::new(Arc::new(Recorder::default()), "42".into(), None);
        REPLY_CONTEXT.scope(context.clone(), async {
            let tool = TelegramReplyTool::new(None);
            tool.execute(post()).await.unwrap();
            assert!(context.delivered().await.is_some());
            assert!(tool.execute(json!({"message":{"type":"document","media":{"source":"file","path":"/missing/report.pdf"},"caption":null}})).await.is_err());
            assert!(context.delivered().await.is_none());
        }).await;
    }

    #[tokio::test]
    async fn uncertain_delivery_is_not_retried_but_confirmed_refusal_can_be_corrected() {
        for refused in [false, true] {
            let interface = Arc::new(Recorder {
                failure: Some(refused),
                ..Default::default()
            });
            let context =
                ReplyContext::new(interface.clone(), "42".into(), Some("guest-query".into()));
            REPLY_CONTEXT
                .scope(context.clone(), async {
                    let tool = TelegramReplyTool::new(None);
                    assert!(tool.execute(post()).await.is_err());
                    assert_eq!(context.guest_consumed().await, !refused);
                    assert!(tool.execute(post()).await.is_err());
                    assert_eq!(
                        interface.seen.lock().await.len(),
                        if refused { 2 } else { 1 }
                    );
                })
                .await;
        }
    }

    #[tokio::test]
    async fn isolated_replies_read_virtual_files_instead_of_host_files() {
        let directory = tempfile::tempdir().unwrap();
        let sandbox = Arc::new(crate::agent::sandbox::Sandbox::new(
            directory.path().to_owned(),
        ));
        sandbox
            .tool("exec_command")
            .execute(json!({"cmd":"printf 'virtual report' > /workspace/report.txt"}))
            .await
            .unwrap();
        let interface = Arc::new(Recorder::default());
        let context = ReplyContext::new(interface.clone(), "-42".into(), None);
        REPLY_CONTEXT.scope(context, async {
            let tool = TelegramReplyTool::new(Some(sandbox));
            let args = |path: &str| json!({"message":{"type":"document","media":{"source":"file","path":path},"caption":null}});
            tool.execute(args("/workspace/report.txt")).await.unwrap();
            assert!(tool.execute(args("/etc/passwd")).await.is_err());
        }).await;
        assert_eq!(
            interface.seen.lock().await.as_slice(),
            &[("-42".into(), "virtual report".into())]
        );
    }
}
