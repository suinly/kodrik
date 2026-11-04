use super::supervisor::{Supervisor, gated_command};
use std::{
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};

#[test]
fn worker() {
    if std::env::var_os("KODRIK_SUPERVISOR_WORKER").is_some() {
        // Test harness output must not become protocol bytes.
        match super::supervisor::worker() {
            Ok(()) => std::process::exit(0),
            Err(error) => {
                println!("WORKER ERROR: {error:#}");
                std::process::exit(1);
            }
        }
    }
}

fn alive(pid: u32) -> bool {
    // SAFETY: querying a positive fixture process ID without sending a signal.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn wait_file(path: &Path) -> String {
    let start = Instant::now();
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if !text.is_empty() {
                return text;
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "fixture did not start"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn lifetime_eof_stops_term_ignoring_payload_and_reaps_supervisor() {
    let dir = tempfile::tempdir().unwrap();
    let supervisor = Supervisor::spawn().unwrap();
    let worker = supervisor.pid();
    let mut shell = gated_command(
        &supervisor,
        "trap '' TERM; echo ready > started; while :; do sleep 1; done",
        dir.path(),
    )
    .unwrap();
    wait_file(&dir.path().join("started"));
    let pid = shell.id();
    let start = Instant::now();
    drop(supervisor);
    let status = shell.wait().unwrap();
    assert!(!status.success());
    assert!(start.elapsed() >= Duration::from_secs(2));
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(!alive(pid));
    assert!(!alive(worker));
}

#[test]
fn stop_all_prevents_payload_and_children_from_outliving_client() {
    let dir = tempfile::tempdir().unwrap();
    let supervisor = Supervisor::spawn().unwrap();
    let mut shell =
        gated_command(&supervisor, "sleep 1; echo leaked > effect", dir.path()).unwrap();
    supervisor.stop_all().unwrap();
    shell.wait().unwrap();
    assert!(!dir.path().join("effect").exists());
}

#[test]
fn killed_client_loses_lifetime_writer_even_with_exec_child_running() {
    let dir = tempfile::tempdir().unwrap();
    let mut client = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "remote::supervisor_tests::client_worker",
            "--nocapture",
        ])
        .env("KODRIK_SUPERVISOR_CLIENT", dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid: u32 = wait_file(&dir.path().join("pid")).trim().parse().unwrap();
    client.kill().unwrap();
    client.wait().unwrap();
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        !dir.path().join("effect").exists(),
        "payload survived SIGKILL client"
    );
    // Some hosts retain an orphan zombie briefly; no running payload is the guarantee.
    if alive(pid) {
        let status = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&status.stdout)
                .trim()
                .starts_with('Z')
        );
    }
}

#[test]
fn client_worker() {
    let Some(dir) = std::env::var_os("KODRIK_SUPERVISOR_CLIENT") else {
        return;
    };
    let supervisor = Supervisor::spawn().unwrap();
    let _child = gated_command(
        &supervisor,
        "echo $$ > pid; sleep 2; echo escaped > effect",
        Path::new(&dir),
    )
    .unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[test]
fn partial_control_frame_cannot_block_lifetime_cleanup() {
    use std::io::Write;
    let (lifetime, reader) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut worker = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "remote::supervisor_tests::worker", "--nocapture"])
        .env("KODRIK_SUPERVISOR_WORKER", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::os::fd::OwnedFd::from(reader)))
        .spawn()
        .unwrap();
    worker
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"REGISTER ")
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    drop(lifetime);
    let start = Instant::now();
    while worker.try_wait().unwrap().is_none() && start.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let exited = worker.try_wait().unwrap().is_some();
    if !exited {
        worker.kill().unwrap();
        worker.wait().unwrap();
    }
    assert!(
        exited,
        "worker blocked on a partial control line after lifetime EOF"
    );
}

