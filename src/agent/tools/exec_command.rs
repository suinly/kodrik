use crate::agent::tools::{Tool, ToolOutput};
use anyhow::Context;
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use std::{collections::VecDeque, io::Write, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecCommandArgs {
    /// Shell command to execute through /bin/sh -c.
    pub(crate) cmd: String,
    /// Working directory. Null inherits Kodrik's current directory.
    pub(crate) workdir: Option<String>,
    /// Positive timeout in milliseconds. Null uses 30000.
    pub(crate) timeout_ms: Option<u64>,
}

#[derive(Default)]
pub struct ExecCommandTool;

#[async_trait::async_trait]
impl Tool for ExecCommandTool {
    fn name(&self) -> String {
        "exec_command".into()
    }

    fn description(&self) -> String {
        "Run a shell command through /bin/sh -c with Kodrik's process permissions. Returns stdout, stderr, exit_code (null if signaled), timed_out, stdout_truncated and stderr_truncated. Retains the first and last 512 KiB per stream with a marker when the middle is omitted. Full raw output of truncated streams is saved in private temporary files, returned as stdout_path/stderr_path (null for untruncated streams). Read sections with sed or tail through exec_command; files remain until removed or the OS cleans its temporary directory. Timeout defaults to 30 seconds; returns captured output on timeout. Timeout and cancellation stop the process group. No interactive input or persistent sessions.".into()
    }

    fn parameters(&self) -> serde_json::Value {
        let mut schema = schema_for!(ExecCommandArgs);
        // OpenAI strict mode requires nullable fields to be present as well.
        schema.insert(
            "required".into(),
            serde_json::json!(["cmd", "workdir", "timeout_ms"]),
        );
        schema.to_value()
    }

    async fn execute(&self, properties: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let args: ExecCommandArgs = serde_json::from_value(properties)?;
        anyhow::ensure!(!args.cmd.trim().is_empty(), "cmd must not be empty");
        let timeout_ms = args.timeout_ms.unwrap_or(30_000);
        anyhow::ensure!(timeout_ms > 0, "timeout_ms must be positive");
        let deadline = tokio::time::Instant::now()
            .checked_add(Duration::from_millis(timeout_ms))
            .context("timeout_ms is too large")?;

        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", &args.cmd])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        if let Some(home) = std::env::var_os("HOME") {
            let local_bin = std::path::PathBuf::from(home).join(".local/bin");
            if local_bin.is_dir() {
                let mut paths = vec![local_bin];
                paths.extend(std::env::split_paths(
                    &std::env::var_os("PATH").unwrap_or_default(),
                ));
                command.env("PATH", std::env::join_paths(paths)?);
            }
        }
        if let Some(workdir) = args.workdir {
            command.current_dir(workdir);
        }
        let mut stdout_output = CommandOutput::new()?;
        let mut stderr_output = CommandOutput::new()?;
        let mut child = command.spawn().context("Failed to start shell command")?;
        let pid = child.id().context("Shell has no process ID")?;
        let mut group = ProcessGroup(Some(
            pid.try_into().context("Shell process ID is too large")?,
        ));
        let mut stdout = child.stdout.take().context("Shell stdout is unavailable")?;
        let mut stderr = child.stderr.take().context("Shell stderr is unavailable")?;

        let result = tokio::time::timeout_at(deadline, async {
            tokio::try_join!(
                child.wait(),
                read_limited(&mut stdout, &mut stdout_output),
                read_limited(&mut stderr, &mut stderr_output),
            )
        })
        .await;
        let (status, timed_out) = match result {
            Ok(result) => (result.context("Failed to collect shell output")?.0, false),
            Err(_) => {
                group.stop().context("Failed to stop timed-out command")?;
                let status = child
                    .wait()
                    .await
                    .context("Failed to reap timed-out shell")?;
                // ponytail: detached descendants can hold pipes; cap draining at 1s,
                // use OS isolation if commands must not escape their process group.
                if let Ok(result) = tokio::time::timeout(Duration::from_secs(1), async {
                    tokio::try_join!(
                        read_limited(&mut stdout, &mut stdout_output),
                        read_limited(&mut stderr, &mut stderr_output),
                    )
                })
                .await
                {
                    result.context("Failed to collect timed-out shell output")?;
                }
                (status, true)
            }
        };
        group
            .stop()
            .context("Failed to clean up command process group")?;

        let stdout_path = stdout_output.persist_if_truncated()?;
        let stderr_path = stderr_output.persist_if_truncated()?;

        Ok(serde_json::json!({
            "stdout": stdout_output.text(),
            "stdout_path": stdout_path,
            "stderr_path": stderr_path,
            "stderr": stderr_output.text(),
            "exit_code": status.code(),
            "timed_out": timed_out,
            "stdout_truncated": stdout_output.truncated,
            "stderr_truncated": stderr_output.truncated,
        })
        .into())
    }
}

