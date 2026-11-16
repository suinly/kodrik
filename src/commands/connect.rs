use crate::i18n::Language;
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
    let language = Language::from_environment();
    let config = config(path)?;
    ensure!(
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
        language.pick(
            "Remote TUI requires an interactive terminal",
            "Для удалённого TUI нужен интерактивный терминал"
        )
    );
    super::tui::remote::run_with(&config, path, session).await
}

async fn response(request: reqwest::RequestBuilder) -> anyhow::Result<serde_json::Value> {
    let language = Language::from_environment();
    use futures::StreamExt;
    let response = request.send().await.map_err(|_| {
        anyhow::anyhow!(language.pick(
            "Cannot reach pairing server",
            "Не удалось подключиться к серверу сопряжения"
        ))
    })?;
    ensure!(
        response.status().is_success(),
        "{} ({})",
        language.pick("Pairing request denied", "Запрос сопряжения отклонён"),
        response.status()
    );
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| {
            anyhow::anyhow!(
                language.pick("Pairing response interrupted; do not retry an uncertain token response", "Ответ сопряжения прерван; не повторяйте запрос, если неизвестно, был ли выдан токен")
            )
        })?;
        ensure!(
            bytes.len() + chunk.len() <= 16 * 1024,
            language.pick(
                "Pairing response exceeds limit",
                "Ответ сопряжения превышает лимит"
            )
        );
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| {
        anyhow::anyhow!(language.pick("Invalid pairing response", "Некорректный ответ сопряжения"))
    })
}

async fn pair(config: &RemoteConfig) -> anyhow::Result<()> {
    let language = Language::from_environment();
    ensure!(
        !config.credential_file.try_exists()?,
        language.pick("Device credential already exists; revoke it and explicitly remove it before pairing again", "Учётные данные устройства уже существуют; отзовите доступ и явно удалите их перед повторным сопряжением")
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
        .context(language.pick("Invalid pairing ID", "Некорректный ID сопряжения"))?;
    let secret = start["polling_secret"]
        .as_str()
        .context(language.pick("Missing polling secret", "Отсутствует секрет опроса"))?;
    let code = start["code"]
        .as_str()
        .context(language.pick("Missing pairing code", "Отсутствует код сопряжения"))?;
    ensure!(
        code.len() == 8 && code.bytes().all(|b| b.is_ascii_alphanumeric()),
        language.pick("Invalid pairing code", "Некорректный код сопряжения")
    );
    eprintln!(
        "{}\n{}: {request}\n{}: {code}",
        language.pick(
            &format!(
                "Open {} and approve device pairing.",
                config.server.join("app/profile")?
            ),
            &format!(
                "Откройте {} и подтвердите подключение устройства.",
                config.server.join("app/profile")?
            )
        ),
        language.pick("Request ID", "ID запроса"),
        language.pick(
            "Compare this code on both devices",
            "Сравните этот код на обоих устройствах"
        )
    );
    std::io::stderr().flush()?;
    let expires = start["expires_at"].as_i64().context(language.pick(
        "Missing pairing expiry",
        "Отсутствует срок действия сопряжения",
    ))?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    loop {
        ensure!(
            tokio::time::Instant::now() < deadline && chrono::Utc::now().timestamp() < expires,
            language.pick("Pairing expired", "Срок сопряжения истёк")
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
                        .context(
                            language.pick("Invalid device ID", "Некорректный ID устройства"),
                        )?,
                    token: poll["token"]
                        .as_str()
                        .context(language.pick(
                            "Missing device credential",
                            "Отсутствуют учётные данные устройства",
                        ))?
                        .into(),
                };
                credential.save(&config.credential_file)?;
                eprintln!(
                    "{}", language.pick("Device paired. Credential saved privately; no model configuration is needed.", "Устройство подключено. Учётные данные сохранены приватно; настройка модели не требуется.")
                );
                return Ok(());
            }
            _ => anyhow::bail!(language.pick(
                "Pairing expired or invalid; start a new pairing request",
                "Сопряжение недействительно или его срок истёк; создайте новый запрос"
            )),
        }
    }
}
