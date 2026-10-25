use serde_json::{Value, json};

// teloxide 0.17 drops rich_message; preserve its readable content before decoding.
pub(super) fn normalize(message: &mut Value) {
    if !message["text"].is_string()
        && !message["caption"].is_string()
        && message["rich_message"].is_object()
    {
        let text = content(&message["rich_message"]);
        if !text.is_empty() {
            message["text"] = json!(text);
        }
    }
    if let Some(reply) = message.get_mut("reply_to_message") {
        normalize(reply);
    }
}

fn content(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts.iter().map(content).collect(),
        Value::Object(object) => {
            let mut parts = Vec::new();
            for key in [
                "summary",
                "label",
                "text",
                "alternative_text",
                "expression",
                "button",
            ] {
                if let Some(value) = object.get(key) {
                    parts.push(content(value));
                }
            }
            for (key, separator) in [("blocks", "\n\n"), ("items", "\n"), ("buttons", " ")] {
                if let Some(values) = object.get(key).and_then(Value::as_array) {
                    parts.push(
                        values
                            .iter()
                            .map(content)
                            .filter(|text| !text.is_empty())
                            .collect::<Vec<_>>()
                            .join(separator),
                    );
                }
            }
            if let Some(rows) = object.get("cells").and_then(Value::as_array) {
                parts.push(
                    rows.iter()
                        .filter_map(Value::as_array)
                        .map(|row| row.iter().map(content).collect::<Vec<_>>().join("\t"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
            for key in ["caption", "credit"] {
                if let Some(value) = object.get(key) {
                    parts.push(content(value));
                }
            }
            let mut text = parts
                .into_iter()
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n");
            if let Some(url) = object.get("url").and_then(Value::as_str)
                && url != text
            {
                text.push_str(&format!(" ({url})"));
            }
            if text.is_empty()
                && object.contains_key("type")
                && !matches!(value["type"].as_str(), Some("divider" | "anchor"))
            {
                // shortcut: unknown rich nodes stay as JSON; add rendering when they need readable formatting.
                return value.to_string();
            }
            text
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rich_content_keeps_nested_text_links_lists_tables_and_captions() {
        let mut message = json!({"rich_message":{"blocks":[
            {"type":"paragraph","text":[{"type":"custom_emoji","custom_emoji_id":"123","alternative_text":"🙂"}," ",{"type":"bold","text":["Привет ",{"type":"italic","text":"мир"}]}," ",{"type":"url","text":"Источник","url":"https://example.com"}]},
            {"type":"list","items":[{"label":"1.","blocks":[{"type":"paragraph","text":"Первый пункт"}]}]},
            {"type":"table","cells":[[{"text":"Имя"},{"text":"Значение"}],[{"text":"A"},{"text":"42"}]]},
            {"type":"blockquote","blocks":[{"type":"paragraph","text":"Цитата"}],"credit":"Автор"},
            {"type":"photo","caption":{"text":"Подпись","credit":"Фотограф"}},
            {"type":"mathematical_expression","expression":"x=1"},
            {"type":"divider"}
        ]}});
        normalize(&mut message);
        assert_eq!(
            message["text"],
            "🙂 Привет мир Источник (https://example.com)\n\n1.\n\nПервый пункт\n\nИмя\tЗначение\nA\t42\n\nЦитата\n\nАвтор\n\nПодпись\n\nФотограф\n\nx=1"
        );
    }

    #[test]
    fn rich_normalization_preserves_plain_messages_and_ignores_empty_rich_blocks() {
        for field in ["text", "caption"] {
            let mut message = json!({field:"Original","rich_message":{"blocks":[{"type":"paragraph","text":"Other"}]}});
            normalize(&mut message);
            assert_eq!(message[field], "Original");
            if field == "caption" {
                assert!(message.get("text").is_none());
            }
        }
        for blocks in [
            json!([]),
            json!([{"type":"divider"},{"type":"anchor","name":"top"}]),
        ] {
            let mut message = json!({"rich_message":{"blocks":blocks}});
            normalize(&mut message);
            assert!(message.get("text").is_none(), "{message}");
        }
    }

    #[test]
    fn unknown_rich_nodes_preserve_their_content() {
        let mut message =
            json!({"rich_message":{"blocks":[{"type":"future","new_content":"Visible content"}]}});
        normalize(&mut message);
        let preserved: Value = serde_json::from_str(message["text"].as_str().unwrap()).unwrap();
        assert_eq!(preserved["new_content"], "Visible content");
    }
}
