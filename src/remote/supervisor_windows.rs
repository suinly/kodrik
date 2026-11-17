use anyhow::{Context, ensure};
use std::{
    collections::HashMap,
    io::Write,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, OnceLock, Weak},
};
use windows_sys::Win32::{
    Foundation::CloseHandle,
    System::{
        JobObjects::*,
        Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE},
    },
};

// Store the handle as an integer so the owner can cross threads; access is read-only.
struct Job(usize);
impl Job {
    fn create(pid: u32) -> anyhow::Result<Self> {
        // SAFETY: null security/name pointers request an unnamed, non-inheritable job.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        ensure!(
            !handle.is_null(),
            "Cannot create process job: {}",
            std::io::Error::last_os_error()
        );
        let job = Self(handle as usize);
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        };
        ensure!(
            ok != 0,
            "Cannot configure process job: {}",
            std::io::Error::last_os_error()
        );
        let process = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        ensure!(
            !process.is_null(),
            "Cannot open gated shell: {}",
            std::io::Error::last_os_error()
        );
        let result = unsafe { AssignProcessToJobObject(handle, process) };
        let error = std::io::Error::last_os_error();
        unsafe { CloseHandle(process) };
        ensure!(result != 0, "Cannot contain shell in process job: {error}");
        Ok(job)
    }
    fn stop(&self) -> std::io::Result<()> {
        if unsafe { TerminateJobObject(self.0 as _, 1) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0 as _) };
    }
}
fn registry() -> &'static Mutex<HashMap<u32, Weak<Job>>> {
    static JOBS: OnceLock<Mutex<HashMap<u32, Weak<Job>>>> = OnceLock::new();
    JOBS.get_or_init(Default::default)
}
pub(crate) struct Supervisor(Mutex<Option<HashMap<u32, Arc<Job>>>>);
impl Supervisor {
    pub(crate) fn spawn() -> anyhow::Result<Self> {
        Ok(Self(Mutex::new(Some(HashMap::new()))))
    }
    pub(crate) fn register_group(&self, pid: u32) -> anyhow::Result<()> {
        ensure!(pid != 0, "Invalid process ID");
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("Process job lock poisoned"))?;
        let jobs = state.as_mut().context("Process supervisor is closed")?;
        ensure!(
            jobs.len() < 16 && !jobs.contains_key(&pid),
            "Process job registration limit reached"
        );
        let job = Arc::new(Job::create(pid)?);
        registry()
            .lock()
            .map_err(|_| anyhow::anyhow!("Process registry poisoned"))?
            .insert(pid, Arc::downgrade(&job));
        jobs.insert(pid, job);
        Ok(())
    }
    pub(crate) fn unregister_group(&self, pid: u32) -> anyhow::Result<()> {
        if let Some(jobs) = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("Process job lock poisoned"))?
            .as_mut()
        {
            jobs.remove(&pid);
        }
        registry()
            .lock()
            .map_err(|_| anyhow::anyhow!("Process registry poisoned"))?
            .remove(&pid);
        Ok(())
    }
    pub(crate) fn stop_all(&self) -> anyhow::Result<()> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("Process job lock poisoned"))?;
        if let Some(jobs) = state.as_ref() {
            for job in jobs.values() {
                job.stop()?;
            }
        }
        if let Some(jobs) = state.take() {
            if let Ok(mut registry) = registry().lock() {
                for pid in jobs.keys() {
                    registry.remove(pid);
                }
            }
        }
        Ok(())
    }
}
impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.stop_all();
    }
}
pub(crate) fn signal_group(pid: u32, _: i32) -> std::io::Result<()> {
    let job = registry()
        .lock()
        .map_err(|_| std::io::Error::other("Process registry poisoned"))?
        .get(&pid)
        .and_then(Weak::upgrade);
    if let Some(job) = job {
        job.stop()?;
    }
    // Missing means registration failed before GO, or stop_all already killed and closed the job.
    // Both callers retain a kill-on-drop Child until registration succeeds.
    Ok(())
}
pub(crate) fn worker() -> anyhow::Result<()> {
    anyhow::bail!("Windows cleanup is provided by kernel Job Objects, not a worker process")
}
pub(crate) fn prepared_command(payload: &str, root: &Path) -> Command {
    let mut command = Command::new("powershell.exe");
    let script = format!(
        "if ([Console]::ReadLine() -ne 'GO') {{ exit 1 }}; [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); & ([scriptblock]::Create('{}'))",
        payload.replace('\'', "''")
    );
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}
pub(crate) fn gated_command(
    supervisor: &Supervisor,
    payload: &str,
    root: &Path,
) -> anyhow::Result<Child> {
    let mut child = prepared_command(payload, root).spawn()?;
    if let Err(error) = supervisor.register_group(child.id()).and_then(|_| {
        child
            .stdin
            .take()
            .context("Missing launch gate")?
            .write_all(b"GO\n")?;
        Ok(())
    }) {
        let _ = signal_group(child.id(), 0);
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok(child)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gated_powershell_preserves_quotes() {
        let supervisor = Supervisor::spawn().unwrap();
        let child = gated_command(
            &supervisor,
            "[Console]::Write('it''s working')",
            Path::new("."),
        )
        .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "it's working");
    }
    #[test]
    fn stopping_supervisor_terminates_live_job() {
        let supervisor = Supervisor::spawn().unwrap();
        let mut child =
            gated_command(&supervisor, "Start-Sleep -Seconds 60", Path::new(".")).unwrap();
        supervisor.stop_all().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(!status.success());
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Job process survived termination"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    #[test]
    fn closed_supervisor_rejects_registration() {
        let supervisor = Supervisor::spawn().unwrap();
        supervisor.stop_all().unwrap();
        assert!(supervisor.register_group(std::process::id()).is_err());
    }
}
