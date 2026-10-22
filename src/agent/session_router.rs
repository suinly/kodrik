use super::{UserMessage, message::Message, session::Session, system_one::SystemOne};
use serde_json::{Value, json};

pub(crate) struct SessionRouter {
    system_one: SystemOne,
    idle: std::time::Duration,
}

impl SessionRouter {
    pub(crate) fn new(system_one: SystemOne, idle_minutes: u32) -> Self {
        Self {
            system_one,
            idle: std::time::Duration::from_secs(u64::from(idle_minutes) * 60),
        }
    }

    pub(crate) async fn should_start_new(&self, session: &Session, input: &UserMessage) -> bool {
        if !idle_long_enough(session, chrono::Utc::now(), self.idle)
            || input.content.trim().is_empty()
            || !session
                .messages()
                .iter()
                .any(|m| matches!(m, Message::User(_) | Message::Assistant(_)))
        {
            return false;
        }
        let decision = self.system_one.choose(routing_state(session, input),
            "How does incoming_message relate to the conversation in messages? Preserve continuity unless it clearly introduces an independent unrelated topic. A change of vocabulary alone is not a new topic. If a message both follows up and introduces another topic, choose continue. Replies to the assistant's questions or proactive suggestions are continuations. Image and file contents are unavailable: use uncertain if they are needed to decide.",
            &[
                ("continue", "A continuation, answer, correction, acknowledgement, reference to earlier discussion, or related topic. Short replies such as yes, why, go ahead, commit it normally continue the conversation."),
                ("new", "A self-contained topic clearly unrelated to the current conversation, with no dependency on it."),
                ("uncertain", "Insufficient evidence or ambiguous relationship."),
            ]).await;
        match decision {
            Ok(choice) => choice == "new",
            Err(error) => {
                log::warn!("Session routing failed; keeping current session: {error:#}");
                false
            }
        }
    }
}

fn idle_long_enough(
    session: &Session,
    now: chrono::DateTime<chrono::Utc>,
    idle: std::time::Duration,
) -> bool {
    session
        .last_message_at()
        .and_then(|last| (now - last).to_std().ok())
        .is_some_and(|elapsed| elapsed >= idle)
}

