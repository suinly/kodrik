use anyhow::{Context, ensure};
use std::{
    collections::HashSet,
    io::{BufReader, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::Path,
    process::{Child, Command, Stdio},
    sync::Mutex,
    time::Duration,
};

fn read_control_line(reader: &mut BufReader<std::process::ChildStdout>) -> anyhow::Result<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut line = Vec::new();
    loop {
        if reader.buffer().is_empty() {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            ensure!(
                !remaining.is_zero(),
                "Cleanup worker acknowledgement timed out"
            );
            let mut fd = libc::pollfd {
                fd: reader.get_ref().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: fd is an initialized entry with a live child output descriptor.
            let ready = unsafe {
                libc::poll(
                    &mut fd,
                    1,
                    remaining.as_millis().min(i32::MAX as u128) as i32,
                )
            };
            if ready < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            ensure!(ready > 0, "Cleanup worker acknowledgement unavailable");
        }
        let mut byte = [0u8];
        ensure!(
            reader.read(&mut byte)? == 1,
            "Cleanup worker exited; partial stdout={:?}",
            String::from_utf8_lossy(&line)
        );
        if byte[0] == b'\n' {
            return Ok(String::from_utf8(line)?);
        }
        line.push(byte[0]);
        ensure!(
            line.len() <= 256,
            "Cleanup worker response too long; partial stdout={:?}",
            String::from_utf8_lossy(&line)
        );
    }
}

struct Control {
    lifetime: Option<UnixStream>,
    writer: std::process::ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    child: Child,
    closed: bool,
    failure: Option<String>,
}

pub(crate) struct Supervisor(Mutex<Control>);

impl Supervisor {
    pub(crate) fn spawn() -> anyhow::Result<Self> {
        // Rust-created sockets are CLOEXEC. Only the read half becomes the worker's stderr.
        let (lifetime, worker_lifetime) = UnixStream::pair()?;
        let mut command = Command::new(std::env::current_exe()?);
        #[cfg(not(test))]
        command.arg("remote-supervisor-worker");
        #[cfg(test)]
        command
            .args([
                "--exact",
                "remote::supervisor_tests::worker",
                "--nocapture",
                "--format=terse",
            ])
            .env("KODRIK_SUPERVISOR_WORKER", "1");
        // stderr is private lifetime input, not a log channel. stdout is ACK-only.
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(std::os::fd::OwnedFd::from(worker_lifetime)))
            .spawn()
            .context("Cannot spawn local cleanup worker")?;
        let writer = child.stdin.take().context("Missing worker input")?;
        let reader = BufReader::new(child.stdout.take().context("Missing worker output")?);
        let mut control = Control {
            lifetime: Some(lifetime),
            writer,
            reader,
            child,
            closed: false,
            failure: None,
        };
        // libtest has a preamble in fixtures; the production worker emits only READY.
        let mut startup = Vec::new();
        let ready = (|| {
            for _ in 0..8 {
                let line = read_control_line(&mut control.reader)?;
                if line.trim() == "READY" {
                    return Ok(());
                }
                startup.push(line);
            }
            anyhow::bail!("Cleanup worker did not initialize")
        })();
        if let Err(error) = ready {
            let status = control.child.try_wait();
            let mut diagnostics = Vec::new();
            if let Some(lifetime) = &mut control.lifetime {
                if lifetime.set_nonblocking(true).is_ok() {
                    let _ = lifetime.take(4096).read_to_end(&mut diagnostics);
                }
            }
            control.lifetime.take();
            let _ = control.child.kill();
            let _ = control.child.wait();
            anyhow::bail!(
                "{error:#}; cleanup worker startup pid={} status={status:?} stdout={startup:?} stderr={:?}",
                control.child.id(),
                String::from_utf8_lossy(&diagnostics)
            );
        }
        Ok(Self(Mutex::new(control)))
    }

    pub(crate) fn register_group(&self, pid: u32) -> anyhow::Result<()> {
        ensure!(pid > 1 && pid <= i32::MAX as u32, "Invalid process group");
        self.request(&format!("REGISTER {pid}"))
    }

    pub(crate) fn unregister_group(&self, pid: u32) -> anyhow::Result<()> {
        self.request(&format!("FORGET {pid}"))
    }

    fn request(&self, request: &str) -> anyhow::Result<()> {
        let mut control = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("Cleanup worker lock poisoned"))?;
        ensure!(!control.closed, "Cleanup worker is closed");
        writeln!(control.writer, "{request}")?;
        control.writer.flush()?;
        ensure!(
            read_control_line(&mut control.reader)?.trim() == "OK",
            "Cleanup registration not acknowledged"
        );
        Ok(())
    }

    pub(crate) fn stop_all(&self) -> anyhow::Result<()> {
        let mut control = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("Cleanup worker lock poisoned"))?;
        if let Some(failure) = &control.failure {
            anyhow::bail!("{failure}");
        }
        if !control.closed {
            control.closed = true;
            control.lifetime.take();
            let result = (|| -> anyhow::Result<()> {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let status = loop {
                    if let Some(status) = control.child.try_wait()? {
                        break status;
                    }
                    ensure!(
                        std::time::Instant::now() < deadline,
                        "Cleanup worker did not exit; local process outcomes are unknown"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                };
                if !status.success() {
                    anyhow::bail!("Cleanup worker failed: {status}");
                }
                Ok(())
            })();
            if let Err(error) = result {
                control.failure = Some(format!("{error:#}"));
                // No live authority remains; reap a stuck worker without claiming its groups stopped.
                let _ = control.child.kill();
                let _ = control.child.wait();
                return Err(error);
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn pid(&self) -> u32 {
        self.0.lock().unwrap().child.id()
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        if let Err(error) = self.stop_all() {
            log::warn!("Local cleanup failed: {error}");
        }
    }
}

pub(crate) fn signal_group(pid: u32, signal: i32) -> std::io::Result<()> {
    if pid <= 1 || pid > i32::MAX as u32 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Invalid group",
        ));
    }
    // SAFETY: the positive registered shell PID is also its process group ID.
    if unsafe { libc::kill(-(pid as i32), signal) } == -1 {
        let error = std::io::Error::last_os_error();
        #[cfg(target_os = "macos")]
        if error.raw_os_error() == Some(libc::EPERM) && zombie_only_group(pid)? {
            return Ok(());
        }
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn zombie_only_group(group: u32) -> std::io::Result<bool> {
    // Darwin returns EPERM, not ESRCH, when only unreaped zombies remain.
    const PROC_PGRP_ONLY: u32 = 2;
    let mut pids = vec![0i32; 4096];
    // SAFETY: the vector is writable for the exact byte count passed to libproc.
    let count = unsafe {
        libc::proc_listpids(
            PROC_PGRP_ONLY,
            group,
            pids.as_mut_ptr().cast(),
            (pids.len() * size_of::<i32>()) as i32,
        )
    };
    if count < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if count as usize >= pids.len() * size_of::<i32>() {
        return Ok(false);
    }
    for &pid in &pids[..count as usize / size_of::<i32>()] {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
        // SAFETY: proc_pidinfo initializes info only when returning its full struct size.
        let bytes = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size_of::<libc::proc_bsdinfo>() as i32,
            )
        };
        if bytes == size_of::<libc::proc_bsdinfo>() as i32 {
            // SAFETY: full struct was initialized by proc_pidinfo above.
            if unsafe { info.assume_init() }.pbi_status != libc::SZOMB {
                return Ok(false);
            }
        } else if bytes < 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn worker() -> anyhow::Result<()> {
    let mut groups = HashSet::new();
    let mut pending = Vec::new();
    let mut output = std::io::stdout();
    writeln!(output, "READY")?;
    output.flush()?;
    loop {
        let mut fds = [
            libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: 2,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: fds has two initialized entries and remains valid for this call.
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
        if result < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if fds[1].revents != 0 {
            break;
        }
        if fds[0].revents != 0 {
            let mut byte = [0u8];
            // SAFETY: byte is writable for one byte; fd 0 is the private control pipe.
            if unsafe { libc::read(0, byte.as_mut_ptr().cast(), 1) } != 1 {
                break;
            }
            if byte[0] != b'\n' {
                pending.push(byte[0]);
                if pending.len() > 128 {
                    break;
                }
                continue;
            }
            let Ok(line) = String::from_utf8(std::mem::take(&mut pending)) else {
                break;
            };
            let Some((operation, pid)) = line.trim().split_once(' ') else {
                break;
            };
            let Ok(pid) = pid.parse::<u32>() else { break };
            if pid <= 1 || pid > i32::MAX as u32 {
                break;
            }
            match operation {
                "REGISTER" if groups.len() < 16 => {
                    groups.insert(pid);
                }
                "FORGET" => {
                    groups.remove(&pid);
                }
                _ => break,
            }
            if writeln!(output, "OK").and_then(|_| output.flush()).is_err() {
                break;
            }
        }
    }
    let mut failure = None;
    for &pid in &groups {
        if let Err(error) = signal_group(pid, libc::SIGTERM) {
            failure = Some(error);
        }
    }
    if !groups.is_empty() {
        std::thread::sleep(Duration::from_secs(2));
    }
    for pid in groups {
        if let Err(error) = signal_group(pid, libc::SIGKILL) {
            failure = Some(error);
        }
    }
    if let Some(error) = failure {
        return Err(error.into());
    }
    Ok(())
}

// stdin is the launch gate, so the shell cannot execute the payload before registration ACK.
pub(crate) fn gated_command(
    supervisor: &Supervisor,
    payload: &str,
    root: &Path,
) -> anyhow::Result<Child> {
    let mut child = prepared_command(payload, root);
    let mut child = child.spawn()?;
    if let Err(error) = supervisor.register_group(child.id()).and_then(|_| {
        let mut gate = child.stdin.take().context("Missing launch gate")?;
        gate.write_all(b"GO\n")?;
        Ok(())
    }) {
        let _ = signal_group(child.id(), libc::SIGKILL);
        let _ = child.wait();
        return Err(error);
    }
    Ok(child)
}

pub(crate) fn prepared_command(payload: &str, root: &Path) -> Command {
    let mut child = Command::new("/bin/sh");
    use std::os::unix::process::CommandExt;
    child
        .args([
            "-c",
            "IFS= read -r gate && [ \"$gate\" = GO ] && exec /bin/sh -c \"$1\"",
            "kodrik-gate",
            payload,
        ])
        .current_dir(root)
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    child
}
