use std::{io, panic};

use crossterm::{
    SynchronizedUpdate,
    cursor::Show,
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags,
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
        let previous_hook = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let _ = reset();
            previous_hook(info);
        }));
        terminal::enable_raw_mode()?;
        let result = (|| {
            execute!(io::stderr(), EnterAlternateScreen)?;
            enable_input_modes(io::stderr())?;
            let mut terminal = Terminal::new(CrosstermBackend::new(io::stderr()))?;
            terminal.hide_cursor()?;
            Ok(Self { terminal })
        })();
        if result.is_err() {
            let _ = reset();
        }
        result
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

    pub fn copy_to_terminal(&mut self, sequence: &str) -> io::Result<()> {
        use io::Write;
        let mut output = io::stderr().lock();
        output.write_all(sequence.as_bytes())?;
        output.flush()
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
        Print("\x1b[?1007l"),
        EnableMouseCapture,
        // Select button tracking last; resetting 1003 disables all tracking in Ghostty.
        Print("\x1b[?1002h"),
        EnableFocusChange,
        EnableBracketedPaste,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
}

fn disable_input_modes(mut output: impl io::Write) -> io::Result<()> {
    execute!(
        output,
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        DisableMouseCapture,
        DisableFocusChange,
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
    fn ghostty_finishes_in_button_tracking_with_sgr_coordinates() {
        // Ghostty's stream handler sets tracking to None on ANY mouse-mode reset.
        // See ghostty v1.4.0 src/terminal/stream_terminal.zig, mouse_event_* cases.
        let mut output = Vec::new();
        enable_input_modes(&mut output).unwrap();
        let mut tracking = None;
        let mut sgr = false;
        for sequence in output.split(|byte| *byte == 0x1b) {
            let Some(sequence) = sequence.strip_prefix(b"[?") else {
                continue;
            };
            let Some(final_index) = sequence.iter().position(|byte| matches!(byte, b'h' | b'l'))
            else {
                continue;
            };
            let enabled = sequence[final_index] == b'h';
            match &sequence[..final_index] {
                b"9" | b"1000" | b"1002" | b"1003" => {
                    tracking = enabled.then_some(sequence[..final_index].to_vec());
                }
                b"1006" => sgr = enabled,
                _ => {}
            }
        }
        assert_eq!(tracking.as_deref(), Some(b"1002".as_slice()));
        assert!(sgr);
    }

    #[test]
    fn mouse_and_focus_reporting_are_enabled_and_reset_on_exit() {
        let mut output = Vec::new();
        enable_input_modes(&mut output).unwrap();
        let enabled = String::from_utf8(output.clone()).unwrap();
        assert!(enabled.contains("\x1b[?1007l"));
        for mode in [1000, 1002, 1006, 1004] {
            assert!(enabled.contains(&format!("\x1b[?{mode}h")));
        }

        output.clear();
        disable_input_modes(&mut output).unwrap();
        let disabled = String::from_utf8(output).unwrap();
        assert!(disabled.contains("\x1b[?1007l"));
        for mode in [1000, 1002, 1003, 1006, 1004] {
            assert!(disabled.contains(&format!("\x1b[?{mode}l")));
        }

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
