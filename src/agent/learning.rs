use std::sync::{Arc, Mutex};

use super::{companion::CompanionStore, llm::LlmProvider, message::Message, session::Session};

pub(crate) struct LearningService {
    store: CompanionStore,
    llm: Arc<dyn LlmProvider>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    journal: Option<crate::audit::Journal>,
}

impl LearningService {
    pub(crate) fn new(store: CompanionStore, llm: Arc<dyn LlmProvider>) -> Self {
        Self {
            store,
            llm,
            task: Mutex::new(None),
            journal: None,
        }
    }

    pub(crate) fn with_journal(mut self, journal: crate::audit::Journal) -> Self {
        self.journal = Some(journal);
        self
    }

    pub(crate) fn schedule(&self, session: &Session, from: usize) {
        if !worth_reviewing(&session.messages()[from..]) {
            return;
        }
        let mut task = self.task.lock().unwrap();
        // ponytail: one background review at a time; a durable queue can follow if skipped reviews matter.
        if task.as_ref().is_some_and(|task| !task.is_finished()) {
            return;
        }
        let store = self.store.clone();
        let llm = self.llm.clone();
        let input = CompanionStore::prepare_review(session, from);
        let session_id = session.id().as_str().to_owned();
        let journal = self.journal.clone();
        *task = Some(tokio::spawn(async move {
            let result = async {
                let context = crate::audit::Context::new(&session_id, "learning");
                let span = crate::audit::Span::start(
                    journal.as_ref(),
                    &context,
                    "run",
                    None,
                    "learning",
                    serde_json::json!({}),
                )
                .await?;
                let result = store.review_prepared(llm, input).await;
                span.finish(
                    if result.is_ok() { "ok" } else { "failed" },
                    serde_json::json!({
                        "learned_entries": result.as_ref().ok(), "error": result.is_err(),
                    }),
                )
                .await?;
                result
            }
            .await;
            match result {
                Ok(count) if count > 0 => {
                    log::info!("Learned {count} entries from session {session_id}")
                }
                Ok(_) => {}
                Err(error) => log::warn!("Companion review failed: {error:#}"),
            }
        }));
    }

    pub(crate) async fn finish(&self) {
        let task = self.task.lock().unwrap().take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

fn worth_reviewing(messages: &[Message]) -> bool {
    // ponytail: short acknowledgements are filtered heuristically; expand only if real conversations need it.
    messages
        .iter()
        .any(|message| matches!(message, Message::ToolResult(_)))
        || messages
            .iter()
            .filter_map(|message| match message {
                Message::User(message) => Some(
                    message
                        .content
                        .trim()
                        .trim_matches(|c: char| c.is_ascii_punctuation())
                        .to_lowercase(),
                ),
                _ => None,
            })
            .any(|text| {
                text.chars().count() >= 8
                    && ![
                        "спасибо",
                        "большое спасибо",
                        "хорошо",
                        "понятно",
                        "отлично",
                        "привет",
                        "thanks",
                        "thank you",
                        "okay",
                        "got it",
                        "hello",
                    ]
                    .contains(&text.as_str())
            })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::UserMessage;

    #[test]
    fn skips_acknowledgements_but_reviews_substantive_feedback() {
        let user = |text: &str| {
            Message::User(UserMessage {
                files: Vec::new(),
                content: text.into(),
                images: vec![],
            })
        };
        assert!(!worth_reviewing(&[user("Спасибо!")]));
        assert!(!worth_reviewing(&[user("Большое спасибо!")]));
        assert!(worth_reviewing(&[user("Покороче")]));
        assert!(worth_reviewing(&[user(
            "Мне нравится обсуждать историю, а не только писать код. Запомни это."
        )]));
    }
}
