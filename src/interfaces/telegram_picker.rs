use super::*;
use crate::{gateway::SessionCallback, interfaces::InlineButton};

impl TelegramInterface {
    pub(super) async fn map_callback(
        &self,
        query: teloxide::types::CallbackQuery,
        bot_username: &str,
    ) -> anyhow::Result<Option<Event>> {
        if query
            .data
            .as_deref()
            .is_some_and(|data| data == "lang" || data.starts_with("lang:"))
        {
            self.language_callback(query, bot_username).await?;
            return Ok(None);
        }
        let message = query.regular_message();
        let chat = query.message.as_ref().map(|message| message.chat());
        let own_private = chat.is_some_and(|chat| {
            chat.is_private()
                && chat.id.0 > 0
                && chat.id.0 as u64 == query.from.id.0
                && !query.from.is_bot
        });
        let language = if own_private {
            let chat = chat.unwrap().id.0;
            self.detect_language(chat, query.from.language_code.as_deref());
            self.language(&chat.to_string())
        } else {
            Language::from_locale(query.from.language_code.as_deref().unwrap_or("en"))
        };
        let valid = own_private
            && message.is_some_and(|message| {
                message.from.as_ref().is_some_and(|bot| {
                    bot.is_bot
                        && bot
                            .username
                            .as_deref()
                            .is_some_and(|name| name.eq_ignore_ascii_case(bot_username))
                })
            });
        let data = query.data.as_deref().unwrap_or_default();
        if !valid || !(data.starts_with("cs:") || data.starts_with("wa:")) || data.len() > 64 {
            self.answer_callback(
                &query.id.0,
                language.pick(
                    "This menu is unavailable. Open /sessions in a private chat with the bot.",
                    "Меню недоступно. Откройте /sessions в личном чате с ботом.",
                ),
            )
            .await?;
            return Ok(None);
        }
        let message = message.unwrap();
        let mut incoming = IncomingMessage::new(self.id(), message.chat.id.to_string(), "");
        incoming.request_id = format!("{}:{}:callback:{}", self.id(), message.chat.id, query.id.0);
        incoming.callback = Some(Box::new(SessionCallback {
            id: query.id.0.clone(),
            message_id: MessageId(message.id.0.to_string()),
            data: data.to_owned(),
        }));
        Ok(Some(Event::Message(incoming)))
    }