// /bin/sh is Unix-only; kill its group so cancellation also stops descendants.
struct ProcessGroup(Option<libc::pid_t>);

impl ProcessGroup {
    fn stop(&mut self) -> std::io::Result<()> {
        if let Some(pid) = self.0 {
            // SAFETY: pid is a positive child PID; its negation targets our process group.
            if unsafe { libc::kill(-pid, libc::SIGKILL) } == -1 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error);
                }
            }
            self.0 = None;
        }
        Ok(())
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            log::warn!("Failed to stop command process group: {error}");
        }
    }
}

struct CommandOutput {
    file: Option<tempfile::NamedTempFile>,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    truncated: bool,
}

impl CommandOutput {
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            file: Some(
                tempfile::Builder::new()
                    .prefix("kodrik-output-")
                    .tempfile()?,
            ),
            head: Vec::new(),
            tail: VecDeque::new(),
            truncated: false,
        })
    }

    fn persist_if_truncated(&mut self) -> anyhow::Result<Option<std::path::PathBuf>> {
        if !self.truncated {
            return Ok(None);
        }
        let (_, path) = self
            .file
            .take()
            .context("Output file is unavailable")?
            .keep()
            .context("Failed to retain full command output")?;
        Ok(Some(path))
    }

    fn push(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        // Write before updating the preview so a disk error cannot silently lose output.
        // ponytail: synchronous 8 KiB writes avoid cancellation midway through a write;
        // move capture to a dedicated worker if slow temporary storage becomes a problem.
        self.file
            .as_mut()
            .expect("capture file exists while reading")
            .write_all(bytes)?;
        const HALF_LIMIT: usize = 524_288;
        let head_count = bytes.len().min(HALF_LIMIT - self.head.len());
        self.head.extend_from_slice(&bytes[..head_count]);
        self.tail.extend(&bytes[head_count..]);
        if self.tail.len() > HALF_LIMIT {
            self.tail.drain(..self.tail.len() - HALF_LIMIT);
            self.truncated = true;
        }
        Ok(())
    }

    fn text(&self) -> String {
        let mut bytes = self.head.clone();
        if self.truncated {
            bytes.extend_from_slice(b"\n... [output truncated] ...\n");
        }
        bytes.extend(&self.tail);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

async fn read_limited(
    mut stream: impl AsyncRead + Unpin,
    output: &mut CommandOutput,
) -> std::io::Result<()> {
    let mut buffer = [0; 8192];
    loop {
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        output.push(&buffer[..count])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::agent::tools::{ExecCommandTool, Tool};
    use serde_json::json;
    use std::time::Duration;

    #[tokio::test]
    async fn executes_in_workdir_and_returns_output_and_exit_code() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("input.txt"), "hello").unwrap();
        let output = ExecCommandTool
            .execute(json!({
                "cmd": "cat input.txt; printf error >&2; exit 7",
                "workdir": directory.path(),
                "timeout_ms": null,
            }))
            .await
            .unwrap()
            .output;
        assert_eq!(output["stdout"], "hello");
        assert_eq!(output["stderr"], "error");
        assert_eq!(output["exit_code"], 7);
        assert_eq!(output["timed_out"], false);
        assert_eq!(output["stdout_truncated"], false);
        assert_eq!(output["stderr_truncated"], false);
        assert!(output["stdout_path"].is_null());
        assert!(output["stderr_path"].is_null());
        assert!(
            ExecCommandTool
                .execute(json!({"cmd": "pwd", "workdir": directory.path().join("missing")}))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn rejects_invalid_arguments_before_executing() {
        for arguments in [
            json!({"cmd": "  "}),
            json!({"cmd": "true", "timeout_ms": 0}),
            json!({"cmd": "true", "extra": true}),
        ] {
            assert!(ExecCommandTool.execute(arguments).await.is_err());
        }
    }

    #[tokio::test]
    async fn drains_and_limits_large_output_on_both_streams() {
        let output = ExecCommandTool
            .execute(json!({"cmd": "printf BEGIN; head -c 1500000 /dev/zero; printf END; { printf ERROR-BEGIN; head -c 1500000 /dev/zero; printf ERROR-END; } >&2"}))
            .await
            .unwrap().output;
        for (stream, start, end) in [
            ("stdout", "BEGIN", "END"),
            ("stderr", "ERROR-BEGIN", "ERROR-END"),
        ] {
            let (head, tail) = output[stream]
                .as_str()
                .unwrap()
                .split_once("\n... [output truncated] ...\n")
                .unwrap();
            assert!(head.starts_with(start));
            assert!(tail.ends_with(end));
            assert_eq!((head.len(), tail.len()), (524_288, 524_288));
            let path = output[format!("{stream}_path")].as_str().unwrap();
            let full = std::fs::read(path).unwrap();
            assert_eq!(full.len(), start.len() + 1_500_000 + end.len());
            assert_eq!(&full[..start.len()], start.as_bytes());
            assert!(
                full[start.len()..start.len() + 1_500_000]
                    .iter()
                    .all(|&b| b == 0)
            );
            assert_eq!(&full[start.len() + 1_500_000..], end.as_bytes());
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            std::fs::remove_file(path).unwrap();
        }
        assert_eq!(output["stdout_truncated"], true);
        assert_eq!(output["stderr_truncated"], true);
        assert_eq!(output["exit_code"], 0);
    }

    #[tokio::test]
    async fn preserves_large_output_on_timeout() {
        let output = ExecCommandTool
            .execute(json!({
                "cmd": "head -c 1500000 /dev/zero; printf END; sleep 10",
                "timeout_ms": 500,
            }))
            .await
            .unwrap()
            .output;
        assert_eq!(output["timed_out"], true);
        let path = output["stdout_path"].as_str().unwrap();
        let full = std::fs::read(path).unwrap();
        assert_eq!(full.len(), 1_500_003);
        assert!(full.ends_with(b"END"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn removes_unpublished_capture_files_on_drop() {
        for size in [10, 1_500_000] {
            let mut output = super::CommandOutput::new().unwrap();
            let path = output.file.as_ref().unwrap().path().to_owned();
            output.push(&vec![0xff; size]).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), vec![0xff; size]);
            drop(output);
            assert!(!path.exists());
        }
    }

    #[tokio::test]
    async fn timeout_stops_shell_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            ExecCommandTool.execute(json!({
                "cmd": "printf partial; printf diagnostic >&2; (sleep 0.3; printf leaked > marker) & wait",
                "workdir": directory.path(),
                "timeout_ms": 50,
            })),
        )
        .await
        .expect("tool must finish within its timeout")
        .unwrap().output;
        assert_eq!(output["timed_out"], true);
        assert_eq!(output["stdout"], "partial");
        assert_eq!(output["stderr"], "diagnostic");
        assert_eq!(output["exit_code"], serde_json::Value::Null);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!directory.path().join("marker").exists());
    }

    #[tokio::test]
    async fn cancellation_stops_shell_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let arguments = json!({
            "cmd": "(sleep 0.3; printf leaked > marker) & printf ready > ready; wait",
            "workdir": directory.path(),
        });
        let task = tokio::spawn(async move { ExecCommandTool.execute(arguments).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !directory.path().join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!directory.path().join("marker").exists());
    }
}
