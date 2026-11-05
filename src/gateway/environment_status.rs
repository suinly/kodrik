use super::*;
use crate::{agent::session::SessionStore, remote::protocol::EnvironmentKey};

#[derive(Default)]
pub(super) struct ShownEnvironmentStatus(VecDeque<(SessionId, Option<EnvironmentKey>, String)>);

fn display_label(label: &str) -> String {
    let mut text = String::new();
    let mut units = 0;
    for character in label.chars() {
        let escaped = character.escape_debug().to_string();
        units += escaped.encode_utf16().count();
        if units > 240 {
            text.push('…');
            break;
        }
        text.push_str(&escaped);
    }
    text
}

impl GatewayRunner {
    pub(super) async fn show_environment_status(
        &self,
        shown: &mut ShownEnvironmentStatus,
    ) -> anyhow::Result<()> {
        // No owner configuration means no recipient, even if an interface allows other users.
        let Some(spaces) = &self.telegram_spaces else {
            return Ok(());
        };
        let Some(owner) = &spaces.config.owner else {
            return Ok(());
        };
        let interface_id = format!("telegram:{}", owner.interface);
        let chat = owner.user_id.to_string();
        let conversation = SessionId::new(format!("{interface_id}:{chat}"));
        let Some(interface) = self.interfaces.iter().find(|interface| {
            interface.id().0 == interface_id && interface.allows_recipient(&chat)
        }) else {
            return Ok(());
        };
        if !self.allows_message(&IncomingMessage::new(interface.id(), &chat, ""))? {
            return Ok(());
        }
        let Ok(runner) = self.runner_for(&conversation) else {
            return Ok(());
        };
        let Some(service) = runner.environments() else {
            return Ok(());
        };
        let Some(store) = runner.sqlite_store() else {
            return Ok(());
        };
        let session_id = store.active_id(&conversation).await?;
        let Some(session) = store.load(&session_id).await? else {
            return Ok(());
        };
        if !crate::web::chat::personal_session(&session)
            || session.webhook_instruction.is_some()
            || session_id
                .as_str()
                .starts_with(crate::agent::subagents::PREFIX)
        {
            return Ok(());
        }
        let Some(status) = service.status(&session_id).await else {
            return Ok(());
        };
        if status.session_id != session_id
            || !matches!(status.state.as_str(), "connected" | "disconnected")
        {
            return Ok(());
        }
        let key = (session_id.clone(), status.environment, status.state.clone());
        if shown.0.contains(&key) {
            return Ok(());
        }
        let language = interface.language(&chat);
        let text = format!(
            "{}: {}\n{}: {}\n{}: {}\n{}\n{}: {}",
            language.pick("Environment", "Окружение"),
            display_label(&status.label),
            language.pick("Session", "Сеанс"),
            display_label(session_id.as_str()),
            language.pick("Project", "Проект"),
            display_label(&status.project_root),
            if status.state == "connected" {
                language.pick(
                    "Connected. Temporary device access is available only in this session.",
                    "Подключено. Временный доступ к устройству действует только в этом сеансе.",
                )
            } else {
                language.pick("Disconnected. Device tools and local files are unavailable; uncertain actions are not replayed.", "Отключено: инструменты устройства и локальные файлы недоступны; действия с неизвестным результатом не повторяются.")
            },
            language.pick("Reason", "Причина"),
            display_label(status.reason.as_deref().unwrap_or("—")),
        );
        // Mark before sending: an unknown delivery outcome must not cause repeated notifications.
        shown.0.push_back(key);
        while shown.0.len() > 128 {
            shown.0.pop_front();
        }
        // Empty literal menus avoid interpreting untrusted labels as Markdown/HTML.
        match tokio::time::timeout(
            Duration::from_secs(5),
            interface.send_buttons(&chat, &text, &[]),
        )
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => warn!("Could not send environment status: {error:#}"),
            Err(_) => warn!("Sending environment status timed out"),
        }
        Ok(())
    }
}
