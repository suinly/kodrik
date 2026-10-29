mod app;
mod clipboard;
#[cfg(all(test, unix))]
mod startup_tests;
mod terminal;
mod ui;
mod view;

use std::{
    io::{self, IsTerminal, Write},
    path::Path,
    sync::Arc,
    time::Duration,
};

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinSet,
};

use crate::{
    agent::{
        AgentEvent,
        message::Message,
        session::{Session, SessionId},
    },
    bootstrap::build_app,
    config::{Config, ProviderConfig},
    gateway::{IncomingMessage, InterfaceId},
    i18n::Language,
};
use app::{App, AppMessageRole, Status, ToolState};
use terminal::Tui;

pub async fn run(config: &Config) -> anyhow::Result<()> {
    let language_path = crate::config::data_dir()?.join("tui-language");
    let language = load_language(&language_path, Language::from_environment())?;
    anyhow::ensure!(
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
        language.pick(
            "TUI requires an interactive terminal; use kodrik ask <prompt> for one-shot requests",
            "Для TUI нужен интерактивный терминал; для разового запроса используйте kodrik ask <prompt>"
        )
    );
    let context = build_app(config).await?;
    let ProviderConfig::OpenAi { model, .. } = &config.provider["primary"];
    run_session(
        context.agent_runner,
        model.clone(),
        language,
        &language_path,
    )
    .await
}

