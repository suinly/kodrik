use std::{
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[test]
fn production_worker_starts_without_provider_and_reaps_a_registered_shell_on_eof() {
    use std::os::{
        fd::OwnedFd,
        unix::{net::UnixStream, process::CommandExt},
    };
    let dir = tempfile::tempdir().unwrap();
    let (lifetime, reader) = UnixStream::pair().unwrap();
    let mut worker = Command::new(env!("CARGO_BIN_EXE_kodrik"))
        .arg("remote-supervisor-worker")
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(OwnedFd::from(reader)))
        .spawn()
        .unwrap();
    let mut ack = BufReader::new(worker.stdout.take().unwrap());
    let mut line = String::new();
    ack.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "READY");
    let mut shell = Command::new("/bin/sh")
        .args(["-c", "read gate; [ \"$gate\" = GO ] && sleep 30"])
        .current_dir(dir.path())
        .process_group(0)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(worker.stdin.as_mut().unwrap(), "REGISTER {}", shell.id()).unwrap();
    line.clear();
    ack.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "OK");
    shell.stdin.take().unwrap().write_all(b"GO\n").unwrap();
    drop(lifetime);
    let started = Instant::now();
    while worker.try_wait().unwrap().is_none() && started.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let result = worker.try_wait().unwrap();
    if result.is_none() {
        worker.kill().unwrap();
        worker.wait().unwrap();
    }
    if shell.try_wait().unwrap().is_none() {
        shell.kill().unwrap();
    }
    let shell_status = shell.wait().unwrap();
    assert!(result.is_some_and(|s| s.success()));
    assert!(!shell_status.success());
}
