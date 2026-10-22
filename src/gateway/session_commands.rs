use super::*;
use crate::agent::session::Session;

impl GatewayRunner {
    pub(super) async fn handle_session_command(
        &self,
        message: &IncomingMessage,
    ) -> anyhow::Result<bool> {
        if !message.source.interface.0.starts_with("telegram:") {
            return Ok(false);
        }
        if message
            .callback
            .as_ref()
            .is_some_and(|callback| callback.data.starts_with("wa:"))
            || message.text.split_whitespace().next() == Some("/start")
                && message
                    .text
                    .split_whitespace()
                    .nth(1)
                    .is_some_and(|payload| payload.starts_with("login_"))
        {
            if let Err(error) = Box::pin(self.handle_login_request(message)).await {
                warn!("Telegram login request failed: {error:#}");
            }
            return Ok(true);
        }
        if !message.images.is_empty() || !message.files.is_empty() {
            return Ok(false);
        }
        if message.callback.is_some() {
            if let Err(error) = Box::pin(self.handle_session_callback(message)).await {
                warn!("Telegram session callback failed: {error:#}");
            }
            return Ok(true);
        }
        let mut words = message.text.split_whitespace();
        let Some(command @ ("/sessions" | "/session")) = words.next() else {
            return Ok(false);
        };
        let interface = self
            .interfaces
            .iter()
            .find(|interface| interface.id() == message.source.interface)
            .context("Telegram command interface unavailable")?;
        let language = self.message_language(message);
        let private = !message.is_group
            && message.guest_query_id.is_none()
            && message
                .source
                .conversation_id
                .parse::<u64>()
                .is_ok_and(|id| id > 0);
        if private && command == "/sessions" {
            if let Err(error) =
                Box::pin(self.send_session_picker(message, &words.collect::<Vec<_>>())).await
            {
                warn!("Telegram session picker failed: {error:#}");
                interface
                    .send(
                        &message.source.conversation_id,
                        language.pick(
                            "Could not open sessions. Try /sessions again.",
                            "Не удалось открыть диалоги. Попробуйте /sessions ещё раз.",
                        ),
                    )
                    .await?;
            }
            return Ok(true);
        }
        let text = if !private {
            language
                .pick(
                    "This command is available only in a private chat with the bot.",
                    "Эта команда доступна только в личном чате с ботом.",
                )
                .to_owned()
        } else {
            match self
                .session_command_text(message, command, words.collect())
                .await
            {
                Ok(text) => text,
                Err(error) => {
                    warn!("Telegram personal command {command} failed: {error:#}");
                    language.pick("Could not run the command. The session may be busy; try again later.", "Не удалось выполнить команду. Диалог может быть занят; попробуйте позже.")
                        .to_owned()
                }
            }
        };
        if let Some(query) = &message.guest_query_id {
            interface.answer_guest(query, &text).await?;
        } else {
            interface
                .send(&message.source.conversation_id, &text)
                .await?;
        }
        Ok(true)
    }

    async fn session_command_text(
        &self,
        message: &IncomingMessage,
        command: &str,
        args: Vec<&str>,
    ) -> anyhow::Result<String> {
        let language = self.message_language(message);
        let conversation = SessionId::new(format!(
            "{}:{}",
            message.source.interface, message.source.conversation_id
        ));
        let runner = self.runner_for(&conversation)?;
        let sessions: Vec<_> = runner
            .list_sessions()
            .await?
            .into_iter()
            .filter(personal_session)
            .collect();
        if command == "/session" {
            let [id] = args.as_slice() else {
                return Ok(language
                    .pick(
                        "Use /session <id>. List: /sessions",
                        "Используйте /session <id>. Список: /sessions",
                    )
                    .into());
            };
            let Some(session) = sessions.iter().find(|session| session.id().as_str() == *id) else {
                return Ok(language
                    .pick(
                        "Session not found. List: /sessions",
                        "Диалог не найден. Список: /sessions",
                    )
                    .into());
            };
            if let Some(store) = runner.sqlite_store()
                && store.inbox_busy(conversation.as_str()).await?
            {
                return Ok(language
                    .pick(
                        "The session is busy. Wait until it finishes or send /stop.",
                        "Диалог занят. Дождитесь завершения или отправьте /stop.",
                    )
                    .into());
            }
            runner.select_session(&conversation, session.id()).await?;
            return Ok(language.pick("Session selected.", "Диалог выбран.").into());
        }
        anyhow::bail!("Unknown session command")
    }
}

