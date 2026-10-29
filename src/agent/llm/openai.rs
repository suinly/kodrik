use crate::agent::{
    AgentEvent, AssistantMessage, Image, LlmContext,
    llm::provider::LlmProvider,
    message::{Message, ToolCall},
    tools::Tool,
};
use anyhow::Context;
use async_openai::{
    Client,
    config::OpenAIConfig,
    types::responses::{
        CreateResponseArgs, EasyInputContent, EasyInputMessage, FunctionCallOutput,
        FunctionCallOutputItemParam, FunctionTool, FunctionToolCall, ImageDetail, InputContent,
        InputFileContent, InputImageContent, InputItem, InputTextContent, Item, Role,
        Tool as OpenAiTool,
    },
};
use futures::StreamExt;
use serde::Deserialize;
use tokio::sync::mpsc;

#[derive(Deserialize)]
#[serde(tag = "type")]
enum StreamEvent {
    #[serde(
        rename = "response.output_text.delta",
        alias = "response.refusal.delta"
    )]
    TextDelta { delta: String },
    #[serde(rename = "response.completed")]
    Completed { response: ResponseOutput },
    #[serde(rename = "response.failed", alias = "response.incomplete")]
    Failed { response: serde_json::Value },
    #[serde(rename = "error")]
    Error {
        message: Option<String>,
        code: Option<String>,
        error: Option<serde_json::Value>,
    },
    #[serde(other)]
    Other,
}

// Compatible backends may omit SDK metadata; require the fields we actually consume.
#[derive(Deserialize)]
struct ResponseOutput {
    status: Option<String>,
    incomplete_details: Option<serde_json::Value>,
    error: Option<serde_json::Value>,
    output: Vec<ResponseItem>,
    usage: Option<ResponseUsage>,
}

