use std::sync::LazyLock;

use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};
use ratatui::text::{Line, Span, Text};

#[derive(Clone, Copy)]
struct MessageStyles;

impl tui_markdown::StyleSheet for MessageStyles {
    fn code_block_fence(&self) -> &str {
        ""
    }
}

const CODE_THEME: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>name</key><string>Kodrik</string><key>settings</key><array>
<dict><key>settings</key><dict><key>foreground</key><string>#CDD6F4</string></dict></dict>
<dict><key>scope</key><string>comment</string><key>settings</key><dict><key>foreground</key><string>#9BA8BD</string></dict></dict>
<dict><key>scope</key><string>keyword, storage</string><key>settings</key><dict><key>foreground</key><string>#CBA6F7</string></dict></dict>
<dict><key>scope</key><string>string</string><key>settings</key><dict><key>foreground</key><string>#A6E3A1</string></dict></dict>
<dict><key>scope</key><string>constant</string><key>settings</key><dict><key>foreground</key><string>#FAB387</string></dict></dict>
<dict><key>scope</key><string>entity.name.function, support.function</string><key>settings</key><dict><key>foreground</key><string>#89DCEB</string></dict></dict>
<dict><key>scope</key><string>entity.name.type, support.type, storage.type</string><key>settings</key><dict><key>foreground</key><string>#F9E2AF</string></dict></dict>
</array></dict></plist>"##;

static OPTIONS: LazyLock<tui_markdown::Options<MessageStyles>> = LazyLock::new(|| {
    tui_markdown::Options::new(MessageStyles).code_theme(
        tui_markdown::CodeTheme::from_textmate(CODE_THEME).expect("valid built-in code theme"),
    )
});

fn highlighted(source: &str) -> Text<'static> {
    let text = tui_markdown::from_str_with_options(source, &OPTIONS);
    Text {
        lines: text
            .lines
            .into_iter()
            .map(|line| Line {
                spans: line
                    .spans
                    .into_iter()
                    .map(|span| Span::styled(span.content.into_owned(), span.style))
                    .collect(),
                style: line.style,
                alignment: line.alignment,
            })
            .collect(),
        style: text.style,
        alignment: text.alignment,
    }
}

pub(super) fn render(source: &str, preserve_text: bool) -> Text<'static> {
    if !preserve_text {
        return highlighted(source);
    }
    let mut lines = Vec::new();
    let mut offset = 0;
    let mut events = Parser::new(source).into_offset_iter();
    while let Some((event, range)) = events.next() {
        let Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(language))) = event else {
            continue;
        };
        let start = source[..range.start]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        lines.extend(
            source[offset..start]
                .split_terminator('\n')
                .map(|line| Line::raw(line.to_owned())),
        );
        let mut code = String::new();
        for (event, _) in events.by_ref() {
            match event {
                Event::Text(text) => code.push_str(&text),
                Event::End(TagEnd::CodeBlock) => break,
                _ => {}
            }
        }
        // A delimiter longer than any run in the code cannot hide a literal fence inside it.
        let fence = "~".repeat(
            code.split(|ch| ch != '~')
                .map(str::len)
                .max()
                .unwrap_or(0)
                .max(2)
                + 1,
        );
        let newline = if code.ends_with('\n') { "" } else { "\n" };
        lines.extend(highlighted(&format!("{fence}{language}\n{code}{newline}{fence}")).lines);
        offset = range.end;
        if source[offset..].starts_with("\r\n") {
            offset += 2;
        } else if source[offset..].starts_with('\n') {
            offset += 1;
        }
    }
    if offset < source.len() || offset == 0 {
        lines.extend(
            source[offset..]
                .split('\n')
                .map(|line| Line::raw(line.to_owned())),
        );
    }
    Text::from(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_blocks_preserve_surrounding_text_and_literal_delimiters() {
        for (source, expected) in [
            (
                "  до 界\nnext\n\n```rust\nlet x = 1;\n```\n\n  после\n",
                "  до 界\nnext\n\nlet x = 1;\n\n  после\n",
            ),
            ("  ```rust\n  let x = 1;\n  ```\nlast", "let x = 1;\nlast"),
            ("> ```rust\n> let x = 1;\n> ```\nlast", "let x = 1;\nlast"),
            ("````text\n```literal\n````", "```literal"),
            ("```text\n~~~\n```\n```text\n  next\n```", "~~~\n  next"),
        ] {
            let text = render(source, true);
            let actual = text
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
            assert_eq!(actual, expected, "{source}");
        }
    }
}
