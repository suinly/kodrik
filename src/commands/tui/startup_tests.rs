use super::*;

struct WaitingModel(std::path::PathBuf);

struct EchoModel;

#[async_trait::async_trait]
impl crate::agent::llm::LlmProvider for EchoModel {
    async fn generate(
        &self,
        context: &crate::agent::LlmContext<'_>,
        _: &[&dyn crate::agent::tools::Tool],
        _: Option<&mpsc::Sender<AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        let user = context
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::User(user) => Some(user),
                _ => None,
            })
            .unwrap();
        Ok((
            vec![Message::Assistant(crate::agent::AssistantMessage {
                content: format!("edited reply: {}", user.content),
            })],
            None,
        ))
    }
}

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
    let case = std::env::var("KODRIK_TUI_CASE").unwrap_or_default();
    if case == "approval-cursor" {
        let mut app = App::default();
        let mut tui = Tui::enter().unwrap();
        tui.draw(&mut app).unwrap();
        event::read().unwrap();
        app.approval = Some(crate::agent::command_audit::CommandApproval {
            id: "request".into(),
            run_id: "run".into(),
            call_id: "call".into(),
            tool_name: "exec_command".into(),
            arguments_json: None,
            target: None,
            cmd: "rm file".into(),
            workdir: None,
            reason: "Destructive".into(),
            reason_code: "destructive".into(),
        });
        app.approval_choice = Some(0);
        tui.draw(&mut app).unwrap();
        event::read().unwrap();
        app.approval = None;
        tui.draw(&mut app).unwrap();
        tui.exit().unwrap();
        return;
    }
    let store = SqliteSessionStore::open_async(directory.join("sessions"))
        .await
        .unwrap();
    let id = SessionId::new("cli:main");
    if case == "draft-startup" {
        let mut previous = Session::new(id.clone());
        previous.push(Message::User(UserMessage {
            browser_context: None,
            content: "previous conversation".into(),
            files: vec![],
            images: vec![],
        }));
        store.save(&previous).await.unwrap();
        let runner = Arc::new(AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(EchoModel),
                ToolRegistry::default(),
                ContextBuilder::new("test"),
            )),
            Arc::new(store.clone()),
        ));
        run_session(
            runner,
            id.clone(),
            1,
            "test-model".into(),
            Language::En,
            &directory.join("language"),
        )
        .await
        .unwrap();
        let active = store.active_id(&id).await.unwrap();
        assert!(store.load(&active).await.unwrap().is_some());
        assert_eq!(store.list().await.unwrap().len(), 2);
        std::fs::write(directory.join("clean-exit"), b"1").unwrap();
        return;
    }
    let mut session = Session::new(id.clone());
    session.initialize_system_prompt("test");
    for index in 0..30 {
        session.push(Message::Assistant(crate::agent::AssistantMessage {
            content: format!("history {index:02}"),
        }));
    }
    session.push(Message::User(UserMessage {
        browser_context: None,
        content: "resume-me".into(),
        files: vec![],
        images: vec![],
    }));
    if case == "edit-message" {
        session.push(Message::Assistant(crate::agent::AssistantMessage {
            content: "original answer".into(),
        }));
    }
    if matches!(
        case.as_str(),
        "queued-only" | "queued-before-scan" | "edit-message" | "herdr"
    ) {
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
    let model: Arc<dyn crate::agent::llm::LlmProvider> = if case == "edit-message" {
        Arc::new(EchoModel)
    } else {
        Arc::new(WaitingModel(directory.clone()))
    };
    let agent = Agent::new(model, ToolRegistry::default(), ContextBuilder::new("test"));
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
    // These scenarios exercise explicit resumption/recovery, not a fresh startup.
    let mut reporter = herdr::Reporter::start(1);
    run_session_inner(
        runner.clone(),
        id.clone(),
        "test-model".into(),
        Language::En,
        &directory.join("language"),
        &mut reporter,
    )
    .await
    .unwrap();
    reporter.finish().await;
    runner.finish_learning().await;
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
    if case == "herdr" {
        let active = store.active_id(&id).await.unwrap();
        assert_ne!(active, id);
        std::fs::write(directory.join("active-session"), active.as_str()).unwrap();
        let selected = store.load(&active).await.unwrap().unwrap();
        assert!(
            selected
                .messages()
                .iter()
                .any(|message| matches!(message, Message::User(user) if user.content == "hello"))
        );
    }
    if case == "edit-message" {
        assert!(
            session.messages().iter().any(
                |message| matches!(message, Message::User(user) if user.content == "resume-me")
            )
        );
        let active = store.active_id(&id).await.unwrap();
        assert_ne!(active, id);
        let branch = store.load(&active).await.unwrap().unwrap();
        assert!(
            branch
                .messages()
                .iter()
                .any(|message| matches!(message, Message::User(user) if user.content == "/new"))
        );
        assert!(
            !branch.messages().iter().any(
                |message| matches!(message, Message::User(user) if user.content == "resume-me")
            )
        );
    }
    assert_eq!(
        session
            .messages()
            .iter()
            .filter(|message| matches!(message, Message::User(_)))
            .count(),
        if case == "queued-only" { 2 } else { 1 },
        "Only submitted messages should be saved"
    );
    std::fs::write(directory.join("clean-exit"), b"1").unwrap();
}