fn routing_state(session: &Session, input: &UserMessage) -> Value {
    let messages: Vec<Value> = session
        .messages()
        .iter()
        .filter_map(|message| match message {
            Message::User(m) => Some(json!({"role":"user", "text":m.content})),
            Message::Assistant(m) => Some(json!({"role":"assistant", "text":m.content})),
            Message::ToolCall(_) | Message::ToolResult(_) => None,
        })
        .collect();
    json!({"telegram_group":session.telegram_group(), "messages":messages, "incoming_message":input.content})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{
        AssistantMessage,
        message::{ToolCall, ToolResult},
        session::SessionId,
    };

    struct MustNotCall;
    #[async_trait::async_trait]
    impl crate::agent::llm::LlmProvider for MustNotCall {
        async fn generate(
            &self,
            _: &crate::agent::LlmContext<'_>,
            _: &[&dyn crate::agent::tools::Tool],
            _: Option<&tokio::sync::mpsc::Sender<crate::agent::AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            panic!("Empty sessions and image-only messages must bypass routing")
        }
    }

    struct NewTopic;
    #[async_trait::async_trait]
    impl crate::agent::llm::LlmProvider for NewTopic {
        async fn generate(
            &self,
            _: &crate::agent::LlmContext<'_>,
            _: &[&dyn crate::agent::tools::Tool],
            _: Option<&tokio::sync::mpsc::Sender<crate::agent::AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            Ok((
                vec![Message::Assistant(AssistantMessage {
                    content: r#"{"choice":"new"}"#.into(),
                })],
                None,
            ))
        }
    }

    #[tokio::test]
    async fn idle_session_uses_decision_but_recent_reply_prevents_switch() {
        let router = SessionRouter::new(SystemOne::new(std::sync::Arc::new(NewTopic)), 30);
        let mut session: Session = serde_json::from_value(json!({
            "id":"restored", "last_message_at":"2000-01-01T00:00:00Z",
            "messages":[{"User":{"content":"old topic"}}]
        }))
        .unwrap();
        let input = UserMessage {
            files: Vec::new(),
            content: "another topic".into(),
            images: vec![],
        };
        assert!(router.should_start_new(&session, &input).await);
        session.push(Message::Assistant(AssistantMessage {
            content: "A recent proactive message".into(),
        }));
        assert!(!router.should_start_new(&session, &input).await);
    }

    #[test]
    fn idle_threshold_handles_exact_boundary_restart_and_clock_rollback() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-04T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        for (stamp, expected) in [
            ("2026-10-04T11:30:01Z", false),
            ("2026-10-04T11:30:00Z", true),
            ("2026-10-04T11:00:00Z", true),
            ("2026-10-04T12:01:00Z", false),
        ] {
            let restored: Session = serde_json::from_value(
                json!({"id":"restored", "messages":[], "last_message_at":stamp}),
            )
            .unwrap();
            assert_eq!(
                idle_long_enough(&restored, now, std::time::Duration::from_secs(1800)),
                expected
            );
        }
    }

    #[tokio::test]
    async fn recent_and_legacy_sessions_bypass_system_one() {
        let router = SessionRouter::new(SystemOne::new(std::sync::Arc::new(MustNotCall)), 30);
        let input = UserMessage {
            files: Vec::new(),
            content: "unrelated topic".into(),
            images: vec![],
        };
        let mut session = Session::new(SessionId::new("recent"));
        session.push(Message::User(UserMessage {
            files: Vec::new(),
            content: "old topic".into(),
            images: vec![],
        }));
        assert!(!router.should_start_new(&session, &input).await);
        let legacy: Session = serde_json::from_value(
            json!({"id":"legacy", "messages":[{"User":{"content":"old topic","images":[]}}]}),
        )
        .unwrap();
        assert!(!router.should_start_new(&legacy, &input).await);
    }

    #[tokio::test]
    async fn empty_sessions_and_image_only_inputs_keep_current_session() {
        let router = SessionRouter::new(SystemOne::new(std::sync::Arc::new(MustNotCall)), 30);
        let mut session = Session::new(SessionId::new("test"));
        let input = UserMessage {
            files: Vec::new(),
            content: "first message".into(),
            images: vec![],
        };
        assert!(!router.should_start_new(&session, &input).await);
        session = serde_json::from_value(json!({
            "id":"test", "last_message_at":"2000-01-01T00:00:00Z",
            "messages":[{"User":input}]
        }))
        .unwrap();
        let image = crate::agent::Image::from_bytes(&crate::agent::image::tests::sample(
            image::ImageFormat::Png,
        ))
        .unwrap();
        assert!(
            !router
                .should_start_new(
                    &session,
                    &UserMessage {
                        files: Vec::new(),
                        content: "".into(),
                        images: vec![image]
                    }
                )
                .await
        );
    }

    #[test]
    fn full_ordered_text_is_preserved_without_tools_or_images() {
        let mut session = Session::new(SessionId::new("test"));
        let long = "Большой разговор ".repeat(2000);
        session.push(Message::User(UserMessage {
            files: Vec::new(),
            content: long.clone(),
            images: vec![],
        }));
        session.push(Message::ToolCall(ToolCall {
            call_id: "x".into(),
            name: "secret".into(),
            arguments: json!({"secret":true}),
        }));
        session.push(Message::ToolResult(ToolResult {
            call_id: "x".into(),
            content: json!({"secret":true}).into(),
        }));
        session.push(Message::Assistant(AssistantMessage {
            content: "Продолжим?".into(),
        }));
        let input = UserMessage {
            files: Vec::new(),
            content: "Да".into(),
            images: vec![
                crate::agent::Image::from_bytes(&crate::agent::image::tests::sample(
                    image::ImageFormat::Png,
                ))
                .unwrap(),
            ],
        };
        let state = routing_state(&session, &input);
        assert!(state["telegram_group"].is_null());
        assert_eq!(
            state["messages"],
            json!([
                {"role":"user","text":long},
                {"role":"assistant","text":"Продолжим?"}
            ])
        );
        assert_eq!(state["incoming_message"], "Да");
        assert!(!state.to_string().contains("secret"));
        assert!(!state.to_string().contains("base64"));
        session.merge_telegram_group(Some(
            &serde_json::from_value(json!({
                "chat_id":-42,"name":"Group","bot_username":"@TestBot",
                "participants":{"user:11":{"name":"Alice"}}
            }))
            .unwrap(),
        ));
        assert_eq!(
            routing_state(&session, &input)["telegram_group"]["participants"]["user:11"]["name"],
            "Alice"
        );
    }
}
