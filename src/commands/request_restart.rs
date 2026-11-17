#[cfg(unix)]
use std::process::Command;

#[cfg(unix)]
use anyhow::Context;
use anyhow::ensure;

use crate::{config, gateway::restart::RestartCoordinator};

pub fn run(pid: u32) -> anyhow::Result<()> {
    validate_pid(pid)?;
    RestartCoordinator::new(config::data_dir()?).request(pid)
}

fn validate_pid(pid: u32) -> anyhow::Result<()> {
    ensure!(pid > 0, "Gateway PID must be positive");
    #[cfg(unix)]
    {
        let status = Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .status()
            .context("Cannot check gateway PID")?;
        ensure!(status.success(), "Gateway PID {pid} is not running");
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, WAIT_TIMEOUT},
            System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
        };
        // SAFETY: this handle is used only for a nonblocking liveness check and closed below.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        ensure!(
            !handle.is_null(),
            "Cannot open gateway PID {pid}: {}",
            std::io::Error::last_os_error()
        );
        let result = unsafe { WaitForSingleObject(handle, 0) };
        unsafe { CloseHandle(handle) };
        ensure!(result == WAIT_TIMEOUT, "Gateway PID {pid} is not running");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_live_pid_and_rejects_zero() {
        assert!(validate_pid(std::process::id()).is_ok());
        assert!(validate_pid(0).is_err());
    }
}
