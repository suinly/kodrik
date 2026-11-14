#![cfg(unix)]

use sha2::{Digest, Sha256};
use std::{
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Gateway(Child);
impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn web_only_gateway_exec_restarts_with_same_pid_new_instance_and_drained_response() {
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join(".kodrik");
    std::fs::create_dir_all(data.join("web")).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = port.local_addr().unwrap();
    let origin = format!("http://{address}");
    let base = format!("{origin}/isolated/");
    // Only loopback URLs; no Telegram, autonomous LLM work or real credentials.
    std::fs::write(
        data.join("config.toml"),
        format!(
            r#"
[gateway]
[provider.primary]
type = "openai"
model = "test"
api_key = "unused-test-key"
api_base = "http://127.0.0.1:1/v1"
[companion]
learning_enabled = false
proactive_enabled = false
[web]
enabled = true
listen = "{address}"
base_url = "{base}"
"#
        ),
    )
    .unwrap();
    let token = "a".repeat(64);
    let hash = format!("{:x}", Sha256::digest(token.as_bytes()));
    std::fs::write(
        data.join("web/auth.json"),
        serde_json::to_vec(&serde_json::json!({
            "password_hash": null,
            "generation": null,
            "sessions": {hash: chrono::Utc::now().timestamp() + 300}
        }))
        .unwrap(),
    )
    .unwrap();
    let log = std::fs::File::create(home.path().join("gateway.log")).unwrap();
    drop(port);
    let mut gateway = Gateway(
        Command::new(env!("CARGO_BIN_EXE_kodrik"))
            .arg("gateway")
            .env("HOME", home.path())
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
    );
    let pid = gateway.0.id();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let cookie = format!("kodrik_web_session={token}");
    let status_url = format!("{base}api/gateway");
    let deadline = Instant::now() + Duration::from_secs(30);
    let initial = loop {
        assert!(
            Instant::now() < deadline,
            "gateway failed to start: {}",
            std::fs::read_to_string(home.path().join("gateway.log")).unwrap()
        );
        assert!(
            gateway.0.try_wait().unwrap().is_none(),
            "isolated gateway exited: {}",
            std::fs::read_to_string(home.path().join("gateway.log")).unwrap()
        );
        if let Ok(response) = client
            .get(&status_url)
            .header("cookie", &cookie)
            .send()
            .await
        {
            if response.status().is_success() {
                break response.json::<serde_json::Value>().await.unwrap();
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(initial["pid"], pid);
    assert_eq!(initial["restart_available"], true);
    let response = client
        .post(format!("{base}api/gateway/restart"))
        .header("cookie", &cookie)
        .header("origin", &origin)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    assert_eq!(response.headers()["cache-control"], "no-store");
    // Reading the complete JSON body must succeed even though shutdown is queued.
    let accepted: serde_json::Value = response.json().await.unwrap();
    assert_eq!(accepted["instance_id"], initial["instance_id"]);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "exec failed: {}",
            std::fs::read_to_string(home.path().join("gateway.log")).unwrap()
        );
        assert!(
            gateway.0.try_wait().unwrap().is_none(),
            "gateway exited instead of exec"
        );
        if let Ok(response) = client
            .get(&status_url)
            .header("cookie", &cookie)
            .send()
            .await
        {
            if response.status().is_success() {
                if let Ok(current) = response.json::<serde_json::Value>().await {
                    if current["instance_id"] != initial["instance_id"] {
                        assert_eq!(current["pid"], pid);
                        assert_eq!(current["restart_available"], true);
                        break;
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
