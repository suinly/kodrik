use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};

use super::{
    app::{AppMessage, AppMessageRole},
    ui,
};

#[derive(Clone)]
struct Row {
    display: Line<'static>,
    source: Arc<str>,
    range: Range<usize>,
    x: u16,
    semantic: bool,
}

impl Row {
    fn capture(buffer: &Buffer, y: u16) -> Self {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut text = String::new();
        let mut x = buffer.area.x;
        while x < buffer.area.right() {
            let cell = &buffer[(x, y)];
            let symbol = cell.symbol();
            let style = cell.style();
            if let Some(last) = spans.last_mut().filter(|last| last.style == style) {
                last.content.to_mut().push_str(symbol);
            } else {
                spans.push(Span::styled(symbol.to_owned(), style));
            }
            text.push_str(symbol);
            x = x.saturating_add(Span::raw(symbol).width().max(1) as u16);
        }
        let source: Arc<str> = Arc::from(text.trim_end_matches(' '));
        Self {
            display: Line::from(spans),
            range: 0..source.len(),
            source,
            x: buffer.area.x,
            semantic: false,
        }
    }

    fn byte_at(&self, column: u16, end: bool) -> usize {
        if column < self.x {
            return self.range.start;
        }
        let target = column.saturating_sub(self.x) as usize;
        let span = Span::raw(&self.source[self.range.clone()]);
        let mut cells = 0;
        let mut byte = self.range.start;
        for grapheme in span.styled_graphemes(Style::default()) {
            byte += self.source[byte..self.range.end]
                .find(grapheme.symbol)
                .unwrap();
            let width = Span::raw(grapheme.symbol).width();
            if cells + width > target {
                return if end {
                    byte + grapheme.symbol.len()
                } else {
                    byte
                };
            }
            cells += width;
            byte += grapheme.symbol.len();
        }
        self.range.end
    }

    fn snap_column(&self, column: u16) -> u16 {
        if column < self.x {
            return column;
        }
        let mut x = self.x;
        for grapheme in
            Span::raw(&self.source[self.range.clone()]).styled_graphemes(Style::default())
        {
            let width = Span::raw(grapheme.symbol).width() as u16;
            if column >= x && column < x.saturating_add(width) {
                return x;
            }
            x = x.saturating_add(width);
        }
        column
    }

    fn blank(width: u16, bg: Color) -> Self {
        Self {
            display: Line::styled(" ".repeat(width as usize), Style::default().bg(bg)),
            source: Arc::from(""),
            range: 0..0,
            x: 0,
            semantic: false,
        }
    }
}

struct Block {
    content: String,
    role: Option<AppMessageRole>,
    rows: Vec<Row>,
}

fn message_block(message: &AppMessage, width: u16) -> Block {
    let text = ui::message_text(message);
    let paragraph = Paragraph::new(text.clone()).wrap(Wrap { trim: false });
    let height = ui::message_height(message, &paragraph, width);
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    ui::render_message(message, &paragraph, &mut buffer);
    let user = matches!(message.role, AppMessageRole::User);
    let mut rows = Vec::new();
    if user {
        rows.push(Row::blank(width, Color::Reset));
    }
    rows.extend((0..height).map(|y| Row::capture(&buffer, y)));

    // Keep the original logical line, including whitespace discarded at a wrap.
    let content_width = width.saturating_sub(3).max(1);
    let mut y = 1 + usize::from(user);
    for line in text.lines {
        let source: Arc<str> = Arc::from(
            line.styled_graphemes(Style::default())
                .map(|grapheme| grapheme.symbol)
                .collect::<String>(),
        );
        let count = Paragraph::new(line)
            .wrap(Wrap { trim: false })
            .line_count(content_width);
        let mut offset = 0;
        for _ in 0..count {
            let Some(row) = rows.get_mut(y) else { break };
            let mut column = 0;
            let visible = row
                .display
                .styled_graphemes(Style::default())
                .filter_map(|grapheme| {
                    let x = column;
                    column += Span::raw(grapheme.symbol).width();
                    (x >= 3).then_some(grapheme.symbol)
                })
                .collect::<String>();
            let visible = visible.trim_end_matches(' ');
            let start = source[offset..]
                .find(visible)
                .map_or(offset, |index| offset + index);
            let end = (start + visible.len()).min(source.len());
            row.source = source.clone();
            row.range = start..end;
            row.x = 3;
            row.semantic = true;
            offset = end;
            y += 1;
        }
    }
    Block {
        content: message.content.clone(),
        role: Some(message.role),
        rows,
    }
}

