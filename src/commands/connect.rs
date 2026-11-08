use crate::remote::{client::Credential, config::RemoteConfig};
use anyhow::{Context, ensure};
use std::{
    io::{IsTerminal, Write},
    path::Path,
    time::Duration,
};

fn config(path: Option<&Path>) -> anyhow::Result<RemoteConfig> {
    RemoteConfig::load(
        &path
            .map(Path::to_path_buf)
            .unwrap_or(crate::config::data_dir()?.join("remote.toml")),
    )
}

pub async fn run(path: Option<&Path>) -> anyhow::Result<()> {
    let config = config(path)?;
    pair(&config).await
}

pub async fn remote_tui(path: Option<&Path>, session: Option<&str>) -> anyhow::Result<()> {
    let config = config(path)?;
    ensure!(
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
        "Remote TUI requires an interactive terminal"
    );
    super::tui::remote::run_with(&config, path, session).await
}

async fn response(request: reqwest::RequestBuilder) -> anyhow::Result<serde_json::Value> {
    use futures::StreamExt;
    let response = request
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("Cannot reach pairing server"))?;
    ensure!(
        response.status().is_success(),
        "Pairing request denied ({})",
        response.status()
    );
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| {
            anyhow::anyhow!(
                "Pairing response interrupted; do not retry an uncertain token response"
            )
        })?;
        ensure!(
            bytes.len() + chunk.len() <= 16 * 1024,
            "Pairing response exceeds limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("Invalid pairing response"))
}

async fn pair(config: &RemoteConfig) -> anyhow::Result<()> {
    ensure!(
        !config.credential_file.try_exists()?,
        "Device credential already exists; revoke it and explicitly remove it before pairing again"
    );
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()?;
    let start = response(
        client
            .post(config.server.join("api/devices/pair/start")?)
            .json(&serde_json::json!({"label":config.label})),
    )
    .await?;
    let request = start["request_id"]
        .as_str()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .context("Invalid pairing ID")?;
    let secret = start["polling_secret"]
        .as_str()
        .context("Missing polling secret")?;
    let code = start["code"].as_str().context("Missing pairing code")?;
    ensure!(
        code.len() == 8 && code.bytes().all(|b| b.is_ascii_alphanumeric()),
        "Invalid pairing code"
    );
    eprintln!(
        "Open {} and approve device pairing.\nRequest ID: {request}\nCompare this code on both devices: {code}",
        config.server.join("app/profile")?
    );
    std::io::stderr().flush()?;
    let expires = start["expires_at"]
        .as_i64()
        .context("Missing pairing expiry")?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    loop {
        ensure!(
            tokio::time::Instant::now() < deadline && chrono::Utc::now().timestamp() < expires,
            "Pairing expired"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
        // An uncertain poll can consume the token; never retry this request automatically.
        let poll = response(
            client
                .post(config.server.join("api/devices/pair/poll")?)
                .json(&serde_json::json!({"request_id":request,"polling_secret":secret})),
        )
        .await?;
        match poll["status"].as_str() {
            Some("pending") => continue,
            Some("authorized") => {
                let credential = Credential {
                    device_id: poll["device_id"]
                        .as_str()
                        .and_then(|s| uuid::Uuid::parse_str(s).ok())
                        .context("Invalid device ID")?,
                    token: poll["token"]
                        .as_str()
                        .context("Missing device credential")?
                        .into(),
                };
                credential.save(&config.credential_file)?;
                eprintln!(
                    "Device paired. Credential saved privately; no model configuration is needed."
                );
                return Ok(());
            }
            _ => anyhow::bail!("Pairing expired or invalid; start a new pairing request"),
        }
    }
}
