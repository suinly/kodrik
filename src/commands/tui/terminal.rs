use std::{io, panic};

use crossterm::{
    SynchronizedUpdate,
    cursor::Show,
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
#[cfg(not(windows))]
use crossterm::{
    event::{
        DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    style::Print,
};
use ratatui::{Terminal, backend::CrosstermBackend};

use super::{app::App, directory_prompt::DirectoryPrompt, ui};
use crate::i18n::Language;

#[cfg(windows)]
type TerminalOutput = io::BufWriter<io::Stderr>;
#[cfg(not(windows))]
type TerminalOutput = io::Stderr;

pub struct Tui {
    terminal: Terminal<CrosstermBackend<TerminalOutput>>,
}

impl Tui {
    pub fn enter() -> anyhow::Result<Self> {
        Self::enter_with_keys(false)
    }

    pub fn enter_remote() -> anyhow::Result<Self> {
        Self::enter_with_keys(true)
    }

    fn enter_with_keys(report_key_events: bool) -> anyhow::Result<Self> {
        let previous_hook = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let _ = reset();
            previous_hook(info);
        }));
        terminal::enable_raw_mode()?;
        let result = (|| {
            execute!(io::stderr(), EnterAlternateScreen)?;
            enable_input_modes(io::stderr(), report_key_events)?;
            let output = io::stderr();
            // Crossterm formats ANSI commands in pieces; avoid a console write per piece.
            #[cfg(windows)]
            let output = io::BufWriter::new(output);
            let mut terminal = Terminal::new(CrosstermBackend::new(output))?;
            terminal.hide_cursor()?;
            Ok(Self { terminal })
        })();
        if result.is_err() {
            let _ = reset();
        }
        result
    }

    pub fn draw(&mut self, app: &mut App) -> anyhow::Result<()> {
        if app.approval.is_some()
            || app.audit_picker.is_some()
            || app.model_picker.is_some()
            || app.remote_grant.is_some()
            || app.navigation.selected.is_some()
        {
            self.terminal.hide_cursor()?;
        }
        io::stderr().sync_update(|_| {
            self.terminal
                .draw(|frame| ui::render(app, frame))
                .map(|_| ())
        })??;
        Ok(())
    }

    pub fn draw_directory_prompt(
        &mut self,
        prompt: &DirectoryPrompt,
        language: Language,
    ) -> anyhow::Result<bool> {
        self.terminal.hide_cursor()?;
        let mut can_allow = false;
        io::stderr().sync_update(|_| {
            self.terminal
                .draw(|frame| can_allow = prompt.render(frame, language))
                .map(|_| ())
        })??;
        Ok(can_allow)
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
    // On Windows, disabling mouse capture restores the mode saved *after* entering raw mode.
    let input_result = disable_input_modes(io::stderr());
    let raw_result = terminal::disable_raw_mode();
    let screen_result = execute!(io::stderr(), LeaveAlternateScreen, Show);
    input_result.and(raw_result).and(screen_result)
}

// The Windows backend reads native input records, not VT mouse/focus/paste sequences.
#[cfg(windows)]
fn enable_input_modes(mut output: impl io::Write, _: bool) -> io::Result<()> {
    execute!(output, EnableMouseCapture)
}

#[cfg(windows)]
fn disable_input_modes(mut output: impl io::Write) -> io::Result<()> {
    execute!(output, DisableMouseCapture)
}

#[cfg(not(windows))]
fn enable_input_modes(mut output: impl io::Write, report_key_events: bool) -> io::Result<()> {
    execute!(
        output,
        Print("\x1b[?1007l"),
        EnableMouseCapture,
        // Select button tracking last; resetting 1003 disables all tracking in Ghostty.
        Print("\x1b[?1002h"),
        EnableFocusChange,
        EnableBracketedPaste
    )?;
    execute!(
        output,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | if report_key_events {
                    KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                } else {
                    KeyboardEnhancementFlags::empty()
                }
        )
    )?;
    Ok(())
}

#[cfg(not(windows))]
fn disable_input_modes(mut output: impl io::Write) -> io::Result<()> {
    execute!(
        output,
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        DisableMouseCapture,
        DisableFocusChange,
        Print("\x1b[?1007l")
    )
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = reset();
    }
}

// On Windows the mode commands use Win32 directly, not the injected writer.
#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn keyboard_protocol_is_enabled_and_reset() {
        let mut output = Vec::new();
        enable_input_modes(&mut output, false).unwrap();
        assert!(output.windows(5).any(|bytes| bytes == b"\x1b[>1u"));
        output.clear();
        disable_input_modes(&mut output).unwrap();
        assert!(output.windows(5).any(|bytes| bytes == b"\x1b[<1u"));
    }

    #[test]
    fn input_modes_preserve_write_errors() {
        struct BrokenWriter;
        impl io::Write for BrokenWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
        }
        assert_eq!(
            enable_input_modes(BrokenWriter, false).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(
            disable_input_modes(BrokenWriter).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn ghostty_finishes_in_button_tracking_with_sgr_coordinates() {
        // Ghostty's stream handler sets tracking to None on ANY mouse-mode reset.
        // See ghostty v1.4.0 src/terminal/stream_terminal.zig, mouse_event_* cases.
        let mut output = Vec::new();
        enable_input_modes(&mut output, false).unwrap();
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
        enable_input_modes(&mut output, false).unwrap();
        let enabled = String::from_utf8(output.clone()).unwrap();
        assert!(enabled.contains("\x1b[?1007l"));
        for mode in [1000, 1002, 1006, 1004, 2004] {
            assert!(enabled.contains(&format!("\x1b[?{mode}h")));
        }

        output.clear();
        disable_input_modes(&mut output).unwrap();
        let disabled = String::from_utf8(output).unwrap();
        assert!(disabled.contains("\x1b[?1007l"));
        for mode in [1000, 1002, 1003, 1006, 1004, 2004] {
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
