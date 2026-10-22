use super::*;
use crate::{bootstrap::AppContext, config::Config};
use std::path::{Path, PathBuf};

pub(super) struct TelegramSpaces {
    pub(super) config: Config,
    pub(super) root: PathBuf,
    pub(super) users: Arc<crate::users::UserSpaces>,
}

impl GatewayRunner {
    pub(crate) fn with_user_spaces(mut self, users: Arc<crate::users::UserSpaces>) -> Self {
        self.user_spaces = Some(users);
        self
    }

    pub(crate) async fn with_telegram_spaces(
        mut self,
        config: &Config,
        root: &Path,
        owner_app: AppContext,
    ) -> anyhow::Result<Self> {
        config.validate_owner()?;
        let auth = crate::web::auth::AuthStore::new(root);
        auth.configure_owner(config.owner.as_ref().map(|owner| owner.user_id.get()))?;
        let recipient = config
            .companion
            .recipient(&config.gateway, config.owner.as_ref())?;
        // Removed interfaces must remain visible without opening their stores
        // under the permissions of a different configured space.
        match tokio::fs::read_dir(root.join("spaces/telegram")).await {
            Ok(mut entries) => {
                while let Some(entry) = entries.next_entry().await? {
                    if entry.file_type().await?.is_dir()
                        && !config
                            .gateway
                            .keys()
                            .any(|key| crate::spaces::telegram_interface(root, key) == entry.path())
                    {
                        log::warn!(
                            "Recovery blocked for saved Telegram space {}: source interface is not configured",
                            entry.path().display()
                        );
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut chats = std::collections::HashSet::new();
        for key in config.gateway.keys() {
            let directory = root.to_owned();
            let interface = key.clone();
            for chat in tokio::task::spawn_blocking(move || {
                crate::spaces::telegram_chats(&directory, &interface)
            })
            .await??
            {
                chats.insert((format!("telegram:{key}"), chat));
            }
        }
        if let Some(owner) = &config.owner {
            chats.insert((
                format!("telegram:{}", owner.interface),
                i64::try_from(owner.user_id.get())?,
            ));
        }
        if let Some((interface, chat)) = recipient {
            chats.insert((interface, chat.parse()?));
        }
        self.chat_runners = Some(HashMap::new());
        self.telegram_spaces = Some(Arc::new(TelegramSpaces {
            config: config.clone(),
            root: root.to_owned(),
            users: self.user_spaces.clone().unwrap_or_else(|| {
                Arc::new(crate::users::UserSpaces::new(config, root, owner_app))
            }),
        }));
        for (interface, chat) in chats {
            if chat > 0 && auth.allowed_user(chat as u64)? {
                self.prepare_telegram_chat(&interface, chat).await?;
            }
        }
        Ok(self)
    }

    pub(super) async fn prepare_telegram_chat(
        &mut self,
        interface: &str,
        chat: i64,
    ) -> anyhow::Result<bool> {
        let Some(spaces) = self.telegram_spaces.clone() else {
            return Ok(false);
        };
        let key = interface
            .strip_prefix("telegram:")
            .context("Expected Telegram interface")?;
        anyhow::ensure!(
            spaces.config.gateway.contains_key(key),
            "Unknown Telegram interface"
        );
        anyhow::ensure!(
            chat > 0 && crate::web::auth::AuthStore::new(&spaces.root).allowed_user(chat as u64)?,
            "Telegram access requires an invitation and a private chat"
        );
        let id = SessionId::new(format!("{interface}:{chat}"));
        if self
            .chat_runners
            .as_ref()
            .is_some_and(|runners| runners.contains_key(&id))
        {
            return Ok(false);
        }
        let space = crate::spaces::telegram(&spaces.root, key, chat);
        tokio::fs::create_dir_all(&space).await?;
        let app = spaces.users.app(chat as u64).await?;
        let recipient = (interface.to_owned(), chat.to_string());
        if spaces
            .config
            .companion
            .recipient(&spaces.config.gateway, spaces.config.owner.as_ref())?
            .as_ref()
            == Some(&recipient)
        {
            self.proactive = Some(Arc::new(super::super::proactive::Proactive::new(
                spaces.config.companion.clone(),
                recipient,
                space.clone(),
                app.proactive_agent,
                app.agent_runner.clone(),
            )));
        }
        let scheduled = self.chat_runners.as_ref().is_some_and(|runners| {
            runners
                .values()
                .any(|runner| Arc::ptr_eq(runner, &app.agent_runner))
        });
        if !scheduled {
            self.reminders
                .push(Arc::new(super::super::reminders::Reminders::new(
                    crate::agent::reminders::ReminderStore::new(&space),
                    app.agent_runner.clone(),
                )));
            self.wakeups
                .push(Arc::new(super::super::wakeups::Wakeups::new(
                    crate::agent::reminders::ReminderStore::wakeups(&space),
                    app.agent_runner.clone(),
                    spaces.config.companion.clone(),
                )));
        }
        self.chat_runners
            .as_mut()
            .context("Missing Telegram chat runners")?
            .insert(id, app.agent_runner);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::session::{SessionStore, SqliteSessionStore};
    use crate::gateway::InterfaceId;
    use serde_json::{Value, json};
    use std::sync::Mutex;

    struct NewChats(Arc<Mutex<Vec<(String, String)>>>);
    #[async_trait::async_trait]
    impl Interface for NewChats {
        fn language(&self, _: &str) -> crate::i18n::Language {
            crate::i18n::Language::Ru
        }
        fn id(&self) -> InterfaceId {
            InterfaceId("telegram:main".into())
        }
        fn stream_replies(&self) -> bool {
            false
        }
        async fn run(&self, tx: mpsc::Sender<Event>) -> anyhow::Result<()> {
            tx.send(Event::Message(IncomingMessage::new(
                self.id(),
                "10",
                "/start login_invalid",
            )))
            .await?;
            let mut guest = IncomingMessage::new(self.id(), "42", "hello");
            guest.guest_query_id = Some("guest".into());
            tx.send(Event::Message(guest)).await?;
            for chat in [7, 8, 9, -42, 42] {
                let mut message = IncomingMessage::new(self.id(), chat.to_string(), "hello");
                message.is_group = chat < 0;
                tx.send(Event::Message(message)).await?;
            }
            Ok(())
        }
        async fn send(&self, chat: &str, text: &str) -> anyhow::Result<MessageId> {
            self.0.lock().unwrap().push((chat.into(), text.into()));
            Ok(MessageId("1".into()))
        }
        async fn typing(&self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn unseen_telegram_chats_are_isolated_and_restored_with_schedulers() {
        let root = tempfile::tempdir().unwrap();
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let captured = requests.clone();
        let router = axum::Router::new().route("/responses", axum::routing::post(move |axum::Json(request): axum::Json<Value>| {
            let captured = captured.clone();
            async move {
                let decision = request["instructions"].as_str().unwrap().starts_with("You are a decision component.");
                let stream = request["stream"] == true;
                captured.lock().unwrap().push(request);
                let response = json!({"output":[{"type":"message","content":[{"type":"output_text","text":if decision { r#"{"choice":"reply"}"# } else { "done" }}]}]});
                use axum::response::IntoResponse;
                if stream {
                    ([("content-type", "text/event-stream")], format!("data: {}\n\n", json!({"type":"response.completed","response":response}))).into_response()
                } else {
                    axum::Json(response).into_response()
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let config: Config = toml::from_str(&format!("system_prompt='OWNER_PROMPT'\n[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='test'\n[sessions]\nauto_switch=false\n[companion]\nlearning_enabled=false\nproactive_enabled=false\n[provider.primary]\ntype='openai'\nmodel='test'\napi_key='test'\napi_base='{endpoint}'")).unwrap();
        let owner_path = crate::spaces::telegram(root.path(), "main", 42);
        tokio::fs::create_dir_all(owner_path.join("memories"))
            .await
            .unwrap();
        tokio::fs::write(owner_path.join("memories/USER.md"), "PRIVATE_OWNER_FACT")
            .await
            .unwrap();
        let app = crate::bootstrap::build_app_at(&config, &owner_path, false)
            .await
            .unwrap();
        let auth = crate::web::auth::AuthStore::new(root.path());
        auth.grant_test_user(7).unwrap();
        auth.grant_test_user(8).unwrap();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut gateway = GatewayRunner::new(app.agent_runner.clone())
            .with_telegram_spaces(&config, root.path(), app.clone())
            .await
            .unwrap()
            .add_interface(NewChats(sent.clone()));
        assert_eq!(gateway.chat_runners.as_ref().unwrap().len(), 1);
        assert!(
            gateway
                .prepare_telegram_chat("telegram:main", -42)
                .await
                .is_err()
        );
        assert!(
            gateway
                .prepare_telegram_chat("telegram:main", 9)
                .await
                .is_err()
        );
        assert!(!crate::spaces::telegram(root.path(), "main", 9).exists());
        assert!(!crate::spaces::telegram(root.path(), "main", -42).exists());
        assert!(!crate::spaces::telegram(root.path(), "main", 10).exists());
        assert!(
            !crate::spaces::telegram(root.path(), "main", 42)
                .with_file_name("guest-42")
                .exists()
        );
        tokio::time::timeout(Duration::from_secs(20), gateway.run())
            .await
            .unwrap()
            .unwrap();
        let mut restored = GatewayRunner::new(app.agent_runner.clone())
            .with_telegram_spaces(&config, root.path(), app.clone())
            .await
            .unwrap();
        assert_eq!(restored.chat_runners.as_ref().unwrap().len(), 3);
        assert_eq!(restored.reminders.len(), 3);
        assert_eq!(restored.wakeups.len(), 3);
        assert!(Arc::ptr_eq(
            restored
                .runner_for(&SessionId::new("telegram:main:42"))
                .unwrap(),
            &app.agent_runner
        ));
        for chat in [7, 8, 42] {
            let store = SqliteSessionStore::open(
                crate::spaces::telegram(root.path(), "main", chat).join("sessions"),
            )
            .unwrap();
            let session = store
                .load(&SessionId::new(format!("telegram:main:{chat}")))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                session.messages().len(),
                2,
                "chat {chat}: {:?}",
                sent.lock().unwrap()
            );
        }
        auth.revoke_user(7).unwrap();
        assert!(
            restored
                .prepare_telegram_chat("telegram:main", 7)
                .await
                .is_err()
        );
        let recovered = GatewayRunner::new(app.agent_runner.clone())
            .with_telegram_spaces(&config, root.path(), app.clone())
            .await
            .unwrap();
        assert_eq!(recovered.chat_runners.as_ref().unwrap().len(), 2);
        assert!(!crate::spaces::telegram(root.path(), "main", 9).exists());
        assert!(!crate::spaces::telegram(root.path(), "main", -42).exists());
        assert!(!crate::spaces::telegram(root.path(), "main", 10).exists());
        assert!(
            !crate::spaces::telegram(root.path(), "main", 42)
                .with_file_name("guest-42")
                .exists()
        );
        let requests = requests.lock().unwrap();
        assert!(
            sent.lock()
                .unwrap()
                .iter()
                .any(|(chat, text)| chat == "10" && text.contains("Начните вход заново"))
        );
        let main: Vec<_> = requests
            .iter()
            .filter(|request| {
                let instructions = request["instructions"].as_str().unwrap();
                !instructions.starts_with("You are a decision component.")
                    && !instructions.starts_with("Generate a conversation title")
            })
            .collect();
        assert_eq!(main.len(), 3);
        assert_eq!(
            main.iter()
                .filter(|request| request["instructions"]
                    .as_str()
                    .unwrap()
                    .contains("PRIVATE_OWNER_FACT"))
                .count(),
            1
        );
        assert_eq!(
            main.iter()
                .filter(
                    |request| request["tools"].as_array().unwrap().iter().any(|tool| {
                        tool["description"]
                            .as_str()
                            .unwrap_or_default()
                            .contains("Bashkit")
                    })
                )
                .count(),
            2
        );
        server.abort();
    }
}
