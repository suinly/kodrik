use super::setup;
use crate::i18n::Language;
use crate::remote::{client::Credential, config::RemoteConfig};
use anyhow::{Context, ensure};
use std::{
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    time::Duration,
};

fn config_path(path: Option<&Path>) -> anyhow::Result<PathBuf> {
    match path {
        Some(path) => Ok(path.to_owned()),
        None => Ok(crate::config::data_dir()?.join("remote.toml")),
    }
}

pub async fn run(server: Option<&str>, path: Option<&Path>) -> anyhow::Result<()> {
    let language = Language::from_environment();
    let path = config_path(path)?;
    let entered_server;
    let server = if server.is_none() && setup::missing(&path)? {
        ensure!(setup::interactive(), "{} {}. {}", language.pick("Remote configuration not found:", "Не найден файл удалённой конфигурации:"), path.display(), language.pick("Run Kodrik in an interactive terminal or use `kodrik connect <server-url>`", "Запустите Kodrik в интерактивном терминале или используйте `kodrik connect <адрес-сервера>`"));
        entered_server = setup::prompt(
            language.pick("Kodrik server URL: ", "Адрес сервера Kodrik: "),
            true,
        )?;
        Some(entered_server.as_str())
    } else {
        server
    };
    let mut new_contents = None;
    let config = if let Some(server) = server {
        let mut url = reqwest::Url::parse(server)
            .context(language.pick("Invalid server URL", "Некорректный URL сервера"))?;
        if !url.path().ends_with('/') {
            url.path_segments_mut()
                .map_err(|_| {
                    anyhow::anyhow!(language.pick("Invalid server URL", "Некорректный URL сервера"))
                })?
                .push("");
        }
        crate::remote::client::endpoint(&url)?;
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {
                let config = RemoteConfig::load(&path)?;
                ensure!(config.server == url, "{}", language.pick(
                    "Server URL does not match the existing remote configuration; use kodrik connect to keep its server",
                    "URL сервера не совпадает с существующей конфигурацией; запустите kodrik connect, чтобы использовать сохранённый сервер"
                ));
                config
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let contents = toml::to_string(&std::collections::BTreeMap::from([(
                    "server",
                    url.as_str(),
                )]))?;
                let config = RemoteConfig::from_toml(&path, &contents)?;
                new_contents = Some(contents);
                config
            }
            Err(error) => return Err(error.into()),
        }
    } else {
        RemoteConfig::load(&path)?
    };
    let credential = pair(&config, server.is_some()).await?;
    if let Some(contents) = new_contents {
        setup::save_new_config(&path, &contents).context(language.pick(
            "Could not save remote configuration after approval; revoke the new device in your profile before retrying",
            "После подтверждения не удалось сохранить конфигурацию; отзовите новое устройство в профиле перед повторной попыткой",
        ))?;
    }
    credential.save(&config.credential_file).context(language.pick(
        "Could not save the credential after approval; revoke the new device in your profile before retrying",
        "После подтверждения не удалось сохранить учётные данные; отзовите новое устройство в профиле перед повторной попыткой",
    ))?;
    eprintln!("{}", language.pick("Device paired. Credential saved privately; no model configuration is needed.", "Устройство подключено. Учётные данные сохранены приватно; настройка модели не требуется."));
    Ok(())
}

pub async fn remote_tui(path: Option<&Path>, session: Option<&str>) -> anyhow::Result<()> {
    let language = Language::from_environment();
    let resolved_path = config_path(path)?;
    ensure!(
        setup::interactive(),
        language.pick(
            "Remote TUI requires an interactive terminal",
            "Для удалённого TUI нужен интерактивный терминал"
        )
    );
    if setup::missing(&resolved_path)? {
        run(None, path).await?;
    }
    let config = RemoteConfig::load(&resolved_path)?;
    if setup::missing(&config.credential_file)? {
        run(None, path).await?;
    }
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

async fn pair(config: &RemoteConfig, open_link: bool) -> anyhow::Result<Credential> {
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
    let expires = start["expires_at"].as_i64().context(language.pick(
        "Missing pairing expiry",
        "Отсутствует срок действия сопряжения",
    ))?;
    let mut profile_url = config.server.join("app/profile")?;
    if open_link {
        profile_url
            .query_pairs_mut()
            .append_pair("pair_request", &request.to_string());
        profile_url.set_fragment(Some("device-connections"));
    }
    eprintln!(
        "{}\n{}: {request}\n{}: {code}",
        language.pick(
            &format!("Open this page and approve device pairing:\n{profile_url}"),
            &format!("Откройте эту страницу и подтвердите подключение устройства:\n{profile_url}")
        ),
        language.pick("Request ID", "ID запроса"),
        if open_link {
            language.pick("Enter this code", "Введите этот код")
        } else {
            language.pick(
                "Compare this code on both devices",
                "Сравните этот код на обоих устройствах",
            )
        }
    );
    std::io::stderr().flush()?;
    if open_link && std::io::stderr().is_terminal() {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            #[cfg(target_os = "macos")]
            let opener = "open";
            #[cfg(target_os = "linux")]
            let opener = "xdg-open";
            // The printed URL is the fallback; the opener must not block polling.
            tokio::spawn(async move {
                let _ = tokio::process::Command::new(opener)
                    .arg(profile_url.as_str())
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await;
            });
        }
    }
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
                return Ok(Credential {
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
                });
            }
            _ => anyhow::bail!(language.pick(
                "Pairing expired or invalid; start a new pairing request",
                "Сопряжение недействительно или его срок истёк; создайте новый запрос"
            )),
        }
    }
}
