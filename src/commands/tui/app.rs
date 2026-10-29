use crate::i18n::Language;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{style::Style, text::Span};
use std::collections::HashMap;
use std::ops::Range;

#[derive(Default)]
pub struct App {
    pub language: Language,
    pub should_quit: bool,
    pub model: String,
    pub context_tokens: Option<u64>,
    pub input: String,
    pub input_cursor: usize,
    pub input_width: usize,
    pub commands_dismissed: bool,
    pub command_index: usize,
    pub sessions: Option<Vec<(crate::agent::session::SessionId, String)>>,
    pub session_index: usize,
    pub messages: Vec<AppMessage>,
    pub streaming_message: Option<usize>,
    pub pending_tools: HashMap<String, usize>,
    pub scroll: usize,
    pub view: super::view::View,
    pub copy_notice: Option<(String, std::time::Instant)>,
    pub animation_frame: usize,
    pub status: Status,
}

#[derive(Default)]
pub enum Status {
    #[default]
    Ready,
    Recovering,
    Working,
    Stopping,
    Stopped,
    Done(String),
    Error,
}

impl App {
    pub fn set_copy_notice(&mut self, text: String) {
        self.copy_notice = Some((
            text,
            std::time::Instant::now() + std::time::Duration::from_secs(3),
        ));
    }
    pub fn command_suggestions(&self) -> Vec<(&'static str, &'static str)> {
        [
            (
                "/new",
                self.language
                    .pick("Start a new session", "Начать новый сеанс"),
            ),
            (
                "/sessions",
                self.language.pick("Select a session", "Выбрать сеанс"),
            ),
            (
                "/language",
                self.language
                    .pick("Interface language: en|ru", "Язык интерфейса: en|ru"),
            ),
        ]
        .into_iter()
        .filter(|(name, _)| {
            !self.commands_dismissed && self.input.starts_with('/') && name.starts_with(&self.input)
        })
        .collect()
    }

    pub fn command_suggestion(&self) -> Option<(&'static str, &'static str)> {
        let commands = self.command_suggestions();
        commands
            .get(self.command_index.min(commands.len().saturating_sub(1)))
            .copied()
    }

    pub fn insert_input(&mut self, text: &str) {
        self.commands_dismissed = false;
        self.command_index = 0;
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        self.input.insert_str(self.input_cursor, &text);
        self.input_cursor += text.len();
    }

    fn line_start(&self) -> usize {
        self.input[..self.input_cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1)
    }

    fn line_end(&self) -> usize {
        self.input[self.input_cursor..]
            .find('\n')
            .map_or(self.input.len(), |index| self.input_cursor + index)
    }

    fn move_vertical(&mut self, up: bool) {
        let width = if self.input_width == 0 {
            usize::MAX
        } else {
            self.input_width
        };
        let (rows, (column, row)) = self.input_layout(width);
        let target = if up {
            row.saturating_sub(1)
        } else {
            (row + 1).min(rows.len() - 1)
        };
        if target == row {
            return;
        }
        let range = &rows[target];
        let line = Span::raw(&self.input[range.clone()]);
        let mut cells = 0;
        self.input_cursor = range.start;
        for grapheme in line.styled_graphemes(Style::default()) {
            cells += Span::raw(grapheme.symbol).width();
            if cells > column || cells >= width {
                break;
            }
            self.input_cursor += self.input[self.input_cursor..range.end]
                .find(grapheme.symbol)
                .unwrap()
                + grapheme.symbol.len();
        }
    }

