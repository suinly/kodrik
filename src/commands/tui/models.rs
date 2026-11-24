use super::app::App;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

pub(super) struct ModelPicker {
    pub models: Vec<String>,
    pub choice: usize,
    pub query: String,
}

impl ModelPicker {
    pub fn new(models: Vec<String>, current: Option<String>) -> Self {
        let choice = current
            .and_then(|id| models.iter().position(|model| *model == id))
            .map_or(0, |index| index + 1);
        Self {
            models,
            choice,
            query: String::new(),
        }
    }

    pub fn filtered(&self) -> Vec<Option<&str>> {
        std::iter::once(None)
            .chain(
                self.models
                    .iter()
                    .filter(|id| id.to_lowercase().contains(&self.query.to_lowercase()))
                    .map(|id| Some(id.as_str())),
            )
            .collect()
    }
}

pub(super) fn update_model_picker(app: &mut App, key: KeyEvent) -> Option<String> {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c' | 'C' | 'с' | 'С'))
    {
        app.should_quit = true;
        return None;
    }
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    let picker = app.model_picker.as_mut()?;
    match key.code {
        KeyCode::Up => picker.choice = picker.choice.saturating_sub(1),
        KeyCode::Down => picker.choice = (picker.choice + 1).min(picker.filtered().len() - 1),
        KeyCode::Char(c) => {
            picker.query.push(c);
            picker.choice = 0;
        }
        KeyCode::Backspace => {
            picker.query.pop();
            picker.choice = 0;
        }
        KeyCode::Esc if key.kind == KeyEventKind::Press => app.model_picker = None,
        KeyCode::Enter if key.kind == KeyEventKind::Press => {
            let picker = app.model_picker.take()?;
            return picker
                .filtered()
                .get(picker.choice)
                .map(|model| match model {
                    Some(id) => format!("/model {id}"),
                    None => "/model".into(),
                });
        }
        _ => {}
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(app: &mut App, code: KeyCode) -> Option<String> {
        app.update(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn model_picker_selects_full_id_and_preserves_draft() {
        let mut app = App::default();
        app.insert_input("unfinished draft");
        app.model_picker = Some(ModelPicker::new(
            vec!["provider/model with spaces".into()],
            None,
        ));
        key(&mut app, KeyCode::Down);
        assert_eq!(
            key(&mut app, KeyCode::Enter).as_deref(),
            Some("/model provider/model with spaces")
        );
        assert_eq!(app.input, "unfinished draft");
        assert!(app.waiting_messages.is_empty());
    }

    #[test]
    fn model_picker_search_reset_and_cancel() {
        let mut app = App {
            model_picker: Some(ModelPicker::new(
                vec!["Alpha".into(), "Beta".into()],
                Some("Alpha".into()),
            )),
            ..Default::default()
        };
        assert_eq!(app.model_picker.as_ref().unwrap().choice, 1);
        key(&mut app, KeyCode::Char('B'));
        assert_eq!(
            app.model_picker.as_ref().unwrap().filtered(),
            vec![None, Some("Beta")]
        );
        assert_eq!(key(&mut app, KeyCode::Enter).as_deref(), Some("/model"));
        app.model_picker = Some(ModelPicker::new(vec![], None));
        key(&mut app, KeyCode::Esc);
        assert!(app.model_picker.is_none());
        assert!(!app.should_quit);
    }
}

pub(super) fn refresh(
    app: &mut App,
    runner: &crate::agent::AgentRunner,
    session: &crate::agent::session::Session,
) -> anyhow::Result<bool> {
    let model = runner.effective_model_identity(session)?;
    let model_override = session.model_override().map(str::to_owned);
    let changed = app.model != model || app.model_override != model_override;
    app.model = model;
    app.model_override = model_override;
    Ok(changed)
}

pub(super) async fn handle_command(
    app: &mut App,
    runner: &crate::agent::AgentRunner,
    conversation: &crate::agent::session::SessionId,
    input: &str,
) -> bool {
    if input != "/models" && input != "/model" && !input.starts_with("/model ") {
        return false;
    }
    let result = async {
        if input == "/models" {
            let session = runner.load_session(conversation.clone()).await?;
            refresh(app, runner, &session)?;
            app.model_picker = Some(ModelPicker::new(
                runner.available_models().await?,
                app.model_override.clone(),
            ));
            return Ok::<_, anyhow::Error>(());
        }
        let model = input
            .strip_prefix("/model ")
            .filter(|model| *model != "auto")
            .map(str::to_owned);
        if let Some(model) = &model {
            anyhow::ensure!(
                !model.is_empty() && model.len() <= 512 && !model.chars().any(char::is_control),
                "Invalid model ID"
            );
            anyhow::ensure!(
                runner.available_models().await?.contains(model),
                "Model is not in the provider catalogue"
            );
        }
        let session = runner.load_session(conversation.clone()).await?;
        let store = runner
            .sqlite_store()
            .ok_or_else(|| anyhow::anyhow!("Model selection requires durable session storage"))?;
        store
            .set_session_model(session.id().as_str(), model)
            .await?;
        let session = runner.load_session(conversation.clone()).await?;
        refresh(app, runner, &session)?;
        Ok(())
    }
    .await;
    if let Err(error) = result {
        app.set_copy_notice(format!(
            "{}: {error:#}",
            app.language
                .pick("Could not select model", "Не удалось выбрать модель")
        ));
    }
    true
}

#[cfg(test)]
mod draft_tests {
    use super::*;
    use crate::agent::{
        Agent, AgentRunner, AssistantMessage, ContextBuilder,
        llm::LlmProvider,
        message::Message,
        session::{SessionId, SessionStore, SqliteSessionStore},
        tools::ToolRegistry,
    };
    use std::sync::Arc;

    struct Model(&'static str);
    #[async_trait::async_trait]
    impl LlmProvider for Model {
        fn identity(&self) -> String {
            format!("test:{}", self.0)
        }
        async fn list_models(&self) -> anyhow::Result<Vec<String>> {
            Ok(vec!["chosen".into()])
        }
        fn with_model(&self, model: &str) -> anyhow::Result<Arc<dyn LlmProvider>> {
            anyhow::ensure!(model == "chosen", "unexpected model");
            Ok(Arc::new(Model("chosen")))
        }
        async fn generate(
            &self,
            _: &crate::agent::LlmContext<'_>,
            _: &[&dyn crate::agent::tools::Tool],
            _: Option<&tokio::sync::mpsc::Sender<crate::agent::AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            Ok((
                vec![Message::Assistant(AssistantMessage {
                    content: format!("reply from {}", self.0),
                })],
                None,
            ))
        }
    }

    #[tokio::test]
    async fn model_selected_before_first_input_survives_refresh_and_drives_generation() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteSessionStore::open(dir.path().join("sessions")).unwrap());
        let runner = AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(Model("default")),
                ToolRegistry::default(),
                ContextBuilder::new("test"),
            )),
            store.clone(),
        );
        let conversation = SessionId::new("cli:main");
        let mut app = App::default();
        for _ in 0..2 {
            let draft = runner.prepare_session(&conversation).await.unwrap();
            handle_command(&mut app, &runner, &conversation, "/models").await;
            assert!(
                app.model_picker.is_some(),
                "catalogue must open before first message"
            );
            app.model_picker = None;
            handle_command(&mut app, &runner, &conversation, "/model chosen").await;
            assert_eq!(
                app.model_override.as_deref(),
                Some("chosen"),
                "{:?}",
                app.copy_notice
            );
            assert!(
                store.list().await.unwrap().is_empty(),
                "model selection must not create a listed dialogue"
            );
            let session = runner.load_session(conversation.clone()).await.unwrap();
            refresh(&mut app, &runner, &session).unwrap();
            assert_eq!(app.model, "test:chosen");
            assert_eq!(session.id(), draft.id());
            handle_command(&mut app, &runner, &conversation, "/model").await;
            assert_eq!(app.model_override, None);
            assert!(store.list().await.unwrap().is_empty());
        }
        handle_command(&mut app, &runner, &conversation, "/model chosen").await;
        let message = crate::gateway::IncomingMessage::new(
            crate::gateway::InterfaceId("cli".into()),
            "main",
            "first message",
        );
        runner
            .run_with_cancel(conversation.clone(), message, None, None, None)
            .await
            .unwrap();
        let saved = runner.load_session(conversation).await.unwrap();
        assert_eq!(saved.model_override(), Some("chosen"));
        assert!(
            saved
                .messages()
                .iter()
                .any(|m| matches!(m, Message::Assistant(a) if a.content == "reply from chosen"))
        );
        assert_eq!(store.list().await.unwrap().len(), 1);
    }
}