#[derive(Deserialize)]
struct ResponseUsage {
    input_tokens: u64,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponseItem {
    Message {
        status: Option<String>,
        content: Vec<ResponseContent>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        namespace: Option<String>,
        arguments: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponseContent {
    OutputText { text: String },
    Refusal { refusal: String },
}

impl ResponseOutput {
    fn validate_compaction(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.status
                .as_deref()
                .is_none_or(|status| status == "completed")
                && self.incomplete_details.is_none()
                && self.error.is_none()
                && self.output.iter().all(|item| match item {
                    ResponseItem::Message { status, content } => {
                        status.as_deref().is_none_or(|status| status == "completed")
                            && content
                                .iter()
                                .all(|part| matches!(part, ResponseContent::OutputText { .. }))
                    }
                    _ => true,
                }),
            "Incomplete or refused compaction response; history was preserved"
        );
        Ok(())
    }

    fn into_messages(self) -> anyhow::Result<Vec<Message>> {
        self.output
            .into_iter()
            .filter_map(|item| match item {
                ResponseItem::Message { content, .. } => {
                    let content = content
                        .into_iter()
                        .map(|content| match content {
                            ResponseContent::OutputText { text } => text,
                            ResponseContent::Refusal { refusal } => refusal,
                        })
                        .collect::<String>();
                    Some(Ok(Message::Assistant(AssistantMessage { content })))
                }
                ResponseItem::FunctionCall {
                    call_id,
                    name,
                    namespace,
                    arguments,
                } => Some(
                    serde_json::from_str(&arguments)
                        .map(|arguments| {
                            Message::ToolCall(ToolCall {
                                call_id,
                                name: match namespace {
                                    Some(namespace)
                                        if namespace.starts_with("mcp__")
                                            && !name.starts_with(&format!("{namespace}__")) =>
                                    {
                                        format!("{namespace}__{name}")
                                    }
                                    _ => name,
                                },
                                arguments,
                            })
                        })
                        .map_err(anyhow::Error::from),
                ),
                ResponseItem::Other => None,
            })
            .collect()
    }
}

tokio::task_local! {
    static RETRY_AFTER: std::cell::Cell<Option<std::time::Duration>>;
}

fn retry_after(value: &str) -> Option<std::time::Duration> {
    value
        .parse::<u64>()
        .ok()
        .map(std::time::Duration::from_secs)
        .or_else(|| {
            let at = chrono::DateTime::parse_from_rfc2822(value)
                .ok()?
                .with_timezone(&chrono::Utc);
            Some((at - chrono::Utc::now()).to_std().unwrap_or_default())
        })
}

pub struct OpenAiLlm {
    client: Client<OpenAIConfig>,
    model: String,
    max_output_tokens: Option<u32>,
    retries: u32,
    compaction: bool,
    access: Option<(crate::web::auth::AuthStore, u64)>,
}

fn image_content(text: String, images: &[Image]) -> Vec<InputContent> {
    std::iter::once(InputContent::InputText(InputTextContent {
        text,
        prompt_cache_breakpoint: None,
    }))
    .chain(images.iter().map(|image| {
        InputContent::InputImage(InputImageContent {
            image_url: Some(image.data_url.clone()),
            detail: ImageDetail::High,
            ..Default::default()
        })
    }))
    .collect()
}

fn is_overflow_code(code: Option<&str>) -> bool {
    code == Some("context_length_exceeded")
}

fn classify_error(error: anyhow::Error) -> anyhow::Error {
    if let Some(async_openai::error::OpenAIError::ApiError(response)) = error.downcast_ref()
        && is_overflow_code(response.api_error.code.as_deref())
    {
        return super::ContextOverflow.into();
    }
    error
}

fn retryable(error: &anyhow::Error) -> bool {
    if error.is::<super::ContextOverflow>() {
        return false;
    }
    if let Some(async_openai::error::OpenAIError::ApiError(response)) = error.downcast_ref() {
        return response.status_code.is_server_error()
            || response.status_code == reqwest::StatusCode::TOO_MANY_REQUESTS
            || response.status_code == reqwest::StatusCode::REQUEST_TIMEOUT;
    }
    true
}

impl OpenAiLlm {
    pub fn new(
        api_key: impl Into<String>,
        model: Option<impl Into<String>>,
        api_base: Option<impl Into<String>>,
    ) -> Self {
        let mut config = OpenAIConfig::new().with_api_key(api_key.into());

        if let Some(api_base) = api_base {
            config = config.with_api_base(api_base.into());
        }

        let http = reqwest::Client::new();
        let service = tower::service_fn(
            move |request: async_openai::middleware::HttpRequestFactory| {
                let http = http.clone();
                async move {
                    let response = http.execute(request.build().await?).await?;
                    let delay = response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|h| h.to_str().ok())
                        .and_then(retry_after);
                    let _ = RETRY_AFTER.try_with(|saved| saved.set(delay));
                    Ok::<_, async_openai::error::OpenAIError>(response)
                }
            },
        );
        let client = Client::with_config(config).with_http_service(service);

        Self {
            client,
            max_output_tokens: None,
            retries: 3,
            compaction: false,
            access: None,
            model: match model {
                Some(model) => model.into(),
                None => String::from("gpt-6-sol"),
            },
        }
    }

    pub(crate) fn for_compaction(mut self) -> Self {
        self.retries = 0;
        self.compaction = true;
        self
    }

    pub(crate) fn with_access(
        mut self,
        access: Option<(crate::web::auth::AuthStore, u64)>,
    ) -> Self {
        self.access = access;
        self
    }

    fn check_access(&self) -> anyhow::Result<()> {
        if let Some((auth, user)) = &self.access {
            anyhow::ensure!(auth.allowed_user(*user)?, "Access requires an invitation");
        }
        Ok(())
    }

    pub(crate) fn with_max_output_tokens(mut self, tokens: u32) -> Self {
        self.max_output_tokens = Some(tokens);
        self
    }

    pub(crate) fn without_retries(mut self) -> Self {
        self.retries = 0;
        self
    }
}

impl OpenAiLlm {
    async fn generate_once(
        &self,
        context: &LlmContext<'_>,
        tools: &[&dyn Tool],
        progress: Option<&mpsc::Sender<AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        let file_bytes = context
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::User(user) => Some(&user.files),
                _ => None,
            })
            .flatten()
            .try_fold(0usize, |size, file| {
                size.checked_add(file.size)
                    .context("Attachment sizes overflow")
            })?;
        anyhow::ensure!(
            file_bytes <= crate::agent::file::MAX_REQUEST_FILE_BYTES,
            "Files in this conversation exceed the 50 MB request limit. Start a new session with /new."
        );
        let mut input_items = Vec::new();
        for message in context.messages.iter() {
            let item = match message {
                Message::User(msg) => InputItem::EasyMessage(EasyInputMessage {
                    role: Role::User,
                    content: if msg.images.is_empty() && msg.files.is_empty() {
                        EasyInputContent::Text(msg.content.clone())
                    } else {
                        let mut content = image_content(msg.content.clone(), &msg.images);
                        for file in &msg.files {
                            content.push(InputContent::InputFile(InputFileContent {
                                filename: Some(file.filename.clone()),
                                file_data: Some(file.data_url().await?),
                                ..Default::default()
                            }));
                        }
                        EasyInputContent::ContentList(content)
                    },
                    ..Default::default()
                }),
                Message::Assistant(msg) => InputItem::EasyMessage(EasyInputMessage {
                    role: Role::Assistant,
                    content: EasyInputContent::Text(msg.content.clone()),
                    ..Default::default()
                }),
                Message::ToolCall(call) => InputItem::Item(Item::FunctionCall(FunctionToolCall {
                    call_id: call.call_id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.to_string(),
                    namespace: None,
                    id: None,
                    status: None,
                    caller: None,
                    r#async: None,
                })),
                Message::ToolResult(result) => {
                    InputItem::Item(Item::FunctionCallOutput(FunctionCallOutputItemParam {
                        call_id: Some(result.call_id.clone()),
                        output: if result.content.images.is_empty() {
                            FunctionCallOutput::Text(result.content.output.to_string())
                        } else {
                            FunctionCallOutput::Content(image_content(
                                result.content.output.to_string(),
                                &result.content.images,
                            ))
                        },
                        id: None,
                        status: None,
                        name: None,
                        namespace: None,
                        caller: None,
                    }))
                }
            };
            input_items.push(item);
        }

