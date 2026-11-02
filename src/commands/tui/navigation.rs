use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::app::{App, AppMessageRole, Status};
use crate::agent::session::SessionId;

#[derive(Default)]
pub(super) struct Navigation {
    history: Option<History>,
    draft: Option<(String, usize)>,
    pub selected: Option<usize>,
    pub focus: Option<usize>,
    pub editing: Option<Edit>,
    pub escape: Option<Instant>,
    pub pending_version: Option<String>,
}

struct History {
    user_turn: usize,
    draft: String,
    cursor: usize,
}

#[derive(Clone)]
pub(super) struct Edit {
    pub source: Option<SessionId>,
    pub user_turn: usize,
}

impl Navigation {
    pub fn clear_history(&mut self) {
        self.history = None;
    }
}

impl App {
    pub fn selected_user_turn(&self) -> Option<usize> {
        self.messages
            .get(..self.navigation.selected?)
            .map(|messages| {
                messages
                    .iter()
                    .filter(|message| message.role == AppMessageRole::User)
                    .count()
            })
    }

    pub fn selected_versions(&self) -> Option<&crate::agent::session::branches::MessageVersions> {
        self.versions.get(&self.selected_user_turn()?)
    }

    pub fn select_user_turn(&mut self, user_turn: usize) {
        if let Some((index, _)) = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message.role == AppMessageRole::User)
            .nth(user_turn)
        {
            self.navigation.draft = Some((self.input.clone(), self.input_cursor));
            self.navigation.selected = Some(index);
            self.navigation.focus = Some(index);
            self.view.clear_selection();
        }
    }

    pub fn recall_input(&mut self, previous: bool) -> bool {
        let users = self
            .messages
            .iter()
            .filter(|message| message.role == AppMessageRole::User)
            .collect::<Vec<_>>();
        let current = self
            .navigation
            .history
            .as_ref()
            .map(|history| history.user_turn);
        let next = if previous {
            current.unwrap_or(users.len()).checked_sub(1)
        } else {
            current.map(|current| current + 1)
        }
        .and_then(|index| users.get(index).map(|message| (index, *message)));
        if let Some((index, message)) = next {
            let history = self.navigation.history.get_or_insert_with(|| History {
                user_turn: index,
                draft: self.input.clone(),
                cursor: self.input_cursor,
            });
            history.user_turn = index;
            self.input = message.content.clone();
            self.input_cursor = self.input.len();
        } else if !previous {
            let Some(history) = self.navigation.history.take() else {
                return false;
            };
            self.input = history.draft;
            self.input_cursor = history.cursor;
        } else if current.is_none() {
            return false;
        }
        self.commands_dismissed = true;
        self.command_index = 0;
        true
    }

    pub fn cancel_message_navigation(&mut self) {
        if let Some((draft, cursor)) = self.navigation.draft.take() {
            self.input = draft;
            self.input_cursor = cursor;
        }
        self.navigation.selected = None;
        self.navigation.focus = None;
        self.navigation.editing = None;
        self.navigation.escape = None;
        self.navigation.pending_version = None;
    }

    pub fn update_message_navigation(&mut self, key: KeyEvent) -> bool {
        if let Some(selected) = self.navigation.selected {
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c' | 'C' | 'с' | 'С'))
            {
                self.cancel_message_navigation();
                return false;
            }
            if !key.modifiers.is_empty() {
                return true;
            }
            match key.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Char('h' | 'l') => {
                    self.navigation.pending_version =
                        self.selected_versions().and_then(|versions| {
                            let index = if matches!(key.code, KeyCode::Left | KeyCode::Char('h')) {
                                versions.current.checked_sub(1)
                            } else {
                                Some(versions.current + 1)
                            };
                            index.and_then(|index| versions.ids.get(index)).cloned()
                        });
                }
                KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
                    let previous = matches!(key.code, KeyCode::Up | KeyCode::Char('k'));
                    let next = if previous {
                        self.messages[..selected]
                            .iter()
                            .rposition(|message| message.role == AppMessageRole::User)
                    } else {
                        self.messages
                            .iter()
                            .enumerate()
                            .skip(selected + 1)
                            .find(|(_, message)| message.role == AppMessageRole::User)
                            .map(|(index, _)| index)
                    };
                    if let Some(index) = next {
                        self.navigation.selected = Some(index);
                        self.navigation.focus = Some(index);
                    }
                }
                KeyCode::Enter => {
                    self.input = self.messages[selected].content.clone();
                    self.input_cursor = self.input.len();
                    self.commands_dismissed = true;
                    self.navigation.editing = Some(Edit {
                        source: self.session.clone(),
                        user_turn: self.messages[..selected]
                            .iter()
                            .filter(|message| message.role == AppMessageRole::User)
                            .count(),
                    });
                    self.navigation.selected = None;
                }
                KeyCode::Esc => self.cancel_message_navigation(),
                KeyCode::PageUp => self.scroll = self.scroll.saturating_add(10),
                KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
                _ => {}
            }
            return true;
        }
        if key.code != KeyCode::Esc || !key.modifiers.is_empty() {
            return false;
        }
        if self.navigation.editing.is_some() {
            self.cancel_message_navigation();
            return true;
        }
        if matches!(
            self.status,
            Status::Working | Status::Recovering | Status::Stopping
        ) {
            self.navigation.escape = None;
            return false;
        }
        let now = Instant::now();
        if self
            .navigation
            .escape
            .take()
            .is_some_and(|last| now.duration_since(last) <= Duration::from_millis(500))
        {
            if let Some(turn) = self
                .messages
                .iter()
                .filter(|message| message.role == AppMessageRole::User)
                .count()
                .checked_sub(1)
            {
                self.select_user_turn(turn);
            }
        } else {
            self.navigation.escape = Some(now);
        }
        true
    }
}
