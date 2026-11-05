use crate::config::McpServerConfig;
use anyhow::{Context, ensure};
use std::{
    collections::HashMap,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

impl RemoteConfig {
    pub(crate) fn manifest_authorized(
        &self,
        manifest: &super::protocol::EnvironmentManifest,
    ) -> anyhow::Result<bool> {
        self.grant_matches(&self.manifest_fingerprint(manifest)?)
    }

    pub(crate) fn authorize_manifest(
        &self,
        manifest: &super::protocol::EnvironmentManifest,
        confirm: impl FnOnce() -> anyhow::Result<bool>,
    ) -> anyhow::Result<()> {
        use std::io::Write;
        let fingerprint = self.manifest_fingerprint(manifest)?;
        if self.grant_matches(&fingerprint)? {
            return Ok(());
        }
        ensure!(confirm()?, "Local environment access denied");
        let path = self.credential_file.with_extension("grant");
        let parent = path.parent().context("Grant directory missing")?;
        super::client::private_directory(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(fingerprint.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(&path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    }

    fn manifest_fingerprint(
        &self,
        manifest: &super::protocol::EnvironmentManifest,
    ) -> anyhow::Result<String> {
        use sha2::{Digest, Sha256};
        super::protocol::validate_manifest(manifest)?;
        let mcp: std::collections::BTreeMap<_, _> = self
            .mcp
            .iter()
            .map(|(name, config)| {
                let McpServerConfig::Http { url, headers } = config;
                (
                    name,
                    (
                        url,
                        headers.iter().collect::<std::collections::BTreeMap<_, _>>(),
                    ),
                )
            })
            .collect();
        // Only this hash is persisted; HTTP headers and tool schemas never enter the grant file.
        Ok(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(
                self.server.as_str(),
                manifest,
                self.allow_shell,
                &self.skill_roots,
                mcp
            ))?)
        ))
    }

    fn grant_matches(&self, fingerprint: &str) -> anyhow::Result<bool> {
        use std::io::Read;
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(self.credential_file.with_extension("grant"))
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.len() <= 64
                && metadata.permissions().mode() & 0o077 == 0,
            "Grant must be a private bounded regular file"
        );
        let mut saved = String::new();
        file.take(65).read_to_string(&mut saved)?;
        Ok(saved == fingerprint)
    }

    pub(crate) fn load(path: &Path) -> anyhow::Result<Self> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Settings {
            server: String,
            credential_file: Option<PathBuf>,
            label: Option<String>,
            #[serde(default)]
            allow_shell: bool,
            #[serde(default)]
            skill_roots: Vec<PathBuf>,
            #[serde(default)]
            mcp: HashMap<String, McpServerConfig>,
            #[serde(default)]
            optional_mcp: Vec<String>,
        }
        use std::io::Read;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .context("Cannot read remote configuration")?;
        ensure!(
            file.metadata()?.is_file(),
            "Remote configuration must be a regular file"
        );
        let mut bytes = Vec::new();
        file.take(256 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 256 * 1024,
            "Remote configuration exceeds limit"
        );
        // TOML diagnostics can include MCP secrets from the offending source line.
        let settings: Settings = toml::from_str(std::str::from_utf8(&bytes)?)
            .map_err(|_| anyhow::anyhow!("Invalid remote configuration"))?;
        let server = settings
            .server
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid server URL"))?;
        super::client::endpoint(&server)?;
        let label = settings.label.unwrap_or_else(|| "Laptop".into());
        ensure!(
            !label.trim().is_empty()
                && label.trim() == label
                && label.chars().count() <= 128
                && !label.chars().any(char::is_control),
            "Invalid device label"
        );
        ensure!(
            settings.mcp.len() <= 64 && settings.skill_roots.len() <= 64,
            "Too many local providers"
        );
        ensure!(
            settings
                .optional_mcp
                .iter()
                .all(|name| settings.mcp.contains_key(name)),
            "Optional MCP must be selected"
        );
        let parent = path.parent().unwrap_or(Path::new("."));
        let resolve = |path: PathBuf| {
            if path.is_absolute() {
                path
            } else {
                parent.join(path)
            }
        };
        Ok(Self {
            server,
            credential_file: resolve(
                settings
                    .credential_file
                    .unwrap_or_else(|| "remote/device.json".into()),
            ),
            label,
            allow_shell: settings.allow_shell,
            skill_roots: settings.skill_roots.into_iter().map(resolve).collect(),
            mcp: settings.mcp,
            optional_mcp: settings.optional_mcp,
        })
    }
}

#[derive(Clone)]
pub(crate) struct RemoteConfig {
    pub server: reqwest::Url,
    pub credential_file: PathBuf,
    pub label: String,
    pub allow_shell: bool,
    pub skill_roots: Vec<PathBuf>,
    pub mcp: HashMap<String, McpServerConfig>,
    pub optional_mcp: Vec<String>,
}