    pub fn input_layout(&self, width: usize) -> (Vec<Range<usize>>, (usize, usize)) {
        let width = width.max(1);
        let mut rows = Vec::new();
        let mut offset = 0;
        let mut cursor = (0, 0);
        for line in self.input.split('\n') {
            let end = offset + line.len();
            let mut start = offset;
            let mut column = 0;
            let span = Span::raw(line);
            for grapheme in span.styled_graphemes(Style::default()) {
                let previous_offset = offset;
                // Control characters are omitted by ratatui's grapheme iterator.
                offset += self.input[offset..].find(grapheme.symbol).unwrap();
                let cells = Span::raw(grapheme.symbol).width();
                if column > 0 && column + cells > width {
                    rows.push(start..offset);
                    start = offset;
                    column = 0;
                }
                if (previous_offset..offset + grapheme.symbol.len()).contains(&self.input_cursor) {
                    cursor = (column, rows.len());
                }
                column += cells;
                offset += grapheme.symbol.len();
            }
            rows.push(start..end);
            if (offset..=end).contains(&self.input_cursor) {
                if column >= width {
                    rows.push(end..end);
                    cursor = (0, rows.len() - 1);
                } else {
                    cursor = (column, rows.len() - 1);
                }
            }
            offset = end + 1;
        }
        (rows, cursor)
    }

    pub fn finish_stream(&mut self, content: String) {
        if let Some(index) = self.streaming_message.take() {
            self.messages[index].content = content;
        } else {
            self.push_kodrik_message(content);
        }
    }

    pub fn push_user_message(&mut self, content: String) {
        self.messages.push(AppMessage {
            role: AppMessageRole::User,
            content,
        });
    }

    pub fn update_plan(&mut self, plan: crate::agent::tools::TaskPlan) {
        let content = plan.render();
        let content = if self.language == Language::Ru {
            content.replacen("Plan ·", "План ·", 1)
        } else {
            content
        };
        self.messages.push(AppMessage {
            role: AppMessageRole::Plan,
            content,
        });
    }

    pub fn push_kodrik_message(&mut self, content: String) {
        self.messages.push(AppMessage {
            role: AppMessageRole::Kodrik,
            content,
        });
    }

    pub fn push_tool_message(&mut self, content: String) {
        self.messages.push(AppMessage {
            role: AppMessageRole::Tool(ToolState::Running),
            content,
        });
    }

    pub fn update(&mut self, key: KeyEvent) -> Option<String> {
        if let Some(sessions) = &self.sessions {
            match key.code {
                KeyCode::Up => self.session_index = self.session_index.saturating_sub(1),
                KeyCode::Down => {
                    self.session_index =
                        (self.session_index + 1).min(sessions.len().saturating_sub(1))
                }
                KeyCode::Esc => self.sessions = None,
                _ => {}
            }
            return None;
        }
        if let Some((command, _)) = self.command_suggestion()
            && key.modifiers.is_empty()
        {
            match key.code {
                KeyCode::Esc => {
                    self.commands_dismissed = true;
                    return None;
                }
                KeyCode::Tab | KeyCode::Enter
                    if self.input != command || key.code == KeyCode::Tab =>
                {
                    self.input = command.to_owned();
                    self.input_cursor = self.input.len();
                    self.commands_dismissed = true;
                    return None;
                }
                KeyCode::Up | KeyCode::Down => {
                    let count = self.command_suggestions().len();
                    self.command_index = if key.code == KeyCode::Down {
                        (self.command_index + 1) % count
                    } else {
                        (self.command_index + count - 1) % count
                    };
                    return None;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Char('c' | 'C' | 'с' | 'С')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                if self.input.is_empty() {
                    self.should_quit = true;
                } else {
                    self.commands_dismissed = false;
                    self.command_index = 0;
                    self.input.clear();
                    self.input_cursor = 0;
                }
            }
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.insert_input("\n");
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert_input("\n");
            }
            KeyCode::Enter
                if !matches!(self.status, Status::Recovering | Status::Stopping)
                    && !self.input.trim().is_empty() =>
            {
                if matches!(self.status, Status::Working)
                    && matches!(self.input.trim(), "/new" | "/sessions")
                {
                    return None;
                }
                let input = std::mem::take(&mut self.input).trim().to_owned();
                self.input_cursor = 0;
                if matches!(input.as_str(), "/new" | "/sessions")
                    || input.split_whitespace().next() == Some("/language")
                {
                    return Some(input);
                }
                self.push_user_message(input.clone());
                self.status = Status::Working;
                self.animation_frame = 0;
                return Some(input);
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.commands_dismissed = false;
                self.command_index = 0;
                self.input.insert(self.input_cursor, c);
                self.input_cursor += c.len_utf8();
            }
            KeyCode::Backspace => {
                self.commands_dismissed = false;
                self.command_index = 0;
                if let Some(c) = self.input[..self.input_cursor].chars().next_back() {
                    self.input_cursor -= c.len_utf8();
                    self.input.remove(self.input_cursor);
                }
            }
            KeyCode::Delete if self.input_cursor < self.input.len() => {
                self.commands_dismissed = false;
                self.command_index = 0;
                self.input.remove(self.input_cursor);
            }
            KeyCode::Left => {
                if let Some(c) = self.input[..self.input_cursor].chars().next_back() {
                    self.input_cursor -= c.len_utf8();
                }
            }
            KeyCode::Right => {
                if let Some(c) = self.input[self.input_cursor..].chars().next() {
                    self.input_cursor += c.len_utf8();
                }
            }
            KeyCode::Home => self.input_cursor = self.line_start(),
            KeyCode::End => self.input_cursor = self.line_end(),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Up | KeyCode::Down if !self.input.is_empty() => {
                self.move_vertical(key.code == KeyCode::Up);
            }
            KeyCode::Up => self.scroll = self.scroll.saturating_add(1),
            KeyCode::Down => self.scroll = self.scroll.saturating_sub(1),
            _ => {}
        }
        None
    }
}