pub(super) fn personal_session(session: &Session) -> bool {
    !session.id().as_str().starts_with("companion:")
        && session.telegram_group().is_none()
        && !session.id().as_str().ends_with(":guest")
        && !(session.id().as_str().starts_with("telegram:")
            && session
                .id()
                .as_str()
                .rsplit(':')
                .next()
                .is_some_and(|chat| chat.starts_with('-')))
}

pub(super) async fn session_titles(
    runner: &AgentRunner,
) -> anyhow::Result<HashMap<String, String>> {
    let Some(store) = runner.sqlite_store() else {
        return Ok(HashMap::new());
    };
    store.with_connection(|db| {
        let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='web_chat_titles')", [], |row| row.get(0))?;
        if !exists { return Ok(HashMap::new()); }
        let mut statement = db.prepare("SELECT conversation_id,title FROM web_chat_titles")?;
        let mut titles: HashMap<String, String> = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let branches: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='web_chat_branches')", [], |row| row.get(0))?;
        if branches {
            let mut statement = db.prepare("SELECT b.session_id,t.title FROM web_chat_branches b JOIN web_chat_titles t ON t.conversation_id=b.root_id")?;
            for row in statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))? {
                let (id, title) = row?;
                titles.entry(id).or_insert(title);
            }
        }
        Ok(titles)
    }).await
}

#[cfg(test)]
mod tests {
    use super::*;
    include!("session_picker_tests.rs");
    include!("telegram_login_tests.rs");
    use crate::interfaces::InlineButton;
    use crate::{
        agent::{
            Agent, AssistantMessage, ContextBuilder, LlmContext,
            llm::LlmProvider,
            message::Message,
            session::{Session, SessionStore, SqliteSessionStore},
            tools::{Tool, ToolRegistry},
        },
        gateway::InterfaceId,
    };
    use std::sync::Mutex;
    type Menus = Arc<Mutex<Vec<(bool, String, Vec<Vec<InlineButton>>)>>>;
    type Callbacks = Arc<Mutex<Vec<String>>>;

    struct NoModel;
    #[async_trait::async_trait]
    impl LlmProvider for NoModel {
        async fn generate(
            &self,
            _: &LlmContext<'_>,
            _: &[&dyn Tool],
            _: Option<&mpsc::Sender<AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            panic!("personal commands must bypass the model");
        }
    }

