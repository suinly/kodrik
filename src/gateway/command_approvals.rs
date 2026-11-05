use super::*;
use crate::{
    agent::{
        command_audit::{CommandApproval, CommandAuditMode},
        session::{Session, SessionStore, SqliteSessionStore},
    },
    interfaces::InlineButton,
};
use std::collections::HashSet;

fn approval_view(
    approval: &CommandApproval,
    session: &str,
    language: Language,
    context: &str,
) -> (String, Vec<Vec<InlineButton>>) {
    // Literal menus and debug quoting keep command control characters visible.
    let text = format!(
        "{}\n{context}\n{}: {:?}\n{}: {:?}\n{}: {}",
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
        let text = format!("{}\n{}\n{}: {session}\n/approvals {session}", language.pick(
            "A tool call needs approval, but is too long to review here. Open this session in Web or TUI using the same local store and review the complete tool call before allowing it. TUI command:",
            "Вызов инструмента требует подтверждения, но слишком длинная для просмотра здесь. Откройте диалог в Web или TUI с тем же локальным хранилищем и просмотрите вызов целиком. Команда TUI:",
        ), context.chars().take(1500).collect::<String>(), language.pick("Session", "Сеанс"));
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

async fn approval_sessions(store: &SqliteSessionStore) -> anyhow::Result<Vec<Session>> {
    let ids: Vec<String> = store.with_connection(|db| {
        let mut query = db.prepare("SELECT DISTINCT a.session_id FROM command_audits a JOIN runs r ON r.id=a.run_id WHERE r.terminal=0 ORDER BY a.session_id")?;
        Ok(query.query_map([], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?)
    }).await?;
    let mut sessions = Vec::new();
    for id in ids {
        if let Some(session) = store.load(&SessionId::new(id)).await?
            && super::session_commands::personal_session(&session)
        {
            sessions.push(session);
        }
    }
    Ok(sessions)
}

impl GatewayRunner {
    async fn approval_recipient_allowed(
        &self,
        store: &SqliteSessionStore,
        conversation: &SessionId,
        session: &Session,
        approval: &CommandApproval,
    ) -> anyhow::Result<bool> {
        if self.chat_runners.is_some() || session.id() == conversation {
            return Ok(true);
        }
        let Some((run, _)) = store.load_run(&approval.run_id).await? else {
            return Ok(false);
        };
        if run.conversation_id.starts_with("telegram:") {
            return Ok(run.conversation_id == conversation.as_str());
        }
        if session.id().as_str().starts_with("telegram:") {
            return Ok(false);
        }
        Ok(self.approval_recipient.as_ref() == Some(conversation))
    }

    async fn approval_context(
        &self,
        store: &SqliteSessionStore,
        session: &Session,
        approval: &CommandApproval,
        language: Language,
    ) -> anyhow::Result<String> {
        let mut context = format!(
            "{}: {:?}\n{}: {:?}",
            language.pick("Session", "Сеанс"),
            session.id().as_str(),
            language.pick("Run", "Запуск"),
            approval.run_id
        );
        if let Some(recorded) = session
            .webhook_instruction
            .as_deref()
            .and_then(|text| text.strip_prefix("## Recorded delivery source\n"))
            .and_then(|text| text.lines().next())
            .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        {
            context.push_str(&format!(
                "\nWebhook: {:?} ({:?})",
                recorded["webhook_name"].as_str().unwrap_or_default(),
                recorded["webhook_id"].as_str().unwrap_or_default()
            ));
        }
        if let Some((run, _)) = store.load_run(&approval.run_id).await? {
            if run.conversation_id.starts_with("companion:wakeup:") {
                context.push_str(&format!(
                    "\n{}: {:?}",
                    language.pick("Wakeup", "Пробуждение"),
                    run.conversation_id
                ));
            }
            for id in &run.submission_ids {
                if let Some(submission) = store.submission_by_id(id).await? {
                    context.push_str(&format!(
                        "\n{}: {:?}",
                        language.pick("Request", "Запрос"),
                        submission.request.request_id
                    ));
                }
            }
        }
        if let Some(base) = &self.approval_web_base {
            let mut url = base.join("app/chat/")?;
            url.path_segments_mut()
                .map_err(|_| anyhow::anyhow!("Invalid Web base URL"))?
                .pop_if_empty()
                .push(session.id().as_str());
            context.push_str(&format!("\n{}: {url}", language.pick("Review", "Просмотр")));
        }
        Ok(context)
    }

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
        let mut jobs: Vec<_> = if let Some(runners) = &self.chat_runners {
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
        if let Some(recipient) = &self.approval_recipient {
            jobs.push((recipient.clone(), self.runner_for(recipient)?.clone()));
        }
        jobs.sort_by_key(|(id, _)| {
            (
                Some(id) != self.approval_recipient.as_ref(),
                id.as_str().to_owned(),
            )
        });
        let mut pending = HashSet::new();
        let mut scanned = HashSet::new();
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
            let store_key: String = store
                .with_connection(|db| {
                    Ok(db.query_row(
                        "SELECT file FROM pragma_database_list WHERE name='main'",
                        [],
                        |row| row.get(0),
                    )?)
                })
                .await?;
            if self.chat_runners.is_some() && !scanned.insert(store_key.clone()) {
                continue;
            }
            for session in approval_sessions(&store).await? {
                for approval in store
                    .pending_command_approvals(session.id().as_str())
                    .await?
                {
                    if !self
                        .approval_recipient_allowed(&store, &conversation, &session, &approval)
                        .await?
                    {
                        continue;
                    }
                    let key = (store_key.clone(), approval.id.clone());
                    pending.insert(key.clone());
                    if shown.contains(&key) || remaining == 0 {
                        continue;
                    }
                    remaining -= 1;
                    let language = interface.language(chat);
                    let context = self
                        .approval_context(&store, &session, &approval, language)
                        .await?;
                    let (text, buttons) =
                        approval_view(&approval, session.id().as_str(), language, &context);
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
        anyhow::ensure!(
            self.interfaces
                .iter()
                .any(|interface| interface.id() == message.source.interface
                    && interface.allows_recipient(&message.source.conversation_id)),
            "Recipient unavailable"
        );
        let id_owned = (*id).to_owned();
        let session_id: Option<String> = store.with_connection(move |db| {
            use rusqlite::OptionalExtension;
            Ok(db.query_row("SELECT a.session_id FROM command_audits a JOIN runs r ON r.id=a.run_id WHERE a.id=?1 AND r.terminal=0", [id_owned], |row| row.get(0)).optional()?)
        }).await?;
        let session = store
            .load(&SessionId::new(
                session_id.context("Command approval unavailable")?,
            ))
            .await?
            .context("Session unavailable")?;
        anyhow::ensure!(
            super::session_commands::personal_session(&session),
            "Personal session required"
        );
        let pending = store
            .pending_command_approvals(session.id().as_str())
            .await?;
        let approval = pending
            .iter()
            .find(|approval| approval.id == *id)
            .context("Command approval unavailable")?;
        anyhow::ensure!(
            self.approval_recipient_allowed(&store, &conversation, &session, approval)
                .await?,
            "Private store binding required"
        );
        anyhow::ensure!(
            !approval_view(
                approval,
                session.id().as_str(),
                self.message_language(message),
                &self
                    .approval_context(&store, &session, approval, self.message_language(message))
                    .await?
            )
            .1
            .is_empty(),
            "Complete review requires Web or TUI"
        );
        let language = self.message_language(message);
        if *action == "s" {
            store
                .disable_command_audit_for_approval(session.id().as_str(), id)
                .await?;
            return Ok(language.pick("No checks for this session. Pending tool calls may proceed. /audit review enables AI review; /audit ask always asks.", "Без проверок для этого диалога. Ожидающие действия могут выполняться. /audit review включает проверку ИИ; /audit ask спрашивает всегда."));
        }
        let approve = *action == "a";
        store
            .decide_command_approval(session.id().as_str(), id, approve)
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
        let (text, buttons) = approval_view(&approval, "session", Language::En, "");
        assert!(text.contains("rm -rf /tmp/test\\\\necho `done`"));
        assert!(text.contains("/tmp/a b"));
        assert!(text.contains("May delete data"));
        assert_eq!(buttons[0][0].data, "ca:a:request");
        assert_eq!(buttons[0][1].data, "ca:r:request");
        assert_eq!(buttons[1][0].data, "ca:s:request");
        let (russian, _) = approval_view(&approval, "session", Language::Ru, "");
        assert!(russian.contains("Возможны удаление данных"));
        assert!(!russian.contains("Destructive action") && !russian.contains("May delete data"));
        approval.cmd = "🙂".repeat(2200);
        let (text, buttons) = approval_view(&approval, "session", Language::En, "");
        assert!(buttons.is_empty());
        assert!(text.contains("Web or TUI"));
        assert!(text.contains("/approvals session"));
    }
}