async fn run_session(
    runner: Arc<crate::agent::AgentRunner>,
    model: String,
    language: Language,
    language_path: &Path,
) -> anyhow::Result<()> {
    let session_id = SessionId::new("cli:main");
    let recovery = runner.recover(crate::agent::recovery::RecoveryTarget::Conversation(
        session_id.clone(),
    ));
    tokio::pin!(recovery);
    let mut recovering = true;

    let session = runner.load_session(session_id.clone()).await?;
    let mut app = App {
        language,
        status: Status::Recovering,
        ..Default::default()
    };
    app.model = model;
    restore_history(&mut app, &session);

    let mut tui = Tui::enter()?;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut history_tick = tokio::time::interval(Duration::from_secs(1));
    history_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut history_revision = None;
    let (progress_tx, mut progress_rx) = mpsc::channel(32);
    let mut requests = JoinSet::new();
    let mut copies = JoinSet::new();
    let (input_tx, input_rx) = mpsc::unbounded_channel::<IncomingMessage>();
    let mut idle_inputs = Some(input_rx);
    let mut cancel_request: Option<oneshot::Sender<()>> = None;
    let mut redraw = true;
    let mut drawn_size = None;

    let shutdown = super::shutdown_signal();
    tokio::pin!(shutdown);
    while !app.should_quit {
        if let Some(inputs) = idle_inputs.as_mut()
            && let Ok(mut input) = inputs.try_recv()
        {
            if runner
                .submission_finished(&session_id, &input.request_id)
                .await?
            {
                continue;
            }
            if let Some(next) = runner.next_input(&session_id).await?
                && next.request_id != input.request_id
            {
                input_tx.send(input)?;
                input = next;
            }

            let inputs = idle_inputs.take().unwrap();
            let runner = runner.clone();
            let session_id = session_id.clone();
            let progress = progress_tx.clone();
            let (cancel, cancelled) = oneshot::channel();
            cancel_request = Some(cancel);
            app.status = Status::Working;
            redraw = true;
            requests.spawn(async move {
                let message = input;
                let result = runner
                    .run_with_cancel(session_id, message, Some(progress), Some(cancelled), None)
                    .await;
                (result, inputs)
            });
        }
        if redraw {
            drawn_size = Some(crossterm::terminal::size()?);
            tui.draw(&mut app)?;
            redraw = false;
        }
        tokio::select! {
            report = &mut recovery, if recovering => {
                recovering = false;
                let mut report = report?;
                let stopped = matches!(app.status, Status::Stopping);
                if stopped {
                    report.blocked.retain(|(_, reason)| !reason.starts_with("Stopped by user"));
                }
                let session = runner.load_session(session_id.clone()).await?;
                app.messages.clear();
                app.pending_tools.clear();
                app.streaming_message = None;
                restore_history(&mut app, &session);
                app.status = if !report.blocked.is_empty() {
                    Status::Error
                } else if stopped || !report.cancelled.is_empty() {
                    Status::Stopped
                } else {
                    Status::Ready
                };
                for (id, reason) in report.blocked {
                    app.push_kodrik_message(format!("{} {id}: {reason}", app.language.pick("Recovery blocked for", "Восстановление заблокировано для")));
                }
                redraw = true;
            },
            _ = &mut shutdown => { app.should_quit = true; runner.suspend(); },
            event = next_terminal_event() => {
                match event {
                    Ok(Event::Key(key)) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                        match selection_command(&mut app, key) {
                            Some(SelectionCommand::Clear) => { redraw = true; continue; }
                            Some(SelectionCommand::Copy(text)) => {
                                copies.abort_all();
                                while copies.try_join_next().is_some() {}
                                let selection_id = app.view.selection_id();
                                copies.spawn(async move { (selection_id, clipboard::copy(text).await) });
                                continue;
                            }
                            None => {}
                        }
                        if key.code == KeyCode::Esc && recovering && app.command_suggestion().is_none() {
                            // Recovery is paused here, so queued work cannot start during cancellation.
                            while let Some(input) = runner.next_input(&session_id).await? {
                                runner.cancel_queued_input(&session_id, &input.request_id).await?;
                            }
                            runner.cancel_conversation(&session_id).await?;
                            app.status = Status::Stopping;
                            redraw = true;
                            continue;
                        }
                        if key.code == KeyCode::Enter && app.sessions.is_some() {
                            if let Some((id, _)) = app.sessions.as_ref().and_then(|sessions| sessions.get(app.session_index)) {
                                match runner.select_session(&session_id, id).await {
                                    Ok(session) => {
                                        reset_session(&mut app);
                                        restore_history(&mut app, &session);
                                        tui.clear()?;
                                    }
                                    Err(error) => {
                                        app.sessions = None;
                                        app.push_kodrik_message(format!("{}: {error:#}", app.language.pick("Could not open session", "Не удалось открыть сеанс")));
                                        app.status = Status::Error;
                                    }
                                }
                            }
                            redraw = true;
                            continue;
                        }
                        if key.code == KeyCode::Esc && app.command_suggestion().is_none() && matches!(app.status, Status::Working) {
                            if let Some(cancel) = cancel_request.take() {
                                let _ = cancel.send(());
                                app.status = Status::Stopping;
                            }
                            redraw = true;
                            continue;
                        }
                        if let Some(input) = app.update(key) {
                            if handle_language_command(&mut app, &input, language_path) {
                                redraw = true;
                                continue;
                            }
                            if input == "/sessions" {
                                match runner.list_sessions().await {
                                    Ok(sessions) => {
                                        app.sessions = Some(sessions.iter().map(|session| {
                                            let title = session.messages().iter().find_map(|message| match message {
                                                Message::User(message) => Some(short(&message.content, 80)),
                                                _ => None,
                                            }).unwrap_or_else(|| app.language.pick("Empty session", "Пустой сеанс").into());
                                            (session.id().clone(), title)
                                        }).collect());
                                        app.session_index = 0;
                                    }
                                    Err(error) => {
                                        app.push_kodrik_message(format!("{}: {error:#}", app.language.pick("Could not load sessions", "Не удалось загрузить сеансы")));
                                        app.status = Status::Error;
                                    }
                                }
                                redraw = true;
                                continue;
                            }
                            if input == "/new" {
                                match runner.new_session(&session_id).await {
                                    Ok(_) => {
                                        reset_session(&mut app);
                                        app.push_kodrik_message(app.language.pick("New session created.", "Создан новый сеанс.").into());
                                        tui.clear()?;
                                    }
                                    Err(error) => {
                                        app.push_kodrik_message(format!("{}: {error:#}", app.language.pick("Could not create session", "Не удалось создать сеанс")));
                                        app.status = Status::Error;
                                    }
                                }
                                redraw = true;
                                continue;
                            }
                            let message=IncomingMessage::new(InterfaceId("cli".into()),"main",input);
                            match runner.submit(crate::agent::inbox::SubmissionRequest::new(&session_id,message.clone(),crate::agent::inbox::BusyMode::Steer)).await {
                                Ok(_)=>{input_tx.send(message)?;},
                                Err(error)=>{app.push_kodrik_message(format!("{}: {error:#}", app.language.pick("Input was not confirmed", "Ввод не подтверждён")));app.status=Status::Error;}
                            }
                        }
                        redraw = true;
                    }
                    Ok(Event::Paste(text)) => {
                        app.view.clear_selection();
                        if app.sessions.is_none() { app.insert_input(&text); }
                        redraw = true;
                    }
                    Ok(Event::Mouse(mouse)) => {
                        if let Some(sessions) = &app.sessions {
                            match mouse.kind {
                                MouseEventKind::ScrollUp => { app.view.clear_selection(); app.session_index = app.session_index.saturating_sub(1); },
                                MouseEventKind::ScrollDown => { app.view.clear_selection(); app.session_index = (app.session_index + 1).min(sessions.len().saturating_sub(1)); },
                                _ => {}
                            }
                        }
                        if let Some(text) = app.view.mouse(mouse, &mut app.scroll) {
                            copies.abort_all();
                            while copies.try_join_next().is_some() {}
                            let selection_id = app.view.selection_id();
                            copies.spawn(async move { (selection_id, clipboard::copy(text).await) });
                        }
                        redraw = true;
                    }
                    Ok(Event::Resize(_, _)) | Ok(Event::FocusLost) => {
                        app.view.clear_selection();
                        redraw = true;
                    }
                    Err(error) => return Err(error.into()),
                    _ => {}
                }
            }
            Some(event) = progress_rx.recv() => {
                show_progress(&mut app, event);
                redraw = true;
            }
            Some(result) = copies.join_next(), if !copies.is_empty() => {
                match result {
                    Ok((selection_id, result)) => complete_copy(&mut app, selection_id, result, |sequence| tui.copy_to_terminal(sequence)),
                    Err(error) if error.is_cancelled() => {},
                    Err(error) => app.set_copy_notice(format!("{}: {error}", app.language.pick("Copy failed", "Не удалось скопировать"))),
                }
                redraw = true;
            }
            Some(result) = requests.join_next(), if !requests.is_empty() => {
                cancel_request = None;
                let result = result.map(|(result, mut inputs)| {
                    if matches!(app.status, Status::Stopping) {
                        while inputs.try_recv().is_ok() {}
                    }
                    idle_inputs = Some(inputs);
                    result
                });
                while let Ok(event) = progress_rx.try_recv() {
                    show_progress(&mut app, event);
                }
                if matches!(app.status, Status::Stopping)
                    && let Ok(Err(error)) = &result
                    && error.to_string() == "Stopped by user"
                {
                    for index in app.pending_tools.values() {
                        app.messages[*index].role = AppMessageRole::Tool(ToolState::Failed);
                        app.messages[*index].content.push_str(app.language.pick(" — interrupted", " — прервано"));
                    }
                    app.pending_tools.clear();
                    app.streaming_message = None;
                    app.status = Status::Stopped;
                    redraw = true;
                    continue;
                }
                match result {
                    Ok(Ok(message)) => {
                        app.finish_stream(message.content);
                        app.status = Status::Done(chrono::Local::now().format("%H:%M").to_string());
                    }
                    Ok(Err(error)) => {
                        app.streaming_message = None;
                        app.push_kodrik_message(format!("{}: {error:#}", app.language.pick("Error", "Ошибка")));
                        app.status = Status::Error;
                    }
                    Err(error) => {
                        app.streaming_message = None;
                        app.push_kodrik_message(format!("{}: {error}", app.language.pick("Error", "Ошибка")));
                        app.status = Status::Error;
                    }
                }
                redraw = true;
            }
            _ = history_tick.tick(), if !recovering && idle_inputs.is_some() && app.sessions.is_none()
                && matches!(app.status, Status::Ready | Status::Done(_)) => {
                if let Some(store) = runner.sqlite_store() {
                    let revision = store.view_revision().await?;
                    if history_revision != Some(revision) {
                        let session = runner.load_session(session_id.clone()).await?;
                        redraw |= refresh_history(&mut app, &session);
                        history_revision = Some(revision);
                    }
                }
            }
            _ = tick.tick() => {
                redraw |= app.view.tick(&mut app.scroll);
                if app.copy_notice.as_ref().is_some_and(|(_, until)| std::time::Instant::now() >= *until) {
                    app.copy_notice = None;
                    redraw = true;
                }
                if matches!(app.status, Status::Recovering | Status::Working | Status::Stopping) {
                    app.animation_frame = app.animation_frame.wrapping_add(1);
                    redraw = true;
                }
                if drawn_size != Some(crossterm::terminal::size()?) {
                    redraw = true;
                }
            }
        }
    }
    runner.suspend();
    if recovering {
        let _ = recovery.await;
    }
    copies.abort_all();
    while !requests.is_empty() {
        tokio::select! {_=requests.join_next()=>{},_=progress_rx.recv()=>{}}
    }
    tui.exit()?;
    runner.finish_learning().await;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum SelectionCommand {
    Clear,
    Copy(String),
}

