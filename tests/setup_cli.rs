use std::{
    fs,
    process::{Command, Stdio},
};

#[test]
fn missing_config_without_a_terminal_exits_with_setup_instructions() {
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join("custom data");
    for command in [None, Some("gateway"), Some("--remote")] {
        let output = Command::new(env!("CARGO_BIN_EXE_kodrik"))
            .args(["--data-dir", data.to_str().unwrap()])
            .args(command)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("interactive terminal"), "{stderr}");
        assert!(!data.exists());
    }
    #[cfg(unix)]
    {
        let default = home.path().join(".kodrik");
        fs::create_dir(&default).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_kodrik"))
            .args(["ask", "hello"])
            .env("HOME", home.path())
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(".kodrik/config.toml"), "{stderr}");
        assert!(stderr.contains("interactive terminal"), "{stderr}");
        assert!(!default.join("config.toml").exists());
    }
}

#[test]
fn broken_existing_config_is_not_replaced() {
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join("config.toml");
    let contents = "[not valid TOML\n";
    fs::write(&path, contents).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_kodrik"))
        .args(["ask", "hello", "--data-dir"])
        .arg(data.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("TOML"));
    assert_eq!(fs::read_to_string(path).unwrap(), contents);
}