    struct Commands {
        language: Language,
        sent: Arc<Mutex<Vec<(String, String)>>>,
        inputs: Mutex<Vec<IncomingMessage>>,
        menus: Menus,
        callbacks: Callbacks,
    }
    #[async_trait::async_trait]
    impl Interface for Commands {
        fn language(&self, _: &str) -> Language {
            self.language
        }
        fn id(&self) -> InterfaceId {
            InterfaceId("telegram:test".into())
        }
        fn stream_replies(&self) -> bool {
            false
        }
        async fn run(&self, tx: mpsc::Sender<Event>) -> anyhow::Result<()> {
            let inputs = std::mem::take(&mut *self.inputs.lock().unwrap());
            for message in inputs {
                let (admission, received) = oneshot::channel();
                let mut message = message;
                message.admission = Some(Arc::new(Mutex::new(Some(admission))));
                tx.send(Event::Message(message)).await?;
                received.await?;
                anyhow::ensure!(
                    !self.sent.lock().unwrap().is_empty(),
                    "acknowledged before command completed"
                );
            }
            Ok(())
        }
        async fn send(&self, chat: &str, text: &str) -> anyhow::Result<MessageId> {
            self.sent.lock().unwrap().push((chat.into(), text.into()));
            Ok(MessageId("1".into()))
        }
        async fn send_buttons(
            &self,
            chat: &str,
            text: &str,
            buttons: &[Vec<InlineButton>],
        ) -> anyhow::Result<MessageId> {
            self.menus
                .lock()
                .unwrap()
                .push((false, text.into(), buttons.to_vec()));
            self.send(
                chat,
                &format!(
                    "{text}\n{}",
                    buttons
                        .iter()
                        .flatten()
                        .map(|button| button.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            )
            .await
        }
        async fn edit_buttons(
            &self,
            _: &str,
            _: &MessageId,
            text: &str,
            buttons: &[Vec<InlineButton>],
        ) -> anyhow::Result<()> {
            self.menus
                .lock()
                .unwrap()
                .push((true, text.into(), buttons.to_vec()));
            Ok(())
        }
        async fn answer_callback(&self, id: &str, _: &str) -> anyhow::Result<()> {
            self.callbacks.lock().unwrap().push(id.into());
            Ok(())
        }
        async fn answer_guest(&self, _: &str, text: &str) -> anyhow::Result<MessageId> {
            self.send("guest", text).await
        }
    }

    async fn setup(
        inputs: Vec<IncomingMessage>,
    ) -> (
        tempfile::TempDir,
        GatewayRunner,
        Arc<SqliteSessionStore>,
        Arc<Mutex<Vec<(String, String)>>>,
    ) {
        let (dir, gateway, store, sent, _, _) = setup_with_model(inputs, Arc::new(NoModel)).await;
        (dir, gateway, store, sent)
    }

    async fn setup_with_model(
        inputs: Vec<IncomingMessage>,
        model: Arc<dyn LlmProvider>,
    ) -> (
        tempfile::TempDir,
        GatewayRunner,
        Arc<SqliteSessionStore>,
        Arc<Mutex<Vec<(String, String)>>>,
        Menus,
        Callbacks,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SqliteSessionStore::open_async(dir.path().into())
                .await
                .unwrap(),
        );
        let runner = Arc::new(AgentRunner::new(
            Arc::new(Agent::new(
                model,
                ToolRegistry::default(),
                ContextBuilder::new(""),
            )),
            store.clone(),
        ));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let menus = Menus::default();
        let callbacks = Callbacks::default();
        let gateway = GatewayRunner::new(runner).add_interface(Commands {
            language: Language::Ru,
            sent: sent.clone(),
            inputs: Mutex::new(inputs),
            menus: menus.clone(),
            callbacks: callbacks.clone(),
        });
        (dir, gateway, store, sent, menus, callbacks)
    }

    fn command(text: &str) -> IncomingMessage {
        IncomingMessage::new(InterfaceId("telegram:test".into()), "42", text)
    }

    #[tokio::test]
    async fn session_commands_and_picker_follow_the_selected_language_without_model_work() {
        for language in [Language::En, Language::Ru] {
            let (_dir, mut gateway, _store, sent, menus, callbacks) =
                setup_with_model(vec![], Arc::new(NoModel)).await;
            gateway.interfaces = vec![Arc::new(Commands {
                language,
                sent: sent.clone(),
                inputs: Mutex::new(Vec::new()),
                menus: menus.clone(),
                callbacks,
            })];
            for text in ["/sessions", "/session missing", "/new", "/stop"] {
                gateway
                    .handle_event(Event::Message(command(text)), None, None)
                    .await
                    .unwrap();
            }
            let menu = menus.lock().unwrap();
            assert!(menu[0].1.starts_with(language.pick("Sessions", "Диалоги")));
            assert_eq!(
                menu[0].2.last().unwrap()[0].text,
                language.pick("＋ New session", "＋ Новый диалог")
            );
            let sent = sent.lock().unwrap();
            assert_eq!(
                sent[1].1,
                language.pick(
                    "Session not found. List: /sessions",
                    "Диалог не найден. Список: /sessions"
                )
            );
            assert_eq!(
                sent[2].1,
                language.pick("New session created.", "Новый диалог создан.")
            );
            assert_eq!(
                sent[3].1,
                language.pick("The agent is not running.", "Агент не запущен.")
            );
        }
    }

    #[tokio::test]
    async fn session_picker_hides_raw_ids() {
        let (_dir, gateway, store, sent) = setup(vec![]).await;
        save(&store, "cli:main", "План питания").await;
        gateway
            .handle_event(Event::Message(command("/sessions")), None, None)
            .await
            .unwrap();
        let sent = sent.lock().unwrap();
        assert!(sent[0].1.contains("План питания"));
        assert!(
            !sent[0].1.contains("cli:main"),
            "raw session IDs are not picker labels"
        );
    }

