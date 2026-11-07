use std::{io, process::Stdio, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD};
use tokio::{io::AsyncWriteExt, process::Command};

pub enum CopyOutcome {
    Confirmed,
    Terminal(String),
}

#[derive(Default)]
struct Environment {
    remote: bool,
    herdr: bool,
    tmux: bool,
    wayland: bool,
    x11: bool,
    wsl: bool,
}

impl Environment {
    fn can_read_image(&self) -> bool {
        !self.remote && !self.tmux
    }

    fn detect() -> Self {
        let tmux = std::env::var_os("TMUX").is_some() || std::env::var_os("TMUX_PANE").is_some();
        Self {
            remote: tmux
                || std::env::var_os("SSH_TTY").is_some()
                || std::env::var_os("SSH_CONNECTION").is_some(),
            herdr: std::env::var("HERDR_ENV").is_ok_and(|value| value == "1"),
            tmux,
            wayland: std::env::var_os("WAYLAND_DISPLAY").is_some(),
            x11: std::env::var_os("DISPLAY").is_some(),
            wsl: std::env::var_os("WSL_DISTRO_NAME").is_some()
                || std::env::var_os("WSL_INTEROP").is_some(),
        }
    }
}

fn osc52(text: &str, tmux: bool) -> io::Result<String> {
    if text.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty selection",
        ));
    }
    if text.len() > 100_000 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "selection exceeds the terminal clipboard limit (100000 bytes)",
        ));
    }
    let encoded = STANDARD.encode(text);
    Ok(if tmux {
        format!("\x1bPtmux;\x1b\x1b]52;c;{encoded}\x07\x1b\\")
    } else {
        format!("\x1b]52;c;{encoded}\x07")
    })
}

fn native_commands(environment: &Environment) -> Vec<(&'static str, &'static [&'static str])> {
    if environment.remote || environment.herdr {
        return Vec::new();
    }
    if cfg!(target_os = "macos") {
        vec![("pbcopy", &[])]
    } else if cfg!(target_os = "windows") || environment.wsl {
        vec![(
            "powershell.exe",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "[Console]::InputEncoding = [System.Text.UTF8Encoding]::new(); Set-Clipboard -Value ([Console]::In.ReadToEnd())",
            ],
        )]
    } else {
        let mut commands: Vec<(&str, &[&str])> = Vec::new();
        if environment.wayland {
            commands.push(("wl-copy", &[]));
        }
        if environment.x11 {
            commands.push(("xclip", &["-selection", "clipboard"]));
            commands.push(("xsel", &["--clipboard", "--input"]));
        }
        commands
    }
}

async fn native_copy(command: &str, args: &[&str], text: &str) -> io::Result<()> {
    let mut child = Command::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let operation = async {
        let mut input = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("clipboard stdin unavailable"))?;
        input.write_all(text.as_bytes()).await?;
        input.shutdown().await?;
        drop(input);
        let status = child.wait().await?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!("{command} exited with {status}")))
        }
    };
    tokio::time::timeout(Duration::from_secs(2), operation)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "clipboard command timed out"))?
}

pub async fn copy(text: String) -> io::Result<CopyOutcome> {
    if text.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty selection",
        ));
    }
    let environment = Environment::detect();
    for (command, args) in native_commands(&environment) {
        if native_copy(command, args, &text).await.is_ok() {
            return Ok(CopyOutcome::Confirmed);
        }
    }
    osc52(&text, environment.tmux).map(CopyOutcome::Terminal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_copy_encodes_unicode_and_wraps_tmux() {
        assert_eq!(
            osc52("Привет 🙂\nnext", false).unwrap(),
            "\x1b]52;c;0J/RgNC40LLQtdGCIPCfmYIKbmV4dA==\x07"
        );
        assert_eq!(
            osc52("hello", true).unwrap(),
            "\x1bPtmux;\x1b\x1b]52;c;aGVsbG8=\x07\x1b\\"
        );
        assert!(osc52("", false).is_err());
        assert!(osc52(&"x".repeat(100_001), true).is_err());
    }

    #[test]
    fn remote_sessions_never_write_to_the_servers_native_clipboard() {
        let environment = Environment {
            remote: true,
            herdr: false,
            tmux: true,
            wayland: true,
            x11: true,
            wsl: true,
        };
        assert!(native_commands(&environment).is_empty());
    }

    #[test]
    fn local_herdr_can_read_images_but_ssh_and_tmux_cannot() {
        let local = Environment {
            herdr: true,
            ..Default::default()
        };
        assert!(local.can_read_image());
        assert!(native_commands(&local).is_empty());
        assert!(
            !Environment {
                remote: true,
                herdr: true,
                ..Default::default()
            }
            .can_read_image()
        );
        assert!(
            !Environment {
                tmux: true,
                ..Default::default()
            }
            .can_read_image()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_copy_writes_literal_text_and_checks_the_exit_status() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("clipboard");
        native_copy(
            "sh",
            &[
                "-c",
                "cat > \"$1\"",
                "clipboard-test",
                output.to_str().unwrap(),
            ],
            "hello\n$HOME `whoami` 界🙂",
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(output).unwrap(),
            "hello\n$HOME `whoami` 界🙂"
        );
        assert!(native_copy("sh", &["-c", "exit 1"], "text").await.is_err());
        assert!(
            native_copy("/nonexistent/kodrik-clipboard", &[], "text")
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stalled_clipboard_process_is_bounded() {
        let error = native_copy("sh", &["-c", "exec sleep 30"], "text")
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}

pub(super) async fn image_bytes() -> anyhow::Result<Vec<u8>> {
    use anyhow::{Context, ensure};
    use tokio::io::AsyncReadExt;
    let environment = Environment::detect();
    ensure!(
        environment.can_read_image(),
        "Clipboard images are unavailable over SSH/tmux; paste a file path instead"
    );
    let mut command;
    // macOS AppleScript can read PNG clipboard data without an extra dependency.
    let temporary = tempfile::NamedTempFile::new()?;
    if cfg!(target_os = "macos") {
        command = Command::new("osascript");
        command.args(["-e", "on run argv\nset f to open for access POSIX file (item 1 of argv) with write permission\ntry\nset eof f to 0\nwrite (the clipboard as «class PNGf») to f\nclose access f\non error msg\nclose access f\nerror msg\nend try\nend run", temporary.path().to_str().context("Invalid clipboard path")?]);
    } else if environment.wayland {
        command = Command::new("wl-paste");
        command.args(["--no-newline", "--type", "image/png"]);
    } else if environment.x11 {
        command = Command::new("xclip");
        command.args(["-selection", "clipboard", "-t", "image/png", "-o"]);
    } else {
        anyhow::bail!("Use an image file path; native clipboard images are not supported here");
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().context("Cannot read image clipboard")?;
    let operation = async {
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .context("No clipboard output")?
            .take((crate::agent::image::MAX_IMAGE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(
            bytes.len() <= crate::agent::image::MAX_IMAGE_BYTES,
            "Clipboard image exceeds 10 MiB"
        );
        ensure!(
            child.wait().await?.success(),
            "Clipboard contains no PNG image"
        );
        if cfg!(target_os = "macos") {
            let file = tokio::fs::File::open(temporary.path()).await?;
            file.take((crate::agent::image::MAX_IMAGE_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .await?;
        }
        Ok(bytes)
    };
    tokio::time::timeout(Duration::from_secs(2), operation)
        .await
        .context("Clipboard read timed out")?
}
