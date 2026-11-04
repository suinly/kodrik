use crate::config::McpServerConfig;
use std::{collections::HashMap, path::PathBuf};

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
