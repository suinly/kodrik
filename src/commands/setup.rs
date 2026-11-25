use std::{
    fs,
    io::{self, ErrorKind, IsTerminal, Write},
    path::Path,
};

use anyhow::{Context, ensure};

use crate::{config::Config, i18n::Language};

pub(crate) fn interactive() -> bool {
    io::stdin().is_terminal() && io::stderr().is_terminal()
}

pub(crate) fn missing(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

pub(crate) fn prompt(label: &str, required: bool) -> anyhow::Result<String> {
    let language = Language::from_environment();
    loop {
        eprint!("{label}");
        io::stderr().flush()?;
        let mut value = String::new();
        ensure!(
            io::stdin().read_line(&mut value)? != 0,
            language.pick("Setup cancelled", "Настройка отменена")
        );
        let value = value.trim();
        if !required || !value.is_empty() {
            return Ok(value.to_owned());
        }
    }
}

pub(crate) fn save_new_config(path: &Path, contents: &str) -> anyhow::Result<()> {
    let parent = path.parent().context("Configuration directory missing")?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    if !parent.try_exists()? {
        crate::platform::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    crate::platform::set_file_private(file.as_file(), 0o600)?;
    file.write_all(contents.as_bytes())?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path)?;
    crate::platform::sync_directory(parent)?;
    Ok(())
}

/// Load local configuration, offering first-run setup only when the file is absent.
pub fn load_local(gateway: bool) -> anyhow::Result<Config> {
    let path = crate::config::data_dir()?.join("config.toml");
    if !missing(&path)? {
        return Config::load();
    }
    let language = Language::from_environment();
    ensure!(interactive(), "{} {}. {}", language.pick("Configuration not found:", "Не найден файл конфигурации:"), path.display(), language.pick("Run Kodrik in an interactive terminal for setup or create config.toml manually", "Запустите Kodrik в интерактивном терминале для настройки или создайте config.toml вручную"));
    eprintln!(
        "{} {}",
        language.pick("First-run setup:", "Первоначальная настройка:"),
        path.display()
    );
    let model = prompt(language.pick("Model ID: ", "ID модели: "), true)?;
    let api_base = loop {
        let input = prompt(
            language.pick(
                "OpenAI-compatible API base URL (Enter for OpenAI): ",
                "URL OpenAI-совместимого API (Enter для OpenAI): ",
            ),
            false,
        )?;
        if input.is_empty() {
            break None;
        }
        if let Ok(url) = reqwest::Url::parse(&input)
            && matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
        {
            break Some(input);
        }
        eprintln!(
            "{}",
            language.pick(
                "Enter an HTTP(S) base URL without credentials, query or fragment.",
                "Введите базовый HTTP(S)-адрес без учётных данных, запроса или фрагмента."
            )
        );
    };
    let api_key = loop {
        let key =
            rpassword::prompt_password(language.pick("API key (hidden): ", "API-ключ (скрыт): "))?;
        if !key.trim().is_empty() {
            break key.trim().to_owned();
        }
    };

    // TOML string encoding keeps model names, URLs and keys literal.
    let value = |text: String| toml::Value::String(text).to_string();
    let mut contents = format!(
        "[gateway]\n\n[provider.openai]\ntype = \"openai\"\napi_key = {}\n",
        value(api_key)
    );
    if let Some(api_base) = api_base {
        contents.push_str(&format!("api_base = {}\n", value(api_base)));
    }
    contents.push_str(&format!(
        "\n[model.smart]\nprovider = \"openai\"\nmodel = {}\n",
        value(model)
    ));
    if gateway {
        contents.push_str("\n[web]\nenabled = true\nlisten = \"127.0.0.1:8080\"\nbase_url = \"http://127.0.0.1:8080/\"\n");
    }
    let config: Config = toml::from_str(&contents)?;
    config.model_profiles()?;
    save_new_config(&path, &contents).with_context(|| format!("Cannot save {}", path.display()))?;
    eprintln!(
        "{} {}",
        language.pick("Configuration saved:", "Конфигурация сохранена:"),
        path.display()
    );
    if gateway {
        eprintln!("{}", language.pick("Web chat: http://127.0.0.1:8080/. Set the owner password with `kodrik web set-password` using the same data directory.", "Веб-чат: http://127.0.0.1:8080/. Задайте пароль владельца командой `kodrik web set-password` с тем же каталогом данных."));
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_config_is_private_and_cannot_replace_a_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("fresh/config.toml");
        save_new_config(&path, "original").unwrap();
        assert!(crate::platform::path_is_private(&path).unwrap());
        assert!(save_new_config(&path, "other").is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "original");
    }
}
