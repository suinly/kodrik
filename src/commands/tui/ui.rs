use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Widget},
};

use super::app::{App, AppMessage, AppMessageRole, Status, ToolState};
use crate::agent::command_audit::CommandAuditMode;

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const USER_TEXT_COLOR: Color = Color::Reset;
pub const VIEWPORT_HEIGHT: u16 = 7;
const LOGO: [&str; 3] = [
    "█  ▄ ▄▄▄▄ ▄▄▄█ ▄▄▄▄ ▀ █  ▄",
    "█▄▀  █  █ █  █ █  ▀ █ █▄▀",
    "█ ▀▄ █▄▄█ █▄▄█ █    █ █ ▀▄",
];
// Pastel steps derived from Catppuccin Mocha Blue (#89b4fa).
const LOGO_COLORS: [Color; 3] = [
    Color::Rgb(194, 209, 235),
    Color::Rgb(124, 158, 212),
    Color::Rgb(78, 124, 197),
];

pub fn logo_height(width: u16) -> u16 {
    // Reserve one column on the left and one blank row below the logo.
    if width.saturating_sub(1) >= LOGO[0].chars().count() as u16 {
        LOGO.len() as u16 + 1
    } else {
        2
    }
}

pub fn render_logo(buf: &mut Buffer) {
    let area = Rect {
        x: buf.area.x.saturating_add(1),
        width: buf.area.width.saturating_sub(1),
        height: buf.area.height.saturating_sub(1),
        ..buf.area
    };
    if area.is_empty() {
        return;
    }
    if area.width >= LOGO[0].chars().count() as u16 {
        let lines = LOGO
            .iter()
            .zip(LOGO_COLORS)
            .map(|(line, color)| Line::styled(*line, Style::default().fg(color).bold()))
            .collect::<Vec<_>>();
        Paragraph::new(lines).render(area, buf);
    } else {
        Paragraph::new("kodrik")
            .style(Style::default().fg(LOGO_COLORS[0]).bold())
            .render(
                Rect::new(area.x, area.y + area.height / 2, area.width, 1),
                buf,
            );
    }
}

pub fn message_height(message: &AppMessage, paragraph: &Paragraph<'_>, width: u16) -> u16 {
    paragraph
        .line_count(width.saturating_sub(3).max(1))
        .saturating_add(1 + usize::from(matches!(message.role, AppMessageRole::User)))
        .min(u16::MAX as usize) as u16
}

pub fn render_message(message: &AppMessage, paragraph: &Paragraph<'_>, buf: &mut Buffer) {
    let area = buf.area;
    let (icon, color, bg) = match message.role {
        AppMessageRole::User => (">", USER_TEXT_COLOR, Color::Rgb(54, 58, 79)),
        AppMessageRole::Approval => ("!", Color::Yellow, Color::default()),
        AppMessageRole::Kodrik => ("●", Color::White, Color::default()),
        AppMessageRole::Tool(ToolState::Running) => ("○", Color::Yellow, Color::default()),
        AppMessageRole::Tool(ToolState::Success) => ("●", Color::Green, Color::default()),
        AppMessageRole::Tool(ToolState::Failed) => ("●", Color::Red, Color::default()),
    };

    Block::new()
        .style(Style::default().bg(bg))
        .render(area, buf);
    Paragraph::new(format!(" {icon}"))
        .style(Style::default().fg(color).bold())
        .render(Rect::new(area.x, area.y + 1, 3, 1), buf);

    let bottom_padding = u16::from(matches!(message.role, AppMessageRole::User));
    paragraph.render(
        Rect::new(
            area.x + 3,
            area.y + 1,
            area.width.saturating_sub(3),
            area.height.saturating_sub(1 + bottom_padding),
        ),
        buf,
    );
}

