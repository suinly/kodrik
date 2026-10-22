use std::process::Command;

use anyhow::{Context, ensure};

use crate::{config, gateway::restart::RestartCoordinator};

pub fn run(pid: u32) -> anyhow::Result<()> {
    validate_pid(pid)?;
    RestartCoordinator::new(config::data_dir()?).request(pid)
}

fn validate_pid(pid: u32) -> anyhow::Result<()> {
    ensure!(pid > 0, "Gateway PID must be positive");
    let status = Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .status()
        .context("Cannot check gateway PID")?;
    ensure!(status.success(), "Gateway PID {pid} is not running");
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
