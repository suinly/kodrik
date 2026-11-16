use crate::i18n::Language;
pub async fn evaluate_views(
    input: &std::path::Path,
    output: &std::path::Path,
) -> anyhow::Result<()> {
    crate::web::views::worker(input, output).await
}

fn password_validation(password: &str, language: Language) -> anyhow::Result<()> {
    crate::web::auth::validate_password(password).map_err(|error| {
        let message = match error {
            "Пароль должен содержать не менее 15 символов." => language.pick("Password must contain at least 15 characters.", error),
            "Этот пароль слишком длинный." => language.pick("This password is too long.", error),
            "Выберите менее предсказуемый пароль: без распространённых значений и повторяющихся коротких шаблонов." => language.pick("Choose a less predictable password: avoid common values and repeating short patterns.", error),
            _ => error,
        };
        anyhow::anyhow!(message)
    })
}

pub fn set_password() -> anyhow::Result<()> {
    let language = Language::from_environment();
    println!(
        "{}", language.pick("Use at least 15 characters. Common passwords and repeating short patterns are not accepted.", "Используйте не менее 15 символов. Распространённые пароли и повторяющиеся короткие шаблоны не допускаются.")
    );
    let password =
        rpassword::prompt_password(language.pick("New web password: ", "Новый веб-пароль: "))?;
    let confirmation =
        rpassword::prompt_password(language.pick("Confirm password: ", "Подтвердите пароль: "))?;
    anyhow::ensure!(
        password == confirmation,
        language.pick("Passwords do not match", "Пароли не совпадают")
    );
    password_validation(&password, language)?;
    crate::web::auth::AuthStore::new(&crate::config::data_dir()?).set_password(&password)?;
    println!(
        "{}",
        language.pick(
            "Web password set. Existing web sessions have been revoked.",
            "Веб-пароль установлен. Существующие веб-сессии отозваны."
        )
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_validation_uses_both_cli_languages() {
        for (language, expected) in [(Language::En, "at least 15"), (Language::Ru, "не менее 15")]
        {
            assert!(
                password_validation("short", language)
                    .unwrap_err()
                    .to_string()
                    .contains(expected)
            );
        }
        assert!(password_validation("a sufficiently unique passphrase", Language::En).is_ok());
    }
}
