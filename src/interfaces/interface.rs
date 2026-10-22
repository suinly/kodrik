use tokio::sync::mpsc;

use super::OutgoingMessage;
use crate::gateway::{Event, InterfaceId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageId(pub String);

#[derive(Debug, Clone, serde::Serialize)]
pub struct InlineButton {
    pub text: String,
    #[serde(rename = "callback_data")]
    pub data: String,
}

/// A confirmed API refusal, unlike a network error with unknown delivery outcome.
#[derive(Debug)]
pub(crate) struct SendRejection {
    pub description: String,
    pub error_code: u16,
    pub retry_after: Option<u64>,
}
impl std::fmt::Display for SendRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.description)
    }
}
impl std::error::Error for SendRejection {}

#[async_trait::async_trait]
pub trait Interface: Send + Sync {
    fn id(&self) -> InterfaceId;
    fn language(&self, _conversation_id: &str) -> crate::i18n::Language {
        crate::i18n::Language::En
    }
    fn stream_replies(&self) -> bool {
        true
    }
    async fn run(&self, tx: mpsc::Sender<Event>) -> anyhow::Result<()>;
    async fn send(&self, conversation_id: &str, message: &str) -> anyhow::Result<MessageId>;

    async fn send_buttons(
        &self,
        conversation_id: &str,
        text: &str,
        _buttons: &[Vec<InlineButton>],
    ) -> anyhow::Result<MessageId> {
        self.send(conversation_id, text).await
    }

    async fn edit_buttons(
        &self,
        conversation_id: &str,
        id: &MessageId,
        text: &str,
        _buttons: &[Vec<InlineButton>],
    ) -> anyhow::Result<()> {
        self.edit(conversation_id, id, text).await
    }

    async fn answer_callback(&self, _id: &str, _text: &str) -> anyhow::Result<()> {
        anyhow::bail!("Interface does not support callback buttons")
    }

    async fn answer_guest(&self, _query_id: &str, _message: &str) -> anyhow::Result<MessageId> {
        anyhow::bail!("Interface does not support guest replies")
    }

    async fn send_content(
        &self,
        _conversation_id: &str,
        _message: &OutgoingMessage,
    ) -> anyhow::Result<MessageId> {
        anyhow::bail!("Interface does not support typed replies")
    }

    async fn answer_guest_content(
        &self,
        _query_id: &str,
        _message: &OutgoingMessage,
    ) -> anyhow::Result<MessageId> {
        anyhow::bail!("Interface does not support typed guest replies")
    }

    async fn read_results(
        &self,
        _conversation_id: &str,
        _kind: &str,
        _id: Option<&str>,
        _offset: usize,
    ) -> anyhow::Result<serde_json::Value> {
        anyhow::bail!("Interface does not support reading poll results")
    }

    async fn send_status(&self, conversation_id: &str, message: &str) -> anyhow::Result<MessageId> {
        self.send(conversation_id, message).await
    }

    fn allows_recipient(&self, _conversation_id: &str) -> bool {
        false
    }

    async fn edit(
        &self,
        _conversation_id: &str,
        _id: &MessageId,
        _message: &str,
    ) -> anyhow::Result<()> {
        anyhow::bail!("Interface does not support editing messages")
    }

    async fn delete(&self, _conversation_id: &str, _id: &MessageId) -> anyhow::Result<()> {
        anyhow::bail!("Interface does not support deleting messages")
    }

    async fn typing(&self, _conversation_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
}
