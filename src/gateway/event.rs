use std::fmt;

pub enum Event {
    Message(IncomingMessage),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IncomingMessage {
    pub source: MessageSource,
    pub request_id: String,
    // Keep ordinary messages small inside nested runtime futures.
    #[serde(skip)]
    pub(crate) callback: Option<Box<SessionCallback>>,
    #[serde(skip)]
    pub(crate) admission:
        Option<std::sync::Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>>,
    pub text: String,
    pub is_group: bool,
    pub guest_query_id: Option<String>,
    pub(crate) telegram_group: Option<crate::agent::telegram_group::TelegramGroup>,
    pub images: Vec<crate::agent::Image>,
    pub files: Vec<crate::agent::file::FileAttachment>,
}

impl IncomingMessage {
    pub(crate) fn acknowledge(&self) {
        if let Some(receipt) = &self.admission
            && let Some(sender) = receipt.lock().unwrap().take()
        {
            let _ = sender.send(());
        }
    }
    pub fn new(
        interface: InterfaceId,
        conversation_id: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            request_id: uuid::Uuid::new_v4().to_string(),
            callback: None,
            admission: None,
            source: MessageSource {
                interface,
                conversation_id: conversation_id.into(),
            },
            text: text.into(),
            is_group: false,
            guest_query_id: None,
            telegram_group: None,
            images: Vec::new(),
            files: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SessionCallback {
    pub id: String,
    pub message_id: crate::MessageId,
    pub data: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MessageSource {
    pub interface: InterfaceId,
    pub conversation_id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, PartialOrd, Eq, Ord)]
pub struct InterfaceId(pub String);

impl fmt::Display for InterfaceId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}