    struct NamedReply {
        release_title: Arc<tokio::sync::Notify>,
        title_started: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for NamedReply {
        async fn generate(
            &self,
            context: &LlmContext<'_>,
            _: &[&dyn Tool],
            _: Option<&mpsc::Sender<AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            let title = context
                .instructions
                .starts_with("Generate a conversation title");
            if title {
                self.title_started.notify_one();
                self.release_title.notified().await;
            }
            Ok((
                vec![Message::Assistant(AssistantMessage {
                    content: if title {
                        "План питания на неделю"
                    } else {
                        "Вот меню."
                    }
                    .into(),
                })],
                None,
            ))
        }
    }

    #[tokio::test]
    async fn telegram_generates_shared_titles_without_delaying_replies() {
        let release_title = Arc::new(tokio::sync::Notify::new());
        let (_dir, gateway, store, sent, _, _) = setup_with_model(
            vec![],
            Arc::new(NamedReply {
                release_title: release_title.clone(),
                title_started: Default::default(),
            }),
        )
        .await;
        let mut input = command("Составь меню на неделю");
        input.request_id = "telegram-title-test".into();
        tokio::time::timeout(
            Duration::from_secs(2),
            gateway.handle_event(Event::Message(input), None, None),
        )
        .await
        .expect("title generation must not delay the reply")
        .unwrap();
        let id = store
            .active_id(&SessionId::new("telegram:test:42"))
            .await
            .unwrap();
        let before = store.load(&id).await.unwrap().unwrap();
        assert!(before.messages().iter().any(|message| matches!(message, Message::Assistant(reply) if reply.content == "Вот меню.")), "{before:?}");
        release_title.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if session_titles(gateway.runner_for(&id).unwrap())
                    .await
                    .unwrap()
                    .get(id.as_str())
                    .is_some_and(|title| title == "План питания на неделю")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Telegram did not save a shared generated title");
        gateway
            .handle_event(Event::Message(command("/sessions")), None, None)
            .await
            .unwrap();
        assert!(
            sent.lock()
                .unwrap()
                .last()
                .unwrap()
                .1
                .contains("План питания на неделю")
        );
        assert_eq!(
            serde_json::to_value(store.load(&id).await.unwrap().unwrap()).unwrap(),
            serde_json::to_value(before).unwrap()
        );
    }

