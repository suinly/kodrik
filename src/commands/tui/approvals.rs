use super::app::{App, AppMessage, AppMessageRole};
use crate::agent::{
    AgentRunner,
    command_audit::{CommandApproval, CommandAuditMode},
    session::{SessionId, SessionStore},
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

pub(super) const AUDIT_MODES: [CommandAuditMode; 3] = [
    CommandAuditMode::Trusted,
    CommandAuditMode::Review,
    CommandAuditMode::Ask,
];

pub(super) struct AuditPicker {
    pub session: Option<SessionId>,
    pub choice: usize,
}

pub(super) fn update_audit_picker(app: &mut App, key: KeyEvent) -> Option<String> {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c' | 'C' | 'с' | 'С'))
    {
        app.should_quit = true;
        return None;
    }
    if !key.modifiers.is_empty() {
        return None;
    }
    let picker = app.audit_picker.as_mut()?;
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => picker.choice = picker.choice.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => {
            picker.choice = (picker.choice + 1).min(AUDIT_MODES.len() - 1)
        }
        KeyCode::Esc if key.kind == KeyEventKind::Press => app.audit_picker = None,
        KeyCode::Enter if key.kind == KeyEventKind::Press => {
            let picker = app.audit_picker.take()?;
            return Some(format!(
                "/audit {}{}",
                AUDIT_MODES[picker.choice].code(),
                picker
                    .session
                    .map_or_else(String::new, |id| format!(" {}", id.as_str()))
            ));
        }
        _ => {}
    }
    None
}

pub(super) fn is_command(input: &str) -> bool {
    matches!(
        input.split_whitespace().next(),
        Some("/approvals" | "/approve" | "/deny" | "/audit")
    )
}

fn literal_line(text: &str) -> String {
    let mut visible = String::new();
    for ch in text.chars() {
        if matches!(ch, '\'' | '"' | '\\') {
            visible.push(ch);
        } else {
            visible.extend(ch.escape_debug());
        }
    }
    visible
}

