#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    En,
    Ru,
}

impl Language {
    pub fn from_locale(locale: &str) -> Self {
        if locale
            .split(['-', '_', '.', '@'])
            .next()
            .is_some_and(|code| code.eq_ignore_ascii_case("ru"))
        {
            Self::Ru
        } else {
            Self::En
        }
    }

    pub fn from_environment() -> Self {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()))
            .map_or(Self::En, |value| Self::from_locale(&value))
    }

    pub fn code(self) -> &'static str {
        self.pick("en", "ru")
    }

    pub fn locale(self) -> &'static str {
        self.pick("en-US", "ru-RU")
    }

    pub fn pick<'a>(self, en: &'a str, ru: &'a str) -> &'a str {
        match self {
            Self::En => en,
            Self::Ru => ru,
        }
    }
}

impl std::str::FromStr for Language {
    type Err = &'static str;

    fn from_str(code: &str) -> Result<Self, Self::Err> {
        match code {
            "en" => Ok(Self::En),
            "ru" => Ok(Self::Ru),
            _ => Err("Supported languages: en, ru"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Language;

    #[test]
    fn detects_supported_locale_prefixes_and_falls_back_to_english() {
        for (locale, expected) in [
            ("ru", Language::Ru),
            ("ru-RU", Language::Ru),
            ("RU_ru.UTF-8", Language::Ru),
            ("en_US.UTF-8", Language::En),
            ("de-DE", Language::En),
            ("", Language::En),
        ] {
            assert_eq!(Language::from_locale(locale), expected);
        }
    }

    #[test]
    fn manual_language_selection_is_strict_and_serializes_as_a_code() {
        assert_eq!("en".parse::<Language>().unwrap(), Language::En);
        assert_eq!("ru".parse::<Language>().unwrap(), Language::Ru);
        assert!("de".parse::<Language>().is_err());
        assert_eq!(serde_json::to_string(&Language::Ru).unwrap(), "\"ru\"");
        assert_eq!(
            serde_json::from_str::<Language>("\"en\"").unwrap(),
            Language::En
        );
        assert_eq!(Language::Ru.pick("English", "Русский"), "Русский");
        assert_eq!(Language::En.code(), "en");
        assert_eq!(Language::Ru.locale(), "ru-RU");
    }
}
