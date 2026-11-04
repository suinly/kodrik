use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
};

#[test]
fn pairing_cli_works_without_provider_keeps_prefix_and_never_prints_token() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = format!("http://{}/kodrik/", listener.local_addr().unwrap());
    let request = uuid::Uuid::new_v4();
    let device = uuid::Uuid::new_v4();
    let fixture = std::thread::spawn(move || {
        for (path, body) in [
            (
                "/kodrik/api/devices/pair/start",
                serde_json::json!({"request_id":request,"code":"ABCD2345","polling_secret":"private-poll-secret","expires_at":chrono::Utc::now().timestamp()+300}),
            ),
            (
                "/kodrik/api/devices/pair/poll",
                serde_json::json!({"status":"authorized","device_id":device,"token":"private-device-token"}),
            ),
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            assert_eq!(first.trim(), format!("POST {path} HTTP/1.1"));
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes).unwrap();
            let input: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            if path.ends_with("poll") {
                assert_eq!(input["polling_secret"], "private-poll-secret");
                assert_eq!(input["request_id"], request.to_string());
            }
            let body = body.to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    let path = dir.path().join("remote.toml");
    std::fs::write(
        &path,
        format!("server = '{server}'\nlabel = 'Test laptop'\n"),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_kodrik"))
        .args(["connect", "--remote-config"])
        .arg(&path)
        .env("HOME", dir.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    fixture.join().unwrap();
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{text}");
    assert!(text.contains(&format!("{server}app/profile")) && text.contains("ABCD2345"));
    assert!(text.contains(&request.to_string()));
    assert!(!text.contains("private-device-token") && !text.contains("private-poll-secret"));
    let credential: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("remote/device.json")).unwrap())
            .unwrap();
    assert_eq!(credential["device_id"], device.to_string());
    assert_eq!(credential["token"], "private-device-token");
    let output = Command::new(env!("CARGO_BIN_EXE_kodrik"))
        .args(["--remote", "--remote-config"])
        .arg(&path)
        .env("HOME", dir.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("interactive terminal"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("config.toml"));
}