    pub(super) async fn send_picker(
        &self,
        conversation: &str,
        id: Option<&MessageId>,
        text: &str,
        buttons: &[Vec<InlineButton>],
    ) -> anyhow::Result<MessageId> {
        anyhow::ensure!(
            buttons
                .iter()
                .flatten()
                .all(|button| !button.data.is_empty() && button.data.len() <= 64),
            "Invalid callback data length"
        );
        let mut body = json!({"chat_id":conversation.parse::<i64>()?, "text":text,
            "reply_markup":{"inline_keyboard":buttons}});
        let method = if let Some(id) = id {
            body["message_id"] = json!(id.0.parse::<i32>()?);
            "editMessageText"
        } else {
            "sendMessage"
        };
        match self.rich_request(method, body).await {
            Err(error)
                if id.is_some()
                    && error
                        .downcast_ref::<super::super::SendRejection>()
                        .is_some_and(|rejection| {
                            rejection.error_code == 400
                                && rejection.description.contains("message is not modified")
                        }) =>
            {
                Ok(id.unwrap().clone())
            }
            result => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn session_picker_rejects_foreign_group_inaccessible_and_invalid_callbacks() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let requests = captured.clone();
        let router = axum::Router::new().fallback(axum::routing::post(
            move |axum::Json(body): axum::Json<Value>| {
                requests.lock().unwrap().push(body);
                async { axum::Json(json!({"ok":true,"result":true})) }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut interface = TelegramInterface::new("test", "123:test", "");
        interface.bot = interface.bot.set_api_url(
            format!("http://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let callback = super::super::tests::picker_callback;
        let mut updates = vec![
            callback(-42, 42, "cs:n"),
            callback(42, 7, "cs:n"),
            callback(42, 42, "invalid"),
            callback(42, 42, &"x".repeat(65)),
        ];
        let UpdateKind::CallbackQuery(mut query) = callback(42, 42, "cs:n").kind else {
            panic!()
        };
        query.message = None;
        updates.push(Update {
            id: teloxide::types::UpdateId(11),
            kind: UpdateKind::CallbackQuery(query),
        });
        for update in updates {
            assert!(
                interface
                    .map_update(update, "TestBot")
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        let mut query = match callback(42, 42, "cs:n").kind {
            UpdateKind::CallbackQuery(query) => query,
            _ => panic!(),
        };
        query.message = Some(teloxide::types::MaybeInaccessibleMessage::Inaccessible(
            teloxide::types::InaccessibleMessage {
                chat: query.message.as_ref().unwrap().chat().clone(),
                message_id: teloxide::types::MessageId(20),
            },
        ));
        assert!(
            interface
                .map_callback(query, "TestBot")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(captured.lock().unwrap().len(), 6);
        assert!(
            captured
                .lock()
                .unwrap()
                .iter()
                .all(|body| body["callback_query_id"] == "picker-query"
                    && body["text"].as_str().unwrap().contains("/sessions"))
        );
        server.abort();
    }

    #[tokio::test]
    async fn session_picker_sends_edits_and_acknowledges_literal_menus() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let requests = captured.clone();
        let router = axum::Router::new().fallback(axum::routing::post(move |uri: axum::http::Uri, axum::Json(body): axum::Json<Value>| {
            let method = uri.path().rsplit('/').next().unwrap().to_owned();
            requests.lock().unwrap().push((method.clone(),body.clone()));
            async move {
                if body["text"] == "unchanged" {
                    (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"ok":false,"error_code":400,"description":"Bad Request: message is not modified"})))
                } else {
                    (axum::http::StatusCode::OK, axum::Json(json!({"ok":true,"result": if method == "answerCallbackQuery" {json!(true)} else {json!({"message_id":20})}})))
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut interface = TelegramInterface::new("test", "123:test", "");
        interface.bot = interface.bot.set_api_url(
            format!("http://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let buttons = vec![vec![InlineButton {
            text: "✓ <Питание>".into(),
            data: "cs:p:1".into(),
        }]];
        let id = interface
            .send_buttons("42", "Диалоги <b>20.</b>", &buttons)
            .await
            .unwrap();
        interface
            .edit_buttons("42", &id, "Новая страница", &buttons)
            .await
            .unwrap();
        interface
            .edit_buttons("42", &id, "unchanged", &buttons)
            .await
            .unwrap();
        interface
            .edit_buttons("42", &id, "Выбран диалог: Питание", &[])
            .await
            .unwrap();
        interface.answer_callback("query", "").await.unwrap();
        let requests = captured.lock().unwrap();
        assert_eq!(
            requests.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            [
                "sendMessage",
                "editMessageText",
                "editMessageText",
                "editMessageText",
                "answerCallbackQuery"
            ]
        );
        assert_eq!(requests[0].1["chat_id"], 42);
        assert_eq!(requests[0].1["text"], "Диалоги <b>20.</b>");
        assert!(requests[0].1["parse_mode"].is_null());
        assert_eq!(
            requests[0].1["reply_markup"]["inline_keyboard"][0][0]["callback_data"],
            "cs:p:1"
        );
        assert_eq!(requests[1].1["message_id"], 20);
        assert_eq!(requests[3].1["reply_markup"]["inline_keyboard"], json!([]));
        assert_eq!(requests[4].1["callback_query_id"], "query");
        drop(requests);
        assert!(
            interface
                .send_buttons(
                    "42",
                    "invalid",
                    &[vec![InlineButton {
                        text: "button".into(),
                        data: "x".repeat(65)
                    }]]
                )
                .await
                .is_err()
        );
        assert_eq!(captured.lock().unwrap().len(), 5);
        server.abort();
    }

    #[tokio::test]
    async fn session_picker_callbacks_reach_the_gateway_alongside_messages() {
        let interface = TelegramInterface::new("test", "token", "");
        let (tx, mut rx) = mpsc::channel(4);
        interface
            .process_updates(
                futures::stream::iter(vec![Ok(super::super::tests::picker_callback(
                    42, 42, "cs:p:2",
                ))]),
                tx,
                "TestBot",
            )
            .await
            .unwrap();
        let Some(Event::Message(message)) = rx.recv().await else {
            panic!("callback was discarded by polling")
        };
        assert_eq!(message.callback.unwrap().data, "cs:p:2");
        assert!(message.text.is_empty());
        assert!(rx.recv().await.is_none());
    }
}