        let api_tools: Vec<OpenAiTool> = tools
            .iter()
            .map(|tool| {
                let mut parameters = tool.parameters();
                if let Some(schema) = parameters.as_object_mut() {
                    // schemars omits empty properties; LM Studio requires the field.
                    schema
                        .entry("properties")
                        .or_insert_with(|| serde_json::json!({}));
                }

                OpenAiTool::Function(FunctionTool {
                    name: tool.name(),
                    description: Some(tool.description()),
                    parameters: Some(parameters),
                    strict: Some(tool.strict_schema()),
                    ..Default::default()
                })
            })
            .collect();

        let mut request = CreateResponseArgs::default()
            .model(&self.model)
            .instructions(context.instructions.as_ref())
            .input(input_items)
            .tools(api_tools)
            .build()?;
        request.max_output_tokens = self.max_output_tokens;

        let response: ResponseOutput = if let Some(progress) = progress {
            request.stream = Some(true);
            let mut stream = self
                .client
                .responses()
                .create_stream_byot::<_, StreamEvent>(&request)
                .await?;
            let mut completed = None;
            while let Some(event) = stream.next().await {
                match event? {
                    StreamEvent::TextDelta { delta } => {
                        if !delta.is_empty() {
                            let _ = progress.send(AgentEvent::TextDelta(delta)).await;
                        }
                    }
                    StreamEvent::Completed { response } => {
                        completed = Some(response);
                        break;
                    }
                    StreamEvent::Failed { response } => {
                        if is_overflow_code(response["error"]["code"].as_str()) {
                            return Err(super::ContextOverflow.into());
                        }
                        anyhow::bail!(
                            "Response stream failed: {}",
                            response["error"]["message"]
                                .as_str()
                                .or_else(|| response["incomplete_details"]["reason"].as_str())
                                .unwrap_or("response did not complete")
                        );
                    }
                    StreamEvent::Error {
                        message,
                        code,
                        error,
                    } => {
                        if is_overflow_code(
                            code.as_deref()
                                .or_else(|| error.as_ref()?.get("code")?.as_str()),
                        ) {
                            return Err(super::ContextOverflow.into());
                        }
                        let message = message
                            .as_deref()
                            .or_else(|| error.as_ref()?.get("message")?.as_str())
                            .unwrap_or("unknown provider error");
                        anyhow::bail!("Response stream error: {message}")
                    }
                    StreamEvent::Other => {}
                }
            }
            completed.ok_or_else(|| anyhow::anyhow!("Response stream ended before completion"))?
        } else {
            self.client.responses().create_byot(&request).await?
        };
        if is_overflow_code(
            response
                .error
                .as_ref()
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str()),
        ) {
            return Err(super::ContextOverflow.into());
        }
        if self.compaction {
            response.validate_compaction()?;
        }
        let input_tokens = response.usage.as_ref().map(|usage| usage.input_tokens);
        Ok((response.into_messages()?, input_tokens))
    }
}

#[async_trait::async_trait]
impl LlmProvider for OpenAiLlm {
    fn identity(&self) -> String {
        format!("openai:{}", self.model)
    }

    async fn generate_attempt(
        &self,
        context: &LlmContext<'_>,
        tools: &[&dyn Tool],
        progress: Option<&mpsc::Sender<AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        self.check_access()?;
        RETRY_AFTER
            .scope(std::cell::Cell::new(None), async {
                self.generate_once(context, tools, progress)
                    .await
                    .map_err(|error| {
                        let error = classify_error(error);
                        if self.retries == 0 || !retryable(&error) {
                            return error;
                        }
                        super::provider::GenerationRetry {
                            limit: self.retries,
                            retry_after: RETRY_AFTER.with(|delay| delay.get()),
                            error,
                        }
                        .into()
                    })
            })
            .await
    }

