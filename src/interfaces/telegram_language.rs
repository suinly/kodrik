use super::*;

impl TelegramInterface {
    fn language_path(&self, chat: i64) -> anyhow::Result<std::path::PathBuf> {
        anyhow::ensure!(chat != 0, "Invalid Telegram chat ID");
        let root = self
            .space_root
            .as_ref()
            .or(self.access_root.as_ref())
            .context("Language storage is unavailable")?;
        Ok(crate::spaces::telegram_interface(root, &self.tag)
            .join("languages")
            .join(format!("{chat}.json")))
    }

    pub(super) fn chat_language(&self, conversation: &str) -> Language {
        let Ok(chat) = conversation.parse::<i64>() else {
            return Language::En;
        };
        let mut languages = self.languages.write().unwrap();
        languages
            .entry(chat)
            .or_insert_with(|| {
                let saved =
                    self.language_path(chat)
                        .ok()
                        .and_then(|path| match std::fs::read(path) {
                            Ok(bytes) => match serde_json::from_slice::<Language>(&bytes) {
                                Ok(language) => Some(language),
                                Err(error) => {
                                    log::warn!("Invalid Telegram language preference: {error}");
                                    None
                                }
                            },
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                            Err(error) => {
                                log::warn!("Could not read Telegram language preference: {error}");
                                None
                            }
                        });
                (saved.unwrap_or(Language::En), saved.is_some())
            })
            .0
    }

    pub(super) fn detect_language(&self, chat: i64, locale: Option<&str>) {
        self.chat_language(&chat.to_string());
        if let Some(locale) = locale {
            let mut languages = self.languages.write().unwrap();
            let entry = languages.get_mut(&chat).unwrap();
            if !entry.1 {
                entry.0 = Language::from_locale(locale);
            }
        }
    }