    #[tokio::test]
    async fn shared_title_generation_keeps_web_branch_names_and_manual_edits() {
        let release_title = Arc::new(tokio::sync::Notify::new());
        let title_started = Arc::new(tokio::sync::Notify::new());
        let (dir, gateway, store, _, _, _) = setup_with_model(
            vec![],
            Arc::new(NamedReply {
                release_title: release_title.clone(),
                title_started: title_started.clone(),
            }),
        )
        .await;
        let runner = gateway.agent_runner.clone();
        let _chat = crate::web::chat::Chat::new(&runner, dir.path())
            .await
            .unwrap();
        save(&store, "root", "Меню на неделю").await;
        save(&store, "branch", "Альтернативное меню").await;
        store
            .with_connection(|db| {
                db.execute(
                    "INSERT INTO web_chat_branches VALUES('branch','root','root',0)",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let job =
            tokio::spawn(
                async move { runner.generate_saved_title(&SessionId::new("branch")).await },
            );
        tokio::time::timeout(Duration::from_secs(2), title_started.notified())
            .await
            .unwrap();
        store
            .with_connection(|db| {
                db.execute(
                    "INSERT INTO web_chat_titles VALUES('root','Название пользователя')",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        release_title.notify_one();
        tokio::time::timeout(Duration::from_secs(2), job)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            store.saved_title("root").await.unwrap().as_deref(),
            Some("Название пользователя")
        );
        assert_eq!(store.saved_title("branch").await.unwrap(), None);
        let titles = session_titles(&gateway.agent_runner).await.unwrap();
        assert_eq!(titles["root"], "Название пользователя");
        assert_eq!(titles["branch"], "Название пользователя");
        tokio::time::timeout(
            Duration::from_secs(2),
            gateway
                .agent_runner
                .generate_saved_title(&SessionId::new("branch")),
        )
        .await
        .unwrap()
        .unwrap();
    }

    #[test]
    fn personal_filter_excludes_background_group_and_guest_histories() {
        for (id, visible) in [
            ("cli:main", true),
            ("telegram:test:42", true),
            ("companion:wakeup:42", false),
            ("companion:proactive:42", false),
            ("telegram:test:-42", false),
            ("telegram:test:42:guest", false),
        ] {
            assert_eq!(
                personal_session(&Session::new(SessionId::new(id))),
                visible,
                "{id}"
            );
        }
    }

    async fn save(store: &SqliteSessionStore, id: &str, title: &str) {
        let mut session = Session::new(SessionId::new(id));
        session.push(Message::User(UserMessage {
            content: title.into(),
            images: vec![],
            files: vec![],
        }));
        session.push(Message::Assistant(AssistantMessage {
            content: "PRIVATE OLD ANSWER".into(),
        }));
        store.save(&session).await.unwrap();
    }

    #[tokio::test]
    async fn personal_selection_confirms_without_replay_or_history() {
        let (_dir, gateway, store, sent) = setup(vec![]).await;
        save(&store, "cli:main", "original title").await;
        gateway
            .handle_event(Event::Message(command("/session cli:main")), None, None)
            .await
            .unwrap();
        assert_eq!(
            store
                .active_id(&SessionId::new("telegram:test:42"))
                .await
                .unwrap()
                .as_str(),
            "cli:main"
        );
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "42");
        assert!(!sent[0].1.contains("PRIVATE OLD ANSWER"));
        assert!(!sent[0].1.contains("original title"));
        assert_eq!(
            store
                .load(&SessionId::new("cli:main"))
                .await
                .unwrap()
                .unwrap()
                .messages()
                .len(),
            2
        );
        assert!(
            store
                .load(&SessionId::new("telegram:test:42"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn personal_commands_bypass_durable_admission() {
        let (_dir, gateway, store, sent) = setup(vec![command("/sessions")]).await;
        save(&store, "cli:main", "Saved topic").await;
        tokio::time::timeout(Duration::from_secs(5), gateway.run())
            .await
            .unwrap()
            .unwrap();
        assert!(sent.lock().unwrap()[0].1.contains("Saved topic"));
        let submissions = store
            .with_connection(|db| {
                Ok(db.query_row("SELECT count(*) FROM submissions", [], |row| {
                    row.get::<_, i64>(0)
                })?)
            })
            .await
            .unwrap();
        assert_eq!(submissions, 0);
    }

    #[tokio::test]
    async fn personal_list_uses_saved_titles_and_paginates_every_session() {
        let (_dir, gateway, store, sent) = setup(vec![]).await;
        for id in 0..12 {
            save(
                &store,
                &format!("saved-{id:02}"),
                &format!("Тема {id:02} {}", "я".repeat(400)),
            )
            .await;
        }
        save(&store, "companion:private", "COMPANION SECRET").await;
        save(&store, "telegram:test:-42", "GROUP SECRET").await;
        save(&store, "telegram:test:42:guest", "GUEST SECRET").await;
        store.with_connection(|db| {
            db.execute_batch("CREATE TABLE web_chat_titles(conversation_id TEXT PRIMARY KEY,title TEXT NOT NULL); INSERT INTO web_chat_titles VALUES('saved-00','Web custom title')")?;
            Ok(())
        }).await.unwrap();
        for text in [
            "/sessions",
            "/sessions 2",
            "/sessions 0",
            "/sessions 9999999999999999999999999999999999999",
        ] {
            gateway
                .handle_event(Event::Message(command(text)), None, None)
                .await
                .unwrap();
        }
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 4);
        assert!(sent[0].1.contains("Далее"));
        let both = format!("{}{}", sent[0].1, sent[1].1);
        for id in 1..12 {
            assert!(both.contains(&format!("Тема {id:02}")));
        }
        assert!(both.contains("Web custom title"));
        assert!(!both.contains("SECRET"));
        assert!(!both.contains("companion:"));
        for (_, text) in sent.iter() {
            assert!(text.encode_utf16().count() < 4096);
        }
        assert!(sent[2].1.contains("/sessions"));
        assert!(sent[3].1.contains("/sessions"));
    }

    #[tokio::test]
    async fn personal_selection_rejects_unknown_foreign_internal_ids_and_queued_work() {
        use crate::agent::inbox::{BusyMode, SubmissionRequest};
        let (_dir, gateway, store, sent) = setup(vec![]).await;
        save(&store, "cli:main", "Saved topic").await;
        save(&store, "companion:private", "COMPANION SECRET").await;
        let conversation = SessionId::new("telegram:test:42");
        for text in [
            "/session ../outside",
            "/session foreign-user-id",
            "/session companion:private",
            "/session",
            "/session cli:main extra",
        ] {
            gateway
                .handle_event(Event::Message(command(text)), None, None)
                .await
                .unwrap();
        }
        assert_eq!(store.active_id(&conversation).await.unwrap(), conversation);
        store
            .submit(SubmissionRequest::new(
                &conversation,
                command("queued input"),
                BusyMode::Steer,
            ))
            .await
            .unwrap();
        gateway
            .handle_event(Event::Message(command("/session cli:main")), None, None)
            .await
            .unwrap();
        assert_eq!(store.active_id(&conversation).await.unwrap(), conversation);
        assert!(sent.lock().unwrap().last().unwrap().1.contains("/stop"));
    }

    #[tokio::test]
    async fn personal_commands_reject_groups_and_guests_without_model_or_history() {
        let (_dir, gateway, store, sent) = setup(vec![]).await;
        for text in ["/sessions", "/session cli:main"] {
            for guest in [false, true] {
                let mut message = command(text);
                message.is_group = true;
                message.source.conversation_id = "-42".into();
                if guest {
                    message.guest_query_id = Some("query".into());
                }
                gateway
                    .handle_event(Event::Message(message), None, None)
                    .await
                    .unwrap();
            }
        }
        assert_eq!(sent.lock().unwrap().len(), 4);
        assert!(
            sent.lock()
                .unwrap()
                .iter()
                .all(|(_, text)| text.contains("личном чате"))
        );
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn personal_commands_use_only_the_current_users_store() {
        let (_owner_dir, gateway, owner_store, sent) = setup(vec![]).await;
        save(&owner_store, "owner-only", "OWNER PRIVATE TOPIC").await;
        let (_user_dir, user_gateway, user_store, _) = setup(vec![]).await;
        save(&user_store, "user-only", "USER TOPIC").await;
        let conversation = SessionId::new("telegram:test:42");
        let gateway = gateway.with_chat_runners(HashMap::from([(
            conversation.clone(),
            user_gateway.agent_runner,
        )]));
        for text in ["/sessions", "/session owner-only", "/session user-only"] {
            gateway
                .handle_event(Event::Message(command(text)), None, None)
                .await
                .unwrap();
        }
        let sent = sent.lock().unwrap();
        assert!(sent[0].1.contains("USER TOPIC"));
        assert!(!sent[0].1.contains("OWNER PRIVATE TOPIC"));
        assert!(!sent[0].1.contains("owner-only"));
        assert!(sent[1].1.contains("не найден"));
        assert_eq!(
            user_store.active_id(&conversation).await.unwrap().as_str(),
            "user-only"
        );
        assert_eq!(
            owner_store.active_id(&conversation).await.unwrap(),
            conversation
        );
    }

    #[tokio::test]
    async fn removed_login_command_is_not_a_personal_command() {
        let (_dir, gateway, store, sent) = setup(vec![]).await;
        for text in [
            "/login",
            "/login anything",
            "/login@TestBot",
            "/login@OtherBot",
        ] {
            for (group, guest) in [(false, false), (true, false), (true, true)] {
                let mut message = command(text);
                message.is_group = group;
                if group {
                    message.source.conversation_id = "-42".into();
                }
                if guest {
                    message.guest_query_id = Some("query".into());
                }
                assert!(!gateway.handle_session_command(&message).await.unwrap());
            }
        }
        assert!(sent.lock().unwrap().is_empty());
        assert!(store.list().await.unwrap().is_empty());
    }
}