pub struct AppMessage {
    pub role: AppMessageRole,
    pub content: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AppMessageRole {
    Kodrik,
    User,
    Plan,
    Tool(ToolState),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    Running,
    Success,
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(app: &mut App, code: KeyCode) -> Option<String> {
        app.update(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn language_commands_are_local_even_while_working() {
        for input in ["/language ru", "/language en", "/language de", "/language"] {
            let mut app = App {
                language: Language::Ru,
                status: Status::Working,
                ..Default::default()
            };
            app.insert_input(input);
            assert_eq!(key(&mut app, KeyCode::Enter).as_deref(), Some(input));
            assert!(app.messages.is_empty());
            assert!(matches!(app.status, Status::Working));
            assert!(app.input.is_empty());
        }
        let mut app = App {
            language: Language::Ru,
            ..Default::default()
        };
        app.insert_input("/l");
        assert_eq!(
            app.command_suggestion(),
            Some(("/language", "Язык интерфейса: en|ru"))
        );
        assert!(key(&mut app, KeyCode::Enter).is_none());
        assert_eq!(app.input, "/language");
    }

    #[test]
    fn sessions_command_and_picker_navigation() {
        let mut app = App::default();
        app.insert_input("/");
        assert_eq!(app.command_suggestions().len(), 3);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.command_suggestion().unwrap().0, "/sessions");
        key(&mut app, KeyCode::Tab);
        assert_eq!(key(&mut app, KeyCode::Enter).as_deref(), Some("/sessions"));
        assert!(app.messages.is_empty());
        assert!(matches!(app.status, Status::Ready));
        app.insert_input("/s");
        assert_eq!(
            app.command_suggestions(),
            vec![("/sessions", "Select a session")]
        );
        key(&mut app, KeyCode::Enter);
        app.status = Status::Working;
        assert!(key(&mut app, KeyCode::Enter).is_none());
        assert_eq!(app.input, "/sessions");
        app.status = Status::Ready;
        app.sessions = Some(vec![
            (crate::agent::session::SessionId::new("one"), "First".into()),
            (
                crate::agent::session::SessionId::new("two"),
                "Second".into(),
            ),
        ]);
        key(&mut app, KeyCode::Up);
        assert_eq!(app.session_index, 0);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.session_index, 1);
        key(&mut app, KeyCode::Char('x'));
        assert_eq!(app.input, "/sessions");
        key(&mut app, KeyCode::Esc);
        assert!(app.sessions.is_none());
        app.sessions = Some(vec![]);
        app.session_index = 0;
        key(&mut app, KeyCode::Down);
        assert_eq!(app.session_index, 0);
        key(&mut app, KeyCode::Esc);
        assert!(app.sessions.is_none());
    }

    #[test]
    fn slash_menu_filters_completes_dismisses_and_preserves_busy_guard() {
        for input in ["", "hello/", " /", "/unknown", "/new argument", "/n\n"] {
            let mut app = App::default();
            app.insert_input(input);
            assert!(app.command_suggestion().is_none(), "{input:?}");
        }
        for accept in [KeyCode::Tab, KeyCode::Enter] {
            let mut app = App::default();
            key(&mut app, KeyCode::Char('/'));
            assert_eq!(app.command_suggestion().unwrap().0, "/new");
            key(&mut app, KeyCode::Down);
            key(&mut app, KeyCode::Up);
            assert_eq!(app.input_cursor, 1);
            key(&mut app, KeyCode::Char('n'));
            assert!(key(&mut app, accept).is_none());
            assert_eq!(app.input, "/new");
            assert_eq!(app.input_cursor, 4);
            assert!(app.messages.is_empty());
            assert!(app.command_suggestion().is_none());
            app.status = Status::Working;
            assert!(key(&mut app, KeyCode::Enter).is_none());
            assert_eq!(app.input, "/new");
            app.status = Status::Ready;
            assert_eq!(key(&mut app, KeyCode::Enter).as_deref(), Some("/new"));
            key(&mut app, KeyCode::Char('/'));
            assert!(app.command_suggestion().is_some());
            key(&mut app, KeyCode::Esc);
            assert!(app.command_suggestion().is_none());
            assert_eq!(app.input, "/");
            key(&mut app, KeyCode::Char('n'));
            assert!(app.command_suggestion().is_some());
        }
    }

    #[test]
    fn ctrl_c_clears_nonempty_input_then_quits() {
        for c in ['c', 'C', 'с', 'С'] {
            for text in ["Привет\nмир", " \n "] {
                let mut app = App::default();
                app.status = Status::Working;
                app.insert_input(text);
                key(&mut app, KeyCode::Left);
                let ctrl_c = KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
                assert!(app.update(ctrl_c).is_none());
                assert!(app.input.is_empty());
                assert_eq!(app.input_cursor, 0);
                assert!(!app.should_quit);
                assert!(matches!(app.status, Status::Working));
                assert!(app.update(ctrl_c).is_none());
                assert!(app.should_quit);
            }
        }
    }

    #[test]
    fn visual_wrap_preserves_text_and_maps_cursor_and_navigation() {
        for (text, width, expected, cursor) in [
            ("abcdef", 4, vec!["abcd", "ef"], (2, 1)),
            ("abcd", 4, vec!["abcd", ""], (0, 1)),
            ("ab\n", 4, vec!["ab", ""], (0, 1)),
            ("abcd\nx", 4, vec!["abcd", "x"], (1, 1)),
            ("я界🙂!", 4, vec!["я界", "🙂!"], (3, 1)),
            ("a👩‍💻bc", 4, vec!["a👩‍💻b", "c"], (1, 1)),
            ("a  bc", 3, vec!["a  ", "bc"], (2, 1)),
        ] {
            let mut app = App::default();
            app.insert_input(text);
            let (rows, position) = app.input_layout(width);
            assert_eq!(
                rows.iter()
                    .map(|r| &app.input[r.clone()])
                    .collect::<Vec<_>>(),
                expected
            );
            assert_eq!(position, cursor);
            assert_eq!(app.input, text);
        }
        let mut app = App::default();
        app.input_width = 4;
        app.insert_input("abcdefghij");
        key(&mut app, KeyCode::Up);
        assert_eq!(app.input_cursor, 6);
        key(&mut app, KeyCode::Up);
        assert_eq!(app.input_cursor, 2);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.input_cursor, 6);
        key(&mut app, KeyCode::Char('!'));
        assert_eq!(app.input, "abcdef!ghij");
        app.input_cursor = 4;
        assert_eq!(app.input_layout(4).1, (0, 1));
        assert_eq!(app.input_layout(8).1, (4, 0));
    }