fn complete_copy(
    app: &mut App,
    selection_id: Option<u64>,
    result: io::Result<clipboard::CopyOutcome>,
    terminal_copy: impl FnOnce(&str) -> io::Result<()>,
) {
    let outcome = result.and_then(|outcome| match outcome {
        clipboard::CopyOutcome::Confirmed => Ok(true),
        clipboard::CopyOutcome::Terminal(sequence) => terminal_copy(&sequence).map(|()| false),
    });
    match outcome {
        Ok(confirmed) => {
            app.view.clear_copied_selection(selection_id);
            app.set_copy_notice(if confirmed {
                app.language
                    .pick("Copied to clipboard", "Скопировано в буфер обмена")
                    .into()
            } else {
                app.language
                    .pick(
                        "Copy requested from terminal",
                        "Запрос копирования отправлен терминалу",
                    )
                    .into()
            });
        }
        Err(error) => app.set_copy_notice(format!(
            "{}: {error}",
            app.language.pick("Copy failed", "Не удалось скопировать")
        )),
    }
}

fn selection_command(app: &mut App, key: KeyEvent) -> Option<SelectionCommand> {
    if key.code == KeyCode::Esc && app.view.clear_selection() {
        return Some(SelectionCommand::Clear);
    }
    if matches!(key.code, KeyCode::Char('c' | 'C' | 'с' | 'С'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && let Some(text) = app.view.selected_text()
    {
        return Some(SelectionCommand::Copy(text));
    }
    app.view.clear_selection();
    None
}

fn load_language(path: &Path, fallback: Language) -> anyhow::Result<Language> {
    match std::fs::read_to_string(path) {
        Ok(value) => Ok(value.trim().parse().unwrap_or(fallback)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(fallback),
        Err(error) => Err(anyhow::anyhow!(
            "{}: {error}",
            fallback.pick(
                "Could not read interface language",
                "Не удалось прочитать язык интерфейса"
            )
        )),
    }
}

fn save_language(path: &Path, language: Language) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(language.code().as_bytes())?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn handle_language_command(app: &mut App, input: &str, path: &Path) -> bool {
    let mut parts = input.split_whitespace();
    if parts.next() != Some("/language") {
        return false;
    }
    let language = parts
        .next()
        .and_then(|value| value.parse::<Language>().ok());
    if let Some(language) = language.filter(|_| parts.next().is_none()) {
        match save_language(path, language) {
            Ok(()) => {
                app.language = language;
                app.push_kodrik_message(
                    language
                        .pick("Interface language: English.", "Язык интерфейса: русский.")
                        .into(),
                );
            }
            Err(error) => app.push_kodrik_message(format!(
                "{}: {error}",
                app.language.pick(
                    "Could not save interface language",
                    "Не удалось сохранить язык интерфейса"
                )
            )),
        }
    } else {
        app.push_kodrik_message(
            app.language
                .pick("Usage: /language en|ru", "Использование: /language en|ru")
                .into(),
        );
    }
    true
}

fn reset_session(app: &mut App) {
    *app = App {
        language: app.language,
        model: std::mem::take(&mut app.model),
        ..Default::default()
    };
}

fn restore_history(app: &mut App, session: &Session) {
    let mut plan_calls = std::collections::HashMap::new();
    for message in session.messages() {
        match message {
            Message::User(message) => app.push_user_message(message.content.clone()),
            Message::Assistant(message) => app.push_kodrik_message(message.content.clone()),
            Message::ToolCall(call) => {
                if call.name == "update_plan" {
                    plan_calls.insert(call.call_id.clone(), call.arguments.clone());
                }
                show_progress(
                    app,
                    AgentEvent::ToolStarted {
                        call_id: call.call_id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    },
                );
            }
            Message::ToolResult(result) => {
                if result.content.output.get("error").is_none()
                    && let Some(arguments) = plan_calls.remove(&result.call_id)
                    && let Ok(plan) = crate::agent::tools::TaskPlan::parse(arguments)
                {
                    app.update_plan(plan);
                }
                show_progress(
                    app,
                    AgentEvent::ToolFinished {
                        call_id: result.call_id.clone(),
                        failed: result.content.output.get("error").is_some(),
                        output: result.content.output.clone(),
                    },
                );
            }
        }
    }
}

fn refresh_history(app: &mut App, session: &Session) -> bool {
    let mut saved = App {
        language: app.language,
        ..Default::default()
    };
    restore_history(&mut saved, session);
    if app.messages.len() == saved.messages.len()
        && app
            .messages
            .iter()
            .zip(&saved.messages)
            .all(|(a, b)| a.role == b.role && a.content == b.content)
    {
        return false;
    }
    app.messages = saved.messages;
    app.pending_tools = saved.pending_tools;
    app.streaming_message = None;
    app.view.clear_selection();
    true
}

#[test]
fn idle_history_refresh_shows_wakeup_without_losing_the_draft() {
    let mut session = Session::new(SessionId::new("history"));
    session.push(Message::Assistant(crate::agent::AssistantMessage {
        content: "Earlier answer".into(),
    }));
    let mut app = App::default();
    restore_history(&mut app, &session);
    app.input = "unfinished draft".into();
    app.input_cursor = 3;
    app.scroll = 7;
    assert!(!refresh_history(&mut app, &session));
    session.push(Message::Assistant(crate::agent::AssistantMessage {
        content: "Scheduled result".into(),
    }));
    assert!(refresh_history(&mut app, &session));
    assert_eq!(app.messages.len(), 2);
    assert_eq!(app.messages[1].content, "Scheduled result");
    assert_eq!(app.input, "unfinished draft");
    assert_eq!(app.input_cursor, 3);
    assert_eq!(app.scroll, 7);
    assert!(!refresh_history(&mut app, &session));
}

async fn next_terminal_event() -> std::io::Result<Event> {
    loop {
        if event::poll(Duration::ZERO)? {
            return event::read();
        }
        tokio::time::sleep(Duration::from_millis(16)).await;
    }
}

fn show_progress(app: &mut App, event: AgentEvent) -> bool {
    match event {
        AgentEvent::InputConsumed => {
            if let Some(index) = app.streaming_message.take() {
                app.messages.remove(index);
                for pending in app.pending_tools.values_mut() {
                    if *pending > index {
                        *pending -= 1;
                    }
                }
                return true;
            }
            return false;
        }
        AgentEvent::SessionChanged {
            previous_user_messages,
        } => {
            app.view = Default::default();
            let boundary = app
                .messages
                .iter()
                .enumerate()
                .filter(|(_, m)| matches!(m.role, AppMessageRole::User))
                .nth(previous_user_messages)
                .map(|(i, _)| i)
                .unwrap_or(app.messages.len());
            app.messages = app.messages.split_off(boundary);
            app.context_tokens = None;
            app.streaming_message = None;
            app.pending_tools.clear();
            app.scroll = 0;
            app.push_kodrik_message(
                app.language
                    .pick(
                        "New topic — started a new session.",
                        "Новая тема — начат новый сеанс.",
                    )
                    .into(),
            );
        }
        AgentEvent::PlanUpdated(plan) => app.update_plan(plan),
        AgentEvent::TextDelta(delta) => {
            let index = match app.streaming_message {
                Some(index) => index,
                None => {
                    let index = app.messages.len();
                    app.push_kodrik_message(String::new());
                    app.streaming_message = Some(index);
                    index
                }
            };
            app.messages[index].content.push_str(&delta);
        }
        AgentEvent::Message(message) => app.finish_stream(message.content),
        // shortcut: progress events carry English text; use typed statuses if these formats change.
        AgentEvent::Retrying(message) => {
            let message = if app.language == Language::Ru {
                message
                    .strip_prefix("Request failed. Retry ")
                    .map(|details| {
                        format!(
                            "Ошибка запроса. Повтор {}",
                            details.replace(" in ", " через ").replace(" sec", " с")
                        )
                    })
                    .unwrap_or(message)
            } else {
                message
            };
            app.finish_stream(message)
        }
        AgentEvent::Compaction(message) => {
            let message = match message.as_str() {
                "Compacting conversation context…" => app
                    .language
                    .pick(
                        "Compacting conversation context…",
                        "Сжимаю контекст беседы…",
                    )
                    .into(),
                "Context compacted; continuing…" => app
                    .language
                    .pick(
                        "Context compacted; continuing…",
                        "Контекст сжат; продолжаю…",
                    )
                    .into(),
                _ => message,
            };
            app.finish_stream(message)
        }
        AgentEvent::ContextTokens(tokens) => app.context_tokens = tokens,
        AgentEvent::ToolStarted {
            call_id,
            name,
            arguments,
        } => {
            if name == "update_plan" {
                return true;
            }
            let detail = match name.as_str() {
                "exec_command" => arguments.get("cmd"),
                "apply_patch" => arguments.get("workdir"),
                "view_image" => arguments.get("path"),
                _ => None,
            }
            .and_then(|value| value.as_str())
            .map(|value| format!(": {}", short(value, 120)))
            .unwrap_or_default();
            app.pending_tools.insert(call_id, app.messages.len());
            app.push_tool_message(format!("{name}{detail}"));
        }
        AgentEvent::ToolFinished {
            call_id,
            failed,
            output,
        } => {
            if !failed && output == serde_json::json!("Plan updated") {
                return false;
            }
            let (failed, status, details) = if failed {
                (
                    true,
                    short(
                        output["error"]
                            .as_str()
                            .unwrap_or(app.language.pick("Tool error", "Ошибка инструмента")),
                        160,
                    ),
                    String::new(),
                )
            } else if let Some(code) = output.get("exit_code") {
                let timed_out = output["timed_out"] == true;
                let status = if timed_out {
                    app.language
                        .pick("timed out", "время ожидания истекло")
                        .to_owned()
                } else {
                    format!("{} {code}", app.language.pick("exit", "код выхода"))
                };
                let details = command_output(&output);
                (timed_out || code.as_i64() != Some(0), status, details)
            } else if let Some(files) = output["changed_files"].as_array() {
                let details = files
                    .iter()
                    .filter_map(|file| {
                        let path = file["path"].as_str()?;
                        let added = file["added"].as_u64();
                        let removed = file["removed"].as_u64();
                        Some(match (added, removed) {
                            (Some(added), Some(removed)) => {
                                format!("  {path}  +{added} -{removed}")
                            }
                            _ => format!(
                                "  {path}  ({})",
                                file["action"]
                                    .as_str()
                                    .unwrap_or(app.language.pick("changed", "изменён"))
                            ),
                        })
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                (
                    false,
                    format!(
                        "{}: {}",
                        app.language.pick("changed files", "изменено файлов"),
                        files.len()
                    ),
                    details,
                )
            } else {
                (false, short(&output.to_string(), 160), String::new())
            };
            if let Some(index) = app.pending_tools.remove(&call_id) {
                app.messages[index].role = AppMessageRole::Tool(if failed {
                    ToolState::Failed
                } else {
                    ToolState::Success
                });
                app.messages[index]
                    .content
                    .push_str(&format!(" — {status}"));
                if !details.is_empty() {
                    app.messages[index].content.push('\n');
                    app.messages[index].content.push_str(&details);
                }
                return true;
            }
            app.push_tool_message(if details.is_empty() {
                status
            } else {
                format!("{status}\n{details}")
            });
            app.messages.last_mut().unwrap().role = AppMessageRole::Tool(if failed {
                ToolState::Failed
            } else {
                ToolState::Success
            });
        }
    }
    false
}

fn short(text: &str, limit: usize) -> String {
    let clean = text.trim().replace(['\n', '\r', '\t'], " ");
    let mut chars = clean.chars();
    let result: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{result}…")
    } else {
        result
    }
}

fn command_output(output: &serde_json::Value) -> String {
    let stream = if output["stderr"]
        .as_str()
        .is_some_and(|text| !text.trim().is_empty())
    {
        "stderr"
    } else {
        "stdout"
    };
    let lines = output[stream]
        .as_str()
        .unwrap_or("")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    lines
        .into_iter()
        .rev()
        .take(4)
        .rev()
        .enumerate()
        .map(|(index, line)| {
            let label = if index == 0 { stream } else { "      " };
            format!("  {label} │ {}", short(line, 160))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_native_copy_clears_its_selection() {
        let mut app = selected_draft_app();
        let id = app.view.selection_id();
        complete_copy(&mut app, id, Ok(clipboard::CopyOutcome::Confirmed), |_| {
            panic!("native success must not write to terminal")
        });
        assert!(app.view.selected_text().is_none());
        assert_eq!(app.copy_notice.as_ref().unwrap().0, "Copied to clipboard");
    }

    #[test]
    fn terminal_copy_clears_selection_only_after_the_write_succeeds() {
        let mut app = selected_draft_app();
        let id = app.view.selection_id();
        complete_copy(
            &mut app,
            id,
            Ok(clipboard::CopyOutcome::Terminal("sequence".into())),
            |_| Err(io::Error::other("write failed")),
        );
        assert_eq!(app.view.selected_text().as_deref(), Some("draft"));
        let mut output = Vec::new();
        complete_copy(
            &mut app,
            id,
            Ok(clipboard::CopyOutcome::Terminal("sequence".into())),
            |sequence| output.write_all(sequence.as_bytes()),
        );
        assert_eq!(output, b"sequence");
        assert!(app.view.selected_text().is_none());
    }

    #[test]
    fn older_copy_completion_preserves_a_newer_selection() {
        let old = selected_draft_app().view.selection_id();
        let mut app = selected_draft_app();
        complete_copy(&mut app, old, Ok(clipboard::CopyOutcome::Confirmed), |_| {
            Ok(())
        });
        assert_eq!(app.view.selected_text().as_deref(), Some("draft"));
        let id = app.view.selection_id();
        complete_copy(
            &mut app,
            id,
            Err(io::Error::other("clipboard unavailable")),
            |_| Ok(()),
        );
        assert_eq!(app.view.selected_text().as_deref(), Some("draft"));
    }

    fn selected_draft_app() -> App {
        use crossterm::event::{MouseButton, MouseEvent};
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App {
            status: Status::Working,
            ..Default::default()
        };
        app.insert_input("draft");
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal.draw(|frame| ui::render(&mut app, frame)).unwrap();
        for (kind, column) in [
            (MouseEventKind::Down(MouseButton::Left), 3),
            (MouseEventKind::Drag(MouseButton::Left), 7),
            (MouseEventKind::Up(MouseButton::Left), 7),
        ] {
            app.view.mouse(
                MouseEvent {
                    kind,
                    column,
                    row: 17,
                    modifiers: KeyModifiers::NONE,
                },
                &mut app.scroll,
            );
        }
        app
    }

    #[test]
    fn selection_keys_copy_and_clear_before_regular_input_actions() {
        let mut app = selected_draft_app();
        assert_eq!(
            selection_command(
                &mut app,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            Some(SelectionCommand::Copy("draft".into()))
        );
        assert_eq!(app.input, "draft");
        assert!(!app.should_quit);
        assert!(matches!(app.status, Status::Working));
        assert_eq!(
            selection_command(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(SelectionCommand::Clear)
        );
        assert!(matches!(app.status, Status::Working));
        assert_eq!(
            selection_command(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn language_command_persists_without_losing_draft_history_or_running_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("preferences/tui-language");
        assert_eq!(load_language(&path, Language::Ru).unwrap(), Language::Ru);
        let mut app = App::default();
        app.insert_input("unfinished draft");
        app.push_user_message("original message".into());
        show_progress(&mut app, AgentEvent::TextDelta("model text".into()));
        app.status = Status::Working;
        app.model = "model-id".into();
        app.scroll = 4;
        let cursor = app.input_cursor;
        assert!(handle_language_command(&mut app, "/language ru", &path));
        assert_eq!(app.language, Language::Ru);
        assert_eq!(load_language(&path, Language::En).unwrap(), Language::Ru);
        assert_eq!(app.input, "unfinished draft");
        assert_eq!(app.input_cursor, cursor);
        assert_eq!(app.messages[0].content, "original message");
        assert_eq!(app.messages[1].content, "model text");
        assert_eq!(app.streaming_message, Some(1));
        assert_eq!(app.scroll, 4);
        assert!(matches!(app.status, Status::Working));
        assert!(app.messages.last().unwrap().content.contains("русский"));
        for input in ["/language", "/language de", "/language ru extra"] {
            assert!(handle_language_command(&mut app, input, &path));
            assert_eq!(app.language, Language::Ru);
            assert_eq!(load_language(&path, Language::En).unwrap(), Language::Ru);
            assert_eq!(
                app.messages.last().unwrap().content,
                "Использование: /language en|ru"
            );
        }
        assert!(!handle_language_command(&mut app, "message", &path));
        assert!(handle_language_command(&mut app, "/language en", &path));
        assert_eq!(load_language(&path, Language::Ru).unwrap(), Language::En);
        app.language = Language::Ru;
        reset_session(&mut app);
        assert_eq!(app.language, Language::Ru);
        assert_eq!(app.model, "model-id");
        assert!(app.messages.is_empty());
        assert!(matches!(app.status, Status::Ready));
    }

    #[test]
    fn failed_language_save_keeps_selection_and_reports_localized_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tui-language");
        std::fs::create_dir(&path).unwrap();
        let mut app = App {
            language: Language::Ru,
            ..Default::default()
        };
        assert!(handle_language_command(&mut app, "/language en", &path));
        assert_eq!(app.language, Language::Ru);
        assert!(
            app.messages[0]
                .content
                .starts_with("Не удалось сохранить язык интерфейса:")
        );
        std::fs::write(directory.path().join("invalid"), "unsupported").unwrap();
        assert_eq!(
            load_language(&directory.path().join("invalid"), Language::Ru).unwrap(),
            Language::Ru
        );
    }

    #[test]
    fn russian_progress_translates_labels_and_preserves_details() {
        let mut app = App {
            language: Language::Ru,
            ..Default::default()
        };
        show_progress(
            &mut app,
            AgentEvent::Retrying("Request failed. Retry 2/3 in 4 sec".into()),
        );
        assert_eq!(
            app.messages[0].content,
            "Ошибка запроса. Повтор 2/3 через 4 с"
        );
        show_progress(
            &mut app,
            AgentEvent::Compaction("Compacting conversation context…".into()),
        );
        assert_eq!(app.messages[1].content, "Сжимаю контекст беседы…");
        show_progress(
            &mut app,
            AgentEvent::ToolFinished {
                call_id: "id".into(),
                failed: false,
                output: serde_json::json!({"exit_code": 1, "stderr": "unchanged error detail"}),
            },
        );
        assert_eq!(
            app.messages[2].content,
            "код выхода 1\n  stderr │ unchanged error detail"
        );
        app.update_plan(
            crate::agent::tools::TaskPlan::parse(
                serde_json::json!({"plan": [{"step": "Original model text", "status": "pending"}]}),
            )
            .unwrap(),
        );
        assert_eq!(app.messages[3].content, "План · 0/1\n○ Original model text");
    }

    #[test]
    fn session_change_keeps_current_and_queued_input_but_clears_old_history() {
        let mut app = App::default();
        app.push_user_message("old".into());
        app.push_kodrik_message("old answer".into());
        app.push_user_message("new topic".into());
        app.push_user_message("queued correction".into());
        app.input = "unfinished draft".into();
        app.context_tokens = Some(900);
        show_progress(
            &mut app,
            AgentEvent::SessionChanged {
                previous_user_messages: 1,
            },
        );
        assert_eq!(app.messages[0].content, "new topic");
        assert_eq!(app.messages[1].content, "queued correction");
        assert_eq!(app.messages.len(), 3);
        assert_eq!(app.input, "unfinished draft");
        assert_eq!(app.context_tokens, None);
    }

    #[test]
    fn retry_replaces_partial_stream_and_preserves_user_correction() {
        let mut app = App::default();
        show_progress(&mut app, AgentEvent::TextDelta("broken partial".into()));
        app.push_user_message("correction".into());
        show_progress(&mut app, AgentEvent::Retrying("Retry 1/3".into()));
        assert!(app.streaming_message.is_none());
        show_progress(&mut app, AgentEvent::TextDelta("new answer".into()));
        app.finish_stream("completed answer".into());
        assert_eq!(
            app.messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>(),
            ["Retry 1/3", "correction", "completed answer"]
        );
    }

    #[test]
    fn streamed_text_is_visible_and_completed_without_duplicates() {
        let mut app = App::default();
        show_progress(&mut app, AgentEvent::TextDelta("Проверяю ".into()));
        show_progress(&mut app, AgentEvent::TextDelta("файлы".into()));
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].content, "Проверяю файлы");
        show_progress(
            &mut app,
            AgentEvent::Message(crate::agent::AssistantMessage {
                content: "Проверяю файлы.".into(),
            }),
        );
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].content, "Проверяю файлы.");
        show_progress(&mut app, AgentEvent::TextDelta("Гото".into()));
        show_progress(&mut app, AgentEvent::TextDelta("во".into()));
        app.finish_stream("Готово!".into());
        assert_eq!(app.messages.len(), 2);
        assert_eq!(app.messages[1].content, "Готово!");
        assert!(app.streaming_message.is_none());
    }

    #[test]
    fn steering_keeps_streamed_replies_and_user_messages_separate() {
        let mut app = App {
            status: Status::Working,
            ..Default::default()
        };
        show_progress(&mut app, AgentEvent::TextDelta("Old ".into()));
        app.insert_input("correction");
        assert_eq!(
            app.update(event::KeyEvent::new(
                KeyCode::Enter,
                event::KeyModifiers::NONE
            ))
            .as_deref(),
            Some("correction")
        );
        show_progress(&mut app, AgentEvent::TextDelta("reply".into()));
        show_progress(
            &mut app,
            AgentEvent::Message(crate::agent::AssistantMessage {
                content: "Old reply".into(),
            }),
        );
        show_progress(&mut app, AgentEvent::TextDelta("Corrected ".into()));
        app.finish_stream("Corrected reply".into());
        assert_eq!(
            app.messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>(),
            ["Old reply", "correction", "Corrected reply"]
        );
        assert!(matches!(app.messages[1].role, AppMessageRole::User));
    }

    #[test]
    fn steering_discards_partial_stream_but_keeps_user_input_and_tools() {
        let mut app = App::default();
        app.push_user_message("initial".into());
        show_progress(&mut app, AgentEvent::TextDelta("obsolete".into()));
        app.push_user_message("correction".into());
        show_progress(
            &mut app,
            AgentEvent::ToolStarted {
                call_id: "tool".into(),
                name: "exec_command".into(),
                arguments: serde_json::json!({"cmd":"pwd"}),
            },
        );
        assert!(show_progress(&mut app, AgentEvent::InputConsumed));
        assert!(app.streaming_message.is_none());
        show_progress(
            &mut app,
            AgentEvent::ToolFinished {
                call_id: "tool".into(),
                failed: false,
                output: serde_json::json!({"exit_code":0}),
            },
        );
        show_progress(&mut app, AgentEvent::TextDelta("new".into()));
        app.finish_stream("corrected answer".into());
        assert_eq!(
            app.messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>(),
            [
                "initial",
                "correction",
                "exec_command: pwd — exit 0",
                "corrected answer"
            ]
        );
    }

    #[test]
    fn plan_updates_append_new_messages() {
        let mut app = App::default();
        for status in ["in_progress", "completed"] {
            if status == "completed" {
                app.push_kodrik_message("Inspection finished".into());
            }
            let arguments = serde_json::json!({"plan": [{"step": "Inspect", "status": status}]});
            show_progress(
                &mut app,
                AgentEvent::ToolStarted {
                    call_id: "plan".into(),
                    name: "update_plan".into(),
                    arguments: arguments.clone(),
                },
            );
            show_progress(
                &mut app,
                AgentEvent::PlanUpdated(crate::agent::tools::TaskPlan::parse(arguments).unwrap()),
            );
            show_progress(
                &mut app,
                AgentEvent::ToolFinished {
                    call_id: "plan".into(),
                    failed: false,
                    output: serde_json::json!("Plan updated"),
                },
            );
        }
        assert_eq!(app.messages.len(), 3);
        assert_eq!(app.messages[0].content, "Plan · 0/1\n→ Inspect");
        assert_eq!(app.messages[1].content, "Inspection finished");
        assert_eq!(app.messages[2].content, "Plan · 1/1\n✓ Inspect");
        assert!(matches!(app.messages[0].role, AppMessageRole::Plan));
        assert!(matches!(app.messages[2].role, AppMessageRole::Plan));
    }

    #[test]
    fn compact_tool_history() {
        let mut app = App::default();
        show_progress(
            &mut app,
            AgentEvent::ToolStarted {
                call_id: "1".into(),
                name: "exec_command".into(),
                arguments: serde_json::json!({"cmd": "cargo test -q", "timeout_ms": 30000}),
            },
        );
        show_progress(
            &mut app,
            AgentEvent::ToolFinished {
                call_id: "1".into(),
                failed: false,
                output: serde_json::json!({"exit_code": 1, "stdout": "first\nsecond\nthird\nfourth", "stderr": ""}),
            },
        );
        assert_eq!(app.messages.len(), 1);
        assert_eq!(
            app.messages[0].content,
            "exec_command: cargo test -q — exit 1\n  stdout │ first\n         │ second\n         │ third\n         │ fourth"
        );
        assert!(matches!(
            app.messages[0].role,
            AppMessageRole::Tool(ToolState::Failed)
        ));
    }

    #[test]
    fn tool_results_match_call_ids() {
        let mut app = App::default();
        for id in ["a", "b"] {
            assert!(!show_progress(
                &mut app,
                AgentEvent::ToolStarted {
                    call_id: id.into(),
                    name: "exec_command".into(),
                    arguments: serde_json::json!({"cmd": id}),
                }
            ));
        }
        assert!(show_progress(
            &mut app,
            AgentEvent::ToolFinished {
                call_id: "a".into(),
                failed: false,
                output: serde_json::json!({"exit_code": 0}),
            }
        ));
        assert_eq!(app.messages.len(), 2);
        assert_eq!(app.messages[0].content, "exec_command: a — exit 0");
        assert_eq!(app.messages[1].content, "exec_command: b");
        assert!(matches!(
            app.messages[0].role,
            AppMessageRole::Tool(ToolState::Success)
        ));
        assert!(matches!(
            app.messages[1].role,
            AppMessageRole::Tool(ToolState::Running)
        ));
    }

    #[test]
    fn command_output_prefers_stderr_when_both_streams_have_output() {
        let output = serde_json::json!({
            "stdout": "one\ntwo\nthree\nfour",
            "stderr": "warning\nerror"
        });
        assert_eq!(
            command_output(&output),
            "  stderr │ warning\n         │ error"
        );
    }

    #[test]
    fn command_output_limits_stderr_to_last_four_lines() {
        let output = serde_json::json!({"stderr": "one\ntwo\nthree\nfour\nfive"});
        assert_eq!(
            command_output(&output),
            "  stderr │ two\n         │ three\n         │ four\n         │ five"
        );
    }

    #[test]
    fn patch_shows_changed_files_and_line_counts() {
        let mut app = App::default();
        show_progress(
            &mut app,
            AgentEvent::ToolStarted {
                call_id: "patch".into(),
                name: "apply_patch".into(),
                arguments: serde_json::json!({"workdir": null}),
            },
        );
        show_progress(
            &mut app,
            AgentEvent::ToolFinished {
                call_id: "patch".into(),
                failed: false,
                output: serde_json::json!({"changed_files": [
                    {"path": "src/main.rs", "action": "updated", "added": 3, "removed": 2},
                    {"path": "new.txt", "action": "added", "added": 1, "removed": 0}
                ]}),
            },
        );
        assert_eq!(
            app.messages[0].content,
            "apply_patch — changed files: 2\n  src/main.rs  +3 -2\n  new.txt  +1 -0"
        );
    }
}
