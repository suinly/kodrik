use super::app::{App, AppMessage};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

// Keep multiline text readable, while retaining all other metadata (including truncation and paths).
pub fn payload(value: &serde_json::Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_owned();
    }
    let mut metadata = value.clone();
    let mut text = String::new();
    for key in ["cmd", "stdout", "stderr", "error"] {
        if let Some(raw) = value.get(key).and_then(serde_json::Value::as_str) {
            text.push_str(&format!("{key}:\n{raw}\n"));
            if let Some(object) = metadata.as_object_mut() {
                object.remove(key);
            }
        }
    }
    text.push_str(&serde_json::to_string_pretty(&metadata).unwrap_or_default());
    text
}

impl App {
    pub fn remove_full_tool(&mut self, index: usize) {
        self.full_tools = std::mem::take(&mut self.full_tools)
            .into_iter()
            .filter_map(|(i, text)| {
                if i == index {
                    None
                } else {
                    Some((if i > index { i - 1 } else { i }, text))
                }
            })
            .collect();
    }

    pub fn tool_details_key(&mut self, key: KeyEvent) -> bool {
        if key.kind != KeyEventKind::Release
            && key.code == KeyCode::Char('t')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.tools_expanded = !self.tools_expanded;
            self.view.clear_selection();
            return true;
        }
        false
    }

    pub fn display_messages(&self) -> Vec<AppMessage> {
        self.messages
            .iter()
            .enumerate()
            .map(|(index, message)| {
                let mut message = message.clone();
                if self.tools_expanded {
                    if let Some(full) = self.full_tools.get(&index) {
                        let header = message.content.lines().next().unwrap_or_default();
                        message.content = format!("{header}\n{full}");
                    }
                }
                message
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inline_toggle_preserves_draft_and_keeps_editing() {
        let mut app = App {
            input: "draft".into(),
            input_cursor: 5,
            scroll: 42,
            ..Default::default()
        };
        app.push_tool_message("compact".into());
        app.full_tools.insert(0, "complete payload".into());
        app.update(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert!(app.tools_expanded);
        assert_eq!(app.input, "draft");
        assert_eq!(app.scroll, 42);
        assert!(
            app.display_messages()[0]
                .content
                .contains("complete payload")
        );
        assert_eq!(app.messages[0].content, "compact");
        app.update(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.input, "draftx");
        app.update(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.tools_expanded, "Escape must retain its normal meaning");
        app.update(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert!(!app.tools_expanded);
        assert_eq!(app.display_messages()[0].content, "compact");
    }

    #[test]
    fn payload_keeps_all_available_data() {
        let error = "failure\n".repeat(500);
        assert!(payload(&serde_json::json!({"error":error})).contains(&error));
        let text = "generic ".repeat(500);
        assert!(payload(&serde_json::json!({"value":text})).contains(&text));
    }
}
