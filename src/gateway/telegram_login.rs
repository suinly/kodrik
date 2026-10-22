use super::*;
use crate::interfaces::InlineButton;

impl GatewayRunner {
    pub(super) async fn handle_login_request(
        &self,
        message: &IncomingMessage,
    ) -> anyhow::Result<()> {
        let interface = self
            .interfaces
            .iter()
            .find(|i| i.id() == message.source.interface)
            .context("Telegram interface unavailable")?;
        let language = self.message_language(message);
        let private = message.images.is_empty()
            && message.files.is_empty()
            && !message.is_group
            && message.guest_query_id.is_none()
            && message
                .source
                .conversation_id
                .parse::<u64>()
                .is_ok_and(|id| id > 0);
        let callback = message.callback.as_ref();
        if let Some(callback) = callback {
            interface.answer_callback(&callback.id, "").await?;
        }
        if !private {
            if let Some(query) = &message.guest_query_id {
                interface
                    .answer_guest(
                        query,
                        language.pick("Login is available only in a private chat with the bot, without attachments.", "Вход доступен только в личном чате с ботом, без вложений."),
                    )
                    .await?;
            } else {
                interface
                    .send_buttons(
                        &message.source.conversation_id,
                        language.pick("Login is available only in a private chat with the bot, without attachments.", "Вход доступен только в личном чате с ботом, без вложений."),
                        &[],
                    )
                    .await?;
            }
            return Ok(());
        }
        let Some(spaces) = &self.telegram_spaces else {
            return Ok(());
        };
        let key = message
            .source
            .interface
            .0
            .strip_prefix("telegram:")
            .context("Not Telegram")?
            .to_owned();
        anyhow::ensure!(
            spaces.config.gateway.contains_key(&key),
            "Unknown Telegram interface"
        );
        let user = message.source.conversation_id.parse::<u64>()?;
        let auth = crate::web::auth::AuthStore::new(&spaces.root);
        let now = chrono::Utc::now().timestamp();
        if let Some(callback) = callback {
            let parts: Vec<_> = callback.data.split(':').collect();
            let accepted = match parts.as_slice() {
                ["wa", decision @ ("y" | "n"), id] => {
                    let id = id.to_string();
                    let approve = *decision == "y";
                    tokio::task::spawn_blocking(move || {
                        auth.decide_telegram_login(&id, &key, user, approve, now)
                    })
                    .await??
                }
                _ => false,
            };
            let text = if !accepted {
                language.pick("This request has expired or is already complete. Start login again in your browser.", "Запрос истёк или уже завершён. Начните вход заново в браузере.")
            } else if parts.get(1) == Some(&"y") {
                language.pick(
                    "Login confirmed. Return to your original Kodrik tab.",
                    "Вход подтверждён. Вернитесь в исходную вкладку Кодрика.",
                )
            } else {
                language.pick("Login declined.", "Вход отклонён.")
            };
            interface
                .edit_buttons(
                    &message.source.conversation_id,
                    &callback.message_id,
                    text,
                    &[],
                )
                .await?;
            return Ok(());
        }
        let words: Vec<_> = message.text.split_whitespace().collect();
        let code = match words.as_slice() {
            ["/start", payload] => {
                let id = payload
                    .strip_prefix("login_")
                    .unwrap_or_default()
                    .to_owned();
                tokio::task::spawn_blocking(move || {
                    auth.present_telegram_login(&id, &key, user, now)
                })
                .await??
            }
            _ => None,
        };
        let Some(code) = code else {
            interface
                .send_buttons(
                    &message.source.conversation_id,
                    language.pick("This login request is unavailable or expired. Start login again in your browser.", "Запрос входа недоступен или истёк. Начните вход заново в браузере."),
                    &[],
                )
                .await?;
            return Ok(());
        };
        let id = words[1].strip_prefix("login_").unwrap();
        let site = spaces.config.web.base_url.as_deref().unwrap_or("Kodrik");
        let text = match language {
            crate::i18n::Language::En => format!(
                "Confirm login to Kodrik?\n\nSite: {site}\nRequest code: {code}\n\nCompare this code with the original tab. Confirm only if you started this login yourself. The request expires in 5 minutes."
            ),
            crate::i18n::Language::Ru => format!(
                "Подтвердить вход в Кодрик?\n\nСайт: {site}\nКод запроса: {code}\n\nСверьте код с исходной вкладкой. Подтверждайте, только если вы начали вход сами. Запрос действует 5 минут."
            ),
        };
        let buttons = vec![vec![
            InlineButton {
                text: language.pick("Sign in", "Войти").into(),
                data: format!("wa:y:{id}"),
            },
            InlineButton {
                text: language.pick("Decline", "Отказать").into(),
                data: format!("wa:n:{id}"),
            },
        ]];
        interface
            .send_buttons(&message.source.conversation_id, &text, &buttons)
            .await?;
        Ok(())
    }
}
