use crate::config::McpServerConfig;
use anyhow::{Context, ensure};
use std::{
    collections::HashMap,
    io::ErrorKind,
    path::{Path, PathBuf},
};

impl RemoteConfig {
    fn valid_label(label: &str) -> bool {
        !label.trim().is_empty()
            && label.trim() == label
            && label.chars().count() <= 128
            && !label.chars().any(char::is_control)
    }

    pub(crate) fn pin_credential(&self) -> anyhow::Result<Self> {
        let mut config = self.clone();
        config.credential = Some(self.load_credential()?);
        Ok(config)
    }

    pub(crate) fn load_credential(&self) -> anyhow::Result<super::client::Credential> {
        self.credential.clone().map_or_else(
            || super::client::Credential::load(&self.credential_file),
            Ok,
        )
    }

    fn grant_device(&self) -> anyhow::Result<Option<uuid::Uuid>> {
        if let Some(credential) = &self.credential {
            return Ok(Some(credential.device_id));
        }
        match std::fs::symlink_metadata(&self.credential_file) {
            Ok(_) => Ok(Some(
                super::client::Credential::load(&self.credential_file)?.device_id,
            )),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn directory_grant_store(&self) -> anyhow::Result<PathBuf> {
        let credential = std::path::absolute(&self.credential_file)?;
        let mut name = credential
            .file_name()
            .context("Credential filename missing")?
            .to_os_string();
        name.push(".directory-grants");
        Ok(credential.with_file_name(name))
    }

    /// Checks a grant for the exact canonical paths presented for approval.
    pub(crate) fn directory_authorized(
        &self,
        root: &Path,
        directory: &Path,
    ) -> anyhow::Result<bool> {
        let (store, fingerprint) = self.directory_grant(root, directory)?;
        for path in [
            store.parent().context("Grant directory missing")?,
            store.as_path(),
        ] {
            let metadata = match std::fs::symlink_metadata(path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error.into()),
            };
            ensure!(
                metadata.is_dir() && crate::platform::path_is_private(path)?,
                "Grant directory must be private and not a symlink"
            );
        }
        Ok(Self::read_grant(&store.join(&fingerprint))?.as_deref() == Some(&fingerprint))
    }

    /// Persists a grant for the exact canonical paths presented for approval.
    pub(crate) fn authorize_directory(&self, root: &Path, directory: &Path) -> anyhow::Result<()> {
        use std::io::Write;
        let (store, fingerprint) = self.directory_grant(root, directory)?;
        let parent = store.parent().context("Grant directory missing")?;
        // A newly created credential hierarchy also needs its parent entries synced.
        let mut ancestor = parent.parent();
        let mut sync_ancestors = Vec::new();
        while let Some(path) = ancestor {
            sync_ancestors.push(path);
            if path.try_exists()? {
                break;
            }
            ancestor = path.parent();
        }
        super::client::private_directory(parent)?;
        super::client::private_directory(&store)?;

        let path = store.join(&fingerprint);
        if Self::read_grant(&path)?.as_deref() != Some(&fingerprint) {
            let mut file = tempfile::NamedTempFile::new_in(&store)?;
            crate::platform::set_file_private(file.as_file(), 0o600)?;
            file.write_all(fingerprint.as_bytes())?;
            file.as_file().sync_all()?;
            // Separate markers avoid lost updates, and concurrent identical approvals are idempotent.
            if let Err(error) = file.persist_noclobber(&path) {
                if error.error.kind() != ErrorKind::AlreadyExists {
                    return Err(error.error.into());
                }
                ensure!(
                    Self::read_grant(&path)?.as_deref() == Some(&fingerprint),
                    "Directory grant marker does not match"
                );
            }
        }
        for directory in [store.as_path(), parent].into_iter().chain(sync_ancestors) {
            crate::platform::sync_directory(directory)?;
        }
        Ok(())
    }

    fn directory_grant(&self, root: &Path, directory: &Path) -> anyhow::Result<(PathBuf, String)> {
        use sha2::{Digest, Sha256};
        // Revalidate rather than substitute: a symlink may have changed while approval was pending.
        ensure!(
            root.canonicalize()
                .context("Cannot resolve project root")?
                .as_os_str()
                == root.as_os_str(),
            "Project root is no longer canonical"
        );
        ensure!(
            directory
                .canonicalize()
                .context("Cannot resolve requested directory")?
                .as_os_str()
                == directory.as_os_str(),
            "Requested directory is no longer canonical"
        );
        ensure!(
            root.metadata()?.is_dir() && directory.metadata()?.is_dir(),
            "Directory grants require directories"
        );
        let store = self.directory_grant_store()?;
        let mut hash = Sha256::new();
        // Directory consent must not survive re-pairing this path to a different account.
        if let Some(device) = self.grant_device()? {
            hash.update(b"device\0");
            hash.update(device.as_bytes());
        }
        for part in [
            self.server.as_str().as_bytes(),
            root.as_os_str().as_encoded_bytes(),
            directory.as_os_str().as_encoded_bytes(),
        ] {
            hash.update((part.len() as u64).to_be_bytes());
            hash.update(part);
        }
        Ok((store, format!("{:x}", hash.finalize())))
    }

    pub(crate) fn manifest_authorized(
        &self,
        manifest: &super::protocol::EnvironmentManifest,
    ) -> anyhow::Result<bool> {
        self.grant_matches(
            &self.grant_path(manifest)?,
            &self.manifest_fingerprint(manifest)?,
        )
    }

    pub(crate) fn authorize_manifest(
        &self,
        manifest: &super::protocol::EnvironmentManifest,
        confirm: impl FnOnce() -> anyhow::Result<bool>,
    ) -> anyhow::Result<()> {
        use std::io::Write;
        let fingerprint = self.manifest_fingerprint(manifest)?;
        let path = self.grant_path(manifest)?;
        if self.grant_matches(&path, &fingerprint)? {
            return Ok(());
        }
        ensure!(confirm()?, "Local environment access denied");
        let parent = path.parent().context("Grant directory missing")?;
        super::client::private_directory(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        crate::platform::set_file_private(file.as_file(), 0o600)?;
        file.write_all(fingerprint.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(&path)?;
        crate::platform::sync_directory(parent)?;
        Ok(())
    }

    pub(super) fn grant_path(
        &self,
        manifest: &super::protocol::EnvironmentManifest,
    ) -> anyhow::Result<PathBuf> {
        use sha2::{Digest, Sha256};
        let scope = serde_json::to_vec(&(
            self.server.as_str(),
            &manifest.project_root,
            self.grant_device()?,
        ))?;
        Ok(self
            .credential_file
            .with_extension(format!("grant-{:x}", Sha256::digest(scope))))
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

    fn grant_matches(&self, path: &Path, fingerprint: &str) -> anyhow::Result<bool> {
        // A scoped decision supersedes the legacy grant for this project.
        match Self::read_grant(path)? {
            Some(saved) => Ok(saved == fingerprint),
            None if self.grant_device()?.is_some() => Ok(false),
            None => Ok(
                Self::read_grant(&self.credential_file.with_extension("grant"))?.as_deref()
                    == Some(fingerprint),
            ),
        }
    }

    fn read_grant(path: &Path) -> anyhow::Result<Option<String>> {
        use std::io::Read;
        let file = match crate::platform::OpenOptions::new()
            .read(true)
            .no_follow()
            .nonblock()
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.len() <= 64 && crate::platform::is_private(&file)?,
            "Grant must be a private bounded regular file"
        );
        let mut saved = String::new();
        file.take(65).read_to_string(&mut saved)?;
        Ok(Some(saved))
    }

    pub(crate) fn load(path: &Path) -> anyhow::Result<Self> {
        use std::io::Read;
        let file = crate::platform::OpenOptions::new()
            .read(true)
            .nonblock()
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
        Self::from_toml(path, std::str::from_utf8(&bytes)?)
    }

    pub(crate) fn from_toml(path: &Path, contents: &str) -> anyhow::Result<Self> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Settings {
            server: String,
            credential_file: Option<PathBuf>,
            label: Option<String>,
            #[serde(default = "crate::config::default_true")]
            allow_shell: bool,
            #[serde(default)]
            skill_roots: Vec<PathBuf>,
            #[serde(default)]
            mcp: HashMap<String, McpServerConfig>,
            #[serde(default)]
            optional_mcp: Vec<String>,
        }
        // TOML diagnostics can include MCP secrets from the offending source line.
        let settings: Settings = toml::from_str(contents)
            .map_err(|_| anyhow::anyhow!("Invalid remote configuration"))?;
        let server = settings
            .server
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid server URL"))?;
        super::client::endpoint(&server)?;
        let label = settings.label.unwrap_or_else(|| {
            crate::platform::hostname()
                .filter(|label| Self::valid_label(label))
                .unwrap_or_else(|| "Device".into())
        });
        ensure!(Self::valid_label(&label), "Invalid device label");
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
            credential: None,
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
    pub(crate) credential: Option<super::client::Credential>,
    pub label: String,
    pub allow_shell: bool,
    pub skill_roots: Vec<PathBuf>,
    pub mcp: HashMap<String, McpServerConfig>,
    pub optional_mcp: Vec<String>,
}