fn literal_block(text: &str) -> String {
    text.split('\n')
        .map(|line| format!("  {}", literal_line(line)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn show(app: &mut App, session: &SessionId, approval: &CommandApproval) {
    let language = app.language;
    let arguments = approval.arguments_text();
    let invocation = if approval.tool_name == "exec_command"
        && let Ok(args) =
            serde_json::from_str::<crate::agent::tools::exec_command::ExecCommandArgs>(&arguments)
    {
        let mut text = format!(
            "{}:\n{}\n\n{}: {}",
            language.pick("Command", "Команда"),
            literal_block(&args.cmd),
            language.pick("Working directory", "Рабочий каталог"),
            literal_line(
                args.workdir
                    .as_deref()
                    .unwrap_or(language.pick("current directory", "текущий каталог"))
            ),
        );
        if let Some(timeout) = args.timeout_ms {
            text.push_str(&format!(
                "\n{}: {timeout} ms",
                language.pick("Timeout", "Тайм-аут")
            ));
        }
        text
    } else {
        format!(
            "{}:\n{}",
            language.pick("Arguments", "Аргументы"),
            literal_block(&arguments)
        )
    };
    app.messages.push(AppMessage {
        role: AppMessageRole::Approval,
        content: format!(
            "{} · {}\n\n{}\n\n{}: {}\n{}: {}\nID: {}",
            language.pick("Tool approval required", "Требуется подтверждение действия"),
            literal_line(&approval.tool_name),
            invocation,
            language.pick("Reason", "Причина"),
            literal_line(&approval.reason_text(language)),
            language.pick("Session", "Сеанс"),
            literal_line(session.as_str()),
            literal_line(&approval.id),
        ),
    });
    app.shown_approvals
        .insert(approval.id.clone(), session.as_str().into());
}

pub(super) fn update(app: &mut App, key: KeyEvent) -> Option<String> {
    let approval = app.approval.as_ref()?;
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c' | 'C' | 'с' | 'С'))
    {
        app.should_quit = true;
    } else if key.modifiers.is_empty() {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                app.approval_choice = Some(
                    app.approval_choice
                        .map_or(0, |choice| choice.saturating_sub(1)),
                )
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.approval_choice =
                    Some(app.approval_choice.map_or(1, |choice| (choice + 1).min(2)))
            }
            KeyCode::Enter | KeyCode::Esc if key.kind == KeyEventKind::Press => {
                let choice = app.approval_choice.take();
                return Some(match if key.code == KeyCode::Esc { 1 } else { choice? } {
                    0 => format!("/approve {}", approval.id),
                    1 => format!("/deny {}", approval.id),
                    2 => format!(
                        "/audit trusted {} {}",
                        app.shown_approvals.get(&approval.id)?,
                        approval.id
                    ),
                    _ => return None,
                });
            }
            KeyCode::PageUp => app.scroll = app.scroll.saturating_add(10),
            KeyCode::PageDown => app.scroll = app.scroll.saturating_sub(10),
            _ => {}
        }
    }
    None
}

pub(super) async fn refresh(
    app: &mut App,
    runner: &AgentRunner,
    conversation: &SessionId,
) -> anyhow::Result<bool> {
    let Some(store) = runner.sqlite_store() else {
        return Ok(false);
    };
    let active = store.active_id(conversation).await?;
    let mut sessions = vec![active.clone()];
    if let Some(session) = &app.approval_session
        && !sessions.contains(session)
    {
        sessions.push(session.clone());
    }
    let mut changed = false;
    let mut modes = std::collections::HashMap::new();
    let mut next = None;
    for session in sessions {
        let mode = store.command_audit_mode(session.as_str()).await?;
        modes.insert(session.as_str().to_owned(), mode);
        let pending = store.pending_command_approvals(session.as_str()).await?;
        app.shown_approvals.retain(|id, owner| {
            owner != session.as_str() || pending.iter().any(|approval| &approval.id == id)
        });
        if next.is_none()
            && let Some(approval) = pending.into_iter().next()
        {
            if !app.shown_approvals.contains_key(&approval.id)
                || app.approval.as_ref().map(|current| &current.id) != Some(&approval.id)
            {
                show(app, &session, &approval);
                changed = true;
            }
            next = Some(approval);
        }
    }
    let target = next
        .as_ref()
        .and_then(|approval| app.shown_approvals.get(&approval.id))
        .map(|id| SessionId::new(id.clone()))
        .unwrap_or(active);
    let mode = modes[target.as_str()];
    if app.audit_session.as_ref() != Some(&target) || app.audit_mode != mode {
        app.audit_session = Some(target);
        app.audit_mode = mode;
        changed = true;
    }
    if app.approval.as_ref().map(|a| &a.id) != next.as_ref().map(|a| &a.id) {
        app.approval_choice = next.as_ref().map(|_| 0);
        app.view.clear_selection();
        if next.is_some() {
            app.audit_picker = None;
            app.sessions = None;
            app.scroll = 0;
        }
        changed = true;
    }
    app.approval = next;
    Ok(changed)
}

pub(super) async fn handle_command(
    app: &mut App,
    runner: &AgentRunner,
    conversation: &SessionId,
    input: &str,
) -> bool {
    if !is_command(input) {
        return false;
    }
    let result = decide(app, runner, conversation, input).await;
    if let Err(error) = result {
        log::warn!("Command control failed: {error:#}");
        app.push_kodrik_message(if error.is::<crate::agent::command_audit::CommandApprovalUnavailable>() {
            app.language.pick("This tool call is no longer pending. Refresh with /approvals.", "Это действие больше не ожидает решения. Обновите список через /approvals.")
        } else {
            app.language.pick("Could not apply the command control. Check the session ID and use /audit trusted|review|ask|status [SESSION_ID] or /approvals.", "Не удалось применить управление командой. Проверьте ID сеанса и используйте /audit trusted|review|ask|status [SESSION_ID] или /approvals.")
        }.into());
        if let Err(error) = refresh(app, runner, conversation).await {
            log::warn!("Could not refresh command approvals: {error:#}");
        }
    }
    true
}

async fn decide(
    app: &mut App,
    runner: &AgentRunner,
    conversation: &SessionId,
    input: &str,
) -> anyhow::Result<()> {
    use anyhow::Context;
    let parts: Vec<_> = input.split_whitespace().collect();
    if parts.first() == Some(&"/audit") {
        return audit_command(app, runner, conversation, &parts[1..]).await;
    }
    if matches!(parts.as_slice(), ["/approvals"] | ["/approvals", _]) {
        app.approval_session = if let Some(id) = parts.get(1) {
            let store = runner
                .sqlite_store()
                .context("Command approval store unavailable")?;
            let session = store
                .load(&SessionId::new(*id))
                .await?
                .context("Session unavailable in this local store")?;
            anyhow::ensure!(
                !(session.telegram_group().is_some()
                    || id.starts_with("companion:")
                    || id.ends_with(":guest")
                    || id.starts_with("telegram:")
                        && id
                            .rsplit(':')
                            .next()
                            .is_some_and(|chat| chat.starts_with('-'))),
                "Personal session required"
            );
            Some(session.id().clone())
        } else {
            None
        };
        app.shown_approvals.clear();
        if !refresh(app, runner, conversation).await? {
            app.push_kodrik_message(
                app.language
                    .pick(
                        "No pending tool approvals.",
                        "Нет действий, ожидающих подтверждения.",
                    )
                    .into(),
            );
        }
        return Ok(());
    }
    let [command @ ("/approve" | "/deny"), id] = parts.as_slice() else {
        anyhow::bail!(
            "{}",
            app.language.pick(
                "Use /approvals [SESSION_ID], /approve ID or /deny ID.",
                "Используйте /approvals [SESSION_ID], /approve ID или /deny ID."
            )
        );
    };
    let session = app.shown_approvals.get(*id).context(app.language.pick(
        "Review the command with /approvals first.",
        "Сначала просмотрите действие через /approvals.",
    ))?;
    let store = runner
        .sqlite_store()
        .context("Command approval store unavailable")?;
    store
        .decide_command_approval(session, id, *command == "/approve")
        .await?;
    app.shown_approvals.remove(*id);
    app.push_kodrik_message(format!(
        "{}: {id}",
        if *command == "/approve" {
            app.language
                .pick("Tool call allowed once", "Действие разрешено один раз")
        } else {
            app.language
                .pick("Tool call rejected", "Действие отклонено")
        }
    ));
    refresh(app, runner, conversation).await?;
    Ok(())
}

async fn audit_command(
    app: &mut App,
    runner: &AgentRunner,
    conversation: &SessionId,
    args: &[&str],
) -> anyhow::Result<()> {
    use anyhow::Context;
    anyhow::ensure!(
        matches!(
            args,
            [] | ["trusted" | "review" | "ask" | "on" | "off" | "status"]
                | ["trusted" | "review" | "ask" | "on" | "off" | "status", _]
                | ["trusted" | "off", _, _]
        ),
        "{}",
        app.language.pick(
            "Use /audit trusted|review|ask|status [SESSION_ID].",
            "Используйте /audit trusted|review|ask|status [SESSION_ID]."
        )
    );
    let store = runner
        .sqlite_store()
        .context("Command approval store unavailable")?;
    let active = store.active_id(conversation).await?;
    if args.is_empty() {
        let mode = store.command_audit_mode(active.as_str()).await?;
        app.audit_picker = Some(AuditPicker {
            session: store.load(&active).await?.map(|_| active),
            choice: match mode {
                CommandAuditMode::Trusted => 0,
                CommandAuditMode::Review => 1,
                CommandAuditMode::Ask => 2,
            },
        });
        app.sessions = None;
        app.view.clear_selection();
        return Ok(());
    }
    let mut session = args
        .get(1)
        .map_or_else(|| active.clone(), |id| SessionId::new(*id));
    if store.load(&session).await?.is_none() {
        anyhow::ensure!(args.len() == 1, "Session unavailable in this local store");
        if args[0] != "status" {
            session = runner.new_session(conversation).await?.id().clone();
        }
    }
    if let ["trusted" | "off", _, approval_id] = args {
        anyhow::ensure!(
            app.shown_approvals.get(*approval_id).map(String::as_str) == Some(session.as_str())
                && app
                    .approval
                    .as_ref()
                    .is_some_and(|approval| approval.id == *approval_id),
            "Command approval is no longer displayed"
        );
        store
            .disable_command_audit_for_approval(session.as_str(), approval_id)
            .await?;
    } else if args[0] != "status" {
        let mode = match args[0] {
            "trusted" | "off" => CommandAuditMode::Trusted,
            "review" | "on" => CommandAuditMode::Review,
            "ask" => CommandAuditMode::Ask,
            _ => unreachable!(),
        };
        store.set_command_audit_mode(session.as_str(), mode).await?;
    }
    let mode = store.command_audit_mode(session.as_str()).await?;
    if args.len() >= 2 && session != active {
        app.approval_session = Some(session.clone());
    }
    app.push_kodrik_message(format!(
        "{}: {}. {}: {}.{}\n/audit trusted|review|ask {}",
        app.language.pick("Session", "Сеанс"),
        session.as_str(),
        app.language.pick("Tool mode", "Режим инструментов"),
        mode.label(app.language),
        if mode == CommandAuditMode::Trusted {
            app.language.pick(
                " Pending tool calls may proceed.",
                " Ожидающие действия могут выполняться.",
            )
        } else {
            ""
        },
        session.as_str()
    ));
    refresh(app, runner, conversation).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{
        Agent, AgentEvent, ContextBuilder, LlmContext,
        llm::LlmProvider,
        message::{Message, ToolCall},
        run_state::{Checkpoint, RunPhase},
        session::{Session, SqliteSessionStore},
        tools::{Tool, ToolRegistry},
    };
    use std::sync::Arc;

    struct NoModel;
    #[async_trait::async_trait]
    impl LlmProvider for NoModel {
        async fn generate(
            &self,
            _: &LlmContext<'_>,
            _: &[&dyn Tool],
            _: Option<&tokio::sync::mpsc::Sender<AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            panic!("approval controls must not call the model")
        }
    }

    #[test]
    fn approval_display_shows_readable_shell_and_escapes_terminal_controls() {
        let mut app = App::default();
        let mut approval = CommandApproval {
            id: "request".into(),
            run_id: "run".into(),
            call_id: "call".into(),
            tool_name: "exec_command".into(),
            arguments_json: Some(
                serde_json::json!({
                    "cmd": "printf 'hello\\n'\necho \"world\"\u{1b}[2J\u{202e}",
                    "workdir": "/workspace/a b",
                    "timeout_ms": 1500,
                })
                .to_string(),
            ),
            cmd: String::new(),
            workdir: None,
            reason: "Destructive".into(),
            reason_code: "destructive".into(),
        };
        show(&mut app, &SessionId::new("owner"), &approval);
        let content = &app.messages.last().unwrap().content;
        assert!(
            content.contains("Command:\n  printf 'hello\\n'\n  echo \"world\"\\u{1b}[2J\\u{202e}")
        );
        assert!(content.contains("Working directory: /workspace/a b"));
        assert!(content.contains("Timeout: 1500 ms"));
        assert!(!content.contains('\u{1b}'));
        assert!(!content.contains('\u{202e}'));
        approval.tool_name = "apply_patch".into();
        approval.arguments_json =
            Some("{\n  \"patch\": \"*** Begin Patch\\n*** End Patch\"\n}".into());
        show(&mut app, &SessionId::new("owner"), &approval);
        assert!(
            app.messages.last().unwrap().content.contains(
                "Arguments:\n  {\n    \"patch\": \"*** Begin Patch\\n*** End Patch\"\n  }"
            )
        );
    }

    #[test]
    fn approval_picker_can_disable_checks_for_the_displayed_owner() {
        let mut app = App {
            approval: Some(CommandApproval {
                id: "request".into(),
                run_id: "run".into(),
                call_id: "call".into(),
                tool_name: "exec_command".into(),
                arguments_json: None,
                cmd: "rm file".into(),
                workdir: Some("/workspace".into()),
                reason: "Destructive".into(),
                reason_code: "destructive".into(),
            }),
            shown_approvals: std::collections::HashMap::from([(
                "request".into(),
                "cli:once:owner".into(),
            )]),
            ..Default::default()
        };
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        assert_eq!(app.update(key(KeyCode::Enter)), None);
        app.update(key(KeyCode::Char('k')));
        assert_eq!(app.approval_choice, Some(0));
        app.update(key(KeyCode::Char('j')));
        assert_eq!(app.approval_choice, Some(1));
        app.update(key(KeyCode::Char('k')));
        assert_eq!(app.approval_choice, Some(0));
        for _ in 0..3 {
            app.update(key(KeyCode::Char('j')));
        }
        assert_eq!(app.approval_choice, Some(2));
        assert_eq!(
            app.update(key(KeyCode::Enter)).as_deref(),
            Some("/audit trusted cli:once:owner request")
        );
        assert_eq!(app.update(key(KeyCode::Enter)), None);
        assert_eq!(
            app.update(key(KeyCode::Esc)).as_deref(),
            Some("/deny request")
        );
    }

    #[tokio::test]
    async fn audit_menu_selects_modes_locally_and_preserves_the_draft_on_cancel() {
        use ratatui::{Terminal, backend::TestBackend};
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteSessionStore::open(directory.path().into()).unwrap());
        let runner = AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(NoModel),
                ToolRegistry::default(),
                ContextBuilder::new(""),
            )),
            store.clone(),
        );
        let conversation = SessionId::new("cli:main");
        let mut app = App::default();
        app.insert_input("draft");
        app.input_cursor = 2;
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        decide(&mut app, &runner, &conversation, "/audit")
            .await
            .unwrap();
        app.insert_input("paste");
        terminal
            .draw(|frame| super::super::ui::render(&mut app, frame))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("> AI review"));
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..24)
            .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let title_y = rows
            .iter()
            .position(|row| row.contains("Command review mode"))
            .unwrap();
        assert!(rows[title_y - 1].trim().is_empty());
        assert!(rows[title_y + 1].trim().is_empty());
        let (description_y, row) = rows
            .iter()
            .enumerate()
            .find(|(_, row)| row.contains("Ask before risky or unclear actions"))
            .unwrap();
        let description_x = row.find("Ask before risky or unclear actions").unwrap();
        let description = &buffer[(description_x as u16, description_y as u16)];
        assert_eq!(description.fg, ratatui::style::Color::Gray);
        assert!(description.modifier.contains(ratatui::style::Modifier::DIM));
        assert!(
            !description
                .modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        assert!(!terminal.backend().cursor_visible());
        assert!(app.messages.is_empty());
        assert!(store.load(&conversation).await.unwrap().is_none());
        assert_eq!(app.input, "draft");
        for (width, height) in [(1, 1), (8, 1), (8, 5), (40, 10)] {
            let mut small = Terminal::new(TestBackend::new(width, height)).unwrap();
            small
                .draw(|frame| super::super::ui::render(&mut app, frame))
                .unwrap();
        }
        app.update(key(KeyCode::Char('j')));
        let command = app.update(key(KeyCode::Enter)).unwrap();
        assert_eq!(command, "/audit ask");
        decide(&mut app, &runner, &conversation, &command)
            .await
            .unwrap();
        let active = store.active_id(&conversation).await.unwrap();
        assert_eq!(
            store.command_audit_mode(active.as_str()).await.unwrap(),
            CommandAuditMode::Ask
        );
        app.language = crate::i18n::Language::Ru;
        app.status = super::super::app::Status::Working;
        decide(&mut app, &runner, &conversation, "/audit")
            .await
            .unwrap();
        terminal
            .draw(|frame| super::super::ui::render(&mut app, frame))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("> Спрашивать всегда"));
        app.update(key(KeyCode::Char('k')));
        app.update(key(KeyCode::Up));
        app.update(key(KeyCode::Down));
        assert_eq!(app.update(key(KeyCode::Esc)), None);
        assert!(matches!(app.status, super::super::app::Status::Working));
        assert_eq!(
            store.command_audit_mode(active.as_str()).await.unwrap(),
            CommandAuditMode::Ask
        );
        assert_eq!(app.input, "draft");
        assert_eq!(app.input_cursor, 2);
        terminal
            .draw(|frame| super::super::ui::render(&mut app, frame))
            .unwrap();
        assert!(terminal.backend().cursor_visible());
        decide(&mut app, &runner, &conversation, "/audit")
            .await
            .unwrap();
        app.update(key(KeyCode::Char('k')));
        app.update(key(KeyCode::Char('k')));
        assert_eq!(
            app.update(KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                KeyEventKind::Repeat
            )),
            None
        );
        let command = app.update(key(KeyCode::Enter)).unwrap();
        assert_eq!(command, format!("/audit trusted {}", active.as_str()));
        decide(&mut app, &runner, &conversation, &command)
            .await
            .unwrap();
        assert_eq!(
            store.command_audit_mode(active.as_str()).await.unwrap(),
            CommandAuditMode::Trusted
        );
    }

    #[test]
    fn approval_picker_rejects_on_escape_and_ignores_repeated_enter() {
        let mut app = App {
            approval: Some(CommandApproval {
                id: "request".into(),
                run_id: "run".into(),
                call_id: "call".into(),
                tool_name: "exec_command".into(),
                arguments_json: None,
                cmd: "rm file".into(),
                workdir: Some("/workspace".into()),
                reason: "Destructive".into(),
                reason_code: "destructive".into(),
            }),
            ..Default::default()
        };
        app.update(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(
            app.update(KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                KeyEventKind::Repeat
            )),
            None
        );
        assert_eq!(
            app.update(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            None
        );
        assert_eq!(
            app.update(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                .as_deref(),
            Some("/deny request")
        );
        assert_eq!(app.approval_choice, None);
        assert_eq!(
            app.update(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
        app.update(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.scroll, 10);
        app.update(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }

    #[tokio::test]
    async fn tui_audit_controls_create_persist_and_reenable_the_selected_sessions_setting() {
        use ratatui::{Terminal, backend::TestBackend};
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteSessionStore::open(dir.path().into()).unwrap());
        let runner = AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(NoModel),
                ToolRegistry::default(),
                ContextBuilder::new(""),
            )),
            store.clone(),
        );
        let conversation = SessionId::new("cli:main");
        let mut app = App::default();
        app.insert_input("draft message");
        app.input_cursor = 5;
        refresh(&mut app, &runner, &conversation).await.unwrap();
        assert_eq!(app.audit_mode, CommandAuditMode::Review);
        decide(&mut app, &runner, &conversation, "/audit off")
            .await
            .unwrap();
        let active = store.active_id(&conversation).await.unwrap();
        assert!(store.load(&active).await.unwrap().is_some());
        assert!(!store.command_audit_enabled(active.as_str()).await.unwrap());
        assert_eq!(app.audit_mode, CommandAuditMode::Trusted);
        let reopened = SqliteSessionStore::open(dir.path().into()).unwrap();
        assert!(
            !reopened
                .command_audit_enabled(active.as_str())
                .await
                .unwrap()
        );
        decide(&mut app, &runner, &conversation, "/audit status")
            .await
            .unwrap();
        assert!(
            app.messages
                .last()
                .unwrap()
                .content
                .contains("/audit trusted|review|ask")
        );
        for input in ["/audit maybe", "/audit on missing", "/audit on extra extra"] {
            assert!(
                decide(&mut app, &runner, &conversation, input)
                    .await
                    .is_err()
            );
            assert!(!store.command_audit_enabled(active.as_str()).await.unwrap());
        }
        decide(&mut app, &runner, &conversation, "/audit on")
            .await
            .unwrap();
        assert_eq!(app.audit_mode, CommandAuditMode::Review);
        let owner = SessionId::new("cli:once:owner");
        let mut session = Session::new(owner.clone());
        let call = ToolCall {
            call_id: "pending".into(),
            name: "exec_command".into(),
            arguments: serde_json::json!({"cmd":"rm important.txt","workdir":"/workspace"}),
        };
        session.push(Message::ToolCall(call.clone()));
        let mut checkpoint = Checkpoint::new(&store, &owner, &session);
        checkpoint.record.pending_calls = vec![call.clone()];
        checkpoint
            .save(
                &session,
                RunPhase::NeedTool {
                    call_id: call.call_id.clone(),
                },
                None,
            )
            .await
            .unwrap();
        store
            .set_command_audit_mode(&checkpoint.record.session_id, CommandAuditMode::Review)
            .await
            .unwrap();
        store
            .record_command_audits(&checkpoint.record, &[(call, "destructive".into())])
            .await
            .unwrap();
        decide(
            &mut app,
            &runner,
            &conversation,
            "/approvals cli:once:owner",
        )
        .await
        .unwrap();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        app.update(key(KeyCode::Up));
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Down));
        let disable = app.update(key(KeyCode::Enter)).unwrap();
        decide(&mut app, &runner, &conversation, &disable)
            .await
            .unwrap();
        assert!(app.approval.is_none());
        assert!(!store.command_audit_enabled(owner.as_str()).await.unwrap());
        assert!(store.command_audit_enabled(active.as_str()).await.unwrap());
        assert_eq!(store.active_id(&conversation).await.unwrap(), active);
        assert_eq!(app.input, "draft message");
        assert_eq!(app.input_cursor, 5);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| super::super::ui::render(&mut app, frame))
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("◇ AI review ·"));
        decide(&mut app, &runner, &conversation, "/audit on cli:once:owner")
            .await
            .unwrap();
        assert!(store.command_audit_enabled(owner.as_str()).await.unwrap());
        assert_eq!(app.audit_mode, CommandAuditMode::Review);
        let pending = app.approval.as_ref().unwrap().id.clone();
        app.update(key(KeyCode::Up));
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Down));
        let stale = app.update(key(KeyCode::Enter)).unwrap();
        store
            .decide_command_approval(owner.as_str(), &pending, false)
            .await
            .unwrap();
        assert!(
            decide(&mut app, &runner, &conversation, &stale)
                .await
                .is_err()
        );
        assert!(
            store.command_audit_enabled(owner.as_str()).await.unwrap(),
            "a stale picker must not disable future checks"
        );
        decide(
            &mut app,
            &runner,
            &conversation,
            "/audit ask cli:once:owner",
        )
        .await
        .unwrap();
        assert_eq!(
            app.audit_mode,
            CommandAuditMode::Review,
            "the status line continues to describe the active session after an inspected request is gone"
        );
        assert_eq!(
            store.command_audit_mode(owner.as_str()).await.unwrap(),
            CommandAuditMode::Ask
        );
        app.language = crate::i18n::Language::Ru;
        assert!(handle_command(&mut app, &runner, &conversation, "/audit impossible").await);
        let error = &app.messages.last().unwrap().content;
        assert!(error.starts_with("Не удалось"));
        assert!(
            !error.contains("Session unavailable") && !error.contains("Command control failed")
        );
        store
            .set_default_command_audit_mode(CommandAuditMode::Ask)
            .await
            .unwrap();
        let fresh = runner.new_session(&conversation).await.unwrap();
        let mut restored = App::default();
        refresh(&mut restored, &runner, &conversation)
            .await
            .unwrap();
        assert_eq!(restored.audit_mode, CommandAuditMode::Ask);
        assert_eq!(restored.audit_session, Some(fresh.id().clone()));
    }

    #[tokio::test]
    async fn tui_command_approval_inspects_other_local_session_without_switching() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteSessionStore::open(dir.path().into()).unwrap());
        let conversation = SessionId::new("cli:main");
        let once = SessionId::new("cli:once:pending");
        let mut session = Session::new(once.clone());
        let call = ToolCall {
            call_id: "once-call".into(),
            name: "exec_command".into(),
            arguments: serde_json::json!({"cmd":"rm important.txt","workdir":"/workspace"}),
        };
        session.push(Message::ToolCall(call.clone()));
        let mut checkpoint = Checkpoint::new(&store, &once, &session);
        checkpoint.record.pending_calls = vec![call.clone()];
        checkpoint
            .save(
                &session,
                RunPhase::NeedTool {
                    call_id: call.call_id.clone(),
                },
                None,
            )
            .await
            .unwrap();
        store
            .set_command_audit_mode(&checkpoint.record.session_id, CommandAuditMode::Review)
            .await
            .unwrap();
        store
            .record_command_audits(&checkpoint.record, &[(call, "destructive".into())])
            .await
            .unwrap();
        let approval = store
            .pending_command_approvals(once.as_str())
            .await
            .unwrap()
            .remove(0);
        let runner = AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(NoModel),
                ToolRegistry::default(),
                ContextBuilder::new(""),
            )),
            store.clone(),
        );
        let mut app = App::default();
        assert!(
            runner.select_session(&conversation, &once).await.is_err(),
            "busy-session selection fence must remain enforced"
        );
        decide(
            &mut app,
            &runner,
            &conversation,
            "/approvals cli:once:pending",
        )
        .await
        .unwrap();
        assert!(
            app.messages
                .last()
                .unwrap()
                .content
                .contains("cli:once:pending")
        );
        assert!(super::super::refresh_history(
            &mut app,
            &Session::new(conversation.clone())
        ));
        refresh(&mut app, &runner, &conversation).await.unwrap();
        assert!(
            app.messages
                .last()
                .unwrap()
                .content
                .contains("cli:once:pending")
        );
        let mut current = Session::new(conversation.clone());
        let call = ToolCall {
            call_id: "current-call".into(),
            name: "exec_command".into(),
            arguments: serde_json::json!({"cmd":"rm current-file","workdir":"/workspace"}),
        };
        current.push(Message::ToolCall(call.clone()));
        let mut cp = Checkpoint::new(&store, &conversation, &current);
        cp.record.pending_calls = vec![call.clone()];
        cp.save(
            &current,
            RunPhase::NeedTool {
                call_id: call.call_id.clone(),
            },
            None,
        )
        .await
        .unwrap();
        store
            .set_command_audit_mode(&cp.record.session_id, CommandAuditMode::Review)
            .await
            .unwrap();
        store
            .record_command_audits(&cp.record, &[(call, "destructive".into())])
            .await
            .unwrap();
        refresh(&mut app, &runner, &conversation).await.unwrap();
        let current_id = app.approval.as_ref().unwrap().id.clone();
        assert!(
            app.messages
                .last()
                .unwrap()
                .content
                .contains("rm current-file")
        );
        store
            .decide_command_approval(conversation.as_str(), &current_id, false)
            .await
            .unwrap();
        refresh(&mut app, &runner, &conversation).await.unwrap();
        assert_eq!(app.approval.as_ref().unwrap().id, approval.id);
        assert!(
            app.messages
                .last()
                .unwrap()
                .content
                .contains("rm important.txt"),
            "returning to a request must display that command again"
        );
        assert!(
            decide(&mut app, &runner, &conversation, "/approvals missing")
                .await
                .is_err()
        );
        for id in [
            "telegram:test:-42",
            "telegram:test:42:guest",
            "companion:test",
        ] {
            store.save(&Session::new(SessionId::new(id))).await.unwrap();
            assert!(
                decide(
                    &mut app,
                    &runner,
                    &conversation,
                    &format!("/approvals {id}")
                )
                .await
                .is_err()
            );
        }
        assert!(
            decide(
                &mut app,
                &runner,
                &conversation,
                "/approvals cli:once:pending extra"
            )
            .await
            .is_err()
        );
        let other = tempfile::tempdir().unwrap();
        let other_runner = AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(NoModel),
                ToolRegistry::default(),
                ContextBuilder::new(""),
            )),
            Arc::new(SqliteSessionStore::open(other.path().into()).unwrap()),
        );
        assert!(
            decide(
                &mut app,
                &other_runner,
                &conversation,
                "/approvals cli:once:pending"
            )
            .await
            .is_err()
        );
        assert!(
            decide(
                &mut app,
                &other_runner,
                &conversation,
                &format!("/approve {}", approval.id)
            )
            .await
            .is_err()
        );
        decide(
            &mut app,
            &runner,
            &conversation,
            &format!("/approve {}", approval.id),
        )
        .await
        .unwrap();
        assert!(
            store
                .pending_command_approvals(once.as_str())
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.active_id(&conversation).await.unwrap(), conversation);
    }

    #[tokio::test]
    async fn tui_command_approval_recovers_displays_and_requires_explicit_current_decision() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::{Terminal, backend::TestBackend};

        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().into()).unwrap();
        let session_id = SessionId::new("cli:main");
        let mut session = Session::new(session_id.clone());
        let command = format!("printf '<untrusted>\\n'\nrm {}", "long-path".repeat(600));
        let calls: Vec<_> = ["allow", "deny", "external"]
            .into_iter()
            .map(|id| ToolCall {
                call_id: id.into(),
                name: "exec_command".into(),
                arguments: serde_json::json!({"cmd":command,"workdir":"/workspace/a b"}),
            })
            .collect();
        for call in &calls {
            session.push(Message::ToolCall(call.clone()));
        }
        let mut checkpoint = Checkpoint::new(&store, &session_id, &session);
        checkpoint.record.pending_calls = calls.clone();
        checkpoint
            .save(
                &session,
                RunPhase::NeedTool {
                    call_id: "allow".into(),
                },
                None,
            )
            .await
            .unwrap();
        store
            .set_command_audit_mode(&checkpoint.record.session_id, CommandAuditMode::Review)
            .await
            .unwrap();
        store
            .record_command_audits(
                &checkpoint.record,
                &calls
                    .into_iter()
                    .map(|call| (call, "destructive".into()))
                    .collect::<Vec<_>>(),
            )
            .await
            .unwrap();
        let pending = store
            .pending_command_approvals(session_id.as_str())
            .await
            .unwrap();
        drop(checkpoint);
        drop(store);
        let store = Arc::new(SqliteSessionStore::open(dir.path().into()).unwrap());
        let runner = AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(NoModel),
                ToolRegistry::default(),
                ContextBuilder::new(""),
            )),
            store.clone(),
        );
        let mut app = App::default();
        app.insert_input("draft message");
        app.input_cursor = 5;
        let allow = format!("/approve {}", pending[0].id);
        assert!(
            decide(&mut app, &runner, &session_id, &allow)
                .await
                .is_err(),
            "unseen command must be reviewed first"
        );
        assert!(refresh(&mut app, &runner, &session_id).await.unwrap());
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        assert_eq!(app.approval_choice, Some(0));
        app.update(key(KeyCode::Char('x')));
        app.insert_input("pasted text");
        assert_eq!(app.input, "draft message");
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| super::super::ui::render(&mut app, frame))
            .unwrap();
        let footer: String = (17..24)
            .flat_map(|y| (0..80).map(move |x| (x, y)))
            .map(|point| terminal.backend().buffer()[point].symbol())
            .collect();
        assert!(
            footer.contains("> Allow once")
                && footer.contains("Reject")
                && footer.contains("No checks for this session")
        );
        assert!(!footer.contains("draft message"));
        app.language = crate::i18n::Language::Ru;
        terminal
            .draw(|frame| super::super::ui::render(&mut app, frame))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            text.contains("> Разрешить один раз")
                && text.contains("Отклонить")
                && text.contains("Без проверок для этого сеанса")
        );
        for (width, height) in [(1, 1), (3, 3), (8, 5)] {
            let mut small = Terminal::new(TestBackend::new(width, height)).unwrap();
            small
                .draw(|frame| super::super::ui::render(&mut app, frame))
                .unwrap();
        }
        app.language = crate::i18n::Language::En;
        let displayed = app.messages.last().unwrap();
        assert!(
            displayed.content.contains(&command.replace('\n', "\n  ")),
            "even long commands must be shown completely"
        );
        assert!(displayed.content.contains("/workspace/a b"));
        assert!(matches!(displayed.role, AppMessageRole::Approval));
        let rendered = super::super::ui::message_text(displayed)
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            rendered, displayed.content,
            "rendering must not interpret command text as Markdown"
        );
        assert!(!refresh(&mut app, &runner, &session_id).await.unwrap());
        assert!(super::super::refresh_history(&mut app, &session));
        assert!(refresh(&mut app, &runner, &session_id).await.unwrap());
        assert!(
            app.messages
                .last()
                .unwrap()
                .content
                .contains(&command.replace('\n', "\n  "))
        );
        assert!(!handle_command(&mut app, &runner, &session_id, "yes, approve that").await);
        assert!(
            decide(&mut app, &runner, &session_id, &format!("{allow} extra"))
                .await
                .is_err()
        );
        assert_eq!(
            store
                .pending_command_approvals(session_id.as_str())
                .await
                .unwrap()
                .len(),
            3
        );
        assert_eq!(app.update(key(KeyCode::Enter)), Some(allow.clone()));
        assert!(handle_command(&mut app, &runner, &session_id, &allow).await);
        assert_eq!(app.approval_choice, Some(0));
        assert_eq!(
            app.update(KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                KeyEventKind::Repeat
            )),
            None,
            "holding Enter must not approve the next request"
        );
        assert!(
            decide(&mut app, &runner, &session_id, &allow)
                .await
                .is_err()
        );
        app.update(key(KeyCode::Down));
        let deny = app.update(key(KeyCode::Enter)).unwrap();
        assert_eq!(deny, format!("/deny {}", pending[1].id));
        assert!(handle_command(&mut app, &runner, &session_id, &deny).await);
        assert_eq!(app.approval.as_ref().unwrap().id, pending[2].id);
        app.update(key(KeyCode::Up));
        store
            .decide_command_approval(session_id.as_str(), &pending[2].id, false)
            .await
            .unwrap();
        assert!(
            refresh(&mut app, &runner, &session_id).await.unwrap(),
            "an external decision must redraw the composer"
        );
        assert!(app.approval.is_none());
        assert!(app.approval_choice.is_none());
        assert!(
            store
                .pending_command_approvals(session_id.as_str())
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(app.input, "draft message");
        assert_eq!(app.input_cursor, 5);
        assert_eq!(
            app.update(KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                KeyEventKind::Repeat
            )),
            None,
            "holding Enter must not submit the restored draft"
        );
        assert_eq!(
            app.update(key(KeyCode::Enter)).as_deref(),
            Some("draft message")
        );
    }
}