pub fn render(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    let language = app.language;
    if app.remote_grant.is_some() {
        render_remote_grant(app, frame);
        return;
    }
    let has_picker =
        app.approval.is_some() || app.audit_picker.is_some() || app.language_choice.is_some();
    if let Some(sessions) = &app.sessions {
        app.view
            .prepare(&app.messages, area, 0, &mut app.scroll, true);
        let items = sessions
            .iter()
            .map(|(id, title)| ListItem::new(format!("{title}  · {}", id.as_str())))
            .collect::<Vec<_>>();
        let mut block = Block::bordered().title(language.pick(
            " Sessions · ↑/↓ select · Enter open · Esc close ",
            " Сеансы · ↑/↓ выбор · Enter открыть · Esc закрыть ",
        ));
        let mut notices = Vec::new();
        if app.sessions_loading {
            notices.push(format!(
                "{} {}",
                FRAMES[app.animation_frame % FRAMES.len()],
                language.pick("Loading sessions…", "Загружаю сеансы…")
            ));
        }
        if let Some(error) = &app.sessions_error {
            notices.push(format!(
                "{} · {}: {}",
                language.pick("Could not load sessions", "Не удалось загрузить сеансы"),
                language.pick("r retry", "r повторить"),
                literal_text(error).replace('\n', " ")
            ));
        } else if app.sessions_more {
            notices.push(
                language
                    .pick("More sessions available", "Есть ещё сеансы")
                    .to_owned(),
            );
        }
        if !notices.is_empty() {
            block = block.title_bottom(format!(" {} ", notices.join(" · ")));
        }
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if items.is_empty() {
            frame.render_widget(
                Paragraph::new(if app.sessions_loading {
                    notices[0].as_str()
                } else if app.sessions_error.is_some() {
                    language.pick(
                        "Could not load sessions · r retry",
                        "Не удалось загрузить сеансы · r повторить",
                    )
                } else {
                    language.pick("No saved sessions", "Нет сохранённых сеансов")
                }),
                inner,
            );
        } else {
            frame.render_stateful_widget(
                List::new(items).highlight_symbol("> ").highlight_style(
                    Style::default()
                        .fg(Color::Yellow)
                        .bg(Color::Rgb(54, 58, 79)),
                ),
                inner,
                &mut ListState::default().with_selected(Some(app.session_index)),
            );
        }
        app.view.restore_footer(frame.buffer_mut());
        app.view.finish_frame(frame.buffer_mut(), None);
        return;
    }
    app.input_width = usize::from(area.width.saturating_sub(3).max(1));
    let (input_rows, (cursor_x, cursor_y)) = app.input_layout(app.input_width);
    let input_lines = if app.audit_picker.is_some() {
        5
    } else if app.language_choice.is_some() {
        2
    } else if has_picker {
        3
    } else if app.navigation.selected.is_some() {
        1
    } else {
        input_rows.len().min(6) as u16
    };
    let command_height = (app.command_suggestions().len() as u16 + 3).max(3);
    let footer_height = VIEWPORT_HEIGHT + input_lines - 1 + command_height - 3;
    let plan_width = area.width.saturating_sub(3).max(1);
    let available_plan_height = area.height.saturating_sub(footer_height + 2);
    let mut plan = Paragraph::new(app.plan.as_deref().unwrap_or_default())
        .style(Style::default().fg(Color::Cyan))
        .wrap(ratatui::widgets::Wrap { trim: false });
    if plan.line_count(plan_width) > usize::from(available_plan_height)
        && let Some(content) = &app.plan
    {
        let heading = content.lines().next().unwrap_or_default();
        let current = content.lines().find(|line| line.starts_with("→ "));
        plan = Paragraph::new(
            current.map_or_else(|| heading.to_owned(), |step| format!("{heading} · {step}")),
        )
        .style(Style::default().fg(Color::Cyan))
        .wrap(ratatui::widgets::Wrap { trim: false });
    }
    let plan_height = if app.plan.is_some() && available_plan_height > 0 {
        plan.line_count(plan_width)
            .min(usize::from(available_plan_height)) as u16
            + 1
    } else {
        0
    };
    let history_height = area.height.saturating_sub(footer_height + plan_height);
    app.view
        .prepare(&app.messages, area, history_height, &mut app.scroll, false);
    if let Some(index) = app.navigation.focus.take() {
        app.view.focus_message(index, &mut app.scroll);
    }
    app.view
        .render_history(frame.buffer_mut(), app.navigation.selected);
    let footer = Rect::new(
        area.x,
        area.y + history_height,
        area.width,
        area.height - history_height,
    );
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![
            Constraint::Length(plan_height),
            Constraint::Length(command_height),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(footer);
    if layout[0].height > 1 {
        frame.render_widget(
            Paragraph::new(" ≡").style(Style::default().fg(Color::Cyan).bold()),
            Rect::new(layout[0].x, layout[0].y + 1, layout[0].width.min(3), 1),
        );
    }
    frame.render_widget(
        plan,
        Rect::new(
            layout[0].x.saturating_add(3),
            layout[0].y.saturating_add(1),
            layout[0].width.saturating_sub(3),
            layout[0].height.saturating_sub(1),
        ),
    );
    let layout = &layout[1..];

    let (input, input_color) = if app.navigation.selected.is_some() {
        (
            vec![Line::raw(language.pick(
                "Press Enter to edit the selected message",
                "Нажмите Enter для редактирования сообщения",
            ))],
            Color::DarkGray,
        )
    } else if app.input.is_empty() {
        (
            vec![Line::raw(language.pick(
                "Ask Kodrik to do anything",
                "Попросите Kodrik о чём угодно",
            ))],
            Color::DarkGray,
        )
    } else {
        (
            input_rows
                .iter()
                .map(|row| Line::raw(&app.input[row.clone()]))
                .collect(),
            USER_TEXT_COLOR,
        )
    };

    let mut working_state = language.pick("Working", "Работаю").to_owned();
    if app.compacting {
        working_state = language
            .pick("Compacting context", "Сжимаю контекст")
            .to_owned();
        if let Some(progress) = &app.compaction_progress {
            let elapsed = (chrono::Utc::now() - progress.started_at)
                .num_seconds()
                .max(0);
            working_state.push_str(&format!(
                " · {}% · {}/{} · {}:{:02}",
                progress.percent(),
                progress.completed_chunks,
                progress.total_chunks,
                elapsed / 60,
                elapsed % 60,
            ));
        }
    }
    working_state.push_str(language.pick(" · Esc to stop", " · Esc для остановки"));

    let (animation_frame, state, done_at) = if app.audit_picker.is_some() {
        (
            "◇",
            language.pick(
                "Tool mode · ↑/↓ or j/k select · Enter apply · Esc close",
                "Режим проверок · ↑/↓ или j/k выбор · Enter применить · Esc закрыть",
            ),
            String::new(),
        )
    } else if app.language_choice.is_some() {
        (
            "◇",
            language.pick(
                "Interface language · ↑/↓ select · Enter apply · Esc close",
                "Язык интерфейса · ↑/↓ выбор · Enter применить · Esc закрыть",
            ),
            String::new(),
        )
    } else if app.approval.is_some() {
        (
            "!",
            language.pick(
                "Approve command · ↑/↓ or j/k select · Enter confirm · Esc reject",
                "Подтверждение · ↑/↓ или j/k выбор · Enter подтвердить · Esc отклонить",
            ),
            String::new(),
        )
    } else if app.navigation.selected.is_some() {
        (
            "↶",
            if app.selected_versions().is_some() {
                language.pick(
                    "Message · ↑/↓ j/k · ←/→ h/l versions · Enter edit · Esc close",
                    "Сообщение · ↑/↓ j/k · ←/→ h/l версии · Enter изменить · Esc закрыть",
                )
            } else {
                language.pick(
                    "Message · ↑/↓ or j/k select · Enter edit · Esc close",
                    "Сообщение · ↑/↓ или j/k выбор · Enter изменить · Esc закрыть",
                )
            },
            app.selected_versions()
                .map_or_else(String::new, |versions| {
                    format!(" · {}/{}", versions.current + 1, versions.ids.len())
                }),
        )
    } else if app.navigation.editing.is_some() {
        (
            "✎",
            language.pick(
                "Editing message · Enter create branch · Esc cancel",
                "Редактирование · Enter создать ветвь · Esc отменить",
            ),
            String::new(),
        )
    } else {
        match &app.status {
            Status::Ready => ("○", language.pick("Ready", "Готов"), String::new()),
            Status::Recovering => (
                FRAMES[app.animation_frame % FRAMES.len()],
                language.pick(
                    "Recovering session · Esc to stop",
                    "Восстанавливаю сеанс · Esc для остановки",
                ),
                String::new(),
            ),
            Status::Stopping => (
                FRAMES[app.animation_frame % FRAMES.len()],
                language.pick("Stopping", "Остановка"),
                String::new(),
            ),
            Status::Stopped => ("■", language.pick("Stopped", "Остановлено"), String::new()),
            Status::Working => (
                FRAMES[app.animation_frame % FRAMES.len()],
                working_state.as_str(),
                String::new(),
            ),
            Status::Done(time) => ("✓", language.pick("Done", "Готово"), format!(" {time}")),
            Status::Error => ("✗", language.pick("Error", "Ошибка"), String::new()),
        }
    };

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(format!(" {animation_frame} {state}")),
            Span::styled(
                done_at,
                Style::default().fg(if app.selected_versions().is_some() {
                    Color::Cyan
                } else {
                    Color::DarkGray
                }),
            ),
        ]))
        .style(Style::default().fg(Color::DarkGray)),
        Rect::new(layout[0].x, layout[0].y + 1, layout[0].width, 1),
    );

    let commands = app.command_suggestions();
    if !commands.is_empty() {
        let selected = app.command_suggestion().unwrap().0;
        let mut lines = commands
            .iter()
            .map(|(command, description)| {
                Line::styled(
                    format!(
                        " {} {command}  {description}",
                        if *command == selected { ">" } else { " " }
                    ),
                    Style::default().fg(if *command == selected {
                        Color::Yellow
                    } else {
                        Color::White
                    }),
                )
            })
            .collect::<Vec<_>>();
        lines.insert(0, Line::default());
        lines.push(Line::styled(
            language.pick(
                "   ↑/↓ · Enter — run · Tab — complete · Esc — close",
                "   ↑/↓ · Enter — запуск · Tab — дополнить · Esc — закрыть",
            ),
            Style::default().fg(Color::Gray),
        ));
        let height = (lines.len() as u16).min(layout[0].height.saturating_sub(1));
        frame.render_widget(Clear, layout[0]);
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(Color::Rgb(54, 58, 79))),
            Rect::new(
                layout[0].x,
                layout[0].bottom().saturating_sub(height),
                layout[0].width,
                height,
            ),
        );
    }

    if !has_picker {
        frame.render_widget(
            Paragraph::new(vec![
                Line::default(),
                Line::styled(" > ", Style::default().fg(Color::White).bold()),
            ])
            .style(Style::default().bg(Color::Rgb(54, 58, 79))),
            layout[1],
        );
    }

    let input_area = Rect::new(
        layout[1].x.saturating_add(3),
        layout[1].y.saturating_add(1),
        layout[1].width.saturating_sub(3),
        layout[1].height.saturating_sub(2),
    );
    let cursor_x = cursor_x.min(u16::MAX as usize) as u16;
    let cursor_y = cursor_y.min(u16::MAX as usize) as u16;
    let scroll_y = cursor_y.saturating_sub(input_area.height.saturating_sub(1));
    if has_picker {
        frame.render_widget(
            Block::new().style(Style::default().bg(Color::Rgb(54, 58, 79))),
            layout[1],
        );
        if app.audit_picker.is_some() {
            frame.render_widget(
                Paragraph::new(language.pick("Command review mode", "Режим проверки команд"))
                    .style(Style::default().fg(Color::White).bold()),
                Rect::new(
                    layout[1].x.saturating_add(1),
                    layout[1].y.saturating_add(1),
                    layout[1].width.saturating_sub(1),
                    1,
                ),
            );
        }
        let selection_style = Style::default().fg(Color::Yellow).bold();
        let (items, selected, highlight_style) = if let Some(picker) = &app.audit_picker {
            let label_width = super::approvals::AUDIT_MODES
                .iter()
                .map(|mode| Span::raw(mode.label(language)).width())
                .max()
                .unwrap_or(0);
            let items = super::approvals::AUDIT_MODES
                .iter()
                .enumerate()
                .map(|(index, mode)| {
                    let description = match mode {
                        CommandAuditMode::Trusted => language.pick(
                            "Run tool calls without review",
                            "Выполнять действия без проверок",
                        ),
                        CommandAuditMode::Review => language.pick(
                            "Ask before risky or unclear actions",
                            "Подтверждать рискованные или неясные действия",
                        ),
                        CommandAuditMode::Ask => {
                            language.pick("Approve every tool call", "Подтверждать каждое действие")
                        }
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!("{:<label_width$}  ", mode.label(language)),
                            if index == picker.choice {
                                selection_style
                            } else {
                                Style::default().fg(Color::White)
                            },
                        ),
                        Span::styled(description, Style::default().fg(Color::Gray).dim()),
                    ]))
                })
                .collect::<Vec<_>>();
            (items, Some(picker.choice), Style::default())
        } else if let Some(choice) = app.language_choice {
            (
                ["English", "Русский"]
                    .into_iter()
                    .map(ListItem::new)
                    .collect(),
                Some(choice),
                selection_style,
            )
        } else {
            (
                [
                    language.pick("Allow once", "Разрешить один раз"),
                    language.pick("Reject", "Отклонить"),
                    language.pick(
                        "No checks for this session",
                        "Без проверок для этого сеанса",
                    ),
                ]
                .into_iter()
                .map(ListItem::new)
                .collect(),
                app.approval_choice,
                selection_style,
            )
        };
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol(Span::styled("> ", selection_style))
                .highlight_style(highlight_style),
            Rect::new(
                layout[1].x.saturating_add(1),
                layout[1]
                    .y
                    .saturating_add(if app.audit_picker.is_some() { 3 } else { 1 }),
                layout[1].width.saturating_sub(1),
                layout[1]
                    .height
                    .saturating_sub(if app.audit_picker.is_some() { 4 } else { 2 }),
            ),
            &mut ListState::default().with_selected(selected),
        );
    } else {
        frame.render_widget(
            Paragraph::new(input)
                .style(Style::default().fg(input_color))
                .scroll((scroll_y, 0)),
            input_area,
        );
    }

    let footer = Rect::new(
        layout[2].x.saturating_add(1),
        layout[2].y,
        layout[2].width.saturating_sub(1),
        layout[2].height,
    );
    render_footer(app, frame, footer);

    app.view.restore_footer(frame.buffer_mut());
    app.view.finish_frame(
        frame.buffer_mut(),
        (!has_picker && app.navigation.selected.is_none()).then_some(super::view::InputRows {
            text: &app.input,
            rows: &input_rows,
            area: input_area,
            scroll: scroll_y as usize,
        }),
    );

    if !has_picker
        && app.navigation.selected.is_none()
        && input_area.width > 0
        && input_area.height > 0
    {
        frame.set_cursor_position((
            input_area.x + cursor_x.min(input_area.width - 1),
            input_area.y + cursor_y.saturating_sub(scroll_y),
        ));
    }
}

