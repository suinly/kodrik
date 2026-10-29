use super::*;

struct WaitingModel(std::path::PathBuf);

#[async_trait::async_trait]
impl crate::agent::llm::LlmProvider for WaitingModel {
    async fn generate(
        &self,
        _: &crate::agent::LlmContext<'_>,
        _: &[&dyn crate::agent::tools::Tool],
        _: Option<&mpsc::Sender<AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        std::fs::write(self.0.join("model-started"), b"1")?;
        std::future::pending().await
    }
}

#[tokio::test]
#[ignore = "Runs only inside the private PTY created by recovery_remains_interactive"]
async fn terminal_recovery_fixture() {
    use crate::agent::{
        Agent, AgentRunner, ContextBuilder, UserMessage,
        run_state::{Checkpoint, RunPhase},
        session::{SessionStore, SqliteSessionStore},
        tools::ToolRegistry,
    };
    let Some(directory) = std::env::var_os("KODRIK_TUI_FIXTURE") else {
        return;
    };
    let directory = std::path::PathBuf::from(directory);
    let store = SqliteSessionStore::open_async(directory.join("sessions"))
        .await
        .unwrap();
    let id = SessionId::new("cli:main");
    let mut session = Session::new(id.clone());
    session.initialize_system_prompt("test");
    for index in 0..30 {
        session.push(Message::Assistant(crate::agent::AssistantMessage {
            content: format!("history {index:02}"),
        }));
    }
    session.push(Message::User(UserMessage {
        content: "resume-me".into(),
        files: vec![],
        images: vec![],
    }));
    let case = std::env::var("KODRIK_TUI_CASE").unwrap_or_default();
    if case == "queued-only" || case == "queued-before-scan" {
        store.save(&session).await.unwrap();
    } else {
        let mut checkpoint = Checkpoint::new(&store, &id, &session);
        checkpoint.record.isolated = Some(false);
        checkpoint
            .save(&session, RunPhase::NeedModel, None)
            .await
            .unwrap();
    }
    if matches!(
        case.as_str(),
        "queued-only" | "queued-before-scan" | "follow-up"
    ) {
        store
            .submit(crate::agent::inbox::SubmissionRequest::new(
                &id,
                IncomingMessage::new(InterfaceId("cli".into()), "main", "queued request"),
                crate::agent::inbox::BusyMode::FollowUp,
            ))
            .await
            .unwrap();
    }
    let agent = Agent::new(
        Arc::new(WaitingModel(directory.clone())),
        ToolRegistry::default(),
        ContextBuilder::new("test"),
    );
    let runner = Arc::new(AgentRunner::new(Arc::new(agent), Arc::new(store.clone())));
    if case == "queued-before-scan" {
        let scan = store.execution_lock("recovery", "scan").await.unwrap();
        let release = directory.join("release-scan");
        tokio::spawn(async move {
            while !release.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            drop(scan);
        });
    }
    run_session(
        runner,
        "test-model".into(),
        Language::En,
        &directory.join("language"),
    )
    .await
    .unwrap();
    let runs = store.unfinished_runs().await.unwrap();
    assert_eq!(
        runs.len(),
        usize::from(case == "suspend"),
        "Escape cancels recovery; quit alone preserves it"
    );
    if case != "suspend" {
        assert!(
            store.next_submission(&id).await.unwrap().is_none(),
            "stopped recovery must not leave runnable queued input"
        );
    }
    let session = store.load(&id).await.unwrap().unwrap();
    assert_eq!(
        session
            .messages()
            .iter()
            .filter(|message| matches!(message, Message::User(_)))
            .count(),
        if case == "queued-only" { 2 } else { 1 },
        "Enter during recovery must leave the draft unsent"
    );
    std::fs::write(directory.join("clean-exit"), b"1").unwrap();
}

struct PrivateTerminal {
    child: std::process::Child,
    master: std::fs::File,
    output: Vec<u8>,
}

