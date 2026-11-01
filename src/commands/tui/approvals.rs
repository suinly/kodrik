use super::app::{App, AppMessage, AppMessageRole};
use crate::agent::{
    AgentRunner,
    command_audit::{CommandApproval, CommandAuditMode},
    session::{SessionId, SessionStore},
};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

pub(super) const MODES: [CommandAuditMode; 3] = [
    CommandAuditMode::Trusted,
    CommandAuditMode::Review,
    CommandAuditMode::Ask,
];

pub(super) struct ModePicker {
    pub session: Option<SessionId>,
    pub current: CommandAuditMode,
    pub choice: Option<usize>,
    pub offset: usize,
}

pub(super) fn toggle_modes(app: &mut App) {
    app.mode_picker = if app.mode_picker.is_some() {
        None
    } else {
        Some(ModePicker {
            session: app.audit_session.clone(),
            current: app.audit_mode,
            choice: None,
            offset: 0,
        })
    };
}

pub(super) fn update_modes(app: &mut App, key: KeyEvent) -> Option<String> {
    let picker = app.mode_picker.as_mut()?;
    if !key.modifiers.is_empty() {
        return None;
    }
    match key.code {
        KeyCode::Up => {
            picker.choice = Some(picker.choice.map_or(0, |index| index.saturating_sub(1)))
        }
        KeyCode::Down => picker.choice = Some(picker.choice.map_or(0, |index| (index + 1).min(2))),
        KeyCode::Esc if key.kind == KeyEventKind::Press => app.mode_picker = None,
        KeyCode::Enter if key.kind == KeyEventKind::Press => {
            let mode = MODES[picker.choice?];
            let command = format!(
                "/audit {}{}",
                mode.code(),
                picker
                    .session
                    .as_ref()
                    .map_or_else(String::new, |id| format!(" {}", id.as_str()))
            );
            app.mode_picker = None;
            return Some(command);
        }
        _ => {}
    }
    None
}