fn literal_text(text: &str) -> String {
    text.chars()
        .flat_map(|ch| {
            if matches!(ch, '\n' | '\'' | '"' | '\\') {
                ch.to_string()
            } else {
                ch.escape_debug().to_string()
            }
            .chars()
            .collect::<Vec<_>>()
        })
        .collect()
}

fn render_remote_grant(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    app.view.clear_selection();
    app.view
        .prepare(&app.messages, area, 0, &mut app.scroll, true);
    let layout = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(4),
        Constraint::Length(1),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(app.language.pick(
            " Local environment access · confirmation required",
            " Доступ к локальному окружению · требуется подтверждение",
        ))
        .style(Style::default().fg(Color::Yellow).bold()),
        layout[0],
    );
    let grant = app.remote_grant.as_ref().unwrap();
    let details =
        Paragraph::new(literal_text(&grant.details)).wrap(ratatui::widgets::Wrap { trim: false });
    let max_scroll = details
        .line_count(layout[1].width.max(1))
        .saturating_sub(usize::from(layout[1].height));
    app.remote_grant_scroll = app
        .remote_grant_scroll
        .min(max_scroll)
        .min(u16::MAX as usize);
    frame.render_widget(
        details.scroll((app.remote_grant_scroll as u16, 0)),
        layout[1],
    );
    frame.render_widget(
        Paragraph::new(app.language.pick(
            " ↑/↓ or j/k select · Enter confirm · Esc reject · PgUp/PgDn scroll",
            " ↑/↓ или j/k выбор · Enter подтвердить · Esc отказ · PgUp/PgDn текст",
        ))
        .style(Style::default().fg(Color::DarkGray)),
        layout[2],
    );
    let selection_style = Style::default().fg(Color::Yellow).bold();
    frame.render_widget(
        Block::new().style(Style::default().bg(Color::Rgb(54, 58, 79))),
        layout[3],
    );
    frame.render_stateful_widget(
        List::new([
            ListItem::new(app.language.pick("Allow", "Разрешить")),
            ListItem::new(app.language.pick("Reject", "Отклонить")),
        ])
        .highlight_symbol(Span::styled("> ", selection_style))
        .highlight_style(selection_style),
        Rect::new(
            layout[3].x.saturating_add(1),
            layout[3].y.saturating_add(1),
            layout[3].width.saturating_sub(1),
            layout[3].height.saturating_sub(2),
        ),
        &mut ListState::default().with_selected(Some(grant.choice.min(1))),
    );
    render_footer(
        app,
        frame,
        Rect::new(
            layout[4].x.saturating_add(1),
            layout[4].y,
            layout[4].width.saturating_sub(1),
            layout[4].height,
        ),
    );
    app.view.finish_frame(frame.buffer_mut(), None);
}

