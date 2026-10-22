use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, List, ListItem, ListState, Paragraph, Widget},
};

use super::app::{App, AppMessage, AppMessageRole, Status, ToolState};

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub const VIEWPORT_HEIGHT: u16 = 7;
const LOGO: [&str; 6] = [
    "██                        ██            ██  ██        ",
    "██    ██  ████████  ████████  ████████      ██    ██  ",
    "██  ██    ██    ██  ██    ██  ██    ██  ██  ██  ██    ",
    "████      ██    ██  ██    ██  ██        ██  ████      ",
    "██  ██    ██    ██  ██    ██  ██        ██  ██  ██    ",
    "██    ██  ████████  ████████  ██        ██  ██    ██  ",
];
// Pastel steps derived from Catppuccin Mocha Blue (#89b4fa).
const LOGO_COLORS: [Color; 6] = [
    Color::Rgb(194, 209, 235),
    Color::Rgb(171, 192, 227),
    Color::Rgb(148, 175, 220),
    Color::Rgb(124, 158, 212),
    Color::Rgb(101, 141, 205),
    Color::Rgb(78, 124, 197),
];

pub fn render_logo(buf: &mut Buffer) {
    let area = Rect {
        x: buf.area.x.saturating_add(1),
        width: buf.area.width.saturating_sub(1),
        height: buf.area.height.saturating_sub(1),
        ..buf.area
    };
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
        AppMessageRole::User => (">", Color::White, Color::Rgb(54, 58, 79)),
        AppMessageRole::Plan => ("☷", Color::Cyan, Color::default()),
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
    if let Some(sessions) = &app.sessions {
        let items = sessions
            .iter()
            .map(|(id, title)| ListItem::new(format!("{title}  · {}", id.as_str())))
            .collect::<Vec<_>>();
        let block = Block::bordered().title(language.pick(
            " Sessions · ↑/↓ select · Enter open · Esc close ",
            " Сеансы · ↑/↓ выбор · Enter открыть · Esc закрыть ",
        ));
        if items.is_empty() {
            frame.render_widget(
                Paragraph::new(language.pick("No saved sessions", "Нет сохранённых сеансов"))
                    .block(block),
                area,
            );
        } else {
            frame.render_stateful_widget(
                List::new(items)
                    .block(block)
                    .highlight_symbol("> ")
                    .highlight_style(
                        Style::default()
                            .fg(Color::Yellow)
                            .bg(Color::Rgb(54, 58, 79)),
                    ),
                area,
                &mut ListState::default().with_selected(Some(app.session_index)),
            );
        }
        return;
    }
    app.input_width = usize::from(area.width.saturating_sub(3).max(1));
    let (input_rows, (cursor_x, cursor_y)) = app.input_layout(app.input_width);
    let input_lines = input_rows.len().min(6) as u16;
    let command_height = (app.command_suggestions().len() as u16 + 1).max(3);
    let history_height = area
        .height
        .saturating_sub(VIEWPORT_HEIGHT + input_lines - 1 + command_height - 3);
    let history = Rect::new(area.x, area.y, area.width, history_height);
    render_history(app, frame, history);
    let footer = Rect::new(
        area.x,
        area.y + history_height,
        area.width,
        area.height - history_height,
    );
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![
            Constraint::Length(command_height),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(footer);

    let (input, input_color) = if app.input.is_empty() {
        (
            vec![Line::raw(language.pick(
                "Ask Kodrik to do anything",
                "Попросите Kodrik о чём угодно",
            ))],
            Color::Gray,
        )
    } else {
        (
            input_rows
                .iter()
                .map(|row| Line::raw(&app.input[row.clone()]))
                .collect(),
            Color::White,
        )
    };

    let (animation_frame, state, done_at) = match &app.status {
        Status::Ready => ("○", language.pick("Ready", "Готов"), String::new()),
        Status::Stopping => (
            FRAMES[app.animation_frame % FRAMES.len()],
            language.pick("Stopping", "Остановка"),
            String::new(),
        ),
        Status::Stopped => ("■", language.pick("Stopped", "Остановлено"), String::new()),
        Status::Working => (
            FRAMES[app.animation_frame % FRAMES.len()],
            language.pick("Working · Esc to stop", "Работаю · Esc для остановки"),
            String::new(),
        ),
        Status::Done(time) => ("✓", language.pick("Done", "Готово"), format!(" {time}")),
        Status::Error => ("✗", language.pick("Error", "Ошибка"), String::new()),
    };

    frame.render_widget(
        Paragraph::new(format!(" {animation_frame} {state}{done_at}"))
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
        lines.push(Line::styled(
            language.pick(
                "   ↑/↓ · Tab/Enter — select · Esc — close",
                "   ↑/↓ · Tab/Enter — выбор · Esc — закрыть",
            ),
            Style::default().fg(Color::Gray),
        ));
        let height = (lines.len() as u16).min(layout[0].height);
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

    frame.render_widget(
        Paragraph::new(vec![
            Line::default(),
            Line::styled(" > ", Style::default().fg(Color::White).bold()),
        ])
        .style(Style::default().bg(Color::Rgb(54, 58, 79))),
        layout[1],
    );

    let input_area = Rect::new(
        layout[1].x.saturating_add(3),
        layout[1].y.saturating_add(1),
        layout[1].width.saturating_sub(3),
        layout[1].height.saturating_sub(2),
    );
    let cursor_x = cursor_x.min(u16::MAX as usize) as u16;
    let cursor_y = cursor_y.min(u16::MAX as usize) as u16;
    let scroll_y = cursor_y.saturating_sub(input_area.height.saturating_sub(1));
    frame.render_widget(
        Paragraph::new(input)
            .style(Style::default().fg(input_color))
            .scroll((scroll_y, 0)),
        input_area,
    );

    frame.render_widget(
        Paragraph::new(match app.context_tokens {
            Some(tokens) => format!("{} · {}K", app.model, tokens / 1000),
            None => app.model.clone(),
        })
        .style(Style::default().fg(Color::Yellow)),
        Rect::new(
            layout[2].x.saturating_add(3),
            layout[2].y,
            layout[2].width.saturating_sub(3),
            layout[2].height,
        ),
    );

    if input_area.width > 0 && input_area.height > 0 {
        frame.set_cursor_position((
            input_area.x + cursor_x.min(input_area.width - 1),
            input_area.y + cursor_y.saturating_sub(scroll_y),
        ));
    }
}

fn render_history(app: &App, frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mut bottom = area.bottom();
    let mut skip = app.scroll;
    for message in app.messages.iter().rev() {
        let source = message.content.replace('\t', "    ");
        let paragraph = Paragraph::new(if matches!(message.role, AppMessageRole::Tool(_)) {
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
        } else if matches!(message.role, AppMessageRole::Plan) {
            ratatui::text::Text::from(source)
        } else {
            tui_markdown::from_str(&source)
        })
        .wrap(ratatui::widgets::Wrap { trim: false });
        let height = message_height(message, &paragraph, area.width);
        let gap = usize::from(matches!(message.role, AppMessageRole::User));
        let total = usize::from(height) + gap;
        if skip >= total {
            skip -= total;
            continue;
        }
        let visible = (total - skip).min(usize::from(bottom - area.y));
        if visible == 0 {
            break;
        }
        let mut buffer = Buffer::empty(Rect::new(0, 0, area.width, height));
        render_message(message, &paragraph, &mut buffer);
        let start = total - skip - visible;
        let width = usize::from(area.width);
        for row in 0..visible {
            if start + row >= gap {
                for x in 0..width {
                    *frame
                        .buffer_mut()
                        .cell_mut((area.x + x as u16, bottom - visible as u16 + row as u16))
                        .unwrap() = buffer.content[(start + row - gap) * width + x].clone();
                }
            }
        }
        bottom -= visible as u16;
        skip = 0;
    }
    if bottom > area.y && skip == 0 {
        let mut logo = Buffer::empty(Rect::new(0, 0, area.width, 7));
        render_logo(&mut logo);
        let visible = (bottom - area.y).min(7);
        let width = usize::from(area.width);
        for row in 0..visible {
            for x in 0..width {
                *frame
                    .buffer_mut()
                    .cell_mut((area.x + x as u16, bottom - visible + row))
                    .unwrap() = logo.content[usize::from(7 - visible + row) * width + x].clone();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

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
        app.insert_input("/");
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let rendered = text(&terminal);
        for expected in [
            "Начать новый сеанс",
            "Выбрать сеанс",
            "Язык интерфейса: en|ru",
            "Tab/Enter — выбор",
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
    fn plan_preserves_separate_lines() {
        let mut app = App::default();
        app.messages.push(AppMessage {
            role: AppMessageRole::Plan,
            content: "Plan · 1/3\n✓ Inspect\n→ Fix\n○ Verify".into(),
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..24)
            .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let start = rows
            .iter()
            .position(|row| row.contains("Plan · 1/3"))
            .unwrap();
        for (offset, expected) in ["✓ Inspect", "→ Fix", "○ Verify"].iter().enumerate() {
            assert_eq!(rows[start + offset + 1].trim(), *expected);
        }
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