pub(super) fn modes_mouse(app: &mut App, event: MouseEvent) -> (bool, Option<String>) {
    let point = ratatui::layout::Position::new(event.column, event.row);
    if event.kind == MouseEventKind::Down(MouseButton::Left) && app.mode_button.contains(point) {
        toggle_modes(app);
        return (true, None);
    }
    if app.mode_picker.is_none() {
        return (false, None);
    }
    if event.kind == MouseEventKind::Down(MouseButton::Left) {
        let row = event.row.saturating_sub(app.mode_popup.y);
        if app.mode_popup.contains(point)
            && event.column > app.mode_popup.x
            && event.column.saturating_add(1) < app.mode_popup.right()
            && row > 0
            && row <= 3
            && row + 1 < app.mode_popup.height
        {
            let picker = app.mode_picker.as_mut().unwrap();
            let index = picker.offset + usize::from(row - 1);
            if index >= MODES.len() {
                return (true, None);
            }
            picker.choice = Some(index);
            return (
                true,
                update_modes(app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            );
        }
        if !app.mode_popup.contains(point) {
            app.mode_picker = None;
        }
    }
    (true, None)
}

pub(super) fn is_command(input: &str) -> bool {
    matches!(
        input.split_whitespace().next(),
        Some("/approvals" | "/approve" | "/deny" | "/audit")
    )
}

fn show(app: &mut App, session: &SessionId, approval: &CommandApproval) {
    // Debug quoting makes control characters visible without interpreting shell text as Markdown.
    app.messages.push(AppMessage {
        role: AppMessageRole::Approval,
        content: format!(
            "{}\n{}: {}\nID: {}\n{}: {:?}\n{}: {:?}\n{}: {}",
            app.language.pick(
                "Command approval required",
                "Требуется подтверждение команды"
            ),
            app.language.pick("Session", "Сеанс"),
            session.as_str(),
            approval.id,
            app.language.pick("Command", "Команда"),
            approval.cmd,
            app.language.pick("Workdir", "Рабочий каталог"),
            approval
                .workdir
                .as_deref()
                .unwrap_or(app.language.pick("(default)", "(по умолчанию)")),
            app.language.pick("Reason", "Причина"),
            approval.reason_text(app.language),
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
            KeyCode::Up => {
                app.approval_choice = Some(
                    app.approval_choice
                        .map_or(0, |choice| choice.saturating_sub(1)),
                )
            }
            KeyCode::Down => {
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
    let mut disabled = Vec::new();
    let mut modes = std::collections::HashMap::new();
    let mut next = None;
    for session in sessions {
        let mode = store.command_audit_mode(session.as_str()).await?;
        modes.insert(session.as_str().to_owned(), mode);
        if mode == CommandAuditMode::Trusted {
            disabled.push(session.as_str().to_owned());
        }
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
    if app.audit_disabled_sessions != disabled {
        app.audit_disabled_sessions = disabled;
        changed = true;
    }
    if app.approval.as_ref().map(|a| &a.id) != next.as_ref().map(|a| &a.id) {
        app.approval_choice = None;
        app.view.clear_selection();
        if next.is_some() {
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
            app.language.pick("This command is no longer pending. Refresh with /approvals.", "Эта команда больше не ожидает решения. Обновите список через /approvals.")
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
                        "No pending command approvals.",
                        "Нет команд, ожидающих подтверждения.",
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
        "Сначала просмотрите команду через /approvals.",
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
                .pick("Command allowed once", "Команда разрешена один раз")
        } else {
            app.language.pick("Command rejected", "Команда отклонена")
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
            ["trusted" | "review" | "ask" | "on" | "off" | "status"]
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
        app.language.pick("Command mode", "Режим команд"),
        mode.label(app.language),
        if mode == CommandAuditMode::Trusted {
            app.language.pick(
                " Pending commands may proceed.",
                " Ожидающие команды могут выполняться.",
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
    fn mode_header_mouse_and_escape_preserve_pending_request_and_draft() {
        use ratatui::{Terminal, backend::TestBackend};
        for language in [crate::i18n::Language::En, crate::i18n::Language::Ru] {
            let approval = CommandApproval {
                id: "request".into(),
                run_id: "run".into(),
                call_id: "call".into(),
                cmd: "rm important.txt".into(),
                workdir: Some("/workspace".into()),
                reason: "English fallback must not appear".into(),
                reason_code: "destructive".into(),
            };
            let mut app = App {
                language,
                input: "draft".into(),
                input_cursor: 2,
                audit_mode: CommandAuditMode::Review,
                audit_session: Some(SessionId::new("owner")),
                approval: Some(approval.clone()),
                approval_choice: Some(1),
                ..Default::default()
            };
            for i in 0..30 {
                app.push_kodrik_message(format!("message {i}"));
            }
            show(&mut app, &SessionId::new("owner"), &approval);
            assert!(
                !app.messages
                    .last()
                    .unwrap()
                    .content
                    .contains("English fallback")
            );
            app.scroll = 10;
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal
                .draw(|frame| super::super::ui::render(&mut app, frame))
                .unwrap();
            let before = app.scroll;
            let click = |column, row| MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            };
            assert_eq!(modes_mouse(&mut app, click(1, 0)), (true, None));
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
            for label in [
                CommandAuditMode::Trusted.label(language),
                CommandAuditMode::Review.label(language),
                CommandAuditMode::Ask.label(language),
            ] {
                assert!(rendered.contains(label), "{rendered}");
            }
            assert_eq!(
                app.update(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
                None
            );
            assert_eq!(app.approval.as_ref().unwrap().id, "request");
            assert_eq!(app.approval_choice, Some(1));
            assert_eq!(app.input, "draft");
            assert_eq!(app.input_cursor, 2);
            assert_eq!(app.scroll, before);
            modes_mouse(&mut app, click(1, 0));
            terminal
                .draw(|frame| super::super::ui::render(&mut app, frame))
                .unwrap();
            let row = app.mode_popup.y + 3;
            assert_eq!(
                modes_mouse(&mut app, click(2, row)),
                (true, Some("/audit ask owner".into()))
            );
            assert_eq!(
                app.audit_mode,
                CommandAuditMode::Review,
                "opening and choosing only dispatch explicit commands"
            );
            app.update(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
            for (width, height) in [(1, 1), (3, 3), (8, 5)] {
                let mut small = Terminal::new(TestBackend::new(width, height)).unwrap();
                small
                    .draw(|frame| super::super::ui::render(&mut app, frame))
                    .unwrap();
            }
            for _ in 0..3 {
                app.update(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            }
            let mut short = Terminal::new(TestBackend::new(80, 5)).unwrap();
            short
                .draw(|frame| super::super::ui::render(&mut app, frame))
                .unwrap();
            assert_eq!(
                modes_mouse(&mut app, click(2, 3)),
                (true, Some("/audit ask owner".into())),
                "clicks must match visible options after the short menu scrolls"
            );
        }
    }

    #[test]
    fn approval_picker_can_disable_checks_for_the_displayed_owner() {
        let mut app = App {
            approval: Some(CommandApproval {
                id: "request".into(),
                run_id: "run".into(),
                call_id: "call".into(),
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
        app.update(key(KeyCode::Up));
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Down));
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

    #[test]
    fn approval_picker_rejects_on_escape_and_ignores_repeated_enter() {
        let mut app = App {
            approval: Some(CommandApproval {
                id: "request".into(),
                run_id: "run".into(),
                call_id: "call".into(),
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
        assert_eq!(app.audit_mode, CommandAuditMode::Trusted);
        decide(&mut app, &runner, &conversation, "/audit off")
            .await
            .unwrap();
        let active = store.active_id(&conversation).await.unwrap();
        assert!(store.load(&active).await.unwrap().is_some());
        assert!(!store.command_audit_enabled(active.as_str()).await.unwrap());
        assert_eq!(app.audit_disabled_sessions, [active.as_str()]);
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
        assert!(app.audit_disabled_sessions.is_empty());
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
        assert!(rendered.contains("No checks: cli:once:owner"));
        decide(&mut app, &runner, &conversation, "/audit on cli:once:owner")
            .await
            .unwrap();
        assert!(store.command_audit_enabled(owner.as_str()).await.unwrap());
        assert!(app.audit_disabled_sessions.is_empty());
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
            "the header continues to describe the active session after an inspected request is gone"
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
        assert_eq!(
            app.update(key(KeyCode::Enter)),
            None,
            "a choice must be selected first"
        );
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
            footer.contains("Allow once")
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
            text.contains("Разрешить один раз")
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
            displayed.content.contains(&format!("{command:?}")),
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
                .contains(&format!("{command:?}"))
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
        app.update(key(KeyCode::Up));
        assert_eq!(app.update(key(KeyCode::Enter)), Some(allow.clone()));
        assert!(handle_command(&mut app, &runner, &session_id, &allow).await);
        assert_eq!(
            app.update(key(KeyCode::Enter)),
            None,
            "the next request requires a new selection"
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