fn render_footer(app: &App, frame: &mut Frame, footer: Rect) {
    let language = app.language;
    let (mode_icon, mode_color) = match app.audit_mode {
        CommandAuditMode::Trusted => ("◈", Color::Yellow),
        CommandAuditMode::Review => ("◇", Color::Cyan),
        CommandAuditMode::Ask => ("◆", Color::Green),
    };
    let audit = format!("{mode_icon} {}", app.audit_mode.label(language));
    if let Some(connection) = app.remote_connection {
        let model = app
            .model
            .split_once('/')
            .map_or(app.model.as_str(), |(_, model)| model);
        let mut parts = vec![audit.clone(), literal_text(model)];
        if let Some(project) = &app.remote_environment {
            parts.push(literal_text(project));
        }
        parts.push(connection.into());
        if let Some(tokens) = app.context_tokens {
            parts.push(format!("{}K", tokens / 1000));
        }
        let width = usize::from(footer.width);
        let notice_width = app
            .copy_notice
            .as_ref()
            .map_or(0, |(notice, _)| Span::raw(notice.as_str()).width() + 1);
        let mandatory_width = Span::raw(format!("{audit} · {connection}")).width();
        let metadata_width = if mandatory_width + notice_width <= width {
            width.saturating_sub(notice_width)
        } else {
            width
        };
        // Audit and connection take priority over notices; notices take priority over metadata.
        if Span::raw(parts.join(" · ")).width() > metadata_width && app.context_tokens.is_some() {
            parts.pop();
        }
        if Span::raw(parts.join(" · ")).width() > metadata_width && app.remote_environment.is_some()
        {
            parts.remove(2);
        }
        if Span::raw(parts.join(" · ")).width() > metadata_width {
            parts.remove(1);
        }
        if Span::raw(parts.join(" · ")).width() > width {
            parts[0] = mode_icon.into();
        }
        let text_width = Span::raw(parts.join(" · ")).width();
        let mut spans = vec![Span::styled(
            parts.remove(0),
            Style::default().fg(mode_color),
        )];
        spans.push(Span::styled(
            format!(" · {}", parts.join(" · ")),
            Style::default().fg(Color::Yellow),
        ));
        frame.render_widget(Paragraph::new(Line::from(spans)), footer);
        if let Some((notice, _)) = &app.copy_notice {
            let notice_width = Span::raw(notice.as_str()).width();
            if text_width + 1 + notice_width <= width {
                frame.render_widget(
                    Paragraph::new(notice.as_str())
                        .right_aligned()
                        .style(Style::default().fg(Color::Gray)),
                    Rect::new(
                        footer.x + text_width as u16 + 1,
                        footer.y,
                        footer.width.saturating_sub(text_width as u16 + 1),
                        footer.height,
                    ),
                );
            }
        }
        return;
    }
    let notice_width = app.copy_notice.as_ref().map_or(0, |(notice, _)| {
        Span::raw(notice.as_str())
            .width()
            .min(usize::from(footer.width)) as u16
    });
    let model_width = footer
        .width
        .saturating_sub(notice_width)
        .saturating_sub(u16::from(notice_width > 0));
    let model = match app.context_tokens {
        Some(tokens) => format!("{} · {}K", app.model, tokens / 1000),
        None => app.model.clone(),
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("{mode_icon} {}", app.audit_mode.label(language)),
                Style::default().fg(mode_color),
            ),
            Span::styled(format!(" · {model}"), Style::default().fg(Color::Yellow)),
        ])),
        Rect::new(footer.x, footer.y, model_width, footer.height),
    );
    if let Some((notice, _)) = &app.copy_notice {
        frame.render_widget(
            Paragraph::new(notice.as_str())
                .right_aligned()
                .style(Style::default().fg(Color::Gray)),
            Rect::new(
                footer.right().saturating_sub(notice_width),
                footer.y,
                notice_width,
                footer.height,
            ),
        );
    }
}