pub(super) struct PrivateTerminal {
    child: std::process::Child,
    master: Option<std::fs::File>,
    pub(super) output: Vec<u8>,
}

pub(super) fn screen_text(output: &[u8]) -> String {
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
        // macOS can block child exit while its PTY master is still open.
        drop(self.master.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl PrivateTerminal {
    fn start(directory: &Path) -> Self {
        Self::start_case(directory, "")
    }

    pub(super) fn start_case(directory: &Path, case: &str) -> Self {
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
        for file in [&master, &slave] {
            assert_eq!(
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
        }
        assert_eq!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
            0
        );
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                if case.starts_with("remote") {
                    "commands::tui::remote_tests::terminal_remote_fixture"
                } else {
                    "commands::tui::startup_tests::terminal_recovery_fixture"
                },
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("KODRIK_TUI_FIXTURE", directory)
            .env("KODRIK_TUI_CASE", case)
            // Keep persisted language settings and locale independent of the host.
            .env("HOME", directory)
            .env("LC_ALL", "C")
            // Capture OSC 52 in this private PTY; never modify the real clipboard.
            .env("HERDR_ENV", "1")
            .env_remove("HERDR_BIN_PATH")
            .env_remove("HERDR_PANE_ID")
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        if case == "herdr" {
            command
                .env("HERDR_BIN_PATH", directory.join("herdr"))
                .env("HERDR_PANE_ID", "private-pane")
                .env("HERDR_SOCKET_PATH", directory.join("private.sock"));
        }
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
            master: Some(master),
            output: Vec::new(),
        }
    }

    pub(super) fn wait_for(&mut self, needle: &[u8], since: usize) {
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
            match self.master.as_mut().unwrap().read(&mut bytes) {
                Ok(count) if count > 0 => {
                    let start = self.output.len().saturating_sub(3);
                    self.output.extend_from_slice(&bytes[..count]);
                    // Terminal::clear queries the cursor when a session is replaced.
                    if self.output[start..]
                        .windows(4)
                        .any(|bytes| bytes == b"\x1b[6n")
                    {
                        self.send(b"\x1b[1;1R");
                    }
                }
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

    pub(super) fn send(&mut self, bytes: &[u8]) {
        self.master.as_mut().unwrap().write_all(bytes).unwrap();
    }
}

#[test]
fn herdr_follows_real_tui_work_cancellation_and_exit() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let binary = directory.path().join("herdr");
    std::fs::write(
        &binary,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$0.log\"\nprintf '\\n' >> \"$0.log\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut terminal = PrivateTerminal::start_case(directory.path(), "herdr");
    let log = || std::fs::read_to_string(directory.path().join("herdr.log")).unwrap_or_default();
    let wait_report = |needle: &str, since: usize| {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !log().get(since..).unwrap_or_default().contains(needle) {
            assert!(
                std::time::Instant::now() < deadline,
                "missing {needle:?}: {}",
                log()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    terminal.wait_for(b"Ready", 0);
    wait_report("--state\nidle\n", 0);
    terminal.send(b"/new\r");
    terminal.wait_for(b"New session created.", 0);
    let before_work = log().len();
    terminal.send(b"hello\r");
    terminal.wait_for(b"Working", 0);
    wait_for_model(directory.path());
    wait_report("--state\nworking\n", before_work);
    terminal.send(b"\x1b[27u");
    terminal.wait_for(b"Stopped", 0);
    terminal.send(b"\x03");
    terminal.wait_for(b"test result: ok", 0);
    let calls = log();
    let calls: Vec<_> = calls.trim_end().split("\n\n").collect();
    assert!(
        calls
            .last()
            .unwrap()
            .starts_with("pane\nrelease-agent\nprivate-pane\n")
    );
    let working = calls
        .iter()
        .rposition(|call| call.contains("--state\nworking\n"))
        .unwrap();
    let active = std::fs::read_to_string(directory.path().join("active-session")).unwrap();
    assert!(calls[working].contains(&format!("--agent-session-id\n{active}\n")));
    assert!(
        calls[working + 1..]
            .iter()
            .any(|call| call.contains("--state\nidle\n"))
    );
    assert!(
        calls
            .iter()
            .any(|call| call.contains("--\nkodrik\n--tui-slot\n1"))
    );
}

#[test]
fn double_escape_edit_runs_a_durable_branch_and_treats_slash_text_as_a_message() {
    let directory = tempfile::tempdir().unwrap();
    let mut terminal = PrivateTerminal::start_case(directory.path(), "edit-message");
    terminal.wait_for(b"Ready", 0);
    terminal.send(b"draft\x1b[27u\x1b[27u\r");
    terminal.wait_for(b"Editing message", 0);
    let since = terminal.output.len();
    terminal.send(b"\x03/new\r");
    terminal.wait_for(b"edited reply: /new", since);
    terminal.wait_for(b"Done", since);
    let since = terminal.output.len();
    terminal.send(b"\x1b[27u\x1b[27u");
    terminal.wait_for(b"2/2", since);
    let since = terminal.output.len();
    terminal.send(b"h");
    terminal.wait_for(b"1/2", since);
    assert!(screen_text(&terminal.output).contains("original answer"));
    let since = terminal.output.len();
    terminal.send(b"l");
    terminal.wait_for(b"2/2", since);
    assert!(screen_text(&terminal.output).contains("edited reply: /new"));
    terminal.send(b"\x1b[27u");
    terminal.send(b"\x03");
    terminal.wait_for(b"test result: ok", since);
    assert!(directory.path().join("clean-exit").exists());
}

#[tokio::test]
async fn switching_to_an_unwritten_canceled_version_keeps_the_current_message() {
    use crate::agent::{
        Agent, AgentRunner, ContextBuilder, UserMessage,
        session::{SessionStore, SqliteSessionStore},
        tools::ToolRegistry,
    };
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteSessionStore::open(directory.path().into()).unwrap());
    let id = SessionId::new("cli:main");
    let mut source = Session::new(id.clone());
    source.push(Message::User(UserMessage {
        browser_context: None,
        content: "original".into(),
        files: vec![],
        images: vec![],
    }));
    store.save(&source).await.unwrap();
    let runner = AgentRunner::new(
        Arc::new(Agent::new(
            Arc::new(EchoModel),
            ToolRegistry::default(),
            ContextBuilder::new("test"),
        )),
        store.clone(),
    );
    let (prefix, replacement) = runner
        .edit_message(&id, &id, 0, "canceled edit".into())
        .await
        .unwrap();
    runner
        .cancel_queued_input(&id, &replacement.request_id)
        .await
        .unwrap();
    runner.select_session(&id, &id).await.unwrap();
    let mut app = App::default();
    restore_history(&mut app, &source);
    app.insert_input("draft");
    app.select_user_turn(0);
    assert!(
        select_message_version(&mut app, &runner, &id, prefix.id(), 0)
            .await
            .is_err()
    );
    assert_eq!(store.active_id(&id).await.unwrap(), id);
    assert!(app.navigation.selected.is_some());
    assert_eq!(app.messages[0].content, "original");
    app.cancel_message_navigation();
    assert_eq!(app.input, "draft");
}

#[tokio::test]
async fn switching_message_versions_preserves_the_draft_and_groups_the_conversation() {
    use crate::agent::{
        Agent, AgentRunner, ContextBuilder, UserMessage,
        session::{SessionStore, SqliteSessionStore},
        tools::ToolRegistry,
    };
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteSessionStore::open(directory.path().into()).unwrap());
    let id = SessionId::new("cli:main");
    let mut source = Session::new(id.clone());
    source.push(Message::User(UserMessage {
        browser_context: None,
        content: "original".into(),
        files: vec![],
        images: vec![],
    }));
    source.push(Message::Assistant(crate::agent::AssistantMessage {
        content: "original answer".into(),
    }));
    store.save(&source).await.unwrap();
    let runner = AgentRunner::new(
        Arc::new(Agent::new(
            Arc::new(EchoModel),
            ToolRegistry::default(),
            ContextBuilder::new("test"),
        )),
        store.clone(),
    );
    let (_, replacement) = runner
        .edit_message(&id, &id, 0, "edited".into())
        .await
        .unwrap();
    runner.run(id.clone(), replacement, None).await.unwrap();
    let edited = runner.load_session(id.clone()).await.unwrap();
    let mut app = App::default();
    restore_history(&mut app, &edited);
    refresh_versions(&mut app, &runner, &edited).await.unwrap();
    app.insert_input("unfinished draft");
    app.input_cursor = 2;
    app.select_user_turn(0);
    assert_eq!(app.selected_versions().unwrap().current, 1);
    select_message_version(&mut app, &runner, &id, &id, 0)
        .await
        .unwrap();
    assert_eq!(app.selected_versions().unwrap().current, 0);
    assert!(
        app.messages
            .iter()
            .any(|message| message.content == "original answer")
    );
    app.update(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.input, "unfinished draft");
    assert_eq!(app.input_cursor, 2);
    let listed = runner.list_conversations().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id(), &id);
}

#[tokio::test]
async fn rejected_edit_preserves_the_editor_and_the_original_draft() {
    use crate::agent::{
        Agent, AgentRunner, ContextBuilder, UserMessage,
        session::{SessionStore, SqliteSessionStore},
        tools::ToolRegistry,
    };
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteSessionStore::open(directory.path().into()).unwrap());
    let id = SessionId::new("cli:main");
    let mut session = Session::new(id.clone());
    session.push(Message::User(UserMessage {
        browser_context: None,
        content: "original".into(),
        files: vec![],
        images: vec![],
    }));
    store.save(&session).await.unwrap();
    let runner = AgentRunner::new(
        Arc::new(Agent::new(
            Arc::new(WaitingModel(directory.path().into())),
            ToolRegistry::default(),
            ContextBuilder::new("test"),
        )),
        store.clone(),
    );
    let mut app = App::default();
    restore_history(&mut app, &session);
    app.insert_input("draft");
    for code in [KeyCode::Esc, KeyCode::Esc, KeyCode::Enter] {
        app.update(KeyEvent::new(code, KeyModifiers::NONE));
    }
    app.input = "/audit ask".into();
    app.input_cursor = app.input.len();
    let replacement = app
        .update(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    store
        .submit(crate::agent::inbox::SubmissionRequest::new(
            &id,
            IncomingMessage::new(InterfaceId("cli".into()), "main", "queued"),
            crate::agent::inbox::BusyMode::FollowUp,
        ))
        .await
        .unwrap();
    assert!(
        submit_edit(&mut app, &runner, &id, replacement)
            .await
            .is_err()
    );
    assert_eq!(app.input, "/audit ask");
    assert!(app.navigation.editing.is_some());
    assert_eq!(app.messages.len(), 1);
    app.update(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.input, "draft");
    assert_eq!(store.active_id(&id).await.unwrap(), id);
}

#[test]
fn approval_hides_cursor_before_redrawing_and_restores_it_afterward() {
    let directory = tempfile::tempdir().unwrap();
    let mut terminal = PrivateTerminal::start_case(directory.path(), "approval-cursor");
    terminal.wait_for(b"\x1b[?2026l", 0);
    assert!(
        terminal
            .output
            .windows(6)
            .any(|bytes| bytes == b"\x1b[?25h")
    );
    let since = terminal.output.len();
    terminal.send(b"\r");
    terminal.wait_for(b"\x1b[?2026l", since);
    assert!(
        terminal.output[since..].starts_with(b"\x1b[?25l\x1b[?2026h"),
        "hide the old cursor before starting the synchronized redraw"
    );
    let since = terminal.output.len();
    terminal.send(b"\r");
    terminal.wait_for(b"\x1b[?2026l", since);
    assert!(
        terminal.output[since..]
            .windows(6)
            .any(|bytes| bytes == b"\x1b[?25h")
    );
    terminal.wait_for(b"test result: ok", since);
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

#[test]
fn startup_and_new_remain_drafts_until_the_first_message() {
    use crate::agent::session::SqliteSessionStore;
    let directory = tempfile::tempdir().unwrap();
    let mut terminal = PrivateTerminal::start_case(directory.path(), "draft-startup");
    terminal.wait_for(b"Ready", 0);
    assert!(!screen_text(&terminal.output).contains("previous conversation"));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let store = SqliteSessionStore::open(directory.path().join("sessions")).unwrap();
    let id = SessionId::new("cli:main");
    let first = runtime.block_on(store.active_id(&id)).unwrap();
    assert_ne!(first, id);
    assert!(runtime.block_on(store.load(&first)).unwrap().is_none());
    for _ in 0..2 {
        let since = terminal.output.len();
        terminal.send(b"/new\r");
        terminal.wait_for(b"New session", since);
        assert_eq!(runtime.block_on(store.list()).unwrap().len(), 1);
        let active = runtime.block_on(store.active_id(&id)).unwrap();
        assert!(runtime.block_on(store.load(&active)).unwrap().is_none());
    }
    terminal.send(b"hello\r");
    terminal.wait_for(b"edited reply: hello", 0);
    terminal.wait_for(b"Done", 0);
    terminal.send(b"/sessions\r");
    terminal.wait_for(b"previous conversation", 0);
    terminal.send(b"\x1b[27u");
    terminal.send(b"\x03");
    terminal.wait_for(b"test result: ok", 0);
    assert!(directory.path().join("clean-exit").exists());
}
