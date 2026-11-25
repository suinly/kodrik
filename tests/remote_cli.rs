use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
};

#[test]
fn pairing_cli_works_without_provider_keeps_prefix_and_never_prints_token() {
    for locale in ["en_US.UTF-8", "ru_RU.UTF-8"] {
        pairing_for_locale(locale, false);
    }
}

#[test]
fn pairing_cli_accepts_server_url_and_saves_default_configuration() {
    pairing_for_locale("en_US.UTF-8", true);
}

#[test]
fn custom_data_dir_supplies_the_default_remote_config() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("custom data");
    std::fs::create_dir(&path).unwrap();
    std::fs::write(
        path.join("remote.toml"),
        "server = 'https://example.com/'\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_kodrik"))
        .args(["--remote", "--data-dir"])
        .arg(&path)
        .env("HOME", dir.path())
        .env("LC_ALL", "en_US.UTF-8")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("interactive terminal"),
        "{output:?}"
    );
}

fn pairing_for_locale(locale: &str, quick: bool) {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = format!("http://{}/kodrik/", listener.local_addr().unwrap());
    let request = uuid::Uuid::new_v4();
    let device = uuid::Uuid::new_v4();
    let config_dir = if quick {
        dir.path().join(".kodrik")
    } else {
        dir.path().to_path_buf()
    };
    let path = config_dir.join("remote.toml");
    let pending_path = path.clone();
    #[cfg(unix)]
    let hostname = {
        let output = Command::new("hostname").output().unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
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
            } else if !quick {
                assert_eq!(input["label"], "Test laptop");
            } else {
                #[cfg(unix)]
                assert_eq!(input["label"], hostname);
            }
            if quick {
                assert!(
                    !pending_path.exists(),
                    "configuration saved before approval"
                );
            }
            let body = body.to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    let mut command = Command::new(env!("CARGO_BIN_EXE_kodrik"));
    command.arg("connect");
    if quick {
        command.arg(server.trim_end_matches('/'));
    } else {
        std::fs::write(
            &path,
            format!("server = '{server}'\nlabel = 'Test laptop'\n"),
        )
        .unwrap();
        command.arg("--remote-config").arg(&path);
    }
    let output = command
        .env("HOME", dir.path())
        .env("LC_ALL", locale)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{text}");
    fixture.join().unwrap();
    assert!(text.contains(&format!("{server}app/profile")) && text.contains("ABCD2345"));
    assert!(text.contains(&request.to_string()));
    assert!(
        text.contains(if locale.starts_with("ru") {
            "Сравните этот код"
        } else {
            if quick {
                "Enter this code"
            } else {
                "Compare this code"
            }
        }),
        "{text}"
    );
    assert!(
        text.contains(if locale.starts_with("ru") {
            "Устройство подключено"
        } else {
            "Device paired"
        }),
        "{text}"
    );
    assert!(!text.contains("private-device-token") && !text.contains("private-poll-secret"));
    if quick {
        let url = format!("{server}app/profile?pair_request={request}#device-connections");
        assert!(text.lines().any(|line| line == url), "{text}");
        let config: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(config["server"].as_str(), Some(server.as_str()));
        assert_eq!(config.as_table().unwrap().len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    let credential: serde_json::Value =
        serde_json::from_slice(&std::fs::read(config_dir.join("remote/device.json")).unwrap())
            .unwrap();
    assert_eq!(credential["device_id"], device.to_string());
    assert_eq!(credential["token"], "private-device-token");
    let output = Command::new(env!("CARGO_BIN_EXE_kodrik"))
        .args(["--remote", "--remote-config"])
        .arg(&path)
        .env("HOME", dir.path())
        .env("LC_ALL", locale)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(if locale.starts_with("ru") {
            "интерактивный терминал"
        } else {
            "interactive terminal"
        })
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("config.toml"));
}

#[test]
fn pairing_cli_does_not_replace_an_existing_server_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join(".kodrik");
    std::fs::create_dir(&config_dir).unwrap();
    let path = config_dir.join("remote.toml");
    let contents = "server = 'https://previous.example/'\nallow_shell = true\n";
    std::fs::write(&path, contents).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_kodrik"))
        .args(["connect", "http://localhost:1"])
        .env("HOME", dir.path())
        .env("LC_ALL", "en_US.UTF-8")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("existing remote configuration"),
        "{:?}",
        output
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), contents);
}

#[test]
fn invitation_validation_uses_cli_locale_without_changing_json_output() {
    for (locale, expected) in [
        ("en_US.UTF-8", "Invitation not found"),
        ("ru_RU.UTF-8", "Приглашение не найдено"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_kodrik"))
            .args([
                "invites",
                "revoke",
                "0000000000000000000000000000000000000000000000000000000000000001",
            ])
            .env("HOME", &home)
            .env("LC_ALL", locale)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{:?}",
            output
        );
        let output = Command::new(env!("CARGO_BIN_EXE_kodrik"))
            .args(["invites", "list"])
            .env("HOME", &home)
            .env("LC_ALL", locale)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
        let _: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    }
}