#[derive(Clone, Default)]
struct Document {
    blocks: Vec<Arc<Block>>,
    ends: Vec<usize>,
    footer: Vec<Row>,
}

impl Document {
    fn history_len(&self) -> usize {
        self.ends.last().copied().unwrap_or_default()
    }

    fn row(&self, index: usize) -> Option<&Row> {
        let block = self.ends.partition_point(|end| *end <= index);
        if let Some(entry) = self.blocks.get(block) {
            let start = if block == 0 { 0 } else { self.ends[block - 1] };
            entry.rows.get(index - start)
        } else {
            self.footer.get(index - self.history_len())
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Point {
    row: usize,
    column: u16,
}

struct Selection {
    id: u64,
    document: Document,
    anchor: Point,
    end: Point,
    dragging: bool,
    moved: bool,
    pointer: Position,
}

pub struct InputRows<'a> {
    pub text: &'a str,
    pub rows: &'a [Range<usize>],
    pub area: Rect,
    pub scroll: usize,
}

#[derive(Default)]
pub struct View {
    document: Document,
    selection: Option<Selection>,
    area: Rect,
    history_height: u16,
    top: usize,
    padding: u16,
    last_scroll: usize,
    cached_width: u16,
    picker: bool,
}

impl View {
    pub fn selection_id(&self) -> Option<u64> {
        self.selection.as_ref().map(|selection| selection.id)
    }

    pub fn clear_copied_selection(&mut self, id: Option<u64>) {
        if id.is_some() && self.selection_id() == id {
            self.clear_selection();
        }
    }

    pub fn clear_selection(&mut self) -> bool {
        self.selection.take().is_some()
    }

    pub fn prepare(
        &mut self,
        messages: &[AppMessage],
        area: Rect,
        height: u16,
        scroll: &mut usize,
        picker: bool,
    ) {
        let resized = self.area != area;
        if resized || self.picker != picker {
            self.clear_selection();
        }
        let width_changed = self.cached_width != area.width;
        self.area = area;
        self.history_height = height;
        self.picker = picker;
        if picker {
            self.top = 0;
            self.padding = 0;
            return;
        }
        if self.selection.is_none() {
            let previous = self.document.history_len();
            if width_changed || picker {
                self.document.blocks.clear();
            }
            self.cached_width = area.width;
            if !picker && area.width > 0 {
                if self.document.blocks.is_empty() {
                    let mut logo = Buffer::empty(Rect::new(0, 0, area.width, 7));
                    ui::render_logo(&mut logo);
                    self.document.blocks.push(Arc::new(Block {
                        content: String::new(),
                        role: None,
                        rows: (0..7).map(|y| Row::capture(&logo, y)).collect(),
                    }));
                }
                for (index, message) in messages.iter().enumerate() {
                    let cached = self.document.blocks.get(index + 1);
                    if cached.is_some_and(|entry| {
                        entry.role == Some(message.role) && entry.content == message.content
                    }) {
                        continue;
                    }
                    let entry = Arc::new(message_block(message, area.width));
                    if index + 1 < self.document.blocks.len() {
                        self.document.blocks[index + 1] = entry;
                    } else {
                        self.document.blocks.push(entry);
                    }
                }
                self.document.blocks.truncate(messages.len() + 1);
            }
            let mut total = 0;
            self.document.ends = self
                .document
                .blocks
                .iter()
                .map(|entry| {
                    total += entry.rows.len();
                    total
                })
                .collect();
            if !resized && *scroll > 0 && *scroll == self.last_scroll {
                *scroll = scroll.saturating_add(total).saturating_sub(previous);
            }
        }
        self.position(scroll);
    }

    fn active_document(&self) -> &Document {
        self.selection
            .as_ref()
            .map_or(&self.document, |selection| &selection.document)
    }

    fn position(&mut self, scroll: &mut usize) {
        let len = self.active_document().history_len();
        let height = self.history_height as usize;
        *scroll = (*scroll).min(len.saturating_sub(height));
        self.top = len.saturating_sub(height + *scroll);
        self.padding = height.saturating_sub(len).min(u16::MAX as usize) as u16;
        self.last_scroll = *scroll;
    }

    pub fn render_history(&self, buffer: &mut Buffer) {
        for y in self.padding..self.history_height {
            if let Some(row) = self
                .active_document()
                .row(self.top + usize::from(y - self.padding))
            {
                (&row.display).render(
                    Rect::new(self.area.x, self.area.y + y, self.area.width, 1),
                    buffer,
                );
            }
        }
    }

    pub fn restore_footer(&self, buffer: &mut Buffer) {
        let footer_y = self.area.y + self.history_height;
        if let Some(selection) = &self.selection {
            for (index, row) in selection.document.footer.iter().enumerate() {
                let y = footer_y + index as u16;
                if y < self.area.bottom() {
                    (&row.display).render(Rect::new(self.area.x, y, self.area.width, 1), buffer);
                }
            }
        }
    }

    pub fn finish_frame(&mut self, buffer: &mut Buffer, input: Option<InputRows<'_>>) {
        let footer_y = self.area.y + self.history_height;
        self.document.footer = (footer_y..self.area.bottom())
            .map(|y| Row::capture(buffer, y))
            .collect();
        if let Some(input) = input.filter(|input| !input.text.is_empty()) {
            let source: Arc<str> = Arc::from(input.text);
            for index in 0..input.area.height as usize {
                if let Some(range) = input.rows.get(input.scroll + index)
                    && let Some(row) = self
                        .document
                        .footer
                        .get_mut(usize::from(input.area.y.saturating_sub(footer_y)) + index)
                {
                    row.source = source.clone();
                    row.range = range.clone();
                    row.x = input.area.x;
                    row.semantic = true;
                }
            }
        }
        if let Some(selection) = &self.selection {
            let (start, end) = ordered(selection.anchor, selection.end);
            if !selection.moved {
                return;
            }
            for y in self.area.y..self.area.bottom() {
                let Some(point) = self.point(Position::new(self.area.x, y)) else {
                    continue;
                };
                if point.row < start.row || point.row > end.row {
                    continue;
                }
                let from = if point.row == start.row {
                    start.column
                } else {
                    self.area.x
                };
                let to = if point.row == end.row {
                    let width = Span::raw(buffer[(end.column, y)].symbol()).width().max(1) as u16;
                    end.column.saturating_add(width).min(self.area.right())
                } else {
                    self.area.right()
                };
                for x in from..to {
                    buffer[(x, y)]
                        .set_bg(Color::Rgb(69, 87, 125))
                        .set_fg(Color::White);
                }
            }
        }
    }

    fn point(&self, position: Position) -> Option<Point> {
        if self.area.width == 0 || self.area.height == 0 {
            return None;
        }
        let x = position.x.clamp(self.area.x, self.area.right() - 1);
        let y = position.y.clamp(self.area.y, self.area.bottom() - 1) - self.area.y;
        let row = if y < self.history_height {
            if y < self.padding {
                return None;
            }
            self.top + usize::from(y - self.padding)
        } else {
            self.active_document().history_len() + usize::from(y - self.history_height)
        };
        let column = self
            .active_document()
            .row(row)
            .map_or(x, |row| row.snap_column(x));
        Some(Point { row, column })
    }

    pub fn selected_text(&self) -> Option<String> {
        let selection = self
            .selection
            .as_ref()
            .filter(|selection| selection.moved)?;
        let (start, end) = ordered(selection.anchor, selection.end);
        let history_len = selection.document.history_len();
        let visible_end =
            (self.top + usize::from(self.history_height - self.padding)).min(history_len);
        let mut text = String::new();
        let mut previous: Option<(&Arc<str>, usize)> = None;
        let mut rows = (start.row..=end.row)
            .filter(|index| end.row < history_len || !(visible_end..history_len).contains(index))
            .filter_map(|index| selection.document.row(index).map(|row| (index, row)))
            .collect::<Vec<_>>();
        while rows
            .last()
            .is_some_and(|(_, row)| !row.semantic && row.source.is_empty())
        {
            rows.pop();
        }
        for (index, row) in rows {
            if previous.is_none() && !row.semantic && row.source.is_empty() {
                continue;
            }
            let from = if index == start.row {
                row.byte_at(start.column, false)
            } else {
                row.range.start
            };
            let to = if index == end.row {
                row.byte_at(end.column, true)
            } else {
                row.range.end
            };
            if let Some((source, previous_end)) = previous {
                if Arc::ptr_eq(source, &row.source) && previous_end <= from {
                    text.push_str(&row.source[previous_end..from]);
                } else {
                    text.push('\n');
                }
            }
            text.push_str(&row.source[from..to]);
            previous = Some((&row.source, to));
        }
        (!text.is_empty()).then_some(text)
    }

    pub fn mouse(&mut self, event: MouseEvent, scroll: &mut usize) -> Option<String> {
        let pointer = Position::new(event.column, event.row);
        match event.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if !self.picker => {
                *scroll = if event.kind == MouseEventKind::ScrollUp {
                    scroll.saturating_add(3)
                } else {
                    scroll.saturating_sub(3)
                };
                self.position(scroll);
                self.drag_to(pointer);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.clear_selection();
                if let Some(point) = self.point(pointer) {
                    self.selection = Some(Selection {
                        id: {
                            static NEXT_ID: AtomicU64 = AtomicU64::new(1);
                            NEXT_ID.fetch_add(1, Ordering::Relaxed)
                        },
                        document: self.document.clone(),
                        anchor: point,
                        end: point,
                        dragging: true,
                        moved: false,
                        pointer,
                    });
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => self.drag_to(pointer),
            MouseEventKind::Up(MouseButton::Left) => {
                self.drag_to(pointer);
                let text = self.selected_text();
                if let Some(selection) = &mut self.selection {
                    selection.dragging = false;
                }
                if text.is_none() {
                    self.clear_selection();
                }
                return text;
            }
            _ => {}
        }
        None
    }

    fn drag_to(&mut self, pointer: Position) {
        let point = self.point(pointer);
        if let Some(selection) = self
            .selection
            .as_mut()
            .filter(|selection| selection.dragging)
        {
            selection.moved |= selection.pointer != pointer;
            selection.pointer = pointer;
            if let Some(point) = point {
                selection.end = point;
                selection.moved |= selection.anchor != point;
            }
        }
    }

    pub fn tick(&mut self, scroll: &mut usize) -> bool {
        let Some(selection) = self
            .selection
            .as_ref()
            .filter(|selection| selection.dragging && selection.moved)
        else {
            return false;
        };
        if (selection.anchor.row >= selection.document.history_len()
            && selection.end.row >= selection.document.history_len())
            || self.history_height == 0
        {
            return false;
        }
        let pointer = selection.pointer;
        let before = *scroll;
        if pointer.y <= self.area.y {
            *scroll = scroll.saturating_add(1);
        } else if pointer.y == self.area.y + self.history_height - 1 {
            *scroll = scroll.saturating_sub(1);
        }
        self.position(scroll);
        self.drag_to(pointer);
        *scroll != before
    }
}

fn ordered(a: Point, b: Point) -> (Point, Point) {
    if a <= b { (a, b) } else { (b, a) }
}

#[cfg(test)]
mod tests {
    use super::super::{app::App, ui::render};
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    #[ignore = "Local selection rendering profile; run explicitly with --ignored --nocapture"]
    fn selection_render_profile() {
        for width in [160, 320] {
            let mut app = App::default();
            app.push_kodrik_message("Ж界🙂e\u{301} alpha beta ".repeat(2000));
            let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
            terminal.draw(|frame| render(&mut app, frame)).unwrap();
            mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 3, 0);
            mouse(
                &mut app,
                MouseEventKind::Drag(MouseButton::Left),
                width - 2,
                30,
            );
            let started = std::time::Instant::now();
            for _ in 0..10 {
                terminal.draw(|frame| render(&mut app, frame)).unwrap();
            }
            eprintln!(
                "selection width={width}: {:.2} ms/frame",
                started.elapsed().as_secs_f64() * 100.0
            );
        }
    }

    fn mouse(app: &mut App, kind: MouseEventKind, x: u16, y: u16) -> Option<String> {
        app.view.mouse(
            MouseEvent {
                kind,
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            },
            &mut app.scroll,
        )
    }

    fn find(terminal: &Terminal<TestBackend>, needle: &str) -> (u16, u16) {
        let buffer = terminal.backend().buffer();
        for y in 0..buffer.area.height {
            let row = Row::capture(buffer, y);
            if let Some(index) = row.source.find(needle) {
                return (Span::raw(&row.source[..index]).width() as u16, y);
            }
        }
        panic!("missing {needle:?} in {buffer:?}");
    }

    fn select(app: &mut App, from: (u16, u16), to: (u16, u16)) -> Option<String> {
        mouse(app, MouseEventKind::Down(MouseButton::Left), from.0, from.1);
        mouse(app, MouseEventKind::Drag(MouseButton::Left), to.0, to.1);
        mouse(app, MouseEventKind::Up(MouseButton::Left), to.0, to.1)
    }

    #[test]
    fn soft_wraps_copy_the_original_spaces_in_both_directions() {
        let mut app = App::default();
        app.push_kodrik_message("alpha beta gamma delta".into());
        let mut terminal = Terminal::new(TestBackend::new(13, 30)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let from = find(&terminal, "alpha");
        let (x, y) = find(&terminal, "delta");
        let to = (x + 4, y);
        assert_eq!(
            select(&mut app, from, to).as_deref(),
            Some("alpha beta gamma delta")
        );
        assert_eq!(
            select(&mut app, to, from).as_deref(),
            Some("alpha beta gamma delta")
        );
    }

    #[test]
    fn wrapping_preserves_multiple_spaces_and_unicode_at_various_widths() {
        let content = "alpha  beta   界🙂 e\u{301} omega";
        for width in [10, 11, 12, 15, 20, 40] {
            let mut app = App::default();
            app.messages.push(AppMessage {
                role: AppMessageRole::Plan,
                content: content.into(),
            });
            let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
            terminal.draw(|frame| render(&mut app, frame)).unwrap();
            let from = find(&terminal, "alpha");
            let (x, y) = find(&terminal, "omega");
            assert_eq!(
                select(&mut app, from, (x + 4, y)).as_deref(),
                Some(content),
                "width {width}"
            );
        }
    }

    #[test]
    fn input_selection_keeps_soft_wraps_and_explicit_newlines() {
        let mut app = App::default();
        let input = "abcdefghijк界🙂klmn\nlast";
        app.insert_input(input);
        let mut terminal = Terminal::new(TestBackend::new(11, 30)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let from = find(&terminal, "abcdefgh");
        let (x, y) = find(&terminal, "last");
        assert_eq!(select(&mut app, from, (x + 3, y)).as_deref(), Some(input));
        assert_eq!(app.input, input);
    }

    #[test]
    fn wide_character_selection_highlights_and_copies_whole_graphemes() {
        let mut app = App::default();
        app.push_kodrik_message("Ж界🙂e\u{301}".into());
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let (x, y) = find(&terminal, "Ж");
        assert_eq!(
            select(&mut app, (x + 2, y), (x + 4, y)).as_deref(),
            Some("界🙂")
        );
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert_eq!(
            terminal.backend().buffer()[(x + 1, y)].bg,
            Color::Rgb(69, 87, 125)
        );
    }

    #[test]
    fn click_does_not_copy_and_streaming_cannot_change_selected_text() {
        let mut app = App::default();
        app.push_kodrik_message("original".into());
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let (x, y) = find(&terminal, "original");
        assert_eq!(select(&mut app, (x, y), (x, y)), None);
        assert_eq!(
            select(&mut app, (x, y), (x + 7, y)).as_deref(),
            Some("original")
        );
        app.messages[0].content = "updated reply".into();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert_eq!(app.view.selected_text().as_deref(), Some("original"));
        assert_eq!(find(&terminal, "original"), (x, y));
        app.view.clear_selection();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        find(&terminal, "updated reply");
    }

    #[test]
    fn wheel_scroll_is_bounded_and_does_not_move_input_cursor() {
        let mut app = App::default();
        for index in 0..30 {
            app.push_kodrik_message(format!("row {index}"));
        }
        app.insert_input("draft");
        let cursor = app.input_cursor;
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        for _ in 0..100 {
            mouse(&mut app, MouseEventKind::ScrollUp, 10, 10);
        }
        let top = app.scroll;
        assert!(top > 0);
        mouse(&mut app, MouseEventKind::ScrollUp, 10, 10);
        assert_eq!(app.scroll, top);
        assert_eq!(app.input_cursor, cursor);
        for _ in 0..100 {
            mouse(&mut app, MouseEventKind::ScrollDown, 10, 10);
        }
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn status_and_session_picker_are_selectable() {
        let mut app = App {
            model: "test-model".into(),
            ..Default::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let (x, y) = find(&terminal, "test-model");
        assert_eq!(
            select(&mut app, (x, y), (x + 9, y)).as_deref(),
            Some("test-model")
        );
        app.sessions = Some(vec![(
            crate::agent::session::SessionId::new("session-id"),
            "session title".into(),
        )]);
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let (x, y) = find(&terminal, "session title");
        assert_eq!(
            select(&mut app, (x, y), (x + 12, y)).as_deref(),
            Some("session title")
        );
    }

    #[test]
    fn crossing_into_footer_excludes_hidden_newer_messages() {
        let mut app = App::default();
        for index in 0..30 {
            app.push_kodrik_message(format!("row {index:02}"));
        }
        app.insert_input("draft");
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        app.scroll = 20;
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let from = find(&terminal, "row 19");
        let (x, y) = find(&terminal, "draft");
        let text = select(&mut app, from, (x + 4, y)).unwrap();
        assert!(text.starts_with("row 19"), "{text}");
        assert!(text.ends_with("draft"), "{text}");
        assert!(!text.contains("row 20"), "{text}");
    }

    #[test]
    fn edge_tick_scrolls_a_drag_and_resize_cancels_it() {
        let mut app = App::default();
        for index in 0..30 {
            app.push_kodrik_message(format!("row {index}"));
        }
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 5, 5);
        mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 5, 0);
        assert!(app.view.tick(&mut app.scroll));
        assert_eq!(app.scroll, 1);
        assert!(app.view.selected_text().is_some());
        let mut resized = Terminal::new(TestBackend::new(20, 10)).unwrap();
        resized.draw(|frame| render(&mut app, frame)).unwrap();
        assert!(app.view.selected_text().is_none());
    }

    #[test]
    fn dragging_from_input_into_history_can_autoscroll() {
        let mut app = App::default();
        for index in 0..30 {
            app.push_kodrik_message(format!("row {index}"));
        }
        app.insert_input("draft");
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let (x, y) = find(&terminal, "draft");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x, y);
        mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 5, 0);
        assert!(app.view.tick(&mut app.scroll));
        assert_eq!(app.scroll, 1);
    }

    #[test]
    fn dragging_across_one_wide_character_copies_it() {
        let mut app = App::default();
        app.push_kodrik_message("界".into());
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let (x, y) = find(&terminal, "界");
        assert_eq!(select(&mut app, (x, y), (x + 1, y)).as_deref(), Some("界"));
    }

    #[test]
    fn pasted_tabs_before_unicode_do_not_break_selection_byte_boundaries() {
        let mut app = App::default();
        app.insert_input("a\t界🙂");
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let (x, y) = find(&terminal, "a界🙂");
        assert_eq!(
            select(&mut app, (x, y), (x + 4, y)).as_deref(),
            Some("a\t界🙂")
        );
    }

    #[test]
    fn explicit_leading_and_trailing_newlines_in_input_are_preserved() {
        let mut app = App::default();
        app.insert_input("\nhello\n");
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let (x, y) = find(&terminal, "hello");
        assert_eq!(
            select(&mut app, (x, y - 1), (x, y + 1)).as_deref(),
            Some("\nhello\n")
        );
    }

    #[test]
    fn tool_rows_copy_indentation_and_menu_descriptions_are_selectable() {
        let mut app = App::default();
        app.push_tool_message("run\n    界🙂\nnext".into());
        let mut terminal = Terminal::new(TestBackend::new(60, 25)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let from = find(&terminal, "run");
        let (x, y) = find(&terminal, "next");
        assert_eq!(
            select(&mut app, from, (x + 3, y)).as_deref(),
            Some("run\n    界🙂\nnext")
        );
        app.view.clear_selection();
        app.insert_input("/");
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let (x, y) = find(&terminal, "Start a new session");
        assert_eq!(
            select(&mut app, (x, y), (x + 18, y)).as_deref(),
            Some("Start a new session")
        );
    }

    #[test]
    fn copy_notice_is_selectable_on_the_next_gesture() {
        let mut app = App::default();
        app.push_kodrik_message("answer".into());
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let (x, y) = find(&terminal, "answer");
        select(&mut app, (x, y), (x + 5, y));
        let id = app.view.selection_id();
        super::super::complete_copy(
            &mut app,
            id,
            Ok(super::super::clipboard::CopyOutcome::Confirmed),
            |_| Ok(()),
        );
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let (x, y) = find(&terminal, "Copied");
        assert_eq!(
            select(&mut app, (x, y), (x + 5, y)).as_deref(),
            Some("Copied")
        );
    }

    #[test]
    fn footer_selection_survives_notice_expiry_and_token_updates_consistently() {
        let mut app = App {
            model: "test-model".into(),
            context_tokens: Some(1000),
            ..Default::default()
        };
        app.set_copy_notice("Copied".into());
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let (x, y) = find(&terminal, "1K");
        assert_eq!(select(&mut app, (x, y), (x + 1, y)).as_deref(), Some("1K"));
        app.copy_notice = None;
        app.context_tokens = Some(2000);
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        let row = |terminal: &Terminal<TestBackend>| {
            (0..60)
                .map(|x| terminal.backend().buffer()[(x, 19)].symbol())
                .collect::<String>()
        };
        assert!(row(&terminal).starts_with("   test-model · 1K"));
        assert!(row(&terminal).ends_with("Copied"));
        assert_eq!(app.view.selected_text().as_deref(), Some("1K"));
        app.view.clear_selection();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert_eq!(row(&terminal).trim(), "test-model · 2K");
    }

    #[test]
    fn closing_a_resized_picker_reflows_history_and_preserves_scroll() {
        let mut app = App::default();
        for index in 0..30 {
            app.push_kodrik_message(format!("message {index:02} has a long line"));
        }
        let mut terminal = Terminal::new(TestBackend::new(60, 30)).unwrap();
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        app.scroll = 10;
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        app.sessions = Some(vec![]);
        terminal.draw(|frame| render(&mut app, frame)).unwrap();
        assert_eq!(app.scroll, 10);
        let mut resized = Terminal::new(TestBackend::new(15, 30)).unwrap();
        resized.draw(|frame| render(&mut app, frame)).unwrap();
        app.sessions = None;
        resized.draw(|frame| render(&mut app, frame)).unwrap();
        let (x, y) = find(&resized, "has a long");
        assert_eq!(
            select(&mut app, (x, y), (x + 9, y)).as_deref(),
            Some("has a long")
        );
    }
}
