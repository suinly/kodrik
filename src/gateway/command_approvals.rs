use super::*;
use crate::{
    agent::{
        command_audit::{CommandApproval, CommandAuditMode},
        session::SessionStore,
    },
    interfaces::InlineButton,
};
use std::collections::HashSet;

fn approval_view(
    approval: &CommandApproval,
    session: &str,
    language: Language,
) -> (String, Vec<Vec<InlineButton>>) {
    // Literal menus and debug quoting keep command control characters visible.
    let text = format!(
        "{}\n{}: {:?}\n{}: {:?}\n{}: {}",
        language.pick("Tool approval required", "Требуется подтверждение действия"),
        language.pick("Tool", "Инструмент"),
        approval.tool_name,
        language.pick("Arguments", "Аргументы"),
        approval.arguments_text(),
        language.pick("Reason", "Причина"),
        approval.reason_text(language),
    );
    // Never offer approval for a command that Telegram cannot display completely.
    if text.encode_utf16().count() > 4000 || approval.id.len() > 59 {
        let text = format!("{}\n{}: {session}\n/approvals {session}", language.pick(
            "A tool call needs approval, but is too long to review here. Open this session in Web or TUI using the same local store and review the complete tool call before allowing it. TUI command:",
            "Вызов инструмента требует подтверждения, но слишком длинная для просмотра здесь. Откройте диалог в Web или TUI с тем же локальным хранилищем и просмотрите вызов целиком. Команда TUI:",
        ), language.pick("Session", "Сеанс"));
        return (
            if text.encode_utf16().count() <= 4000 {
                text
            } else {
                language.pick("A tool call needs approval but cannot be reviewed here. Open this session in Web.", "Вызов инструмента требует подтверждения, но её нельзя просмотреть здесь. Откройте диалог в Web.").into()
            },
            Vec::new(),
        );
    }
    (
        text,
        vec![
            vec![
                InlineButton {
                    text: language.pick("Allow once", "Разрешить один раз").into(),
                    data: format!("ca:a:{}", approval.id),
                },
                InlineButton {
                    text: language.pick("Reject", "Отклонить").into(),
                    data: format!("ca:r:{}", approval.id),
                },
            ],
            vec![InlineButton {
                text: language
                    .pick(
                        "No checks for this session",
                        "Без проверок для этого диалога",
                    )
                    .into(),
                data: format!("ca:s:{}", approval.id),
            }],
        ],
    )
}

impl GatewayRunner {
    pub(super) async fn audit_command_text(
        &self,
        message: &IncomingMessage,
        args: &[&str],
    ) -> anyhow::Result<String> {
        anyhow::ensure!(
            message.callback.is_none()
                && message.webhook.is_none()
                && !message.is_group
                && message.guest_query_id.is_none()
                && message
                    .source
                    .conversation_id
                    .parse::<u64>()
                    .is_ok_and(|id| id > 0)
                && self.allows_message(message)?,
            "Private authorized user command required"
        );
        let language = self.message_language(message);
        if !matches!(
            args,
            [] | ["trusted" | "review" | "ask" | "on" | "off" | "status"]
        ) {
            return Ok(language
                .pick(
                    "Use /audit trusted|review|ask|status.",
                    "Используйте /audit trusted|review|ask|status.",
                )
                .into());
        }
        let conversation = message.session_id();
        let runner = self.runner_for(&conversation)?;
        let store = runner
            .sqlite_store()
            .context("Command approval store unavailable")?;
        let mut session = store.active_id(&conversation).await?;
        if let Some(value) = args.first().filter(|value| **value != "status") {
            if store.load(&session).await?.is_none() {
                session = runner.new_session(&conversation).await?.id().clone();
            }
            let mode = match *value {
                "trusted" | "off" => CommandAuditMode::Trusted,
                "review" | "on" => CommandAuditMode::Review,
                "ask" => CommandAuditMode::Ask,
                _ => unreachable!(),
            };
            store.set_command_audit_mode(session.as_str(), mode).await?;
        }
        let mode = store.command_audit_mode(session.as_str()).await?;
        Ok(format!(
            "◈ {}: {}.{}\n/audit trusted — {}\n/audit review — {}\n/audit ask — {}",
            language.pick("Session command mode", "Режим инструментов диалога"),
            mode.label(language),
            if mode == CommandAuditMode::Trusted {
                language.pick(
                    " Pending tool calls may proceed.",
                    " Ожидающие действия могут выполняться.",
                )
            } else {
                ""
            },
            CommandAuditMode::Trusted.label(language),
            CommandAuditMode::Review.label(language),
            CommandAuditMode::Ask.label(language)
        ))
    }

