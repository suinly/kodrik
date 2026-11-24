use crate::{
    i18n::Language,
    remote::local::{DirectoryDecision, DirectoryRequest},
};
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Color, Style},
    widgets::{Block, List, ListItem, ListState, Paragraph, Wrap},
};
use tokio::sync::mpsc;

pub(super) struct DirectoryPrompts {
    pub(super) active: Option<DirectoryPrompt>,
    requests: mpsc::Receiver<DirectoryRequest>,
}

impl DirectoryPrompts {
    pub(super) fn new(requests: mpsc::Receiver<DirectoryRequest>) -> Self {
        Self {
            active: None,
            requests,
        }
    }

    pub(super) fn refresh(&mut self, show_next: bool) -> bool {
        if self
            .active
            .as_ref()
            .is_some_and(|prompt| prompt.request.reply.is_closed())
        {
            self.active.take();
        }
        if self.active.is_none() && show_next {
            while let Ok(request) = self.requests.try_recv() {
                if !request.reply.is_closed() {
                    self.active = Some(DirectoryPrompt::new(request));
                    return true;
                }
            }
        }
        false
    }

    pub(super) fn input(&mut self, event: &Event, can_allow: bool) {
        self.refresh(false);
        if let Some(prompt) = &mut self.active
            && let Some(decision) = prompt.input(event, can_allow)
            && let Some(prompt) = self.active.take()
        {
            let _ = prompt.request.reply.send(decision);
        }
    }
}

pub(super) struct DirectoryPrompt {
    pub(super) request: DirectoryRequest,
    selected: usize,
}

impl DirectoryPrompt {
    pub(super) fn new(request: DirectoryRequest) -> Self {
        Self {
            request,
            selected: 2,
        }
    }

    pub(super) fn input(&mut self, event: &Event, can_allow: bool) -> Option<DirectoryDecision> {
        let Event::Key(key) = event else { return None };
        if key.kind != KeyEventKind::Press || !key.modifiers.is_empty() {
            return None;
        }
        match key.code {
            KeyCode::Up if can_allow => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down if can_allow => self.selected = (self.selected + 1).min(2),
            KeyCode::Esc => return Some(DirectoryDecision::Deny),
            KeyCode::Enter => {
                return Some(match (can_allow, self.selected) {
                    (true, 0) => DirectoryDecision::Once,
                    (true, 1) => DirectoryDecision::Always,
                    _ => DirectoryDecision::Deny,
                });
            }
            _ => (),
        }
        None
    }

    pub(super) fn render(&self, frame: &mut Frame, language: Language) -> bool {
        let block = Block::bordered()
            .title(language.pick(" Local directory access ", " Доступ к локальному каталогу "));
        let inner = block.inner(frame.area());
        frame.render_widget(block, frame.area());
        let labels = [
            language.pick("Allow once", "Разрешить один раз"),
            language.pick("Always allow", "Разрешать всегда"),
            language.pick("Deny", "Запретить"),
        ];
        // Debug formatting keeps control characters and non-Unicode bytes literal and distinguishable.
        let path = Paragraph::new(format!("{:?}", self.request.path)).wrap(Wrap { trim: false });
        let help = Paragraph::new(language.pick(
            "↑/↓ select · Enter confirm · Esc deny",
            "↑/↓ выбор · Enter подтвердить · Esc отказ",
        ))
        .wrap(Wrap { trim: false });
        let path_rows = path.line_count(inner.width);
        let help_rows = help.line_count(inner.width);
        if labels
            .iter()
            .any(|label| label.chars().count() + 2 > usize::from(inner.width))
            || path_rows + help_rows + labels.len() > usize::from(inner.height)
        {
            frame.render_widget(
                Paragraph::new(language.pick(
                    "Enlarge terminal to review the full path, or press Esc to deny.",
                    "Увеличьте окно, чтобы увидеть весь путь, или нажмите Esc для отказа.",
                ))
                .wrap(Wrap { trim: true }),
                inner,
            );
            return false;
        }
        let rows = Layout::vertical([
            Constraint::Length(path_rows as u16),
            Constraint::Length(help_rows as u16),
            Constraint::Min(3),
        ])
        .split(inner);
        frame.render_widget(path, rows[0]);
        frame.render_widget(help, rows[1]);
        frame.render_stateful_widget(
            List::new(labels.map(ListItem::new))
                .highlight_symbol("> ")
                .highlight_style(Style::default().fg(Color::Yellow).bold()),
            rows[2],
            &mut ListState::default().with_selected(Some(self.selected)),
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};
    use tokio::sync::oneshot;

    fn request(path: &str) -> (DirectoryPrompt, oneshot::Receiver<DirectoryDecision>) {
        let (reply, decision) = oneshot::channel();
        (
            DirectoryPrompt::new(DirectoryRequest {
                path: path.into(),
                reply,
            }),
            decision,
        )
    }

    fn key(code: KeyCode, kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent::new_with_kind(code, KeyModifiers::NONE, kind))
    }

