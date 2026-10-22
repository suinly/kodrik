use super::Image;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UserMessage {
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<Image>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<super::FileAttachment>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AssistantMessage {
    pub content: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolResult {
    pub call_id: String,
    #[serde(flatten)]
    pub content: ToolOutput,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolOutput {
    pub output: serde_json::Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<Image>,
}

impl From<serde_json::Value> for ToolOutput {
    fn from(output: serde_json::Value) -> Self {
        Self {
            output,
            images: Vec::new(),
        }
    }
}

pub(crate) fn truncate_tool_outputs(
    mut messages: std::borrow::Cow<'_, [Message]>,
) -> std::borrow::Cow<'_, [Message]> {
    // Match the context estimator's UTF-8 bytes / 3 heuristic: about 10,000 tokens.
    const MAX_BYTES: usize = 30_000;
    for index in 0..messages.len() {
        let Message::ToolResult(result) = &messages[index] else {
            continue;
        };
        let text = result.content.output.to_string();
        if text.len() <= MAX_BYTES {
            continue;
        }
        let mut half = MAX_BYTES / 2;
        let output = loop {
            let head = &text[..text.floor_char_boundary(half)];
            let tail = &text[text.ceil_char_boundary(text.len() - half)..];
            let output = serde_json::Value::String(format!(
                "{head}\n... [tool output truncated for context; full result remains in session history] ...\n{tail}"
            ));
            let size = output.to_string().len();
            if size <= MAX_BYTES {
                break output;
            }
            half = half
                .saturating_sub((size - MAX_BYTES).div_ceil(2))
                .max(half / 2);
        };
        if let Message::ToolResult(result) = &mut messages.to_mut()[index] {
            result.content.output = output;
        }
    }
    messages
}

// A summary boundary must never separate a tool call from its result.
pub(crate) fn tool_boundaries(messages: &[Message]) -> Vec<usize> {
    let mut pending = std::collections::HashSet::new();
    let mut boundaries = vec![0];
    for (index, message) in messages.iter().enumerate() {
        match message {
            Message::ToolCall(call) => {
                pending.insert(call.call_id.as_str());
            }
            Message::ToolResult(result) => {
                pending.remove(result.call_id.as_str());
            }
            _ => {}
        }
        if pending.is_empty() {
            boundaries.push(index + 1);
        }
    }
    boundaries
}
