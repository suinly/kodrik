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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) guest_user_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) webhook: Option<Box<serde_json::Value>>,
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
            guest_user_id: None,
            webhook: None,
            telegram_group: None,
            images: Vec::new(),
            files: Vec::new(),
        }
    }

    pub(crate) fn session_id(&self) -> crate::agent::session::SessionId {
        if let Some((first, second)) = self.guest_participants() {
            return crate::agent::session::SessionId::new(format!(
                "{}:{first}:{second}:guest",
                self.source.interface
            ));
        }
        let mut id = format!("{}:{}", self.source.interface, self.source.conversation_id);
        if self.guest_query_id.is_some() {
            if let Some(user) = self.guest_user_id {
                id.push_str(&format!(":{user}"));
            }
            id.push_str(":guest");
        }
        crate::agent::session::SessionId::new(id)
    }

    pub(crate) fn guest_participants(&self) -> Option<(u64, u64)> {
        self.guest_query_id.as_ref()?;
        if self.is_group {
            return None;
        }
        let user = self
            .guest_user_id
            .filter(|user| *user > 0 && *user <= i64::MAX as u64)?;
        let peer = self
            .source
            .conversation_id
            .parse::<u64>()
            .ok()
            .filter(|peer| *peer > 0 && *peer <= i64::MAX as u64)?;
        Some((user.min(peer), user.max(peer)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_guest_session_is_the_same_from_both_sides_and_separate_from_personal() {
        let guest = |user, chat: &str| {
            let mut message =
                IncomingMessage::new(InterfaceId("telegram:main".into()), chat, "hello");
            message.guest_query_id = Some("query".into());
            message.guest_user_id = Some(user);
            message.session_id()
        };
        assert_eq!(guest(7, "8"), guest(8, "7"));
        assert_ne!(guest(7, "8"), guest(7, "9"));
        assert_ne!(guest(7, "8"), guest(9, "8"));
        assert_ne!(
            guest(7, "8"),
            IncomingMessage::new(InterfaceId("telegram:main".into()), "8", "personal").session_id()
        );
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
