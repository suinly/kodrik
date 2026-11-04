use std::{ffi::OsString, process::Stdio, time::Duration};

use tokio::{process::Command, sync::watch, task::JoinHandle};

use super::app::{App, Status};
use crate::agent::session::SessionId;

struct Environment {
    binary: OsString,
    pane: OsString,
    socket: OsString,
}

#[derive(Clone, PartialEq, Eq)]
struct Report {
    state: &'static str,
    session: Option<SessionId>,
}

impl Report {
    fn from_app(app: &App) -> Self {
        Self {
            state: if app.approval.is_some() {
                "blocked"
            } else if matches!(
                app.status,
                Status::Recovering | Status::Working | Status::Stopping
            ) {
                "working"
            } else {
                "idle"
            },
            session: app.session.clone(),
        }
    }
}

#[derive(Default)]
pub(super) struct Reporter {
    updates: Option<watch::Sender<Option<Report>>>,
    worker: Option<JoinHandle<()>>,
}

impl Reporter {
    pub fn start(slot: u64) -> Self {
        if std::env::var("HERDR_ENV").as_deref() != Ok("1") {
            return Self::default();
        }
        let required = |key| std::env::var_os(key).filter(|value| !value.is_empty());
        let (Some(binary), Some(pane), Some(socket)) = (
            required("HERDR_BIN_PATH"),
            required("HERDR_PANE_ID"),
            required("HERDR_SOCKET_PATH"),
        ) else {
            return Self::default();
        };
        Self::spawn(
            Environment {
                binary,
                pane,
                socket,
            },
            slot,
        )
    }

    fn spawn(environment: Environment, slot: u64) -> Self {
        let (updates, mut receiver) = watch::channel(None::<Report>);
        let worker = tokio::spawn(async move {
            let mut sequence = 0;
            let mut reported = false;
            while receiver.changed().await.is_ok() {
                let report = receiver.borrow_and_update().clone();
                environment.send(report.as_ref(), slot, &mut sequence).await;
                reported = true;
            }
            if reported {
                environment.send(None, slot, &mut sequence).await;
            }
        });
        Self {
            updates: Some(updates),
            worker: Some(worker),
        }
    }

    pub fn update(&mut self, app: &App) {
        if let Some(updates) = &self.updates {
            let report = Report::from_app(app);
            updates.send_if_modified(|current| {
                if current.as_ref() == Some(&report) {
                    return false;
                }
                *current = Some(report);
                true
            });
        }
    }

    pub async fn finish(&mut self) {
        self.updates.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.await;
        }
    }
}