    #[test]
    fn edit_unicode_at_cursor_and_submit_multiline_input() {
        let mut app = App::default();
        app.insert_input("Привет🙂");
        key(&mut app, KeyCode::Left);
        key(&mut app, KeyCode::Backspace);
        key(&mut app, KeyCode::Char('т'));
        key(&mut app, KeyCode::Delete);
        assert_eq!(app.input, "Привет");
        app.update(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        app.insert_input("мир\r\n三\r!");
        assert_eq!(app.input, "Привет\nмир\n三\n!");
        key(&mut app, KeyCode::Home);
        app.insert_input("новый ");
        key(&mut app, KeyCode::End);
        let expected = "Привет\nмир\n三\nновый !";
        assert_eq!(key(&mut app, KeyCode::Enter).as_deref(), Some(expected));
        assert_eq!(app.messages[0].content, expected);
        assert!(app.input.is_empty());
        assert_eq!(app.input_cursor, 0);
        assert!(matches!(app.status, Status::Working));
    }

    #[test]
    fn navigation_and_deletion_across_line_boundaries() {
        let mut app = App::default();
        app.insert_input("абв\nx\n🙂yz");
        key(&mut app, KeyCode::Home);
        key(&mut app, KeyCode::Right);
        key(&mut app, KeyCode::Up);
        assert_eq!(&app.input[..app.input_cursor], "абв\nx");
        key(&mut app, KeyCode::Up);
        assert_eq!(&app.input[..app.input_cursor], "а");
        key(&mut app, KeyCode::Up);
        assert_eq!(&app.input[..app.input_cursor], "а");
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Home);
        key(&mut app, KeyCode::Backspace);
        assert_eq!(app.input, "абвx\n🙂yz");
        key(&mut app, KeyCode::End);
        key(&mut app, KeyCode::Delete);
        assert_eq!(app.input, "абвx🙂yz");
        key(&mut app, KeyCode::Home);
        key(&mut app, KeyCode::Left);
        key(&mut app, KeyCode::Backspace);
        assert_eq!(app.input_cursor, 0);
        key(&mut app, KeyCode::End);
        key(&mut app, KeyCode::Right);
        key(&mut app, KeyCode::Delete);
        assert_eq!(app.input_cursor, app.input.len());
        assert_eq!(app.scroll, 0);
        key(&mut app, KeyCode::PageUp);
        assert_eq!(app.scroll, 10);
    }

    #[test]
    fn stopping_and_busy_new_session_preserve_the_draft() {
        for (status, text) in [(Status::Stopping, "correction"), (Status::Working, "/new")] {
            let mut app = App {
                status,
                ..Default::default()
            };
            app.insert_input(text);
            assert!(key(&mut app, KeyCode::Enter).is_none());
            assert_eq!(app.input, text);
            assert!(app.messages.is_empty());
        }
    }

    #[test]
    fn newline_shortcut_works_while_busy_and_new_resets_cursor() {
        let mut app = App::default();
        app.status = Status::Working;
        app.insert_input("draft");
        assert_eq!(key(&mut app, KeyCode::Enter).as_deref(), Some("draft"));
        app.insert_input("draft");
        app.update(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert_eq!(app.input, "draft\n");
        app.status = Status::Ready;
        key(&mut app, KeyCode::Enter);
        app.status = Status::Ready;
        app.insert_input("/new");
        assert_eq!(key(&mut app, KeyCode::Enter).as_deref(), Some("/new"));
        assert_eq!(app.input_cursor, 0);
    }
}
