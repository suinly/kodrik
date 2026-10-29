use super::*;
use crate::{bootstrap::AppContext, config::Config};
use std::path::{Path, PathBuf};

pub(super) struct TelegramSpaces {
    pub(super) config: Config,
    pub(super) root: PathBuf,
    pub(super) users: Arc<crate::users::UserSpaces>,
}

impl GatewayRunner {
    pub(super) async fn prepare_telegram_guest(
        &mut self,
        message: &IncomingMessage,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.allows_message(message)?,
            "Telegram guest access denied"
        );
        let id = message.session_id();
        if self
            .chat_runners
            .as_ref()
            .is_some_and(|runners| runners.contains_key(&id))
        {
            return Ok(());
        }
        let runner = self.build_telegram_guest_runner(message).await?;
        // Guest response slots cannot survive a process restart; retain history, cancel old work.
        runner.discard_guest_queries(&id).await?;
        self.chat_runners
            .as_mut()
            .context("Missing Telegram chat runners")?
            .insert(id, runner);
        Ok(())
    }

    pub(super) async fn build_telegram_guest_runner(
        &self,
        message: &IncomingMessage,
    ) -> anyhow::Result<Arc<AgentRunner>> {
        let spaces = self
            .telegram_spaces
            .as_ref()
            .context("Missing Telegram spaces")?;
        anyhow::ensure!(
            self.allows_message(message)?,
            "Telegram guest access denied"
        );
        let user = message
            .guest_user_id
            .context("Missing Telegram guest sender")?;
        let key = message
            .source
            .interface
            .0
            .strip_prefix("telegram:")
            .context("Expected Telegram interface")?;
        anyhow::ensure!(
            spaces.config.gateway.contains_key(key),
            "Unknown Telegram interface"
        );
        let (first, second) = message
            .guest_participants()
            .context("Guest access requires a private chat")?;
        let directory = crate::spaces::telegram_interface(&spaces.root, key)
            .join("guest-chats")
            .join(format!("{first}-{second}"));
        let mut config = spaces.config.clone();
        config.sessions.auto_switch = false;
        let app = crate::bootstrap::build_app_with_access(
            &config,
            &directory,
            true,
            Some((crate::web::auth::AuthStore::new(&spaces.root), user)),
        )
        .await?;
        Ok(app.agent_runner)
    }

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

    struct GuestChats(Arc<Mutex<Vec<(String, String)>>>);
    #[async_trait::async_trait]
    impl Interface for GuestChats {
        fn id(&self) -> InterfaceId {
            InterfaceId("telegram:main".into())
        }
        async fn run(&self, tx: mpsc::Sender<Event>) -> anyhow::Result<()> {
            for (user, chat) in [(7, 8), (8, 7), (7, 99), (42, 99), (9, 8)] {
                let mut message = IncomingMessage::new(
                    self.id(),
                    chat.to_string(),
                    format!("guest {user}/{chat}"),
                );
                message.guest_query_id = Some(format!("query-{user}-{chat}"));
                message.guest_user_id = Some(user);
                tx.send(Event::Message(message)).await?;
            }
            Ok(())
        }
        async fn send(&self, _: &str, _: &str) -> anyhow::Result<MessageId> {
            panic!("guests must never send ordinary messages");
        }
        async fn answer_guest(&self, query: &str, text: &str) -> anyhow::Result<MessageId> {
            self.0.lock().unwrap().push((query.into(), text.into()));
            Ok(MessageId("guest-reply".into()))
        }
    }

    #[tokio::test]
    async fn invited_guests_share_history_and_space_only_with_their_peer() {
        let root = tempfile::tempdir().unwrap();
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let captured = requests.clone();
        let started = Arc::new(tokio::sync::Notify::new());
        let model_started = started.clone();
        let router = axum::Router::new().route("/responses", axum::routing::post(move |axum::Json(request): axum::Json<Value>| {
            let captured = captured.clone();
            let started = model_started.clone();
            async move {
                if request["input"].to_string().contains("hold shared guest") {
                    started.notify_one();
                    std::future::pending::<()>().await;
                }
                let stream = request["stream"] == true;
                let input = request["input"].as_array().unwrap();
                let last_user = input.iter().rposition(|item| item["role"] == "user");
                let output = if let Some(last_user) = last_user {
                    let steps = input[last_user + 1..].iter().filter(|item| item["type"] == "function_call_output").count();
                    if steps == 3 {
                        json!({"type":"message","content":[{"type":"output_text","text":"guest done"}]})
                    } else {
                        let content = input[last_user]["content"].to_string();
                        let writer = content.contains("guest 7/8");
                        let (name, args) = match (steps, writer) {
                            (0, _) => ("exec_command", json!({"cmd":if writer { "printf Arrival > /workspace/watchlist.txt" } else { "cat /workspace/watchlist.txt" },"workdir":null,"timeout_ms":null})),
                            (1, true) => ("datasets", json!({"action":"create","dataset":"watchlist","title":"Watchlist","fields":[{"name":"title","type":"string","required":true},{"name":"watched","type":"boolean","required":true},{"name":"added_by","type":"integer","required":true}]})),
                            (1, false) => ("datasets", json!({"action":"list"})),
                            (2, true) => ("datasets", json!({"action":"upsert","dataset":"watchlist","request_id":"72aab8f6-1584-479b-b84a-74bde54275cc","records":[{"id":"arrival","expected_revision":null,"values":[{"field":"title","value":"Arrival"},{"field":"watched","value":false},{"field":"added_by","value":7}]}]})),
                            (2, false) => ("datasets", json!({"action":"query","dataset":"watchlist"})),
                            _ => unreachable!(),
                        };
                        json!({"type":"function_call","name":name,"call_id":format!("call-{}", input.len()),"arguments":args.to_string()})
                    }
                } else { json!({"type":"message","content":[{"type":"output_text","text":"guest done"}]}) };
                captured.lock().unwrap().push(request);
                let response = json!({"output":[output]});
                use axum::response::IntoResponse;
                if stream {
                    ([("content-type", "text/event-stream")], format!("data: {}\n\n", json!({"type":"response.completed","response":response}))).into_response()
                } else { axum::Json(response).into_response() }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let config: Config = toml::from_str(&format!("system_prompt='PRIVATE_OWNER_PROMPT'\n[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='test'\n[sessions]\nauto_switch=false\n[companion]\nlearning_enabled=false\nproactive_enabled=false\n[provider.primary]\ntype='openai'\nmodel='test'\napi_key='test'\napi_base='{endpoint}'")).unwrap();
        let owner_path = crate::spaces::personal(root.path(), 42);
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
        for user in [7, 8] {
            auth.grant_test_user(user).unwrap();
        }
        // A previous process stopped with an active run and an unanswered queued query.
        let guest_path =
            crate::spaces::telegram_interface(root.path(), "main").join("guest-chats/7-8");
        let store = SqliteSessionStore::open(guest_path.join("sessions")).unwrap();
        let id = SessionId::new("telegram:main:7:8:guest");
        let session = crate::agent::session::Session::new(id.clone());
        let mut checkpoint = crate::agent::run_state::Checkpoint::new(&store, &id, &session);
        checkpoint.record.isolated = Some(true);
        checkpoint
            .save(&session, crate::agent::run_state::RunPhase::NeedModel, None)
            .await
            .unwrap();
        let mut expired = IncomingMessage::new(
            InterfaceId("telegram:main".into()),
            "8",
            "expired guest query",
        );
        expired.guest_query_id = Some("expired-query".into());
        expired.guest_user_id = Some(7);
        let mut request = crate::agent::inbox::SubmissionRequest::new(
            &id,
            expired,
            crate::agent::inbox::BusyMode::FollowUp,
        );
        request.isolated = true;
        let expired = store.submit(request).await.unwrap();
        let sent = Arc::new(Mutex::new(Vec::new()));
        for _ in 0..2 {
            let gateway = GatewayRunner::new(app.agent_runner.clone())
                .with_telegram_spaces(&config, root.path(), app.clone())
                .await
                .unwrap()
                .add_interface(GuestChats(sent.clone()));
            tokio::time::timeout(Duration::from_secs(20), gateway.run())
                .await
                .unwrap()
                .unwrap();
        }
        auth.revoke_user(7).unwrap();
        let gateway = GatewayRunner::new(app.agent_runner.clone())
            .with_telegram_spaces(&config, root.path(), app.clone())
            .await
            .unwrap()
            .add_interface(GuestChats(sent.clone()));
        tokio::time::timeout(Duration::from_secs(20), gateway.run())
            .await
            .unwrap()
            .unwrap();
        assert!(store.unfinished_runs().await.unwrap().is_empty());
        let mut gateway = GatewayRunner::new(app.agent_runner.clone())
            .with_telegram_spaces(&config, root.path(), app.clone())
            .await
            .unwrap()
            .add_interface(GuestChats(sent.clone()));
        let mut peer = IncomingMessage::new(InterfaceId("telegram:main".into()), "7", "guest 8/7");
        peer.guest_query_id = Some("peer-after-revocation".into());
        peer.guest_user_id = Some(8);
        gateway.prepare_telegram_guest(&peer).await.unwrap();
        let mut revoked = IncomingMessage::new(
            InterfaceId("telegram:main".into()),
            "8",
            "already queued before revocation",
        );
        revoked.guest_query_id = Some("queued-revoked".into());
        revoked.guest_user_id = Some(7);
        let runner = gateway.runner_for(&peer.session_id()).unwrap();
        let revoked = runner
            .submit(crate::agent::inbox::SubmissionRequest::new(
                &peer.session_id(),
                revoked,
                crate::agent::inbox::BusyMode::FollowUp,
            ))
            .await
            .unwrap();
        runner
            .submit(crate::agent::inbox::SubmissionRequest::new(
                &peer.session_id(),
                peer.clone(),
                crate::agent::inbox::BusyMode::FollowUp,
            ))
            .await
            .unwrap();
        let (queue, inputs) = mpsc::channel(1);
        queue.send(peer.clone()).await.unwrap();
        drop(queue);
        tokio::time::timeout(
            Duration::from_secs(3),
            Box::pin(gateway.run_conversation(inputs)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            store
                .submission_by_id(&revoked.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            crate::agent::inbox::SubmissionStatus::Cancelled
        );

        peer.text = "hold shared guest".into();
        peer.guest_query_id = Some("interrupted-query".into());
        peer.request_id = uuid::Uuid::new_v4().to_string();
        let worker = tokio::spawn(async move {
            Box::pin(gateway.handle_event(Event::Message(peer), None, None)).await
        });
        tokio::time::timeout(Duration::from_secs(3), started.notified())
            .await
            .unwrap();
        app.agent_runner.suspend();
        tokio::time::timeout(Duration::from_secs(3), worker)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        server.abort();
        assert_eq!(
            store.unfinished_runs().await.unwrap().len(),
            1,
            "shutdown must retain the interrupted checkpoint without sending a guest reply"
        );
        assert_eq!(
            store
                .submission_by_id(&expired.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            crate::agent::inbox::SubmissionStatus::Cancelled
        );
        let sent = sent.lock().unwrap();
        assert_eq!(
            sent.len(),
            11,
            "owner and invited callers must receive guest replies"
        );
        assert!(sent.iter().all(|(_, text)| text == "guest done"));
        assert!(!sent.iter().any(|(query, _)| query == "expired-query"));
        let requests = requests.lock().unwrap();
        let main: Vec<_> = requests
            .iter()
            .filter(|r| {
                r["instructions"]
                    .as_str()
                    .unwrap()
                    .starts_with("You are Kodrik")
                    && {
                        let input = r["input"].as_array().unwrap();
                        input
                            .iter()
                            .rposition(|item| item["role"] == "user")
                            .is_some_and(|last_user| {
                                input[last_user + 1..]
                                    .iter()
                                    .filter(|item| item["type"] == "function_call_output")
                                    .count()
                                    == 3
                            })
                    }
            })
            .collect();
        assert_eq!(main.len(), 11);
        let mut history_lengths = std::collections::BTreeMap::<String, Vec<usize>>::new();
        for request in main {
            let text = request.to_string();
            assert!(!text.contains("PRIVATE_OWNER"));
            let input = request["input"].as_array().unwrap();
            let users: Vec<_> = input.iter().filter(|item| item["role"] == "user").collect();
            let latest = users.last().unwrap()["content"].to_string();
            let caller = ["guest 7/8", "guest 8/7", "guest 7/99", "guest 42/99"]
                .into_iter()
                .find(|caller| latest.contains(caller))
                .unwrap();
            history_lengths
                .entry(caller.into())
                .or_default()
                .push(users.len());
            let last_user = input
                .iter()
                .rposition(|item| item["role"] == "user")
                .unwrap();
            let outputs: Vec<Value> = input[last_user + 1..]
                .iter()
                .filter(|item| item["type"] == "function_call_output")
                .map(|item| serde_json::from_str(item["output"].as_str().unwrap()).unwrap())
                .collect();
            if caller == "guest 8/7" {
                assert_eq!(outputs[0]["stdout"], "Arrival");
                assert_eq!(
                    outputs[2]["records"][0]["values"],
                    json!({"title":"Arrival","watched":false,"added_by":7})
                );
            }
            if matches!(caller, "guest 7/99" | "guest 42/99") {
                assert_ne!(outputs[0]["exit_code"], 0);
                assert_eq!(outputs[1]["datasets"], json!([]));
            }
            let tools = request["tools"].as_array().unwrap();
            assert!(!tools.iter().any(|tool| tool["name"] == "publish_page"));
            let shell = tools
                .iter()
                .find(|tool| tool["name"] == "exec_command")
                .unwrap();
            assert!(shell["description"].as_str().unwrap().contains("Bashkit"));
        }
        assert_eq!(history_lengths["guest 7/8"], vec![1, 3]);
        assert_eq!(history_lengths["guest 8/7"], vec![2, 4, 5, 6]);
        assert_eq!(history_lengths["guest 7/99"], vec![1, 2]);
        assert_eq!(history_lengths["guest 42/99"], vec![1, 2, 3]);
    }

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