#[test]
fn escaped_session_is_an_explicit_limit_and_fixture_cleans_it_up() {
    let dir = tempfile::tempdir().unwrap();
    let supervisor = Supervisor::spawn().unwrap();
    let mut shell = gated_command(&supervisor, "sleep 30", dir.path()).unwrap();
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "remote::supervisor_tests::escaped_worker",
            "--nocapture",
        ])
        .env("KODRIK_ESCAPED_FIXTURE", dir.path())
        .process_group(shell.id() as i32)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_file(&dir.path().join("escaped"));
    // The child escaped the registered shell's group via setsid.
    supervisor.stop_all().unwrap();
    shell.wait().unwrap();
    let survived = child.try_wait().unwrap().is_none();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        survived,
        "fixture should demonstrate the documented setsid escape"
    );
}

#[test]
fn escaped_worker() {
    let Some(dir) = std::env::var_os("KODRIK_ESCAPED_FIXTURE") else {
        return;
    };
    // SAFETY: fixture child is not a process-group leader; no Rust code runs inside a fork callback.
    assert!(unsafe { libc::setsid() } > 0);
    std::fs::write(
        Path::new(&dir).join("escaped"),
        std::process::id().to_string(),
    )
    .unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[tokio::test]
async fn escaped_descendant_holding_stdout_cannot_block_exec_collection() {
    let dir = tempfile::tempdir().unwrap();
    let supervisor = std::sync::Arc::new(Supervisor::spawn().unwrap());
    let exe = std::env::current_exe().unwrap();
    let cmd = format!(
        "KODRIK_ESCAPED_FIXTURE='{}' '{}' --exact remote::supervisor_tests::escaped_worker --nocapture & wait",
        dir.path().display(),
        exe.display()
    );
    let start = Instant::now();
    let output = crate::agent::tools::exec_command::execute(
        serde_json::json!({"cmd":cmd,"workdir":dir.path(),"timeout_ms":500}),
        Some(supervisor.clone()),
    )
    .await
    .unwrap();
    let pid: u32 = wait_file(&dir.path().join("escaped")).parse().unwrap();
    // SAFETY: terminate only the exact escaped fixture PID, which intentionally outlives its group.
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    supervisor.stop_all().unwrap();
    assert_eq!(output.output["timed_out"], true);
    assert!(
        start.elapsed() < Duration::from_secs(4),
        "escaped stdout blocked collection"
    );
}

#[test]
fn failed_cleanup_remains_an_error_on_repeated_close() {
    let supervisor = Supervisor::spawn().unwrap();
    // SAFETY: stop only the private fixture worker, not an application process.
    assert_eq!(
        unsafe { libc::kill(supervisor.pid() as i32, libc::SIGKILL) },
        0
    );
    std::thread::sleep(Duration::from_millis(30));
    assert!(supervisor.stop_all().is_err());
    assert!(
        supervisor.stop_all().is_err(),
        "cleanup failure was replaced by a false success"
    );
}

#[test]
fn remote_exec_uses_the_same_local_bin_path_as_standalone() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".local/bin")).unwrap();
    let path = dir.path().join(".local/bin/kodrik_path_fixture");
    std::fs::write(&path, "#!/bin/sh\necho path-ok\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "remote::supervisor_tests::path_worker",
            "--nocapture",
        ])
        .env("KODRIK_PATH_FIXTURE", "1")
        .env("HOME", dir.path())
        .env("PATH", "/bin:/usr/bin")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[tokio::test]
async fn path_worker() {
    if std::env::var_os("KODRIK_PATH_FIXTURE").is_none() {
        return;
    }
    let supervisor = std::sync::Arc::new(Supervisor::spawn().unwrap());
    for remote in [None, Some(supervisor.clone())] {
        let output = crate::agent::tools::exec_command::execute(
            serde_json::json!({"cmd":"kodrik_path_fixture"}),
            remote,
        )
        .await
        .unwrap();
        assert_eq!(output.output["exit_code"], 0);
        assert_eq!(output.output["stdout"], "path-ok\n");
    }
    supervisor.stop_all().unwrap();
}