    async fn save_language(&self, chat: i64, language: Language) -> anyhow::Result<()> {
        let path = self.language_path(chat)?;
        let languages = self.languages.clone();
        // Finish an atomic preference write even if the polling task is cancelled.
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            use std::io::Write;
            std::fs::create_dir_all(path.parent().unwrap())?;
            let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
            let result = (|| {
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temporary)?;
                file.write_all(&serde_json::to_vec(&language)?)?;
                file.sync_all()?;
                std::fs::rename(&temporary, path)?;
                languages.write().unwrap().insert(chat, (language, true));
                Ok(())
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(temporary);
            }
            result
        })
        .await??;
        Ok(())
    }

    pub(super) async fn language_command(
        &self,
        chat: i64,
        args: &str,
        guest: Option<&str>,
    ) -> anyhow::Result<()> {
        let language = self.language(&chat.to_string());
        let words: Vec<_> = args.split_whitespace().collect();
        if guest.is_none() && words.is_empty() {
            self.send_buttons(
                &chat.to_string(),
                language.pick("Choose a language:", "Выберите язык:"),
                &[vec![
                    super::super::InlineButton {
                        text: "English".into(),
                        data: format!("lang:{chat}:en"),
                    },
                    super::super::InlineButton {
                        text: "Русский".into(),
                        data: format!("lang:{chat}:ru"),
                    },
                ]],
            )
            .await?;
            return Ok(());
        }
        let text = if guest.is_some() {
            language.pick(
                "Change language in a regular chat: /language en or /language ru.",
                "Измените язык в обычном чате: /language en или /language ru.",
            )
        } else if let [value] = words.as_slice()
            && let Ok(selected) = value.parse::<Language>()
        {
            self.select_language(chat, selected).await
        } else {
            language.pick(
                "Choose a language: /language en or /language ru.",
                "Выберите язык: /language en или /language ru.",
            )
        };
        if let Some(query) = guest {
            self.answer_guest(query, text).await?;
        } else {
            self.api_request("sendMessage", json!({"chat_id":chat,"text":text}))
                .await?;
        }
        Ok(())
    }

    pub(super) async fn language_callback(
        &self,
        query: teloxide::types::CallbackQuery,
        bot_username: &str,
    ) -> anyhow::Result<()> {
        let message = query.regular_message();
        let parts: Vec<_> = query
            .data
            .as_deref()
            .unwrap_or_default()
            .split(':')
            .collect();
        let selected = match parts.as_slice() {
            ["lang", chat, language]
                if message.is_some_and(|message| *chat == message.chat.id.0.to_string()) =>
            {
                language.parse::<Language>().ok()
            }
            _ => None,
        };
        let own_chat = !query.from.is_bot
            && message.is_some_and(|message| {
                message.chat.id.0 != 0
                    && (message.chat.is_private()
                        && message.chat.id.0 > 0
                        && message.chat.id.0 as u64 == query.from.id.0
                        || message.chat.is_group()
                        || message.chat.is_supergroup())
                    && (self.access_root.is_none()
                        || message.chat.is_private()
                            && self.allows_recipient(&message.chat.id.to_string()))
            });
        let valid = selected.is_some()
            && own_chat
            && message.is_some_and(|message| {
                message.from.as_ref().is_some_and(|bot| {
                    bot.is_bot
                        && bot
                            .username
                            .as_deref()
                            .is_some_and(|username| username.eq_ignore_ascii_case(bot_username))
                })
            });
        if !valid {
            let language = if own_chat {
                let chat = message.unwrap().chat.id.0;
                self.detect_language(chat, query.from.language_code.as_deref());
                self.language(&chat.to_string())
            } else {
                Language::from_locale(query.from.language_code.as_deref().unwrap_or("en"))
            };
            self.answer_callback(
                &query.id.0,
                language.pick(
                    "This menu is unavailable. Open /language again.",
                    "Меню недоступно. Откройте /language ещё раз.",
                ),
            )
            .await?;
            return Ok(());
        }
        let message = message.unwrap();
        self.detect_language(message.chat.id.0, query.from.language_code.as_deref());
        self.answer_callback(&query.id.0, "").await?;
        let text = self
            .select_language(message.chat.id.0, selected.unwrap())
            .await;
        self.edit_buttons(
            &message.chat.id.to_string(),
            &MessageId(message.id.0.to_string()),
            text,
            &[],
        )
        .await?;
        Ok(())
    }

    async fn select_language(&self, chat: i64, selected: Language) -> &'static str {
        match self.save_language(chat, selected).await {
            Ok(()) => selected.pick("Language set to English.", "Выбран русский язык."),
            Err(error) => {
                log::warn!("Could not save Telegram language preference: {error:#}");
                self.language(&chat.to_string()).pick(
                    "Could not save the language. Try again later.",
                    "Не удалось сохранить язык. Попробуйте позже.",
                )
            }
        }
    }
}

