use crate::config::WebConfig;
use anyhow::{Context, ensure};
pub(crate) mod auth;
pub(crate) mod chat;
pub(crate) mod chat_assets;
mod chat_stream;
#[cfg(test)]
mod chat_tests;
mod chat_upload;
pub(crate) mod documents;
pub(crate) mod pages;
pub(crate) mod server;
pub(crate) mod views;

#[derive(Clone)]
pub(crate) struct Settings {
    pub trusted_proxies: Vec<std::net::IpAddr>,
    pub listen: std::net::SocketAddr,
    pub base_url: reqwest::Url,
}

impl Settings {
    pub(crate) fn from_config(config: &WebConfig) -> anyhow::Result<Option<Self>> {
        if !config.enabled {
            return Ok(None);
        }
        let raw = config
            .base_url
            .as_deref()
            .context("web.base_url is required")?;
        ensure!(!raw.contains('\\'), "Invalid web.base_url");
        let mut base_url = reqwest::Url::parse(raw).context("Invalid web.base_url")?;
        ensure!(
            base_url.username().is_empty()
                && base_url.password().is_none()
                && base_url.query().is_none()
                && base_url.fragment().is_none()
                && base_url.host_str().is_some(),
            "Invalid web.base_url"
        );
        let host = base_url.host_str().unwrap_or("");
        let loopback = host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        ensure!(
            base_url.scheme() == "https" || (base_url.scheme() == "http" && loopback),
            "web.base_url must use HTTPS (HTTP is allowed on loopback)"
        );
        ensure!(
            !base_url.path().contains('%') && !base_url.path().contains("//"),
            "Encoded or double-slash web prefix is unsupported"
        );
        base_url.set_path(&format!("{}/", base_url.path().trim_end_matches('/')));
        Ok(Some(Self {
            trusted_proxies: config.trusted_proxies.clone(),
            listen: config.listen.parse().context("Invalid web.listen")?,
            base_url,
        }))
    }

    pub(crate) fn prefix(&self) -> &str {
        self.base_url.path()
    }
    pub(crate) fn origin(&self) -> String {
        self.base_url.origin().ascii_serialization()
    }
    pub(crate) fn secure(&self) -> bool {
        self.base_url.scheme() == "https"
    }
    pub(crate) fn url(&self, relative: &str) -> anyhow::Result<String> {
        Ok(self.base_url.join(relative)?.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WebConfig;

    #[test]
    fn web_settings_validate_and_keep_proxy_prefix() {
        assert!(
            Settings::from_config(&WebConfig::default())
                .unwrap()
                .is_none()
        );
        let config = |url: &str| WebConfig {
            trusted_proxies: Vec::new(),
            enabled: true,
            listen: "0.0.0.0:8080".into(),
            base_url: Some(url.into()),
        };
        let settings = Settings::from_config(&config("https://pages.example.com/kodrik"))
            .unwrap()
            .unwrap();
        assert_eq!(settings.prefix(), "/kodrik/");
        assert_eq!(
            settings.url("pages/nutrition/").unwrap(),
            "https://pages.example.com/kodrik/pages/nutrition/"
        );
        assert_eq!(settings.origin(), "https://pages.example.com");
        assert!(settings.secure());
        for url in [
            "http://example.com",
            "https://user:pass@example.com",
            "https://example.com/?x=1",
            "https://example.com/#x",
            "https://example.com/%2f",
            "https://example.com//evil.example/",
            "https://example.com/a//b/",
            "file:///tmp",
        ] {
            assert!(Settings::from_config(&config(url)).is_err(), "{url}");
        }
        assert!(Settings::from_config(&config("http://127.0.0.1:8080/")).is_ok());
        assert!(
            Settings::from_config(&WebConfig {
                trusted_proxies: Vec::new(),
                enabled: true,
                ..WebConfig::default()
            })
            .is_err()
        );
        let old: crate::config::Config = toml::from_str(
            "[gateway]\n[provider.primary]\ntype='openai'\nmodel='test'\napi_key='test'\n",
        )
        .unwrap();
        assert!(!old.web.enabled);
    }
}