    #[test]
    fn default_enter_and_escape_deny() {
        let (mut prompt, _) = request("/outside");
        assert!(matches!(
            prompt.input(&key(KeyCode::Enter, KeyEventKind::Press), true),
            Some(DirectoryDecision::Deny)
        ));
        prompt.input(&key(KeyCode::Up, KeyEventKind::Press), true);
        assert!(matches!(
            prompt.input(&key(KeyCode::Esc, KeyEventKind::Press), true),
            Some(DirectoryDecision::Deny)
        ));
    }

    #[test]
    fn only_fresh_terminal_presses_can_choose_an_allow_option() {
        let (mut prompt, _) = request("/outside");
        assert!(
            prompt
                .input(&Event::Paste("Allow once\n".into()), true)
                .is_none()
        );
        prompt.input(&key(KeyCode::Up, KeyEventKind::Repeat), true);
        assert!(matches!(
            prompt.input(&key(KeyCode::Enter, KeyEventKind::Press), true),
            Some(DirectoryDecision::Deny)
        ));
        prompt.input(&key(KeyCode::Up, KeyEventKind::Press), true);
        assert!(
            prompt
                .input(&key(KeyCode::Enter, KeyEventKind::Repeat), true)
                .is_none()
        );
        assert!(matches!(
            prompt.input(&key(KeyCode::Enter, KeyEventKind::Press), true),
            Some(DirectoryDecision::Always)
        ));
        prompt.input(&key(KeyCode::Up, KeyEventKind::Press), true);
        assert!(matches!(
            prompt.input(&key(KeyCode::Enter, KeyEventKind::Press), true),
            Some(DirectoryDecision::Once)
        ));
        assert!(matches!(
            prompt.input(&key(KeyCode::Enter, KeyEventKind::Press), false),
            Some(DirectoryDecision::Deny)
        ));
    }

    #[test]
    fn expired_requests_never_replace_an_active_prompt_and_queue_drop_fences_binding() {
        let (sender, receiver) = mpsc::channel(4);
        let mut prompts = DirectoryPrompts::new(receiver);
        let (expired, closed) = request("/expired");
        let (active, mut active_result) = request("/active");
        let (queued, mut queued_result) = request("/queued");
        sender.try_send(expired.request).ok().unwrap();
        sender.try_send(active.request).ok().unwrap();
        sender.try_send(queued.request).ok().unwrap();
        drop(closed);
        assert!(!prompts.refresh(false));
        assert!(prompts.refresh(true));
        assert_eq!(
            prompts.active.as_ref().unwrap().request.path,
            std::path::Path::new("/active")
        );
        assert!(!prompts.refresh(true));
        drop(prompts);
        assert!(
            active_result
                .try_recv()
                .is_err_and(|err| err == oneshot::error::TryRecvError::Closed)
        );
        assert!(
            queued_result
                .try_recv()
                .is_err_and(|err| err == oneshot::error::TryRecvError::Closed)
        );
    }

    #[test]
    fn a_key_from_an_expired_review_cannot_answer_the_next_request() {
        let (sender, receiver) = mpsc::channel(2);
        let mut prompts = DirectoryPrompts::new(receiver);
        let (first, first_result) = request("/first");
        let (next, mut next_result) = request("/next");
        sender.try_send(first.request).ok().unwrap();
        sender.try_send(next.request).ok().unwrap();
        assert!(prompts.refresh(true));
        prompts.input(&key(KeyCode::Up, KeyEventKind::Press), true);
        drop(first_result);
        prompts.input(&key(KeyCode::Enter, KeyEventKind::Press), true);
        assert!(prompts.active.is_none());
        assert!(matches!(
            next_result.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(prompts.refresh(true));
        prompts.input(&key(KeyCode::Enter, KeyEventKind::Press), true);
        assert!(matches!(
            next_result.try_recv(),
            Ok(DirectoryDecision::Deny)
        ));
    }

    #[test]
    fn path_and_choices_render_in_both_languages_and_controls_are_escaped() {
        let (prompt, _) = request("/outside/folder\nnot another option");
        let mut terminal = Terminal::new(TestBackend::new(75, 12)).unwrap();
        for (language, once, always, deny) in [
            (Language::En, "Allow once", "Always allow", "Deny"),
            (
                Language::Ru,
                "Разрешить один раз",
                "Разрешать всегда",
                "Запретить",
            ),
        ] {
            terminal
                .draw(|frame| assert!(prompt.render(frame, language)))
                .unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(text.contains("/outside/folder\\nnot another option"));
            assert!(text.contains(once) && text.contains(always) && text.contains(deny));
        }

        let mut tiny = Terminal::new(TestBackend::new(20, 5)).unwrap();
        tiny.draw(|frame| assert!(!prompt.render(frame, Language::En)))
            .unwrap();
    }
}