pub(super) fn commands(language: Language) -> Value {
    json!([
        {"command":"new","description":language.pick("Start a new session", "Начать новый диалог")},
        {"command":"stop","description":language.pick("Stop the agent", "Остановить агента")},
        {"command":"sessions","description":language.pick("Choose a session", "Выбрать диалог")},
        {"command":"chatid","description":language.pick("Show this chat's ID", "Показать ID чата")},
        {"command":"language","description":language.pick("Choose language: en or ru", "Выбрать язык: en или ru")}
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn picker_server(
        root: &std::path::Path,
    ) -> (
        TelegramInterface,
        mpsc::UnboundedReceiver<(String, Value)>,
        tokio::task::JoinHandle<()>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let app = axum::Router::new().fallback(axum::routing::post(move |uri: axum::http::Uri, axum::Json(body): axum::Json<Value>| {
            let callback = uri.path().ends_with("/answerCallbackQuery");
            tx.send((uri.path().to_owned(), body)).unwrap();
            async move { axum::Json(json!({"ok":true,"result":if callback { json!(true) } else { json!({"message_id":20}) }})) }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut interface =
            TelegramInterface::new("test", "123:test", "").with_space_root(root.into());
        interface.bot = interface.bot.set_api_url(
            format!("http://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (interface, rx, server)
    }

    #[tokio::test]
    async fn bare_language_command_buttons_select_and_persist_in_private_and_group_chats() {
        for (chat, sender, profile, selected) in
            [(42, 42, "en", Language::Ru), (-42, 7, "ru", Language::En)]
        {
            let directory = tempfile::tempdir().unwrap();
            let (interface, mut requests, server) = picker_server(directory.path()).await;
            interface.detect_language(chat, Some(profile));
            let (tx, mut events) = mpsc::channel(1);
            interface
                .process_updates(
                    futures::stream::iter([Ok(super::super::tests::text_update(
                        chat,
                        "/language",
                    ))]),
                    tx,
                    "TestBot",
                )
                .await
                .unwrap();
            assert!(events.recv().await.is_none());
            assert!(!interface.language_path(chat).unwrap().exists());
            let (method, menu) = requests.recv().await.unwrap();
            assert!(method.ends_with("/sendMessage"));
            assert_eq!(
                menu["text"],
                Language::from_locale(profile).pick("Choose a language:", "Выберите язык:")
            );
            let buttons = menu["reply_markup"]["inline_keyboard"][0]
                .as_array()
                .unwrap();
            assert_eq!(
                buttons
                    .iter()
                    .map(|button| button["text"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["English", "Русский"]
            );
            let data = buttons[usize::from(selected == Language::Ru)]["callback_data"]
                .as_str()
                .unwrap();
            let (tx, mut events) = mpsc::channel(1);
            interface
                .process_updates(
                    futures::stream::iter([Ok(super::super::tests::picker_callback(
                        chat, sender, data,
                    ))]),
                    tx,
                    "TestBot",
                )
                .await
                .unwrap();
            assert!(events.recv().await.is_none());
            let (method, acknowledgment) = requests.recv().await.unwrap();
            assert!(method.ends_with("/answerCallbackQuery"));
            assert_eq!(acknowledgment["text"], "");
            let (method, confirmation) = requests.recv().await.unwrap();
            assert!(method.ends_with("/editMessageText"));
            assert_eq!(confirmation["chat_id"], chat);
            assert_eq!(confirmation["message_id"], 20);
            assert_eq!(
                confirmation["text"],
                selected.pick("Language set to English.", "Выбран русский язык.")
            );
            assert_eq!(confirmation["reply_markup"]["inline_keyboard"], json!([]));
            let restarted = TelegramInterface::new("test", "123:test", "")
                .with_space_root(directory.path().into());
            assert_eq!(restarted.language(&chat.to_string()), selected);
            server.abort();
        }
    }

    #[tokio::test]
    async fn language_callbacks_reject_invalid_foreign_and_inaccessible_menus_without_model_work() {
        let directory = tempfile::tempdir().unwrap();
        let (interface, mut requests, server) = picker_server(directory.path()).await;
        let callback = super::super::tests::picker_callback;
        let mut updates: Vec<_> = [
            "lang:42:fr",
            "lang:42:RU",
            "lang:42:ru:extra",
            "lang:7:ru",
            "lang:042:ru",
            "lang",
            "lang:42:ru ",
        ]
        .into_iter()
        .map(|data| callback(42, 42, data))
        .collect();
        updates.push(callback(42, 7, "lang:42:ru"));
        for invalid_bot in [false, true] {
            let mut update = callback(42, 42, "lang:42:ru");
            if let UpdateKind::CallbackQuery(query) = &mut update.kind {
                if invalid_bot {
                    query.from.is_bot = true;
                } else {
                    let teloxide::types::MaybeInaccessibleMessage::Regular(message) =
                        query.message.as_mut().unwrap()
                    else {
                        panic!()
                    };
                    message.from.as_mut().unwrap().username = Some("OtherBot".into());
                }
            }
            updates.push(update);
        }
        let mut inaccessible = callback(42, 42, "lang:42:ru");
        if let UpdateKind::CallbackQuery(query) = &mut inaccessible.kind {
            query.message = Some(teloxide::types::MaybeInaccessibleMessage::Inaccessible(
                teloxide::types::InaccessibleMessage {
                    chat: query.message.as_ref().unwrap().chat().clone(),
                    message_id: teloxide::types::MessageId(20),
                },
            ));
        }
        updates.push(inaccessible);
        let count = updates.len();
        let (tx, mut events) = mpsc::channel(1);
        interface
            .process_updates(
                futures::stream::iter(updates.into_iter().map(Ok)),
                tx,
                "TestBot",
            )
            .await
            .unwrap();
        assert!(events.recv().await.is_none());
        assert!(!interface.language_path(42).unwrap().exists());
        for _ in 0..count {
            let (method, body) = requests.try_recv().unwrap();
            assert!(method.ends_with("/answerCallbackQuery"));
            assert!(body["text"].as_str().unwrap().contains("/language"));
        }
        assert!(requests.try_recv().is_err());
        server.abort();
    }

    #[tokio::test]
    async fn language_buttons_do_not_bypass_admission() {
        let directory = tempfile::tempdir().unwrap();
        let (interface, mut requests, server) = picker_server(directory.path()).await;
        let interface = interface.with_access(directory.path().into());
        let (tx, mut events) = mpsc::channel(1);
        interface
            .process_updates(
                futures::stream::iter([
                    Ok(super::super::tests::picker_callback(42, 42, "lang:42:ru")),
                    Ok(super::super::tests::picker_callback(-42, 7, "lang:-42:ru")),
                    Ok(super::super::tests::text_update(42, "/language")),
                ]),
                tx,
                "TestBot",
            )
            .await
            .unwrap();
        assert!(events.recv().await.is_none());
        assert!(requests.try_recv().is_err());
        assert!(!interface.language_path(42).unwrap().exists());
        assert!(!interface.language_path(-42).unwrap().exists());
        server.abort();
    }

    #[tokio::test]
    async fn detected_language_is_per_chat_and_manual_choice_survives_restart() {
        let directory = tempfile::tempdir().unwrap();
        let interface = TelegramInterface::new("../../bot", "test", "")
            .with_space_root(directory.path().into());
        assert_eq!(interface.language("42"), Language::En);
        interface.detect_language(42, Some("ru-RU"));
        interface.detect_language(-7, Some("en-US"));
        assert_eq!(interface.language("42"), Language::Ru);
        assert_eq!(interface.language("-7"), Language::En);
        interface.save_language(42, Language::En).await.unwrap();
        interface.detect_language(42, Some("ru"));
        assert_eq!(interface.language("42"), Language::En);
        let restarted = TelegramInterface::new("../../bot", "test", "")
            .with_space_root(directory.path().into());
        restarted.detect_language(42, Some("ru"));
        assert_eq!(restarted.language("42"), Language::En);
        assert!(
            interface
                .language_path(42)
                .unwrap()
                .starts_with(directory.path().join("spaces/telegram"))
        );
        assert!(interface.language_path(0).is_err());
        assert_eq!(interface.language("../../outside"), Language::En);
        assert_eq!(commands(Language::En)[2]["description"], "Choose a session");
        assert_eq!(commands(Language::Ru)[2]["description"], "Выбрать диалог");
        for language in [Language::En, Language::Ru] {
            assert!(
                commands(language)
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|command| command["command"] != "login")
            );
        }
    }

    #[tokio::test]
    async fn message_callback_and_guest_detect_the_sender_locale() {
        let interface = TelegramInterface::new("test", "test", "");
        let mut update =
            serde_json::to_value(super::super::tests::text_update(42, "hello")).unwrap();
        update["message"]["from"] =
            json!({"id":42,"is_bot":false,"first_name":"Test","language_code":"ru"});
        interface
            .map_update(
                serde_json::from_str(&update.to_string()).unwrap(),
                "TestBot",
            )
            .await
            .unwrap();
        assert_eq!(interface.language("42"), Language::Ru);
        let mut callback = super::super::tests::picker_callback(42, 42, "cs:n");
        if let UpdateKind::CallbackQuery(query) = &mut callback.kind {
            query.from.language_code = Some("en".into());
        }
        interface.map_update(callback, "TestBot").await.unwrap();
        assert_eq!(interface.language("42"), Language::En);
        interface.map_guest(json!({"chat":{"id":-7,"type":"group"},"from":{"id":12,"language_code":"ru"},"guest_query_id":"query","text":"original user text"}), "TestBot").unwrap();
        assert_eq!(interface.language("-7"), Language::Ru);
        assert_eq!(interface.language("42"), Language::En);
    }

    #[tokio::test]
    async fn invalid_private_menu_uses_the_saved_language_instead_of_the_profile_locale() {
        let directory = tempfile::tempdir().unwrap();
        let (interface, request) =
            super::super::tests::server("200 OK", json!({"ok":true,"result":true})).await;
        let interface = interface.with_space_root(directory.path().into());
        interface.save_language(42, Language::En).await.unwrap();
        let mut callback = super::super::tests::picker_callback(42, 42, "invalid");
        if let UpdateKind::CallbackQuery(query) = &mut callback.kind {
            query.from.language_code = Some("ru".into());
        }
        assert!(
            interface
                .map_update(callback, "TestBot")
                .await
                .unwrap()
                .is_none()
        );
        let (_, body) = request.await.unwrap();
        assert_eq!(
            body["text"],
            "This menu is unavailable. Open /sessions in a private chat with the bot."
        );
    }

    #[tokio::test]
    async fn guest_language_command_uses_only_the_guest_slot_and_never_changes_preferences() {
        for command in ["/language", "/language en"] {
            let directory = tempfile::tempdir().unwrap();
            let (interface, request) = super::super::tests::server(
                "200 OK",
                json!({"ok":true,"result":{"inline_message_id":"guest-reply"}}),
            )
            .await;
            let interface = interface.with_space_root(directory.path().into());
            let guest = json!({"chat":{"id":42,"type":"private"},"from":{"id":42,"language_code":"ru"},"guest_query_id":"query","text":command});
            let mut malformed = guest.clone();
            malformed["guest_query_id"] = Value::Null;
            let (tx, mut rx) = mpsc::channel(1);
            interface
                .process_updates(
                    futures::stream::iter([
                        Ok(IncomingUpdate::Guest(guest)),
                        Ok(IncomingUpdate::Guest(malformed)),
                    ]),
                    tx,
                    "TestBot",
                )
                .await
                .unwrap();
            assert!(rx.recv().await.is_none());
            assert!(!interface.language_path(42).unwrap().exists());
            assert_eq!(interface.language("42"), Language::Ru);
            let (path, body) = request.await.unwrap();
            assert!(path.contains("answerGuestQuery"));
            assert_eq!(body["guest_query_id"], "query");
            assert!(
                body["result"]["input_message_content"]["message_text"]
                    .as_str()
                    .unwrap()
                    .contains("в обычном чате")
            );
        }
    }

    #[tokio::test]
    async fn language_command_is_local_and_respects_admission_and_bot_target() {
        for (argument, expected, language) in [
            ("ru", "Выбран русский язык.", Language::Ru),
            ("en", "Language set to English.", Language::En),
            ("fr", "Choose a language:", Language::En),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let (interface, request) =
                super::super::tests::server("200 OK", json!({"ok":true,"result":{"message_id":7}}))
                    .await;
            let interface = interface.with_space_root(directory.path().into());
            let (tx, mut rx) = mpsc::channel(4);
            interface
                .process_updates(
                    futures::stream::iter([
                        Ok(super::super::tests::text_update(
                            42,
                            "/language@OtherBot ru",
                        )),
                        Ok(super::super::tests::text_update(
                            42,
                            &format!("/language@TestBot {argument}"),
                        )),
                        Ok(super::super::tests::text_update(42, "hello")),
                    ]),
                    tx,
                    "TestBot",
                )
                .await
                .unwrap();
            let Event::Message(message) = rx.recv().await.unwrap();
            assert_eq!(message.text, "hello");
            assert!(rx.recv().await.is_none());
            let (_, body) = request.await.unwrap();
            assert!(body["text"].as_str().unwrap().starts_with(expected));
            assert_eq!(interface.language("42"), language);
        }
        let directory = tempfile::tempdir().unwrap();
        let interface = TelegramInterface::new("test", "test", "")
            .with_space_root(directory.path().into())
            .with_access(directory.path().into());
        let (tx, mut rx) = mpsc::channel(1);
        interface
            .process_updates(
                futures::stream::iter([Ok(super::super::tests::text_update(42, "/language ru"))]),
                tx,
                "TestBot",
            )
            .await
            .unwrap();
        assert!(rx.recv().await.is_none());
        assert!(!interface.language_path(42).unwrap().exists());
    }
}