    pub(super) async fn show_command_approvals(
        &self,
        shown: &mut HashSet<(String, String)>,
    ) -> anyhow::Result<()> {
        let jobs: Vec<_> = if let Some(runners) = &self.chat_runners {
            runners
                .iter()
                .map(|(id, runner)| (id.clone(), runner.clone()))
                .collect()
        } else {
            self.agent_runner
                .unfinished_conversations()
                .await?
                .into_iter()
                .map(|id| (id, self.agent_runner.clone()))
                .collect()
        };
        let mut pending = HashSet::new();
        let mut remaining = 5;
        for (conversation, runner) in jobs {
            let Some((interface_id, chat)) = conversation.as_str().rsplit_once(':') else {
                continue;
            };
            if !interface_id.starts_with("telegram:") || !chat.parse::<u64>().is_ok_and(|id| id > 0)
            {
                continue;
            }
            let Some(interface) = self.interfaces.iter().find(|interface| {
                interface.id().0 == interface_id && interface.allows_recipient(chat)
            }) else {
                continue;
            };
            let message = IncomingMessage::new(interface.id(), chat, "");
            if !self.allows_message(&message)? {
                continue;
            }
            let Some(store) = runner.sqlite_store() else {
                continue;
            };
            let session = store.active_id(&conversation).await?;
            for approval in store.pending_command_approvals(session.as_str()).await? {
                let key = (conversation.as_str().to_owned(), approval.id.clone());
                pending.insert(key.clone());
                if shown.contains(&key) || remaining == 0 {
                    continue;
                }
                remaining -= 1;
                let (text, buttons) =
                    approval_view(&approval, session.as_str(), interface.language(chat));
                match tokio::time::timeout(
                    Duration::from_secs(5),
                    interface.send_buttons(chat, &text, &buttons),
                )
                .await
                {
                    Ok(Ok(_)) => {
                        shown.insert(key);
                    }
                    Ok(Err(error)) => warn!("Could not send command approval: {error:#}"),
                    Err(_) => warn!("Sending command approval timed out"),
                }
            }
        }
        shown.retain(|key| pending.contains(key));
        Ok(())
    }

    pub(super) async fn handle_command_approval(
        &self,
        message: &IncomingMessage,
    ) -> anyhow::Result<()> {
        let callback = message
            .callback
            .as_ref()
            .context("Missing command approval callback")?;
        let interface = self
            .interfaces
            .iter()
            .find(|interface| interface.id() == message.source.interface)
            .context("Telegram interface unavailable")?;
        let language = self.message_language(message);
        let result = self.decide_command_approval(message).await;
        let notice = match &result {
            Ok(notice) => *notice,
            Err(_) => language.pick("Approval unavailable or already decided. Review pending tool calls in Web or TUI.", "Подтверждение недоступно или уже обработано. Проверьте ожидающие действия в Web или TUI."),
        };
        // A failed acknowledgement must never undo or repeat a durable decision.
        interface.answer_callback(&callback.id, notice).await?;
        if result.is_ok() {
            interface
                .edit_buttons(
                    &message.source.conversation_id,
                    &callback.message_id,
                    notice,
                    &[],
                )
                .await?;
        }
        result.map(|_| ())
    }

    async fn decide_command_approval(
        &self,
        message: &IncomingMessage,
    ) -> anyhow::Result<&'static str> {
        anyhow::ensure!(
            message.source.interface.0.starts_with("telegram:")
                && !message.is_group
                && message.guest_query_id.is_none()
                && message
                    .source
                    .conversation_id
                    .parse::<u64>()
                    .is_ok_and(|id| id > 0)
                && self.allows_message(message)?,
            "Private authorized chat required"
        );
        let callback = message
            .callback
            .as_ref()
            .context("Missing approval callback")?;
        let parts: Vec<_> = callback.data.split(':').collect();
        let ["ca", action @ ("a" | "r" | "s"), id] = parts.as_slice() else {
            anyhow::bail!("Invalid command approval action")
        };
        let conversation = message.session_id();
        let runner = self.runner_for(&conversation)?;
        let store = runner
            .sqlite_store()
            .context("Command approval store unavailable")?;
        let session = store.active_id(&conversation).await?;
        let pending = store.pending_command_approvals(session.as_str()).await?;
        let approval = pending
            .iter()
            .find(|approval| approval.id == *id)
            .context("Command approval unavailable")?;
        anyhow::ensure!(
            !approval_view(approval, session.as_str(), self.message_language(message))
                .1
                .is_empty(),
            "Complete review requires Web or TUI"
        );
        let language = self.message_language(message);
        if *action == "s" {
            store
                .disable_command_audit_for_approval(session.as_str(), id)
                .await?;
            return Ok(language.pick("No checks for this session. Pending tool calls may proceed. /audit review enables AI review; /audit ask always asks.", "Без проверок для этого диалога. Ожидающие действия могут выполняться. /audit review включает проверку ИИ; /audit ask спрашивает всегда."));
        }
        let approve = *action == "a";
        store
            .decide_command_approval(session.as_str(), id, approve)
            .await?;
        Ok(if approve {
            language.pick("Tool call allowed once.", "Действие разрешено один раз.")
        } else {
            language.pick("Tool call rejected.", "Действие отклонено.")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_approval_menus_show_all_arguments_or_have_no_actions() {
        let mut approval = CommandApproval {
            id: "request".into(),
            run_id: "run".into(),
            call_id: "call".into(),
            tool_name: "exec_command".into(),
            arguments_json: None,
            cmd: "rm -rf /tmp/test\necho `done`".into(),
            workdir: Some("/tmp/a b".into()),
            reason: "Destructive action".into(),
            reason_code: "destructive".into(),
        };
        let (text, buttons) = approval_view(&approval, "session", Language::En);
        assert!(text.contains("rm -rf /tmp/test\\\\necho `done`"));
        assert!(text.contains("/tmp/a b"));
        assert!(text.contains("May delete data"));
        assert_eq!(buttons[0][0].data, "ca:a:request");
        assert_eq!(buttons[0][1].data, "ca:r:request");
        assert_eq!(buttons[1][0].data, "ca:s:request");
        let (russian, _) = approval_view(&approval, "session", Language::Ru);
        assert!(russian.contains("Возможны удаление данных"));
        assert!(!russian.contains("Destructive action") && !russian.contains("May delete data"));
        approval.cmd = "🙂".repeat(2200);
        let (text, buttons) = approval_view(&approval, "session", Language::En);
        assert!(buttons.is_empty());
        assert!(text.contains("Web or TUI"));
        assert!(text.contains("/approvals session"));
    }
}
