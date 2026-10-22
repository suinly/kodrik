use std::{collections::HashMap, fs, path::PathBuf};

use anyhow::Context;
use serde::Deserialize;

pub(crate) fn data_dir() -> anyhow::Result<PathBuf> {
    Ok(std::env::home_dir()
        .context("Cannot determine the home directory for ~/.kodrik")?
        .join(".kodrik"))
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub owner: Option<OwnerConfig>,
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub web: WebConfig,
    #[serde(default)]
    pub companion: CompanionConfig,
    #[serde(default)]
    pub sessions: SessionsConfig,
    #[serde(default)]
    pub research: ResearchConfig,
    pub gateway: HashMap<String, GatewayConfig>,
    pub provider: HashMap<String, ProviderConfig>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct OwnerConfig {
    pub interface: String,
    pub user_id: std::num::NonZeroU64,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct ResearchConfig {
    pub searxng_url: Option<String>,
    pub obscura_path: PathBuf,
}

impl Default for ResearchConfig {
    fn default() -> Self {
        Self {
            searxng_url: None,
            obscura_path: "obscura".into(),
        }
    }
}

impl ResearchConfig {
    pub(crate) fn search_endpoint(&self) -> anyhow::Result<Option<reqwest::Url>> {
        anyhow::ensure!(
            !self.obscura_path.as_os_str().is_empty(),
            "research.obscura_path must not be empty"
        );
        let Some(raw) = &self.searxng_url else {
            return Ok(None);
        };
        let mut url = reqwest::Url::parse(raw).context("Invalid research.searxng_url")?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "research.searxng_url must be an HTTP(S) base URL without credentials, query or fragment"
        );
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("Invalid SearXNG base path"))?
            .pop_if_empty()
            .push("search");
        Ok(Some(url))
    }
}

impl Config {
    pub(crate) fn validate_owner(&self) -> anyhow::Result<()> {
        if let Some(owner) = &self.owner {
            i64::try_from(owner.user_id.get())?;
            anyhow::ensure!(
                matches!(
                    self.gateway.get(&owner.interface),
                    Some(GatewayConfig::Telegram { .. })
                ),
                "owner must name a configured Telegram interface"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    pub trusted_proxies: Vec<std::net::IpAddr>,
    pub enabled: bool,
    pub listen: String,
    pub base_url: Option<String>,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            trusted_proxies: Vec::new(),
            enabled: false,
            listen: "0.0.0.0:8080".into(),
            base_url: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionsConfig {
    pub restart_limit: u32,
    pub auto_switch: bool,
    pub idle_minutes: u32,
    pub compact_threshold_tokens: u64,
    pub compact_background_margin_tokens: u64,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            auto_switch: true,
            idle_minutes: 30,
            compact_threshold_tokens: 80_000,
            compact_background_margin_tokens: 16_000,
            restart_limit: 5,
        }
    }
}

impl<'de> Deserialize<'de> for SessionsConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(default, deny_unknown_fields)]
        struct Fields {
            restart_limit: u32,
            auto_switch: bool,
            idle_minutes: u32,
            #[serde(deserialize_with = "deserialize_compact_threshold")]
            compact_threshold_tokens: u64,
            compact_background_margin_tokens: u64,
        }
        impl Default for Fields {
            fn default() -> Self {
                let v = SessionsConfig::default();
                Self {
                    restart_limit: v.restart_limit,
                    auto_switch: v.auto_switch,
                    idle_minutes: v.idle_minutes,
                    compact_threshold_tokens: v.compact_threshold_tokens,
                    compact_background_margin_tokens: v.compact_background_margin_tokens,
                }
            }
        }
        let v = Fields::deserialize(deserializer)?;
        if v.compact_threshold_tokens != 0
            && v.compact_background_margin_tokens >= v.compact_threshold_tokens
        {
            return Err(serde::de::Error::custom(
                "compact_background_margin_tokens must be smaller than compact_threshold_tokens (or 0 to disable background)",
            ));
        }
        Ok(Self {
            restart_limit: v.restart_limit,
            auto_switch: v.auto_switch,
            idle_minutes: v.idle_minutes,
            compact_threshold_tokens: v.compact_threshold_tokens,
            compact_background_margin_tokens: v.compact_background_margin_tokens,
        })
    }
}