impl Environment {
    async fn send(&self, report: Option<&Report>, slot: u64, sequence: &mut u64) {
        *sequence = chrono::Utc::now()
            .timestamp_micros()
            .unsigned_abs()
            .max(sequence.saturating_add(1));
        let mut command = Command::new(&self.binary);
        command
            .args([
                "pane",
                if report.is_some() {
                    "report-agent"
                } else {
                    "release-agent"
                },
            ])
            .arg(&self.pane)
            .args(["--source", "kodrik", "--agent", "kodrik", "--seq"])
            .arg(sequence.to_string())
            .env("HERDR_SOCKET_PATH", &self.socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(report) = report {
            command.args(["--state", report.state]);
            if report.state == "blocked" {
                command.args(["--message", "Tool approval required"]);
            }
            if let Some(session) = &report.session {
                command.args(["--agent-session-id", session.as_str()]);
            }
            command
                .args(["--", "kodrik", "--tui-slot"])
                .arg(slot.to_string());
        }
        let _ = tokio::time::timeout(Duration::from_secs(1), command.status()).await;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::commands::tui::app::{App, Status};
    use std::{os::unix::fs::PermissionsExt, path::Path};

    fn executable(directory: &Path, body: &str) -> Environment {
        let binary = directory.join("herdr test");
        std::fs::write(
            &binary,
            format!("#!/bin/sh\n{body}\nprintf '%s\\n' \"$@\" >> \"${{0}}.log\"\nprintf '\\n' >> \"${{0}}.log\"\n"),
        ).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        Environment {
            binary: binary.into_os_string(),
            pane: "pane-test".into(),
            socket: directory.join("api.sock").into_os_string(),
        }
    }

    fn reports(directory: &Path) -> Vec<Vec<String>> {
        let text = std::fs::read_to_string(directory.join("herdr test.log")).unwrap_or_default();
        let Some((complete, _)) = text.rsplit_once("\n\n") else {
            return Vec::new();
        };
        complete
            .split("\n\n")
            .filter(|line| !line.is_empty())
            .map(|line| line.lines().map(str::to_owned).collect())
            .collect()
    }

    async fn wait_for(mut condition: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    fn value<'a>(args: &'a [String], flag: &str) -> &'a str {
        &args[args.iter().position(|arg| arg == flag).unwrap() + 1]
    }

    #[tokio::test]
    async fn reports_lifecycle_selected_session_resume_and_release() {
        let dir = tempfile::tempdir().unwrap();
        let mut reporter = Reporter::spawn(executable(dir.path(), ""), 2);
        let mut app = App {
            status: Status::Recovering,
            session: Some(SessionId::new("selected-session")),
            ..Default::default()
        };
        reporter.update(&app);
        wait_for(|| reports(dir.path()).len() == 1).await;
        app.status = Status::Working;
        reporter.update(&app);
        app.approval = Some(crate::agent::command_audit::CommandApproval {
            id: "approval".into(),
            run_id: "run".into(),
            call_id: "call".into(),
            tool_name: "exec_command".into(),
            arguments_json: None,
            target: None,
            cmd: "command".into(),
            workdir: None,
            reason: "approval required".into(),
            reason_code: "test".into(),
        });
        reporter.update(&app);
        wait_for(|| reports(dir.path()).len() == 2).await;
        app.approval = None;
        app.status = Status::Done("12:00".into());
        app.session = Some(SessionId::new("new-session"));
        reporter.update(&app);
        reporter.finish().await;

        let calls = reports(dir.path());
        assert_eq!(calls.len(), 4);
        for (call, state) in calls.iter().zip(["working", "blocked", "idle"]) {
            assert_eq!(&call[..3], ["pane", "report-agent", "pane-test"]);
            assert_eq!(value(call, "--source"), "kodrik");
            assert_eq!(value(call, "--agent"), "kodrik");
            assert_eq!(value(call, "--state"), state);
            assert_eq!(&call[call.len() - 4..], ["--", "kodrik", "--tui-slot", "2"]);
        }
        assert_eq!(value(&calls[0], "--agent-session-id"), "selected-session");
        assert_eq!(value(&calls[2], "--agent-session-id"), "new-session");
        assert!(!value(&calls[1], "--message").is_empty());
        assert_eq!(&calls[3][..3], ["pane", "release-agent", "pane-test"]);
        let sequences: Vec<u64> = calls
            .iter()
            .map(|call| value(call, "--seq").parse().unwrap())
            .collect();
        assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn projects_work_stopping_and_idle_states() {
        for (status, expected) in [
            (Status::Recovering, "working"),
            (Status::Working, "working"),
            (Status::Stopping, "working"),
            (Status::Ready, "idle"),
            (Status::Stopped, "idle"),
            (Status::Done("12:00".into()), "idle"),
            (Status::Error, "idle"),
        ] {
            let app = App {
                status,
                ..Default::default()
            };
            assert_eq!(Report::from_app(&app).state, expected);
        }
    }

    #[tokio::test]
    async fn coalesces_in_flight_changes_and_reports_session_switches_while_idle() {
        let dir = tempfile::tempdir().unwrap();
        let env = executable(
            dir.path(),
            "if [ ! -e \"${0}.gate\" ]; then\n touch \"${0}.started\"\n while [ ! -e \"${0}.gate\" ]; do sleep 0.01; done\nfi",
        );
        let mut reporter = Reporter::spawn(env, 1);
        let mut app = App {
            status: Status::Working,
            ..Default::default()
        };
        reporter.update(&app);
        wait_for(|| dir.path().join("herdr test.started").exists()).await;
        app.session = Some(SessionId::new("discarded"));
        reporter.update(&app);
        app.status = Status::Ready;
        app.session = Some(SessionId::new("latest"));
        reporter.update(&app);
        std::fs::write(dir.path().join("herdr test.gate"), "").unwrap();
        wait_for(|| reports(dir.path()).len() == 2).await;
        reporter.update(&app);
        app.session = Some(SessionId::new("another-session"));
        reporter.update(&app);
        reporter.finish().await;
        let calls = reports(dir.path());
        assert_eq!(calls.len(), 4);
        assert_eq!(value(&calls[1], "--agent-session-id"), "latest");
        assert_eq!(value(&calls[2], "--agent-session-id"), "another-session");
        assert_eq!(value(&calls[2], "--state"), "idle");
    }

    #[tokio::test]
    async fn failed_and_stalled_commands_do_not_prevent_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let mut failed = Reporter::spawn(executable(dir.path(), "exit 1"), 1);
        failed.update(&App::default());
        tokio::time::timeout(Duration::from_secs(3), failed.finish())
            .await
            .unwrap();
        let mut stalled = Reporter::spawn(executable(dir.path(), "exec sleep 30"), 1);
        stalled.update(&App::default());
        tokio::time::timeout(Duration::from_secs(3), stalled.finish())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn dropping_the_reporter_releases_the_pane() {
        let dir = tempfile::tempdir().unwrap();
        let mut reporter = Reporter::spawn(executable(dir.path(), ""), 1);
        reporter.update(&App::default());
        wait_for(|| reports(dir.path()).len() == 1).await;
        drop(reporter);
        wait_for(|| reports(dir.path()).len() == 2).await;
        assert_eq!(reports(dir.path())[1][1], "release-agent");
    }

    #[tokio::test]
    #[ignore = "Private subprocess fixture for requires_a_complete_herdr_environment"]
    async fn environment_fixture() {
        let mut reporter = Reporter::start(2);
        reporter.update(&App::default());
        reporter.finish().await;
    }

    #[test]
    fn requires_a_complete_herdr_environment() {
        for (enabled, missing) in [
            ("0", None),
            ("1", Some("HERDR_BIN_PATH")),
            ("1", Some("HERDR_PANE_ID")),
            ("1", Some("HERDR_SOCKET_PATH")),
            ("1", None),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let env = executable(dir.path(), "");
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "commands::tui::herdr::tests::environment_fixture",
                    "--ignored",
                ])
                .env("HERDR_ENV", enabled)
                .env("HERDR_BIN_PATH", env.binary)
                .env("HERDR_PANE_ID", env.pane)
                .env("HERDR_SOCKET_PATH", env.socket)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            if let Some(missing) = missing {
                command.env(missing, "");
            }
            let result = command.output().unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert_eq!(
                reports(dir.path()).len(),
                if enabled == "1" && missing.is_none() {
                    2
                } else {
                    0
                }
            );
        }
    }
}