fn screen_text(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output);
    let mut chars = text.chars().peekable();
    let mut screen = vec![vec![' '; 80]; 24];
    let (mut x, mut y) = (0usize, 0usize);
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => match chars.next() {
                Some('[') => {
                    let mut sequence = String::new();
                    for ch in chars.by_ref() {
                        if ('@'..='~').contains(&ch) {
                            match ch {
                                'H' | 'f' => {
                                    let mut values = sequence.split(';').map(|value| {
                                        value.parse::<usize>().unwrap_or(1).saturating_sub(1)
                                    });
                                    y = values.next().unwrap_or(0).min(23);
                                    x = values.next().unwrap_or(0).min(79);
                                }
                                'h' if sequence == "?1049" => {
                                    screen.iter_mut().for_each(|row| row.fill(' '));
                                    (x, y) = (0, 0);
                                }
                                'J' if sequence == "2" => {
                                    screen.iter_mut().for_each(|row| row.fill(' '))
                                }
                                _ => {}
                            }
                            break;
                        }
                        sequence.push(ch);
                    }
                }
                Some(']') => {
                    while let Some(ch) = chars.next() {
                        if ch == '\x07' || (ch == '\x1b' && chars.next() == Some('\\')) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => x = 0,
            '\n' => y = (y + 1).min(23),
            ch if !ch.is_control() => {
                screen[y][x] = ch;
                x = (x + ratatui::text::Span::raw(ch.to_string()).width()).min(79);
            }
            _ => {}
        }
    }
    screen
        .into_iter()
        .map(|row| row.into_iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

impl Drop for PrivateTerminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl PrivateTerminal {
    fn start(directory: &Path) -> Self {
        Self::start_case(directory, "")
    }

    fn start_case(directory: &Path, case: &str) -> Self {
        use std::os::{
            fd::{AsRawFd, FromRawFd},
            unix::process::CommandExt,
        };
        use std::process::{Command, Stdio};
        let mut master = -1;
        let mut slave = -1;
        let mut size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // Both descriptors belong only to this test, and are immediately wrapped in Files.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut size,
                )
            },
            0
        );
        let master = unsafe { std::fs::File::from_raw_fd(master) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        assert_eq!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
            0
        );
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "commands::tui::startup_tests::terminal_recovery_fixture",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("KODRIK_TUI_FIXTURE", directory)
            .env("KODRIK_TUI_CASE", case)
            // Capture OSC 52 in this private PTY; never modify the real clipboard.
            .env("HERDR_ENV", "1")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        // Give the child its own controlling terminal, isolated from the test runner.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        Self {
            child,
            master,
            output: Vec::new(),
        }
    }

    fn wait_for(&mut self, needle: &[u8], since: usize) {
        use std::io::Read;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if self.output[since..]
                .windows(needle.len())
                .any(|window| window == needle)
                || (needle.first() != Some(&0x1b)
                    && screen_text(&self.output).contains(std::str::from_utf8(needle).unwrap()))
            {
                return;
            }
            let mut bytes = [0; 8192];
            match self.master.read(&mut bytes) {
                Ok(count) if count > 0 => self.output.extend_from_slice(&bytes[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                other => panic!(
                    "terminal read {other:?}: {:?}",
                    String::from_utf8_lossy(&self.output)
                ),
            }
            assert!(
                std::time::Instant::now() < deadline,
                "missing {:?}: {:?}",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&self.output)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
    }
}

#[test]
fn recovery_remains_interactive() {
    let directory = tempfile::tempdir().unwrap();
    let mut terminal = PrivateTerminal::start(directory.path());
    terminal.wait_for(b"Recovering session", 0);
    terminal.send(b"draft\r");
    terminal.wait_for(b"draft", 0);
    let since = terminal.output.len();
    terminal.send(b"\x1b[<64;10;5M");
    terminal.wait_for(b"history 22", since);
    let since = terminal.output.len();
    terminal.send(b"\x1b[<65;10;5M");
    terminal.wait_for(b"resume-me", since);
    terminal.send(b"\x1b[<0;4;16M\x1b[<32;12;16M\x1b[<0;12;16m");
    terminal.wait_for(b"\x1b]52;c;cmVzdW1lLW1l\x07", 0);
    terminal.send(b"\x1b[27u\x1b[27u");
    terminal.wait_for(b"Stopped", 0);
    terminal.send(b"\x03\x03");
    terminal.wait_for(b"test result: ok", 0);
    assert!(directory.path().join("model-started").exists());
    assert!(directory.path().join("clean-exit").exists());
}

fn wait_for_model(directory: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !directory.join("model-started").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "recovery did not reach model"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn escape_stops_recovered_follow_up_queue() {
    let directory = tempfile::tempdir().unwrap();
    let mut terminal = PrivateTerminal::start_case(directory.path(), "follow-up");
    terminal.wait_for(b"Recovering session", 0);
    wait_for_model(directory.path());
    terminal.send(b"\x1b[27u");
    terminal.wait_for(b"Stopped", 0);
    assert!(!screen_text(&terminal.output).contains("Recovery blocked"));
    terminal.send(b"\x03");
    terminal.wait_for(b"test result: ok", 0);
}

#[test]
fn cancelling_queued_only_recovery_is_stopped_without_error() {
    let directory = tempfile::tempdir().unwrap();
    let mut terminal = PrivateTerminal::start_case(directory.path(), "queued-only");
    terminal.wait_for(b"Recovering session", 0);
    wait_for_model(directory.path());
    terminal.send(b"\x1b[27u");
    terminal.wait_for(b"Stopped", 0);
    assert!(!screen_text(&terminal.output).contains("Recovery blocked"));
    terminal.send(b"\x03");
    terminal.wait_for(b"test result: ok", 0);
}

#[test]
fn quitting_during_recovery_suspends_it_without_hanging() {
    let directory = tempfile::tempdir().unwrap();
    let mut terminal = PrivateTerminal::start_case(directory.path(), "suspend");
    terminal.wait_for(b"Recovering session", 0);
    wait_for_model(directory.path());
    terminal.send(b"\x03");
    terminal.wait_for(b"test result: ok", 0);
}

#[test]
fn escape_before_checkpoint_creation_prevents_queued_model_execution() {
    let directory = tempfile::tempdir().unwrap();
    let mut terminal = PrivateTerminal::start_case(directory.path(), "queued-before-scan");
    terminal.wait_for(b"Recovering session", 0);
    terminal.send(b"\x1b[27u");
    terminal.wait_for(b"Stopping", 0);
    std::fs::write(directory.path().join("release-scan"), b"1").unwrap();
    terminal.wait_for(b"Stopped", 0);
    terminal.send(b"\x03");
    terminal.wait_for(b"test result: ok", 0);
    assert!(!directory.path().join("model-started").exists());
}