fn deserialize_compact_threshold<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<u64, D::Error> {
    let value = u64::deserialize(deserializer)?;
    if value != 0 && value < 4096 {
        return Err(serde::de::Error::custom(
            "compact_threshold_tokens must be 0 (disabled) or at least 4096",
        ));
    }
    Ok(value)
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct CompanionConfig {
    pub learning_enabled: bool,
    pub proactive_enabled: bool,
    pub interface: Option<String>,
    pub chat_id: Option<i64>,
    pub timezone: String,
    pub proactive_hour: u32,
    pub quiet_start: u32,
    pub quiet_end: u32,
}

impl Default for CompanionConfig {
    fn default() -> Self {
        Self {
            learning_enabled: true,
            proactive_enabled: true,
            interface: None,
            chat_id: None,
            timezone: "Europe/Moscow".into(),
            proactive_hour: 12,
            quiet_start: 22,
            quiet_end: 9,
        }
    }
}

impl CompanionConfig {
    pub(crate) fn quiet_at(&self, now: chrono::DateTime<chrono::Utc>) -> anyhow::Result<bool> {
        use chrono::Timelike;
        let hour = now
            .with_timezone(&self.timezone.parse::<chrono_tz::Tz>()?)
            .hour();
        Ok(if self.quiet_start > self.quiet_end {
            hour >= self.quiet_start || hour < self.quiet_end
        } else {
            hour >= self.quiet_start && hour < self.quiet_end
        })
    }

    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        self.timezone
            .parse::<chrono_tz::Tz>()
            .context("Invalid companion timezone")?;
        anyhow::ensure!(
            self.proactive_hour < 24 && self.quiet_start < 24 && self.quiet_end < 24,
            "Companion hours must be between 0 and 23"
        );
        anyhow::ensure!(
            self.quiet_start != self.quiet_end,
            "Companion quiet_start and quiet_end must differ"
        );
        anyhow::ensure!(
            self.interface.is_some() == self.chat_id.is_some(),
            "Set both companion.interface and companion.chat_id"
        );
        Ok(())
    }

    pub(crate) fn recipient(
        &self,
        gateways: &HashMap<String, GatewayConfig>,
        owner: Option<&OwnerConfig>,
    ) -> anyhow::Result<Option<(String, String)>> {
        if !self.proactive_enabled {
            return Ok(None);
        }
        if let (Some(interface), Some(chat)) = (&self.interface, self.chat_id) {
            let Some(GatewayConfig::Telegram { .. }) = gateways.get(interface) else {
                anyhow::bail!("Unknown proactive Telegram interface");
            };
            anyhow::ensure!(
                chat > 0,
                "Proactive recipient must be a private Telegram chat"
            );
            return Ok(Some((format!("telegram:{interface}"), chat.to_string())));
        }
        if let Some(owner) = owner {
            anyhow::ensure!(
                gateways.contains_key(&owner.interface),
                "Unknown owner Telegram interface"
            );
            let chat = i64::try_from(owner.user_id.get())?;
            return Ok(Some((
                format!("telegram:{}", owner.interface),
                chat.to_string(),
            )));
        }
        Ok(None)
    }
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let path = data_dir()?.join("config.toml");
        let content = fs::read_to_string(&path)
            .with_context(|| format!("Cannot read config at {}", path.display()))?;
        let config = toml::from_str(&content)?;

        Ok(config)
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum GatewayConfig {
    Telegram {
        token: String,
        #[serde(default = "default_true")]
        stream: bool,
    },
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ProviderConfig {
    OpenAi {
        api_key: String,
        api_base: Option<String>,
        model: String,
    },
}

#[cfg(test)]
mod tests {
    #[test]
    fn research_defaults_and_prefixed_endpoint() {
        let config: super::Config = toml::from_str(
            "[gateway]\n[provider.primary]\ntype = 'openai'\nmodel = 'test'\napi_key = 'test'",
        )
        .unwrap();
        assert!(config.research.search_endpoint().unwrap().is_none());
        assert_eq!(
            config.research.obscura_path,
            std::path::PathBuf::from("obscura")
        );
        let research: super::ResearchConfig = toml::from_str(
            "searxng_url = 'http://127.0.0.1:8080/prefix'\nobscura_path = '/opt/obscura'",
        )
        .unwrap();
        assert_eq!(
            research.search_endpoint().unwrap().unwrap().as_str(),
            "http://127.0.0.1:8080/prefix/search"
        );
        for raw in [
            "",
            "file:///tmp/search",
            "https://u:p@host/",
            "https://host/?q=x",
            "https://host/#fragment",
        ] {
            let settings = super::ResearchConfig {
                searxng_url: Some(raw.into()),
                ..Default::default()
            };
            assert!(settings.search_endpoint().is_err(), "{raw}");
        }
        let settings = super::ResearchConfig {
            searxng_url: Some("https://host/".into()),
            obscura_path: "".into(),
        };
        assert!(settings.search_endpoint().is_err());
    }

    #[test]
    fn owner_must_be_an_explicit_positive_telegram_user() {
        for id in ["-42", "0"] {
            let config =
                format!("[gateway]\n[provider]\n[owner]\ninterface = 'main'\nuser_id = {id}\n");
            assert!(toml::from_str::<super::Config>(&config).is_err());
        }
    }

    #[test]
    fn telegram_stream_can_be_disabled() {
        let default: GatewayConfig = toml::from_str("type = 'telegram'\ntoken = 'test'").unwrap();
        let disabled: GatewayConfig =
            toml::from_str("type = 'telegram'\ntoken = 'test'\nstream = false").unwrap();
        assert!(matches!(
            default,
            GatewayConfig::Telegram { stream: true, .. }
        ));
        assert!(matches!(
            disabled,
            GatewayConfig::Telegram { stream: false, .. }
        ));
    }
    use super::*;

    #[test]
    fn proactive_recipient_accepts_any_explicit_private_chat() {
        let gateways = HashMap::from([(
            "main".into(),
            GatewayConfig::Telegram {
                token: "test".into(),
                stream: true,
            },
        )]);
        let config = CompanionConfig {
            interface: Some("main".into()),
            chat_id: Some(7),
            ..Default::default()
        };
        assert_eq!(
            config.recipient(&gateways, None).unwrap(),
            Some(("telegram:main".into(), "7".into()))
        );
    }

    #[test]
    fn compaction_background_margin_validates_with_threshold() {
        assert_eq!(
            super::SessionsConfig::default().compact_background_margin_tokens,
            16000
        );
        for config in [
            "compact_threshold_tokens=0",
            "compact_background_margin_tokens=0",
            "compact_threshold_tokens=8000\ncompact_background_margin_tokens=2000",
        ] {
            assert!(toml::from_str::<super::SessionsConfig>(config).is_ok());
        }
        for config in [
            "compact_threshold_tokens=16000",
            "compact_threshold_tokens=8000",
            "compact_background_margin_tokens=80000",
        ] {
            assert!(toml::from_str::<super::SessionsConfig>(config).is_err());
        }
    }

    #[test]
    fn compaction_threshold_accepts_override_and_zero_but_rejects_tiny_budgets() {
        assert!(
            toml::from_str::<SessionsConfig>(
                "compact_threshold_tokens = 12000\ncompact_background_margin_tokens = 0"
            )
            .is_ok()
        );
        assert!(toml::from_str::<SessionsConfig>("compact_threshold_tokens = 0").is_ok());
        for value in ["-1", "1", "4095"] {
            assert!(
                toml::from_str::<SessionsConfig>(&format!("compact_threshold_tokens = {value}"))
                    .is_err()
            );
        }
    }

    #[test]
    fn session_switching_defaults_on_and_can_be_disabled() {
        assert!(toml::from_str::<SessionsConfig>("").unwrap().auto_switch);
        assert!(
            !toml::from_str::<SessionsConfig>("auto_switch = false")
                .unwrap()
                .auto_switch
        );
        let config: Config = toml::from_str(
            r#"
            [gateway]
            [provider.primary]
            type = "openai"
            model = "main"
            api_key = "test"
            [provider.system_one]
            type = "openai"
            model = "fast"
            api_key = "test"
        "#,
        )
        .unwrap();
        assert!(config.sessions.auto_switch);
        assert_eq!(config.sessions.idle_minutes, 30);
        assert_eq!(
            toml::from_str::<SessionsConfig>("idle_minutes = 15")
                .unwrap()
                .idle_minutes,
            15
        );
        assert!(toml::from_str::<SessionsConfig>("idle_minutes = -1").is_err());
        assert!(
            matches!(&config.provider["system_one"], ProviderConfig::OpenAi { model, .. } if model == "fast")
        );
    }

    #[test]
    fn companion_defaults_validate_and_recipient_is_never_ambiguous_or_public() {
        let config: CompanionConfig = toml::from_str("").unwrap();
        config.validate().unwrap();
        assert_eq!(config.timezone, "Europe/Moscow");
        let mut gateways = HashMap::new();
        let telegram = || GatewayConfig::Telegram {
            token: "test".into(),
            stream: true,
        };
        gateways.insert("telegram".into(), telegram());
        let owner = OwnerConfig {
            interface: "telegram".into(),
            user_id: std::num::NonZeroU64::new(123).unwrap(),
        };
        use crate::Interface;
        let actual_id = crate::interfaces::TelegramInterface::new(
            "telegram",
            "test",
            std::path::PathBuf::new(),
        )
        .id();
        assert!(config.recipient(&gateways, None).unwrap().is_none());
        assert_eq!(
            config
                .recipient(&gateways, Some(&owner))
                .unwrap()
                .unwrap()
                .0,
            actual_id.0
        );
        assert_eq!(
            config.recipient(&gateways, Some(&owner)).unwrap(),
            Some(("telegram:telegram".into(), "123".into()))
        );
        gateways.insert("second".into(), telegram());
        assert!(config.recipient(&gateways, None).unwrap().is_none());
        let explicit = CompanionConfig {
            interface: Some("telegram".into()),
            chat_id: Some(123),
            ..config.clone()
        };
        assert!(
            explicit
                .recipient(&gateways, Some(&owner))
                .unwrap()
                .is_some()
        );
        for chat_id in [-999, 0] {
            assert!(
                CompanionConfig {
                    chat_id: Some(chat_id),
                    ..explicit.clone()
                }
                .recipient(&gateways, Some(&owner))
                .is_err()
            );
        }
        assert!(
            CompanionConfig {
                quiet_start: 24,
                ..config.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            CompanionConfig {
                timezone: "Bad/Zone".into(),
                ..config.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            CompanionConfig {
                interface: Some("telegram".into()),
                ..config.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            CompanionConfig {
                proactive_enabled: false,
                ..explicit
            }
            .recipient(&gateways, Some(&owner))
            .unwrap()
            .is_none()
        );
    }
}

#[cfg(test)]
#[test]
fn config_restart_limit_defaults_and_rejects_negative() {
    assert_eq!(
        toml::from_str::<SessionsConfig>("").unwrap().restart_limit,
        5
    );
    assert_eq!(
        toml::from_str::<SessionsConfig>("restart_limit=0")
            .unwrap()
            .restart_limit,
        0
    );
    assert!(toml::from_str::<SessionsConfig>("restart_limit=-1").is_err());
}