pub fn message_text(message: &AppMessage) -> Text<'static> {
    let source = message.content.replace('\t', "    ");
    if matches!(message.role, AppMessageRole::Tool(_)) {
        let mut lines = source.lines();
        let mut text = vec![Line::raw(lines.next().unwrap_or_default().to_owned())];
        text.extend(lines.map(|line| {
            if message.content.starts_with("apply_patch")
                && let Some((path, counts)) = line.rsplit_once("  +")
                && let Some((added, removed)) = counts.split_once(" -")
                && added.bytes().all(|byte| byte.is_ascii_digit())
                && !added.is_empty()
                && removed.bytes().all(|byte| byte.is_ascii_digit())
                && !removed.is_empty()
            {
                Line::from(vec![
                    Span::styled(format!("{path}  "), Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("+{added}"),
                        Style::default().fg(Color::Rgb(105, 160, 115)),
                    ),
                    Span::styled(
                        format!(" -{removed}"),
                        Style::default().fg(Color::Rgb(180, 110, 110)),
                    ),
                ])
            } else {
                Line::styled(line.to_owned(), Style::default().fg(Color::DarkGray))
            }
        }));
        ratatui::text::Text::from(text)
    } else if matches!(message.role, AppMessageRole::Approval) {
        Text::from(
            source
                .lines()
                .enumerate()
                .map(|(index, line)| {
                    let style = if index == 0 {
                        Style::default().fg(Color::Yellow).bold()
                    } else if line.starts_with("  ") {
                        Style::default().fg(Color::White)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    Line::styled(line.to_owned(), style)
                })
                .collect::<Vec<_>>(),
        )
    } else {
        super::markdown::render(&source, matches!(message.role, AppMessageRole::User))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn remote_footer_shortens_provider_model_prefix() {
        let mut app = App {
            model: "openai/gpt-5.4".into(),
            remote_environment: Some("kodrik".into()),
            remote_connection: Some("connected"),
            ..Default::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let row = (0..80)
            .map(|x| terminal.backend().buffer()[(x, 23)].symbol())
            .collect::<String>();
        assert!(row.contains("gpt-5.4 · kodrik · connected"), "{row}");
        assert!(!row.contains("openai/"), "{row}");
    }

    #[test]
    fn remote_footer_keeps_connection_visible_before_project_model_and_notice() {
        for connection in ["connected", "disconnected", "detached"] {
            for width in [24, 28, 40, 80] {
                let mut app = App {
                    model: "provider/an-extremely-long-model-name".into(),
                    remote_environment: Some("a-very-long-project-name".into()),
                    remote_connection: Some(connection),
                    context_tokens: Some(12_000),
                    ..Default::default()
                };
                app.set_copy_notice("Copied something very long".into());
                let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
                terminal.draw(|frame| render(&mut app, frame)).unwrap();
                let row = (0..width)
                    .map(|x| terminal.backend().buffer()[(x, 23)].symbol())
                    .collect::<String>();
                assert!(row.contains(connection), "width {width}: {row}");
            }
        }
    }

    #[test]
    fn remote_footer_reserves_copy_notice_before_optional_metadata() {
        let mut app = App {
            model: "provider/server-test-model".into(),
            remote_environment: Some("temporary-project".into()),
            remote_connection: Some("connected"),
            context_tokens: Some(12_000),
            ..Default::default()
        };
        app.set_copy_notice("Copy requested from terminal".into());
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let row = (0..80)
            .map(|x| terminal.backend().buffer()[(x, 23)].symbol())
            .collect::<String>();
        assert!(row.contains("AI review"), "{row}");
        assert!(row.contains("connected"), "{row}");
        assert!(row.ends_with("Copy requested from terminal"), "{row}");
    }

    #[test]
    fn remote_grant_disclosure_is_literal_scrollable_and_separate_from_server_approvals() {
        let mut app = App {
            remote_grant: Some(super::super::app::RemoteGrant {
                details: format!(
                    "**literal**\nunsafe\u{1b}[31m\u{202e}\n{}\nLAST DISCLOSURE",
                    "detail\n".repeat(60)
                ),
                choice: 1,
            }),
            input: "do not show this draft".into(),
            ..Default::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("**literal**"));
        assert!(text.contains("unsafe\\u{1b}[31m\\u{202e}"), "{text}");
        assert!(text.contains("> Reject"));
        assert!(!text.contains("do not show this draft"));
        assert!(!text.contains("No checks for this session"));
        assert!(!terminal.backend().cursor_visible());
        for _ in 0..10 {
            app.update(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::PageDown,
                crossterm::event::KeyModifiers::NONE,
            ));
            terminal.draw(|frame| render(&mut app, frame)).unwrap();
        }
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("LAST DISCLOSURE"));
        for width in [1, 12, 24] {
            let mut narrow = Terminal::new(TestBackend::new(width, 8)).unwrap();
            narrow.draw(|frame| render(&mut app, frame)).unwrap();
        }
    }

    #[test]
    fn compact_logo_keeps_its_gradient_and_bottom_spacing() {
        let width = LOGO[0].chars().count() as u16 + 1;
        assert_eq!(logo_height(width), 4);
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, logo_height(width)));
        render_logo(&mut buffer);
        for (y, line) in LOGO.iter().enumerate() {
            let actual = (0..width)
                .map(|x| buffer[(x, y as u16)].symbol())
                .collect::<String>();
            assert_eq!(actual.trim_end(), format!(" {line}"));
            assert_eq!(buffer[(1, y as u16)].fg, LOGO_COLORS[y]);
        }
        assert!((0..width).all(|x| buffer[(x, 3)].symbol() == " "));
    }

    #[test]
    fn narrow_logo_uses_only_two_rows() {
        let width = LOGO[0].chars().count() as u16;
        assert_eq!(logo_height(width), 2);
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, logo_height(width)));
        render_logo(&mut buffer);
        let first_row = (0..width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect::<String>();
        assert_eq!(first_row.trim_end(), " kodrik");
        assert!((0..width).all(|x| buffer[(x, 1)].symbol() == " "));
    }

    #[test]
    fn logo_handles_empty_and_tiny_buffers() {
        for width in 0..=2 {
            for height in 0..=2 {
                let mut buffer = Buffer::empty(Rect::new(3, 5, width, height));
                render_logo(&mut buffer);
            }
        }
    }

    #[test]
    fn pasted_user_text_keeps_literal_indentation_and_line_breaks() {
        let message = AppMessage {
            role: AppMessageRole::User,
            content: "  first line\n    second line\n\n# literal heading\n*literal text*  \n"
                .into(),
        };
        let rendered = message_text(&message);
        assert_eq!(
            rendered
                .lines
                .iter()
                .map(|line| line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>())
                .collect::<Vec<_>>()
                .join("\n"),
            message.content
        );
    }

    #[test]
    fn fenced_code_is_highlighted_without_fences_or_a_background() {
        let code = "fn main() {\n    let text = \"hello\"; // comment\n}";
        let luminance = |color: (u8, u8, u8)| {
            let component = |value: u8| {
                let value = f64::from(value) / 255.0;
                if value <= 0.04045 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * component(color.0) + 0.7152 * component(color.1) + 0.0722 * component(color.2)
        };
        for role in [AppMessageRole::Kodrik, AppMessageRole::User] {
            let rendered = message_text(&AppMessage {
                role,
                content: format!("```rust\n{code}\n```"),
            });
            assert_eq!(
                rendered
                    .lines
                    .iter()
                    .map(|line| line
                        .spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>())
                    .collect::<Vec<_>>()
                    .join("\n"),
                code
            );
            let colors = rendered
                .lines
                .iter()
                .flat_map(|line| &line.spans)
                .filter_map(|span| {
                    assert!(span.style.bg.is_none_or(|color| color == Color::Reset));
                    if let Some(Color::Rgb(r, g, b)) = span.style.fg {
                        Some((r, g, b))
                    } else {
                        None
                    }
                })
                .collect::<std::collections::HashSet<_>>();
            assert!(colors.len() >= 3, "code should have distinct syntax colors");
            for color in colors {
                assert!(
                    (luminance(color) + 0.05) / (luminance((54, 58, 79)) + 0.05) >= 4.5,
                    "low-contrast syntax color: {color:?}"
                );
            }
        }
        for source in [
            "```unknown-language\n  literal text\n```",
            "```\n  literal text\n```",
            "```rust\n  literal text",
        ] {
            let text = message_text(&AppMessage {
                role: AppMessageRole::Kodrik,
                content: source.into(),
            });
            let rendered = text
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
            assert_eq!(rendered, "  literal text");
        }
    }

    #[test]
    fn message_selection_does_not_highlight_the_separator_above_the_bubble() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::default();
        app.push_user_message("selected message".into());
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        for _ in 0..2 {
            app.update(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        }
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let y = (0..24)
            .find(|y| {
                (0..80)
                    .map(|x| buffer[(x, *y)].symbol())
                    .collect::<String>()
                    .contains("selected message")
            })
            .unwrap();
        assert_eq!(buffer[(0, y - 2)].bg, Color::Reset);
        assert_eq!(buffer[(0, y - 1)].bg, Color::Rgb(69, 71, 90));
        assert_eq!(buffer[(3, y)].bg, Color::Rgb(69, 71, 90));
    }

    #[test]
    fn message_navigation_focuses_the_selected_turn_and_hides_the_cursor() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::default();
        for index in 0..30 {
            app.push_user_message(format!("question {index:02}"));
            app.push_kodrik_message(format!("answer {index:02}"));
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        for code in [KeyCode::Esc, KeyCode::Esc]
            .into_iter()
            .chain(std::iter::repeat_n(KeyCode::Char('k'), 29))
        {
            app.update(KeyEvent::new(code, KeyModifiers::NONE));
        }
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..24)
            .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let y = rows
            .iter()
            .position(|row| row.contains("question 00"))
            .unwrap();
        assert_eq!(buffer[(3, y as u16)].bg, Color::Rgb(69, 71, 90));
        assert!(!terminal.backend().cursor_visible());
        app.update(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert!(terminal.backend().cursor_visible());
    }

    #[test]
    fn copy_notice_is_at_the_bottom_right_with_model_and_tokens_on_the_left() {
        let mut app = App {
            model: "test-model".into(),
            context_tokens: Some(12_000),
            audit_mode: crate::agent::command_audit::CommandAuditMode::Trusted,
            audit_session: Some(crate::agent::session::SessionId::new("current-session")),
            ..Default::default()
        };
        app.set_copy_notice("Copied".into());
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>();
        assert!(row(23).starts_with(" ◈ No checks · test-model · 12K"));
        assert_eq!(buffer[(1, 23)].fg, Color::Yellow);
        assert!(row(0).trim().is_empty());
        let notice = (74..80)
            .map(|x| buffer[(x, 23)].symbol())
            .collect::<String>();
        assert_eq!(notice, "Copied");
        assert!(!row(22).contains("Copied"));
        for (mode, label, color) in [
            (
                crate::agent::command_audit::CommandAuditMode::Review,
                " ◇ AI review · test-model · 12K",
                Color::Cyan,
            ),
            (
                crate::agent::command_audit::CommandAuditMode::Ask,
                " ◆ Always ask · test-model · 12K",
                Color::Green,
            ),
        ] {
            app.audit_mode = mode;
            terminal.draw(|frame| render(&mut app, frame)).unwrap();
            let buffer = terminal.backend().buffer();
            let row: String = (0..80).map(|x| buffer[(x, 23)].symbol()).collect();
            assert!(row.starts_with(label));
            assert_eq!(buffer[(1, 23)].fg, color);
        }
        for width in [1, 3, 6, 12] {
            let mut small = Terminal::new(TestBackend::new(width, 8)).unwrap();
            small.draw(|frame| render(&mut app, frame)).unwrap();
        }
    }

    #[test]
    fn history_scroll_stops_at_the_beginning() {
        let mut app = App::default();
        app.push_kodrik_message("short answer".into());
        app.scroll = usize::MAX;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert_eq!(app.scroll, 0);
        let buffer = terminal.backend().buffer();
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("short answer"));
    }

    #[test]
    fn reading_history_keeps_its_position_when_output_grows() {
        let mut app = App::default();
        for index in 0..40 {
            app.push_kodrik_message(format!("line {index:02}"));
        }
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        app.scroll = 20;
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let before = terminal.backend().buffer().content[..40 * 13].to_vec();
        app.push_kodrik_message("new answer".into());
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert_eq!(&terminal.backend().buffer().content[..40 * 13], before);
    }

    #[test]
    fn compaction_replaces_working_without_moving_the_input() {
        for language in [crate::i18n::Language::En, crate::i18n::Language::Ru] {
            let mut app = App {
                language,
                status: Status::Working,
                ..Default::default()
            };
            let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
            let lines = |terminal: &Terminal<TestBackend>| {
                terminal
                    .backend()
                    .buffer()
                    .content
                    .chunks(90)
                    .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
                    .collect::<Vec<_>>()
            };
            terminal.draw(|frame| render(&mut app, frame)).unwrap();
            let before = lines(&terminal);
            let working = language.pick("Working", "Работаю");
            let status_row = before
                .iter()
                .position(|line| line.contains(working))
                .unwrap();
            let input_row = before.iter().position(|line| line.contains(" > ")).unwrap();
            app.compacting = true;
            app.compaction_progress = Some(crate::agent::CompactionProgress {
                completed_tokens: 400,
                total_tokens: 1000,
                completed_chunks: 2,
                total_chunks: 5,
                model_calls: 3,
                started_at: chrono::Utc::now() - chrono::Duration::seconds(75),
                request_started_at: Some(chrono::Utc::now()),
                finished_at: None,
                background: false,
                before_tokens: 1200,
                after_tokens: None,
            });
            terminal.draw(|frame| render(&mut app, frame)).unwrap();
            let after = lines(&terminal);
            let status = &after[status_row];
            assert!(
                status.contains(language.pick("Compacting context", "Сжимаю контекст")),
                "{status}"
            );
            assert!(status.contains("40%"), "{status}");
            assert!(status.contains("2/5"), "{status}");
            assert!(
                status.contains(language.pick("Esc to stop", "Esc для остановки")),
                "{status}"
            );
            assert!(status.contains(FRAMES[0]));
            assert!(!after.join("\n").contains(working));
            assert!(
                !after
                    .join("\n")
                    .contains(language.pick("model response", "ответ модели"))
            );
            assert_eq!(
                after.iter().position(|line| line.contains(" > ")),
                Some(input_row)
            );
            app.animation_frame = 1;
            terminal.draw(|frame| render(&mut app, frame)).unwrap();
            assert!(lines(&terminal)[status_row].contains(FRAMES[1]));
            app.compacting = false;
            app.compaction_progress.as_mut().unwrap().finished_at = Some(chrono::Utc::now());
            terminal.draw(|frame| render(&mut app, frame)).unwrap();
            assert!(lines(&terminal)[status_row].contains(working));
        }
    }

    #[test]
    fn russian_controls_empty_states_and_session_picker_render() {
        let mut app = App {
            language: crate::i18n::Language::Ru,
            ..Default::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        let text = |terminal: &Terminal<TestBackend>| {
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let rendered = text(&terminal);
        assert!(rendered.contains("Готов"));
        assert!(rendered.contains("Попросите Kodrik о чём угодно"));
        app.status = Status::Working;
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert!(text(&terminal).contains("Работаю · Esc для остановки"));
        app.compacting = true;
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert!(text(&terminal).contains("Сжимаю контекст · Esc для остановки"));
        app.status = Status::Stopping;
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert!(text(&terminal).contains("Остановка"));
        assert!(!text(&terminal).contains("Сжимаю контекст"));
        app.status = Status::Working;
        app.compacting = false;
        app.insert_input("/");
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let rendered = text(&terminal);
        for expected in [
            "Начать новый сеанс",
            "Выбрать сеанс",
            "Язык интерфейса: en|ru",
            "Enter — запуск · Tab — дополнить",
        ] {
            assert!(rendered.contains(expected), "{expected}");
        }
        app.sessions = Some(vec![]);
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let rendered = text(&terminal);
        assert!(rendered.contains("Сеансы · ↑/↓ выбор · Enter открыть · Esc закрыть"));
        assert!(rendered.contains("Нет сохранённых сеансов"));
        for (width, height) in [(1, 1), (3, 3), (8, 5)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(&mut app, frame)).unwrap();
        }
    }

    #[test]
    fn session_picker_scrolls_to_selection_and_handles_empty_and_small_screens() {
        let mut app = App::default();
        app.sessions = Some(
            (0..30)
                .map(|i| {
                    (
                        crate::agent::session::SessionId::new(format!("session-{i}")),
                        format!("Conversation {i}"),
                    )
                })
                .collect(),
        );
        app.session_index = 29;
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("> Conversation 29"));
        for (width, height) in [(1, 1), (3, 3), (8, 5)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(&mut app, frame)).unwrap();
        }
        app.sessions = Some(vec![]);
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("No saved sessions"));
    }

    #[test]
    fn session_picker_shows_loading_errors_and_more_without_hiding_rows() {
        let mut app = App {
            sessions: Some(vec![]),
            sessions_loading: true,
            ..Default::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(90, 12)).unwrap();
        let text = |terminal: &Terminal<TestBackend>| {
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert!(text(&terminal).contains("Loading sessions…"));
        assert!(!text(&terminal).contains("No saved sessions"));
        assert!(text(&terminal).contains(FRAMES[0]));
        app.animation_frame = 1;
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert!(text(&terminal).contains(FRAMES[1]));
        app.sessions_loading = false;
        app.sessions = Some(vec![(
            crate::agent::session::SessionId::new("one"),
            "Saved conversation".into(),
        )]);
        app.sessions_error = Some("Connection failed".into());
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert!(text(&terminal).contains("Saved conversation"));
        assert!(text(&terminal).contains("Could not load sessions"));
        assert!(text(&terminal).contains("r retry"));
        assert!(matches!(app.status, Status::Ready));
        app.sessions_error = None;
        app.sessions_more = true;
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert!(text(&terminal).contains("More sessions available"));
        app.language = crate::i18n::Language::Ru;
        app.sessions = Some(vec![]);
        app.sessions_loading = true;
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert!(text(&terminal).contains("Загружаю сеансы…"));
        for (width, height) in [(1, 1), (3, 3), (8, 5)] {
            app.sessions_error = Some("offline".into());
            let mut small = Terminal::new(TestBackend::new(width, height)).unwrap();
            small.draw(|frame| render(&mut app, frame)).unwrap();
        }
    }

    #[test]
    fn language_picker_renders_choices_and_hides_composer_cursor() {
        let mut app = App::default();
        app.insert_input("/language");
        app.update(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("> English"));
        assert!(text.contains("Русский"));
        assert!(!terminal.backend().cursor_visible());
    }

    #[test]
    fn slash_menu_renders_description_and_disappears_on_escape() {
        let mut app = App::default();
        app.insert_input("/");
        let mut terminal = Terminal::new(TestBackend::new(64, 20)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("/new  Start a new session"));
        let buffer = terminal.backend().buffer();
        let command_y = (0..20)
            .find(|&y| {
                (0..64)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .contains("/new")
            })
            .unwrap();
        assert!(command_y >= 2);
        for x in 0..64 {
            assert_eq!(buffer[(x, command_y - 1)].symbol(), " ");
            assert_eq!(buffer[(x, command_y - 1)].bg, Color::Rgb(54, 58, 79));
            assert_eq!(buffer[(x, command_y - 2)].symbol(), " ");
            assert_ne!(buffer[(x, command_y - 2)].bg, Color::Rgb(54, 58, 79));
        }
        assert_eq!(terminal.get_cursor_position().unwrap(), (4, 17).into());
        app.update(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ));
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!text.contains("/new"));
        app.insert_input("n");
        for (width, height) in [(1, 1), (3, 3), (8, 5)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(&mut app, frame)).unwrap();
        }
    }

    #[test]
    fn plan_stays_above_status_when_history_grows_and_scrolls() {
        let mut app = App::default();
        app.update_plan(
            crate::agent::tools::TaskPlan::parse(serde_json::json!({
                "plan": [
                    {"step": "Inspect", "status": "completed"},
                    {"step": "Fix", "status": "in_progress"},
                    {"step": "Verify", "status": "pending"}
                ]
            }))
            .unwrap(),
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        for scroll in [0, 15] {
            for index in 0..30 {
                app.push_kodrik_message(format!("Output {index}"));
            }
            app.scroll = scroll;
            terminal.draw(|frame| render(&mut app, frame)).unwrap();
            let buffer = terminal.backend().buffer();
            let rows: Vec<String> = (0..24)
                .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect())
                .collect();
            let status = rows.iter().position(|row| row.contains("Ready")).unwrap();
            assert_eq!(rows[status - 5].trim(), "≡ Plan · 1/3");
            assert!(rows[status - 6].trim().is_empty(), "blank line above plan");
            assert_eq!(buffer[(1, (status - 5) as u16)].symbol(), "≡");
            for (offset, expected) in ["✓ Inspect", "→ Fix", "○ Verify"].iter().enumerate() {
                assert_eq!(rows[status - 4 + offset].trim(), *expected);
            }
        }
    }

    #[test]
    fn clearing_plan_restores_history_space_without_moving_input() {
        let mut app = App::default();
        app.update_plan(
            crate::agent::tools::TaskPlan::parse(serde_json::json!({
                "plan": [{"step": "Inspect", "status": "pending"}]
            }))
            .unwrap(),
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let before_cursor = terminal.get_cursor_position().unwrap();
        app.update_plan(
            crate::agent::tools::TaskPlan::parse(serde_json::json!({"plan": []})).unwrap(),
        );
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert_eq!(terminal.get_cursor_position().unwrap(), before_cursor);
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!text.contains("Plan ·"));
        assert!(!text.contains("Inspect"));
        assert!(app.messages.is_empty());
    }

    #[test]
    fn long_plan_wraps_and_keeps_status_and_input_visible_on_small_screens() {
        let mut app = App::default();
        app.update_plan(crate::agent::tools::TaskPlan::parse(serde_json::json!({
            "plan": [{"step": "Inspect the implementation carefully", "status": "in_progress"}],
            "explanation": "A long explanation that must not displace the input or status".repeat(30)
        })).unwrap());
        for (width, height) in [(24, 16), (80, 24), (1, 1), (3, 3), (8, 5)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(&mut app, frame)).unwrap();
            if width >= 24 {
                let text = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(text.contains("Ready"));
                assert!(text.contains(" > "));
                assert!(text.contains("Plan · 0/1"));
                assert!(text.contains("implementation"));
            }
        }
    }

    #[test]
    fn truncated_plan_keeps_the_current_step_visible() {
        let mut app = App::default();
        app.update_plan(
            crate::agent::tools::TaskPlan::parse(serde_json::json!({
                "plan": (0..8).map(|index| serde_json::json!({
                    "step": format!("Step {index}"),
                    "status": if index == 7 { "in_progress" } else { "completed" }
                })).collect::<Vec<_>>()
            }))
            .unwrap(),
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Plan · 7/8"));
        assert!(text.contains("→ Step 7"));
        assert!(text.contains("Ready"));
    }

    #[test]
    fn input_wraps_to_available_width_without_changing_text() {
        let mut app = App::default();
        let text = "0123456789abcdefXYZ";
        app.insert_input(text);
        for (width, expected) in [
            (11, vec!["01234567", "89abcdef", "XYZ"]),
            (20, vec!["0123456789abcdefX", "YZ"]),
        ] {
            let mut terminal = Terminal::new(TestBackend::new(width, 20)).unwrap();
            terminal.draw(|frame| render(&mut app, frame)).unwrap();
            let buffer = terminal.backend().buffer();
            for (index, line) in expected.iter().enumerate() {
                let y = 18 - expected.len() as u16 + index as u16;
                let rendered = (3..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>();
                assert_eq!(rendered.trim_end(), *line);
            }
            assert_eq!(
                terminal.get_cursor_position().unwrap(),
                (3 + expected.last().unwrap().len() as u16, 17).into()
            );
            assert_eq!(app.input, text);
        }
    }

    #[test]
    fn multiline_input_scrolls_to_cursor_and_handles_small_terminals() {
        let mut app = App::default();
        app.insert_input("one\n界🙂\n");
        let mut terminal = Terminal::new(TestBackend::new(20, 20)).unwrap();
        let frame = terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert_eq!(frame.buffer[(3, 15)].symbol(), "o");
        assert_eq!(frame.buffer[(3, 16)].symbol(), "界");
        assert_eq!(terminal.get_cursor_position().unwrap(), (3, 17).into());
        app.update(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Left,
            crossterm::event::KeyModifiers::NONE,
        ));
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert_eq!(terminal.get_cursor_position().unwrap(), (7, 16).into());

        app.insert_input("\na\nb\nc\nd\ne\nf\n012345678901234567890");
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert_eq!(terminal.get_cursor_position().unwrap(), (7, 17).into());
        for (width, height) in [(1, 1), (3, 3), (8, 5)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(&mut app, frame)).unwrap();
        }
    }

    #[test]
    fn user_message_has_space_above_not_below() {
        let mut app = App::default();
        app.push_kodrik_message("answer".into());
        app.push_user_message("question".into());
        app.push_kodrik_message("reply".into());
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y| (0..40).map(|x| buffer[(x, y)].symbol()).collect::<String>();
        assert!(row(7).trim().is_empty());
        assert!(row(9).contains("question"));
        assert!(row(10).trim().is_empty());
        assert_eq!(buffer[(0, 10)].bg, Color::Rgb(54, 58, 79));
        assert!(row(11).trim().is_empty());
        assert!(row(12).contains("reply"));
    }
}
