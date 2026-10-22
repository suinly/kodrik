use std::{io, panic};

use crossterm::{
    SynchronizedUpdate,
    cursor::Show,
    event::{
        DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    style::Print,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use super::{app::App, ui};

pub struct Tui {
    terminal: Terminal<CrosstermBackend<io::Stderr>>,
}

impl Tui {
    pub fn enter() -> anyhow::Result<Self> {
        execute!(io::stderr(), EnterAlternateScreen)?;
        let previous_hook = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let _ = reset();
            previous_hook(info);
        }));
        terminal::enable_raw_mode()?;
        enable_input_modes(io::stderr())?;
        let mut terminal = Terminal::new(CrosstermBackend::new(io::stderr()))?;
        terminal.hide_cursor()?;
        Ok(Self { terminal })
    }

    pub fn draw(&mut self, app: &mut App) -> anyhow::Result<()> {
        io::stderr().sync_update(|_| {
            self.terminal
                .draw(|frame| ui::render(app, frame))
                .map(|_| ())
        })??;
        Ok(())
    }

    pub fn clear(&mut self) -> anyhow::Result<()> {
        self.terminal.clear()?;
        Ok(())
    }

    pub fn exit(&mut self) -> anyhow::Result<()> {
        reset()?;
        Ok(())
    }
}

fn reset() -> io::Result<()> {
    if !terminal::is_raw_mode_enabled()? {
        return Ok(());
    }
    let raw_result = terminal::disable_raw_mode();
    disable_input_modes(io::stderr())?;
    raw_result
}

fn enable_input_modes(mut output: impl io::Write) -> io::Result<()> {
    execute!(
        output,
        // Translate the wheel into Up/Down without capturing native text selection.
        Print("\x1b[?1007h"),
        EnableBracketedPaste,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
}

fn disable_input_modes(mut output: impl io::Write) -> io::Result<()> {
    execute!(
        output,
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        Print("\x1b[?1007l"),
        LeaveAlternateScreen,
        Show
    )
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_scrolling_preserves_native_selection_and_is_reset_on_exit() {
        let mut output = Vec::new();
        enable_input_modes(&mut output).unwrap();
        let enabled = String::from_utf8(output.clone()).unwrap();
        assert!(enabled.contains("\x1b[?1007h"));
        for mode in [1000, 1002, 1003, 1006] {
            assert!(!enabled.contains(&format!("\x1b[?{mode}h")));
        }

        output.clear();
        disable_input_modes(&mut output).unwrap();
        let disabled = String::from_utf8(output).unwrap();
        assert!(disabled.contains("\x1b[?1007l"));

        let mut app = App::default();
        for (key, expected) in [
            (crossterm::event::KeyCode::Up, 1),
            (crossterm::event::KeyCode::Down, 0),
            (crossterm::event::KeyCode::Down, 0),
        ] {
            app.update(crossterm::event::KeyEvent::new(
                key,
                crossterm::event::KeyModifiers::NONE,
            ));
            assert_eq!(app.scroll, expected);
        }
    }
}