    async fn generate(
        &self,
        context: &LlmContext<'_>,
        tools: &[&dyn Tool],
        progress: Option<&mpsc::Sender<AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        let mut retries = 0;
        loop {
            self.check_access()?;
            super::REQUEST_BUDGET
                .try_with(|budget| {
                    let left = budget.get();
                    anyhow::ensure!(left > 0, "Wakeup request budget exhausted");
                    budget.set(left - 1);
                    Ok(())
                })
                .unwrap_or(Ok(()))?;
            match self
                .generate_once(context, tools, progress)
                .await
                .map_err(classify_error)
            {
                Ok(response) => return Ok(response),
                Err(error) if retries < self.retries && retryable(&error) => {
                    let delay = 1 << retries;
                    retries += 1;
                    log::warn!("LLM request failed; retry {retries}/3 in {delay}s: {error:#}");
                    if let Some(tx) = progress {
                        let _ = tx
                            .send(AgentEvent::Retrying(format!(
                                "Request failed. Retry {retries}/3 in {delay} sec"
                            )))
                            .await;
                    }
                    // Retry only generation: completed tool calls remain in the context.
                    tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        sync::oneshot,
    };

    #[tokio::test]
    async fn revoked_user_cannot_make_new_provider_attempts_or_background_calls() {
        let root = tempfile::tempdir().unwrap();
        let auth = crate::web::auth::AuthStore::new(root.path());
        auth.grant_test_user(7).unwrap();
        let llm = OpenAiLlm::new("test", Some("test"), Some("http://127.0.0.1:9"))
            .with_access(Some((auth.clone(), 7)));
        llm.check_access().unwrap();
        auth.revoke_user(7).unwrap();
        let context = LlmContext {
            instructions: "test".into(),
            messages: (&[] as &[Message]).into(),
        };
        for result in [
            llm.generate_attempt(&context, &[], None).await,
            llm.generate(&context, &[], None).await,
        ] {
            assert_eq!(
                result.unwrap_err().to_string(),
                "Access requires an invitation"
            );
        }
        auth.grant_test_user(7).unwrap();
        llm.check_access().unwrap();
    }

    #[tokio::test]
    async fn overflow_and_other_bad_requests_do_not_use_generic_retry() {
        for (code, streaming, durable) in [
            ("context_length_exceeded", false, true),
            ("invalid_request", false, true),
            ("context_length_exceeded", false, false),
            ("invalid_request", false, false),
            ("context_length_exceeded", true, true),
            ("context_length_exceeded", true, false),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(socket);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).await.unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':')
                        && key.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
                reader.read_exact(&mut vec![0; length]).await.unwrap();
                let (status, kind, body) = if streaming {
                    let event = if durable {
                        json!({"type":"response.failed","response":{"error":{"code":code,"message":"bad input"}}})
                    } else {
                        json!({"type":"error","code":code,"message":"bad input"})
                    };
                    (
                        "200 OK",
                        "text/event-stream",
                        format!("data: {event}\n\ndata: [DONE]\n\n"),
                    )
                } else {
                    ("400 Bad Request","application/json",json!({"error":{"message":"bad input", "type":"invalid_request_error", "code":code}}).to_string())
                };
                reader.get_mut().write_all(format!("HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            });
            let llm = OpenAiLlm::new("test", Some("test"), Some(format!("http://{address}")));
            let context = LlmContext {
                instructions: "rules".into(),
                messages: (&[][..]).into(),
            };
            let (tx, _rx) = mpsc::channel(8);
            let progress = streaming.then_some(&tx);
            let error = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                if durable {
                    llm.generate_attempt(&context, &[], progress).await
                } else {
                    llm.generate(&context, &[], progress).await
                }
            })
            .await
            .expect("no hidden retry")
            .unwrap_err();
            assert!(
                error
                    .downcast_ref::<super::super::GenerationRetry>()
                    .is_none(),
                "must not retry {code}: {error:?}"
            );
            assert_eq!(
                error.is::<super::super::ContextOverflow>(),
                code == "context_length_exceeded"
            );
            server.await.unwrap();
        }
    }

    #[test]
    fn overflow_classification_never_matches_free_text_or_attachment_errors() {
        for code in [None, Some("image_too_large"), Some("invalid_request_body")] {
            let error =
                async_openai::error::OpenAIError::ApiError(async_openai::error::ApiErrorResponse {
                    status_code: reqwest::StatusCode::PAYLOAD_TOO_LARGE,
                    api_error: serde_json::from_value(
                        json!({"message":"context_length_exceeded", "code":code}),
                    )
                    .unwrap(),
                });
            let error = classify_error(error.into());
            assert!(!error.is::<super::super::ContextOverflow>());
            assert!(!retryable(&error));
        }
        assert!(
            !classify_error(anyhow::anyhow!("context_length_exceeded"))
                .is::<super::super::ContextOverflow>()
        );
    }

    #[tokio::test]
    async fn durable_attempt_returns_retry_after_without_hidden_retry() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(&mut socket);
            let mut length = 0;
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).await.unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some((key, value)) = line.split_once(':')
                    && key.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).await.unwrap();
            drop(reader);
            let body =
                r#"{"error":{"message":"slow down","type":"rate_limit","param":null,"code":null}}"#;
            socket.write_all(format!("HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: 30\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).as_bytes()).await.unwrap();
        });
        let llm = OpenAiLlm::new("test", Some("test"), Some(format!("http://{address}")));
        let error = llm
            .generate_attempt(
                &LlmContext {
                    instructions: "test".into(),
                    messages: (&[][..]).into(),
                },
                &[],
                None,
            )
            .await
            .unwrap_err();
        let retry = error
            .downcast_ref::<super::super::provider::GenerationRetry>()
            .unwrap();
        assert_eq!(retry.retry_after, Some(std::time::Duration::from_secs(30)));
        assert_eq!(retry.limit, 3);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn retries_generation_with_same_context_and_stops_at_limit() {
        for (streaming, recover) in [(true, true), (false, true), (true, false)] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let llm = OpenAiLlm::new(
                "test",
                Some("test"),
                Some(format!("http://{}", listener.local_addr().unwrap())),
            );
            let server = tokio::spawn(async move {
                let mut requests = Vec::new();
                for attempt in 0..4 {
                    let (socket, _) = listener.accept().await.unwrap();
                    let mut reader = BufReader::new(socket);
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).await.unwrap();
                        if line == "\r\n" {
                            break;
                        }
                        if let Some(value) =
                            line.to_ascii_lowercase().strip_prefix("content-length:")
                        {
                            length = value.trim().parse().unwrap();
                        }
                    }
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).await.unwrap();
                    requests.push(serde_json::from_slice::<serde_json::Value>(&body).unwrap());
                    let success = recover && attempt == 3;
                    let response = json!({"output":[{"type":"message","content":[{"type":"output_text","text":"recovered"}]}],"usage":{"input_tokens":42}});
                    let (status, kind, body) = if streaming {
                        let terminal = if success {
                            json!({"type":"response.completed","response":response})
                        } else if attempt == 0 {
                            json!({"type":"response.failed","response":{"error":{"message":"provider unavailable"}}})
                        } else if attempt == 1 {
                            json!({"type":"response.created"})
                        } else {
                            json!({"type":"error","message":"provider unavailable"})
                        };
                        (
                            "200 OK",
                            "text/event-stream",
                            format!(
                                "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}}\n\ndata: {terminal}\n\ndata: [DONE]\n\n"
                            ),
                        )
                    } else if success {
                        ("200 OK", "application/json", response.to_string())
                    } else {
                        ("500 Internal Server Error", "application/json", json!({"error":{"message":"provider unavailable","type":"invalid_request_error"}}).to_string())
                    };
                    reader.get_mut().write_all(format!("HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                }
                requests
            });
            let messages: Vec<Message> = serde_json::from_value(json!([
                {"User":{"content":"continue"}},
                {"ToolCall":{"call_id":"done","name":"exec_command","arguments":{"cmd":"echo done"}}},
                {"ToolResult":{"call_id":"done","output":{"exit_code":0}}}
            ])).unwrap();
            let (tx, mut rx) = mpsc::channel(32);
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                llm.generate(
                    &LlmContext {
                        instructions: "test".into(),
                        messages: messages.as_slice().into(),
                    },
                    &[],
                    streaming.then_some(&tx),
                ),
            )
            .await
            .unwrap();
            if recover {
                let (messages, tokens) = result.unwrap();
                assert!(
                    matches!(&messages[0], Message::Assistant(message) if message.content == "recovered")
                );
                assert_eq!(tokens, Some(42));
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("provider unavailable")
                );
            }
            let requests = tokio::time::timeout(std::time::Duration::from_secs(1), server)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(requests.len(), 4);
            assert!(requests.iter().all(|request| request == &requests[0]));
            let mut retries = 0;
            while let Ok(event) = rx.try_recv() {
                if matches!(event, AgentEvent::Retrying(_)) {
                    retries += 1;
                }
            }
            assert_eq!(retries, if streaming { 3 } else { 0 });
        }
    }

    #[tokio::test]
    async fn wakeups_budget_counts_provider_retry_attempts() {
        use crate::agent::{
            Agent, ContextBuilder, UserMessage,
            session::{Session, SessionId},
            tools::ToolRegistry,
        };
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let llm = OpenAiLlm::new(
            "test",
            Some("test"),
            Some(format!("http://{}", listener.local_addr().unwrap())),
        );
        let attempts = Arc::new(AtomicUsize::new(0));
        let received = attempts.clone();
        let server = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(socket);
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
                reader.read_exact(&mut vec![0; length]).await.unwrap();
                received.fetch_add(1, Ordering::SeqCst);
                let body =
                    json!({"error":{"message":"unavailable", "type":"invalid_request_error"}})
                        .to_string();
                reader.get_mut().write_all(format!("HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        let agent = Agent::new(
            Arc::new(llm),
            ToolRegistry::default(),
            ContextBuilder::new("test"),
        );
        let mut session = Session::new(SessionId::new("companion:wakeup:retry-budget"));
        let error = agent
            .run_bounded(
                &mut session,
                UserMessage {
                    files: vec![],
                    images: vec![],
                    content: "work".into(),
                },
                2,
            )
            .await
            .unwrap_err();
        server.abort();
        let _ = server.await;
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(error.to_string().contains("budget exhausted"), "{error:#}");
    }

    #[tokio::test]
    async fn retry_wait_can_be_cancelled() {
        let (llm, finish, server) = streaming_server(None).await;
        let (tx, mut rx) = mpsc::channel(32);
        finish.send(()).unwrap();
        let task = tokio::spawn(async move {
            llm.generate(
                &LlmContext {
                    instructions: "test".into(),
                    messages: (&[][..]).into(),
                },
                &[],
                Some(&tx),
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !matches!(rx.recv().await.unwrap(), AgentEvent::Retrying(_)) {}
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        })
        .await
        .unwrap();
        server.await.unwrap();
    }

    async fn streaming_server(
        terminal: Option<serde_json::Value>,
    ) -> (
        OpenAiLlm,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<serde_json::Value>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let llm = OpenAiLlm::new(
            "test",
            Some("test"),
            Some(format!("http://{}", listener.local_addr().unwrap())),
        );
        let (finish, finished) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(socket);
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
            let socket = reader.get_mut();
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").await.unwrap();
            for event in [
                json!({"type":"response.created"}),
                json!({"type":"response.reasoning_text.delta", "delta":"private reasoning"}),
                json!({"type":"response.function_call_arguments.delta", "delta":"tool arguments"}),
                json!({"type":"response.output_text.delta", "delta":"При"}),
                json!({"type":"response.refusal.delta", "delta":"вет"}),
            ] {
                socket
                    .write_all(format!("data: {event}\n\n").as_bytes())
                    .await
                    .unwrap();
            }
            finished.await.unwrap();
            if let Some(terminal) = terminal {
                socket
                    .write_all(format!("data: {terminal}\n\n").as_bytes())
                    .await
                    .unwrap();
            }
            socket.write_all(b"data: [DONE]\n\n").await.unwrap();
            serde_json::from_slice(&body).unwrap()
        });
        (llm, finish, server)
    }

    #[tokio::test]
    async fn rejects_oversized_file_history_before_loading_files_or_sending_request() {
        let messages: Vec<Message> = serde_json::from_value(json!([
            {"User":{"content":"Analyze", "files":[
                {"filename":"a.pdf","mime_type":"application/pdf","path":"/missing-a","size":20_000_000,"sha256":"unused"},
                {"filename":"b.pdf","mime_type":"application/pdf","path":"/missing-b","size":20_000_000,"sha256":"unused"},
                {"filename":"c.pdf","mime_type":"application/pdf","path":"/missing-c","size":20_000_000,"sha256":"unused"}
            ]}}
        ])).unwrap();
        let llm =
            OpenAiLlm::new("test", Some("test"), Some("http://127.0.0.1:9")).without_retries();
        let error = llm
            .generate(
                &LlmContext {
                    instructions: "test".into(),
                    messages: messages.as_slice().into(),
                },
                &[],
                None,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("50 MB"), "{error:#}");
    }

    #[tokio::test]
    async fn sends_stored_documents_after_session_reload() {
        let dir = tempfile::tempdir().unwrap();
        let file =
            crate::agent::FileAttachment::save(dir.path(), "report.pdf", None, b"%PDF-1.7\ntest")
                .unwrap();
        let path = file.path.clone();
        let messages: Vec<Message> = serde_json::from_value(json!([
            {"User":{"content":"Summarize", "files":[file]}},
            {"User":{"content":"plain text"}}
        ]))
        .unwrap();
        let messages: Vec<Message> =
            serde_json::from_str(&serde_json::to_string(&messages).unwrap()).unwrap();
        let (llm, finish, server) = streaming_server(Some(json!({
            "type":"response.completed", "response":{"output":[]}
        })))
        .await;
        let (tx, _rx) = mpsc::channel(32);
        finish.send(()).unwrap();
        llm.generate(
            &LlmContext {
                instructions: "test".into(),
                messages: messages.as_slice().into(),
            },
            &[],
            Some(&tx),
        )
        .await
        .unwrap();
        let request = server.await.unwrap();
        assert_eq!(request["input"][0]["content"][1]["type"], "input_file");
        assert_eq!(request["input"][0]["content"][1]["filename"], "report.pdf");
        assert_eq!(
            request["input"][0]["content"][1]["file_data"],
            "data:application/pdf;base64,JVBERi0xLjcKdGVzdA=="
        );
        assert_eq!(request["input"][1]["content"], "plain text");
        assert!(!request.to_string().contains(path.to_str().unwrap()));
    }

    #[tokio::test]
    async fn sends_user_and_tool_images_as_content_blocks() {
        let image = json!({"data_url":"data:image/png;base64,cG5n", "width":1, "height":1});
        let messages: Vec<Message> = serde_json::from_value(json!([
            {"User":{"content":"Describe this", "images":[image.clone()]}},
            {"ToolCall":{"call_id":"image_1","name":"view_image","arguments":{"path":"screen.png"}}},
            {"ToolResult":{"call_id":"image_1","output":{"path":"screen.png"},"images":[image]}},
            {"User":{"content":"plain text"}},
            {"ToolResult":{"call_id":"old","output":{"ok":true}}}
        ])).unwrap();
        // Image bytes must survive session serialization, not rely on a file still existing.
        let messages: Vec<Message> =
            serde_json::from_str(&serde_json::to_string(&messages).unwrap()).unwrap();
        let (llm, finish, server) = streaming_server(Some(json!({
            "type":"response.completed", "response":{"output":[]}
        })))
        .await;
        let (tx, _rx) = mpsc::channel(32);
        finish.send(()).unwrap();
        llm.generate(
            &LlmContext {
                instructions: "test".into(),
                messages: messages.as_slice().into(),
            },
            &[],
            Some(&tx),
        )
        .await
        .unwrap();
        let request = server.await.unwrap();
        assert_eq!(request["input"][0]["content"][0]["text"], "Describe this");
        for content in [
            &request["input"][0]["content"],
            &request["input"][2]["output"],
        ] {
            assert_eq!(content[1]["type"], "input_image");
            assert_eq!(content[1]["image_url"], "data:image/png;base64,cG5n");
        }
        assert_eq!(request["input"][2]["call_id"], "image_1");
        assert_eq!(request["input"][3]["content"], "plain text");
        assert_eq!(request["input"][4]["output"], "{\"ok\":true}");
    }

    #[tokio::test]
    async fn streams_text_before_completion_and_preserves_final_tool_calls() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let (llm, finish, server) = streaming_server(Some(json!({
                "type":"response.completed",
                "response": {"output":[
                    {"type":"message","content":[{"type":"output_text","text":"Привет!"}]},
                    {"type":"function_call","call_id":"call_1","name":"datetime","arguments":"{\"timezone\":null}"}
                ], "usage":{"input_tokens":42}}
            }))).await;
            let (tx, mut rx) = mpsc::channel(32);
            let request = tokio::spawn(async move {
                llm.generate(&LlmContext { instructions: "test".into(), messages: (&[][..]).into() }, &[], Some(&tx)).await
            });
            for expected in ["При", "вет"] {
                assert!(matches!(rx.recv().await, Some(AgentEvent::TextDelta(delta)) if delta == expected));
            }
            assert!(!request.is_finished());
            finish.send(()).unwrap();
            let (messages, tokens) = request.await.unwrap().unwrap();
            assert_eq!(tokens, Some(42));
            assert!(matches!(&messages[0], Message::Assistant(message) if message.content == "Привет!"));
            assert!(matches!(&messages[1], Message::ToolCall(call) if call.call_id == "call_1" && call.arguments == json!({"timezone":null})));
            assert!(rx.recv().await.is_none());
            assert_eq!(server.await.unwrap()["stream"], true);
        }).await.unwrap();
    }

    #[tokio::test]
    async fn rejects_failed_incomplete_and_truncated_streams() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let context_error = r#"Engine protocol predict request returned 400: {"error":{"code":400,"message":"request (50012 tokens) exceeds the available context size (8192 tokens), try increasing it","type":"exceed_context_size_error","n_prompt_tokens":50012,"n_ctx":8192}}"#;
            for (terminal, expected) in [
                (None, "ended before completion"),
                (Some(json!({"type":"response.failed","response":{"error":{"message":"backend failed"}}})), "backend failed"),
                (Some(json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}})), "max_output_tokens"),
                (Some(json!({"type":"error","message":"stream error"})), "stream error"),
                (Some(json!({"type":"error","error":{"message":context_error,"type":"internal_error","code":"unknown","param":null},"sequence_number":2})), context_error),
            ] {
                let (llm, finish, server) = streaming_server(terminal).await;
                let (tx, _rx) = mpsc::channel(32);
                finish.send(()).unwrap();
                let error = llm.generate_once(&LlmContext { instructions: "test".into(), messages: (&[][..]).into() }, &[], Some(&tx)).await.unwrap_err();
                assert!(error.to_string().contains(expected), "{error:#}");
                assert!(error.to_string().starts_with("Response stream"), "{error:#}");
                server.await.unwrap();
            }
        }).await.unwrap();
    }

    #[test]
    fn accepts_provider_message_without_status() {
        let body = r#"{
            "id":"resp_02226c12be149a2f016abfb8a3e36087d2aed131930cf51fd2",
            "object":"response","created_at":1790949539,"model":"gpt-6-sol",
            "status":"completed","background":false,"error":null,
            "output":[{"id":"msg_02226c12be149a2f016abfb8a5645887d2bb6bd18cf408dcce",
                "type":"message","role":"assistant","content":[{
                    "type":"output_text","annotations":[],"logprobs":[],
                    "text":"Версия проекта изменена на 0.1.1 в Cargo.toml и Cargo.lock."
                }]}],
            "output_text":"Версия проекта изменена на 0.1.1 в Cargo.toml и Cargo.lock.",
            "usage":{"input_tokens":2623,"output_tokens":42,"total_tokens":2665,
                "input_tokens_details":{"cache_write_tokens":0,"cached_tokens":0},
                "output_tokens_details":{"reasoning_tokens":0}}
        }"#;
        let response: ResponseOutput = serde_json::from_str(body).unwrap();
        assert_eq!(
            response.usage.as_ref().map(|usage| usage.input_tokens),
            Some(2623)
        );
        let messages = response.into_messages().unwrap();
        assert_eq!(messages.len(), 1);
        let Message::Assistant(message) = &messages[0] else {
            panic!("expected assistant text")
        };
        assert_eq!(
            message.content,
            "Версия проекта изменена на 0.1.1 в Cargo.toml и Cargo.lock."
        );
    }

    #[test]
    fn preserves_mcp_tool_namespace_from_provider() {
        for (namespace, name, expected) in [
            (
                Some("mcp__rustrak"),
                "mcp__helper",
                "mcp__rustrak__mcp__helper",
            ),
            (
                Some("mcp__rustrak"),
                "search_issues",
                "mcp__rustrak__search_issues",
            ),
            (
                Some("mcp__rustrak"),
                "mcp__rustrak__search_issues",
                "mcp__rustrak__search_issues",
            ),
            (Some("functions"), "mcp_search", "mcp_search"),
            (
                None,
                "mcp__rustrak__search_issues",
                "mcp__rustrak__search_issues",
            ),
        ] {
            let response: ResponseOutput = serde_json::from_value(json!({"output":[{
                "type":"function_call","call_id":"call","namespace":namespace,
                "name":name,"arguments":"{\"project\":7}"
            }]}))
            .unwrap();
            let messages = response.into_messages().unwrap();
            let Message::ToolCall(call) = &messages[0] else {
                panic!("expected tool call")
            };
            assert_eq!(call.name, expected);
            assert_eq!(call.arguments, json!({"project":7}));
        }
    }

    #[test]
    fn preserves_tool_calls_and_rejects_malformed_payloads() {
        let body = json!({"output": [
            {"type": "reasoning", "summary": []},
            {"type": "message", "status": "completed", "content": [
                {"type": "output_text", "text": "hello "},
                {"type": "refusal", "refusal": "refusal"},
            ]},
            {"type": "function_call", "call_id": "call_1", "name": "datetime", "arguments": "{\"timezone\":null}"},
        ]});
        let response: ResponseOutput = serde_json::from_value(body).unwrap();
        let messages = response.into_messages().unwrap();
        assert_eq!(messages.len(), 2);
        let Message::Assistant(message) = &messages[0] else {
            panic!("expected assistant text")
        };
        assert_eq!(message.content, "hello refusal");
        let Message::ToolCall(call) = &messages[1] else {
            panic!("expected tool call")
        };
        assert_eq!(call.call_id, "call_1");
        assert_eq!(call.name, "datetime");
        assert_eq!(call.arguments, json!({"timezone": null}));

        assert!(
            serde_json::from_value::<ResponseOutput>(
                json!({"output": [{"type": "message", "content": [{"type": "output_text"}]}]})
            )
            .is_err()
        );
        assert!(serde_json::from_value::<ResponseOutput>(json!({"output": [{"type": "function_call", "call_id": "call_1", "arguments": "{}"}]})).is_err());
        let response: ResponseOutput = serde_json::from_value(json!({"output": [{"type": "function_call", "call_id": "call_1", "name": "datetime", "arguments": "invalid json"}]})).unwrap();
        assert!(response.into_messages().is_err());
    }
}
