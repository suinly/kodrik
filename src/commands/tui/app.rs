use crate::i18n::Language;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{style::Style, text::Span};
use std::collections::HashMap;
use std::ops::Range;

#[derive(Default)]
pub struct App {
    pub language: Language,
    pub should_quit: bool,
    pub model: String,
    pub remote_environment: Option<String>,
    pub remote_connection: Option<&'static str>,
    pub remote_grant: Option<RemoteGrant>,
    pub remote_grant_scroll: usize,
    pub context_tokens: Option<u64>,
    pub compacting: bool,
    pub compaction_progress: Option<crate::agent::CompactionProgress>,
    pub input: String,
    pub input_images: Vec<crate::agent::Image>,
    pub input_cursor: usize,
    pub input_width: usize,
    pub commands_dismissed: bool,
    pub command_index: usize,
    pub language_choice: Option<usize>,
    pub sessions: Option<Vec<(crate::agent::session::SessionId, String)>>,
    pub sessions_loading: bool,
    pub sessions_error: Option<String>,
    pub sessions_more: bool,
    pub session_index: usize,
    pub session: Option<crate::agent::session::SessionId>,
    pub versions: HashMap<usize, crate::agent::session::branches::MessageVersions>,
    pub navigation: super::navigation::Navigation,
    pub messages: Vec<AppMessage>,
    pub plan: Option<String>,
    pub plan_completed: bool,
    pub streaming_message: Option<usize>,
    pub pending_tools: HashMap<String, usize>,
    pub shown_approvals: HashMap<String, String>,
    pub approval_session: Option<crate::agent::session::SessionId>,
    pub approval: Option<crate::agent::command_audit::CommandApproval>,
    pub approval_choice: Option<usize>,
    pub audit_mode: crate::agent::command_audit::CommandAuditMode,
    pub audit_session: Option<crate::agent::session::SessionId>,
    pub audit_picker: Option<super::approvals::AuditPicker>,
    pub scroll: usize,
    pub view: super::view::View,
    pub copy_notice: Option<(String, std::time::Instant)>,
    pub animation_frame: usize,
    pub status: Status,
    pub task_started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub task_finished_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub struct RemoteGrant {
    pub details: String,
    pub choice: usize,
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
    pub fn expire_copy_notice(&mut self, now: std::time::Instant) -> bool {
        if self
            .copy_notice
            .as_ref()
            .is_some_and(|(_, until)| now >= *until)
        {
            self.copy_notice = None;
            return true;
        }
        false
    }

    pub fn command_suggestions(&self) -> Vec<(&'static str, &'static str)> {
        if self.remote_grant.is_some()
            || self.approval.is_some()
            || self.audit_picker.is_some()
            || self.language_choice.is_some()
            || self.sessions.is_some()
            || self.navigation.selected.is_some()
            || self.navigation.editing.is_some()
        {
            return Vec::new();
        }
        let mut commands = vec![
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
            (
                "/approvals",
                self.language.pick(
                    "Show pending tool calls: /approvals [SESSION_ID]",
                    "Ожидающие действия: /approvals [SESSION_ID]",
                ),
            ),
            (
                "/approve",
                self.language.pick(
                    "Allow one tool call: /approve ID",
                    "Разрешить одно действие: /approve ID",
                ),
            ),
            (
                "/deny",
                self.language.pick(
                    "Reject a tool call: /deny ID",
                    "Отклонить действие: /deny ID",
                ),
            ),
            (
                "/audit",
                self.language.pick(
                    "Choose tool mode: /audit [trusted|review|ask|status] [SESSION_ID]",
                    "Выбор режима: /audit [trusted|review|ask|status] [SESSION_ID]",
                ),
            ),
        ];
        if self.remote_environment.is_some() {
            commands.extend([
                (
                    "/attach",
                    self.language.pick(
                        "Connect local environment",
                        "Подключить локальное окружение",
                    ),
                ),
                (
                    "/detach",
                    self.language.pick(
                        "Disconnect local environment",
                        "Отключить локальное окружение",
                    ),
                ),
                (
                    "/older",
                    self.language
                        .pick("Load older messages", "Загрузить предыдущие сообщения"),
                ),
                (
                    "/retry",
                    self.language
                        .pick("Retry the last request", "Повторить последний запрос"),
                ),
            ]);
        }
        commands
            .into_iter()
            .filter(|(name, _)| {
                !self.commands_dismissed
                    && self.input.starts_with('/')
                    && name.starts_with(&self.input)
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
        if self.remote_grant.is_some()
            || self.approval.is_some()
            || self.audit_picker.is_some()
            || self.language_choice.is_some()
            || self.navigation.selected.is_some()
        {
            return;
        }
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
        if self.plan_completed {
            self.plan = None;
            self.plan_completed = false;
        }
        self.messages.push(AppMessage {
            role: AppMessageRole::User,
            content,
        });
    }

    pub fn update_plan(&mut self, plan: crate::agent::tools::TaskPlan) {
        self.plan_completed = plan.is_completed();
        if plan.plan.is_empty() {
            self.plan = None;
            return;
        }
        let content = plan.render();
        let content = if self.language == Language::Ru {
            content.replacen("Plan ·", "План ·", 1)
        } else {
            content
        };
        self.plan = Some(content);
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

    fn update_remote_grant(&mut self, key: KeyEvent) -> Option<String> {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C' | 'с' | 'С'))
        {
            self.should_quit = true;
            return None;
        }
        if !key.modifiers.is_empty() {
            return None;
        }
        let grant = self.remote_grant.as_mut()?;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => grant.choice = grant.choice.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                grant.choice = grant.choice.saturating_add(1).min(1)
            }
            KeyCode::PageUp => {
                self.remote_grant_scroll = self.remote_grant_scroll.saturating_sub(10)
            }
            KeyCode::PageDown => {
                self.remote_grant_scroll = self.remote_grant_scroll.saturating_add(10)
            }
            KeyCode::Enter | KeyCode::Esc => {
                let allow = key.code == KeyCode::Enter && grant.choice == 0;
                self.remote_grant = None;
                self.remote_grant_scroll = 0;
                return Some(
                    if allow {
                        "/attach allow"
                    } else {
                        "/attach deny"
                    }
                    .into(),
                );
            }
            _ => {}
        }
        None
    }

    pub fn update(&mut self, key: KeyEvent) -> Option<String> {
        if matches!(key.code, KeyCode::Enter | KeyCode::Esc) && key.kind != KeyEventKind::Press {
            return None;
        }
        if self.remote_grant.is_some() {
            self.navigation.escape = None;
            return self.update_remote_grant(key);
        }
        if key.code != KeyCode::Esc
            || self.sessions.is_some()
            || self.audit_picker.is_some()
            || self.language_choice.is_some()
            || self.approval.is_some()
        {
            self.navigation.escape = None;
        }
        if self.audit_picker.is_some() {
            return super::approvals::update_audit_picker(self, key);
        }
        if self.language_choice.is_some() {
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c' | 'C' | 'с' | 'С'))
            {
                self.should_quit = true;
            } else if key.modifiers.is_empty() {
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => self.language_choice = Some(0),
                    KeyCode::Down | KeyCode::Char('j') => self.language_choice = Some(1),
                    KeyCode::Esc => self.language_choice = None,
                    KeyCode::Enter => {
                        let choice = self.language_choice.take()?;
                        return Some(format!(
                            "/language {}",
                            if choice == 0 { "en" } else { "ru" }
                        ));
                    }
                    _ => {}
                }
            }
            return None;
        }
        if self.approval.is_some() {
            return super::approvals::update(self, key);
        }
        if let Some(sessions) = &self.sessions {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
                    self.session_index = self.session_index.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
                    self.session_index =
                        (self.session_index + 1).min(sessions.len().saturating_sub(1))
                }
                KeyCode::Char('r')
                    if key.modifiers.is_empty()
                        && key.kind == KeyEventKind::Press
                        && self.sessions_error.is_some() =>
                {
                    return Some("/sessions retry".into());
                }
                KeyCode::Esc => {
                    self.sessions = None;
                    self.sessions_loading = false;
                    self.sessions_error = None;
                    self.sessions_more = false;
                }
                _ => {}
            }
            return None;
        }
        if let Some((command, _)) = self.command_suggestion()
            && self.navigation.editing.is_none()
            && key.modifiers.is_empty()
        {
            match key.code {
                KeyCode::Esc => {
                    self.commands_dismissed = true;
                    return None;
                }
                KeyCode::Tab | KeyCode::Enter => {
                    self.input = command.to_owned();
                    self.input_cursor = self.input.len();
                    self.commands_dismissed = true;
                    if key.code == KeyCode::Tab {
                        return None;
                    }
                }
                KeyCode::Up | KeyCode::Down | KeyCode::Char('j' | 'k') => {
                    let count = self.command_suggestions().len();
                    self.command_index = if matches!(key.code, KeyCode::Down | KeyCode::Char('j')) {
                        (self.command_index + 1) % count
                    } else {
                        (self.command_index + count - 1) % count
                    };
                    return None;
                }
                _ => {}
            }
        }
        if self.update_message_navigation(key) {
            return None;
        }
        if matches!(key.code, KeyCode::Up | KeyCode::Down)
            && key.modifiers.is_empty()
            && self.navigation.editing.is_none()
        {
            let (rows, (_, row)) = self.input_layout(self.input_width.max(1));
            let previous = key.code == KeyCode::Up;
            if ((previous && row == 0) || (!previous && row + 1 == rows.len()))
                && self.recall_input(previous)
            {
                return None;
            }
        }
        match key.code {
            KeyCode::Char('c' | 'C' | 'с' | 'С')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                if self.input.is_empty() && self.input_images.is_empty() {
                    self.should_quit = true;
                } else {
                    self.navigation.clear_history();
                    self.commands_dismissed = false;
                    self.command_index = 0;
                    self.input.clear();
                    self.input_images.clear();
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
                if (!matches!(self.status, Status::Recovering | Status::Stopping)
                    || super::approvals::is_command(&self.input))
                    && (!self.input.trim().is_empty() || !self.input_images.is_empty()) =>
            {
                if matches!(self.status, Status::Working)
                    && matches!(self.input.trim(), "/new" | "/sessions")
                {
                    return None;
                }
                if self.navigation.editing.is_some() {
                    self.input_cursor = 0;
                    return Some(std::mem::take(&mut self.input));
                }
                self.navigation.clear_history();
                let input = std::mem::take(&mut self.input);
                self.input_cursor = 0;
                let command = input.trim();
                if command == "/language" {
                    self.language_choice = Some(usize::from(self.language == Language::Ru));
                    return None;
                }
                if matches!(command, "/approve" | "/deny") {
                    return Some("/approvals".into());
                }
                if matches!(command, "/new" | "/sessions")
                    || (self.remote_environment.is_some()
                        && matches!(
                            command.split_whitespace().next(),
                            Some("/attach" | "/detach" | "/older" | "/retry")
                        ))
                    || command.split_whitespace().next() == Some("/language")
                    || super::approvals::is_command(command)
                {
                    return Some(command.to_owned());
                }
                self.push_user_message(if self.input_images.is_empty() {
                    input.clone()
                } else {
                    format!("{}\n[Images: {}]", input, self.input_images.len())
                });
                if !matches!(self.status, Status::Working) {
                    self.task_started_at = Some(chrono::Utc::now());
                    self.task_finished_at = None;
                }
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
    Approval,
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

    #[test]
    fn copy_notice_expires_and_replacement_gets_its_own_deadline() {
        let mut app = App::default();
        app.set_copy_notice("First copy".into());
        let first_deadline = app.copy_notice.as_ref().unwrap().1;
        assert!(!app.expire_copy_notice(first_deadline - std::time::Duration::from_millis(1)));
        assert!(app.copy_notice.is_some());
        app.set_copy_notice("Second copy".into());
        assert!(!app.expire_copy_notice(first_deadline));
        let second_deadline = app.copy_notice.as_ref().unwrap().1;
        assert_eq!(app.copy_notice.as_ref().unwrap().0, "Second copy");
        assert!(app.expire_copy_notice(second_deadline));
        assert!(app.copy_notice.is_none());
        assert!(!app.expire_copy_notice(second_deadline));
    }

    fn key(app: &mut App, code: KeyCode) -> Option<String> {
        app.update(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn remote_grant_denies_by_default_and_keeps_chat_input_untouched() {
        let mut app = App {
            remote_grant: Some(RemoteGrant {
                details: "manifest".into(),
                choice: 1,
            }),
            input: "/audit trusted".into(),
            ..Default::default()
        };
        app.insert_input("pasted");
        key(&mut app, KeyCode::Char('x'));
        assert_eq!(app.input, "/audit trusted");
        assert!(app.command_suggestions().is_empty());
        assert_eq!(
            key(&mut app, KeyCode::Enter).as_deref(),
            Some("/attach deny")
        );
        assert!(app.remote_grant.is_none());
        assert!(app.messages.is_empty());
        assert_eq!(
            app.audit_mode,
            crate::agent::command_audit::CommandAuditMode::default()
        );
    }

    #[test]
    fn remote_grant_keyboard_choices_scroll_and_escape_are_local() {
        for up in [KeyCode::Up, KeyCode::Char('k')] {
            let mut app = App {
                remote_grant: Some(RemoteGrant {
                    details: "manifest".into(),
                    choice: 1,
                }),
                audit_picker: Some(super::super::approvals::AuditPicker {
                    session: None,
                    choice: 0,
                }),
                ..Default::default()
            };
            key(&mut app, up);
            assert_eq!(app.remote_grant.as_ref().unwrap().choice, 0);
            key(&mut app, KeyCode::PageDown);
            assert_eq!(app.remote_grant_scroll, 10);
            key(&mut app, KeyCode::PageUp);
            assert_eq!(app.remote_grant_scroll, 0);
            assert_eq!(
                key(&mut app, KeyCode::Enter).as_deref(),
                Some("/attach allow")
            );
            assert!(app.audit_picker.is_some());
        }
        for down in [KeyCode::Down, KeyCode::Char('j'), KeyCode::Esc] {
            let mut app = App {
                remote_grant: Some(RemoteGrant {
                    details: "manifest".into(),
                    choice: 0,
                }),
                ..Default::default()
            };
            key(&mut app, down);
            if down != KeyCode::Esc {
                assert_eq!(
                    key(&mut app, KeyCode::Enter).as_deref(),
                    Some("/attach deny")
                );
            } else {
                assert!(app.remote_grant.is_none());
            }
        }
        let mut app = App {
            remote_grant: Some(RemoteGrant {
                details: "manifest".into(),
                choice: 1,
            }),
            ..Default::default()
        };
        assert!(
            app.update(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert!(app.should_quit);
    }

    #[test]
    fn sending_pasted_text_keeps_whitespace_but_local_commands_are_normalized() {
        let pasted = "    first line\n\tsecond line\n\n  third line  \n";
        let mut app = App::default();
        app.insert_input(pasted);
        assert_eq!(key(&mut app, KeyCode::Enter).as_deref(), Some(pasted));
        assert_eq!(app.messages.last().unwrap().content, pasted);
        app.insert_input("  /audit ask  ");
        assert_eq!(key(&mut app, KeyCode::Enter).as_deref(), Some("/audit ask"));
    }

    #[test]
    fn sending_a_request_clears_only_completed_plans() {
        for (status, expected) in [
            ("pending", Some("Plan · 0/1\n○ Inspect")),
            ("in_progress", Some("Plan · 0/1\n→ Inspect")),
            ("completed", None),
        ] {
            let mut app = App::default();
            app.update_plan(
                crate::agent::tools::TaskPlan::parse(serde_json::json!({
                    "plan": [{"step": "Inspect", "status": status}]
                }))
                .unwrap(),
            );
            app.insert_input("next request");
            assert_eq!(
                key(&mut app, KeyCode::Enter).as_deref(),
                Some("next request")
            );
            assert_eq!(app.plan.as_deref(), expected, "{status}");
            assert_eq!(app.messages.last().unwrap().content, "next request");
        }
    }

    #[test]
    fn local_commands_and_unsent_input_keep_completed_plans() {
        for input in ["/language ru", "/audit status", "/sessions", "   "] {
            let mut app = App::default();
            app.update_plan(
                crate::agent::tools::TaskPlan::parse(serde_json::json!({
                    "plan": [{"step": "Inspect", "status": "completed"}]
                }))
                .unwrap(),
            );
            app.insert_input(input);
            assert_eq!(app.plan.as_deref(), Some("Plan · 1/1\n✓ Inspect"));
            key(&mut app, KeyCode::Enter);
            assert_eq!(app.plan.as_deref(), Some("Plan · 1/1\n✓ Inspect"));
            assert!(app.messages.is_empty());
        }
    }

    #[test]
    fn recall_only_replaces_multiline_input_at_its_outer_rows() {
        let mut app = App {
            input_width: 80,
            ..Default::default()
        };
        app.push_user_message("earlier".into());
        app.insert_input("top\nbottom");
        key(&mut app, KeyCode::Up);
        assert_eq!(app.input, "top\nbottom");
        assert!(app.input_cursor <= 3);
        let cursor = app.input_cursor;
        key(&mut app, KeyCode::Up);
        assert_eq!(app.input, "earlier");
        key(&mut app, KeyCode::Down);
        assert_eq!(app.input, "top\nbottom");
        assert_eq!(app.input_cursor, cursor);
    }

    #[test]
    fn double_escape_ignores_repeats_and_an_expired_first_press() {
        let mut app = App::default();
        app.push_user_message("earlier".into());
        key(&mut app, KeyCode::Esc);
        app.update(KeyEvent::new_with_kind(
            KeyCode::Esc,
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        ));
        assert!(app.navigation.selected.is_none());
        app.navigation.escape = Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
        key(&mut app, KeyCode::Esc);
        assert!(app.navigation.selected.is_none());
        key(&mut app, KeyCode::Char('x'));
        key(&mut app, KeyCode::Esc);
        assert!(app.navigation.selected.is_none());
        key(&mut app, KeyCode::Esc);
        assert!(app.navigation.selected.is_some());
    }

    #[test]
    fn history_recall_restores_the_draft_and_does_not_change_messages() {
        let mut app = App {
            input_width: 80,
            ..Default::default()
        };
        app.push_user_message("first".into());
        app.push_kodrik_message("answer".into());
        app.push_user_message("second".into());
        app.insert_input("draft");
        app.input_cursor = 2;
        key(&mut app, KeyCode::Up);
        assert_eq!(app.input, "second");
        key(&mut app, KeyCode::Up);
        assert_eq!(app.input, "first");
        key(&mut app, KeyCode::Up);
        assert_eq!(app.input, "first");
        key(&mut app, KeyCode::Down);
        assert_eq!(app.input, "second");
        key(&mut app, KeyCode::Down);
        assert_eq!(app.input, "draft");
        assert_eq!(app.input_cursor, 2);
        assert_eq!(app.messages.len(), 3);
        assert_eq!(app.messages[0].content, "first");
    }

    #[test]
    fn double_escape_selects_a_user_turn_and_enter_opens_it_for_editing() {
        let mut app = App {
            input_width: 80,
            ..Default::default()
        };
        app.push_user_message("first".into());
        app.push_kodrik_message("answer".into());
        app.push_user_message("second".into());
        app.insert_input("draft");
        key(&mut app, KeyCode::Esc);
        key(&mut app, KeyCode::Esc);
        key(&mut app, KeyCode::Char('k'));
        key(&mut app, KeyCode::Char('j'));
        key(&mut app, KeyCode::Up);
        assert_eq!(key(&mut app, KeyCode::Enter), None);
        assert_eq!(app.input, "first");
        assert_eq!(app.messages.len(), 3);
        app.insert_input(" edited");
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.input, "draft");
        assert_eq!(app.input_cursor, 5);
        key(&mut app, KeyCode::Esc);
        key(&mut app, KeyCode::Esc);
        assert_eq!(key(&mut app, KeyCode::Enter), None);
        app.insert_input(" edited");
        assert_eq!(
            key(&mut app, KeyCode::Enter).as_deref(),
            Some("second edited")
        );
        assert_eq!(
            app.messages.len(),
            3,
            "edits are displayed only after durable admission"
        );
    }

    #[test]
    fn jk_navigates_menus_but_remains_text_in_the_composer() {
        let mut app = App::default();
        key(&mut app, KeyCode::Char('j'));
        key(&mut app, KeyCode::Char('k'));
        app.update(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert_eq!(app.input, "jk\n");
        app.sessions = Some(vec![
            (crate::agent::session::SessionId::new("one"), "one".into()),
            (crate::agent::session::SessionId::new("two"), "two".into()),
        ]);
        key(&mut app, KeyCode::Char('j'));
        assert_eq!(app.session_index, 1);
        key(&mut app, KeyCode::Char('k'));
        assert_eq!(app.session_index, 0);
        app.sessions = None;
        app.input = "/".into();
        app.input_cursor = 1;
        key(&mut app, KeyCode::Char('j'));
        assert_eq!(app.command_suggestion().unwrap().0, "/sessions");
        key(&mut app, KeyCode::Char('k'));
        assert_eq!(app.command_suggestion().unwrap().0, "/new");
    }

    #[test]
    fn f2_leaves_the_composer_active() {
        let mut app = App::default();
        app.insert_input("draft");
        app.input_cursor = 2;
        app.scroll = 7;
        assert_eq!(key(&mut app, KeyCode::F(2)), None);
        app.insert_input("x");
        assert_eq!(app.input, "drxaft");
        assert_eq!(app.input_cursor, 3);
        assert_eq!(app.scroll, 7);
    }

    #[test]
    fn language_commands_are_local_even_while_working() {
        for input in ["/language ru", "/language en", "/language de"] {
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
        assert!(app.input.is_empty());
        key(&mut app, KeyCode::Up);
        assert_eq!(
            key(&mut app, KeyCode::Enter).as_deref(),
            Some("/language en")
        );
    }

    #[test]
    fn slash_enter_executes_selected_command_in_one_press_without_chat_submission() {
        for (input, expected) in [
            ("/n", "/new"),
            ("/ses", "/sessions"),
            ("/aud", "/audit"),
            ("/approvals", "/approvals"),
            ("/approve", "/approvals"),
            ("/deny", "/approvals"),
            ("/approve request-id", "/approve request-id"),
            ("/deny request-id", "/deny request-id"),
        ] {
            let mut app = App::default();
            app.insert_input(input);
            assert_eq!(
                key(&mut app, KeyCode::Enter).as_deref(),
                Some(expected),
                "{input}"
            );
            assert!(app.input.is_empty());
            assert!(app.messages.is_empty());
            assert!(matches!(app.status, Status::Ready));
            assert!(
                app.update(KeyEvent::new_with_kind(
                    KeyCode::Enter,
                    KeyModifiers::NONE,
                    KeyEventKind::Repeat
                ))
                .is_none()
            );
        }
    }

    #[test]
    fn language_picker_requires_separate_press_and_supports_cancel() {
        let mut app = App::default();
        app.insert_input("/lang");
        assert!(key(&mut app, KeyCode::Enter).is_none());
        assert!(app.input.is_empty());
        app.insert_input("ignored paste");
        assert!(app.input.is_empty());
        assert!(
            app.update(KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                KeyEventKind::Repeat
            ))
            .is_none()
        );
        key(&mut app, KeyCode::Down);
        assert_eq!(
            key(&mut app, KeyCode::Enter).as_deref(),
            Some("/language ru")
        );
        app.insert_input("/language");
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Esc);
        assert!(key(&mut app, KeyCode::Enter).is_none());
        assert!(app.messages.is_empty());
    }

    #[test]
    fn session_picker_retry_and_cancel_do_not_change_chat_status() {
        let mut app = App {
            sessions: Some(vec![]),
            sessions_loading: true,
            sessions_error: Some("offline".into()),
            sessions_more: true,
            status: Status::Working,
            ..Default::default()
        };
        assert_eq!(
            key(&mut app, KeyCode::Char('r')).as_deref(),
            Some("/sessions retry")
        );
        assert!(
            app.update(KeyEvent::new_with_kind(
                KeyCode::Char('r'),
                KeyModifiers::NONE,
                KeyEventKind::Repeat
            ))
            .is_none()
        );
        assert!(matches!(app.status, Status::Working));
        assert!(app.messages.is_empty());
        key(&mut app, KeyCode::Esc);
        assert!(app.sessions.is_none());
        assert!(!app.sessions_loading);
        assert!(app.sessions_error.is_none());
        assert!(!app.sessions_more);
    }

    #[test]
    fn remote_commands_are_suggested_and_execute_only_in_remote_mode() {
        for command in ["/attach", "/detach", "/older", "/retry"] {
            let mut app = App::default();
            app.insert_input(command);
            assert!(app.command_suggestions().is_empty());
            app.remote_environment = Some("laptop".into());
            assert_eq!(app.command_suggestion().unwrap().0, command);
            assert_eq!(key(&mut app, KeyCode::Enter).as_deref(), Some(command));
            assert!(app.messages.is_empty());
            assert!(matches!(app.status, Status::Ready));
        }
    }

    #[test]
    fn sessions_command_and_picker_navigation() {
        let mut app = App::default();
        app.insert_input("/");
        assert_eq!(app.command_suggestions().len(), 7);
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
        assert_eq!(key(&mut app, KeyCode::Enter).as_deref(), Some("/sessions"));
        app.status = Status::Working;
        app.insert_input("/ses");
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
        for accept in [KeyCode::Tab] {
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
    fn command_approval_input_is_local_even_during_recovery() {
        for status in [Status::Ready, Status::Working, Status::Recovering] {
            let mut app = App {
                status,
                ..Default::default()
            };
            for input in [
                "/approve request-id",
                "/deny request-id",
                "/approvals",
                "/audit off",
                "/audit on",
                "/audit status",
            ] {
                app.insert_input(input);
                assert_eq!(key(&mut app, KeyCode::Enter).as_deref(), Some(input));
                assert!(
                    app.messages.is_empty(),
                    "approval controls must not become model input"
                );
            }
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
