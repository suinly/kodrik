use crate::agent::{message::ToolOutput, session::SessionId};
use anyhow::ensure;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use uuid::Uuid;

pub(crate) const MAX_FRAME_BYTES: usize = 256 * 1024;
pub(crate) const MAX_CHUNK_BYTES: usize = 64 * 1024;
pub(crate) const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnvironmentKey {
    pub connection_id: Uuid,
    pub generation: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnvironmentManifest {
    pub label: String,
    pub project_root: String,
    pub tools: Vec<ToolDescriptor>,
    pub skills: Vec<SkillDescriptor>,
    pub project_instructions: Option<ProjectInstructions>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolDescriptor {
    pub id: String,
    pub kind: ToolKind,
    pub description: String,
    pub parameters: Value,
    pub deferred: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolKind {
    Exec,
    Patch,
    Image,
    ReadFile,
    Mcp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SkillDescriptor {
    pub id: String,
    pub name: String,
    pub description: String,
    pub path: String,
    pub content_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectInstructions {
    pub path: String,
    pub content: String,
    pub content_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Invocation {
    pub environment: EnvironmentKey,
    pub session_id: SessionId,
    pub run_id: String,
    pub call_id: String,
    pub tool_id: String,
    pub arguments: Value,
    pub deadline: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OperationState {
    Accepted,
    Running,
    Completed,
    Stopped,
    Unknown,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OperationReceipt {
    pub invocation: Invocation,
    pub arguments_hash: String,
    pub state: OperationState,
    pub result: Option<ToolOutput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnvironmentStatus {
    pub session_id: SessionId,
    pub environment: Option<EnvironmentKey>,
    pub label: String,
    pub project_root: String,
    pub state: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ClientFrame {
    Hello {
        session_id: SessionId,
        manifest: EnvironmentManifest,
    },
    Heartbeat {
        environment: EnvironmentKey,
    },
    Receipt {
        receipt: OperationReceipt,
    },
    ResultChunk {
        environment: EnvironmentKey,
        run_id: String,
        call_id: String,
        arguments_hash: String,
        sequence: u32,
        final_chunk: bool,
        data: Vec<u8>,
    },
    Close {
        environment: EnvironmentKey,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ServerFrame {
    Welcome {
        environment: EnvironmentKey,
        heartbeat_seconds: u64,
        lease_seconds: u64,
    },
    Invoke {
        invocation: Invocation,
    },
    Cancel {
        environment: EnvironmentKey,
        run_id: String,
        call_id: String,
    },
    ReceiptAck {
        environment: EnvironmentKey,
        run_id: String,
        call_id: String,
        arguments_hash: String,
    },
    HeartbeatAck {
        environment: EnvironmentKey,
    },
    Revoke {
        environment: EnvironmentKey,
        reason: String,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteChatSnapshot {
    pub session_id: SessionId,
    pub revision: i64,
    pub environment_revision: u64,
    pub messages: Vec<Value>,
    pub previous_cursor: Option<String>,
    pub run: Option<Value>,
    pub versions: Value,
    pub environment: Option<EnvironmentStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ChatCommand {
    Send {
        request_id: Uuid,
        text: String,
    },
    Stop,
    New,
    Select {
        session_id: SessionId,
    },
    Edit {
        request_id: Uuid,
        user_turn: usize,
        text: String,
    },
    SelectVersion {
        session_id: SessionId,
        user_turn: usize,
    },
    DecideApproval {
        approval_id: String,
        allow: bool,
    },
    SetAuditMode {
        mode: String,
    },
}

pub(crate) fn validate_manifest(manifest: &EnvironmentManifest) -> anyhow::Result<()> {
    ensure!(
        serde_json::to_vec(manifest)?.len() <= MAX_FRAME_BYTES,
        "Manifest exceeds 256 KiB"
    );
    ensure!(
        manifest.tools.len() <= 256 && manifest.skills.len() <= 64,
        "Too many capabilities"
    );
    ensure!(!manifest.label.trim().is_empty(), "Missing device label");
    ensure!(
        std::path::Path::new(&manifest.project_root).is_absolute(),
        "Project root must be absolute"
    );
    let mut ids = HashSet::new();
    for tool in &manifest.tools {
        ensure!(
            valid_id(&tool.id) && ids.insert(&tool.id),
            "Invalid or duplicate tool ID"
        );
        ensure!(tool.parameters.is_object(), "Tool schema must be an object");
    }
    ids.clear();
    for skill in &manifest.skills {
        ensure!(
            valid_id(&skill.id) && ids.insert(&skill.id),
            "Invalid or duplicate skill ID"
        );
        ensure!(
            std::path::Path::new(&skill.path).is_absolute(),
            "Skill path must be absolute"
        );
        ensure!(valid_hash(&skill.content_hash), "Invalid skill hash");
    }
    if let Some(project) = &manifest.project_instructions {
        ensure!(
            project.content.len() <= 64 * 1024,
            "Project instructions exceed 64 KiB"
        );
        ensure!(
            std::path::Path::new(&project.path).is_absolute(),
            "Instructions path must be absolute"
        );
        ensure!(
            valid_hash(&project.content_hash),
            "Invalid instructions hash"
        );
    }
    Ok(())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

pub(crate) fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(crate) fn arguments_hash(arguments: &Value) -> anyhow::Result<String> {
    fn canonical(value: &Value) -> Value {
        match value {
            Value::Object(fields) => {
                let mut keys: Vec<_> = fields.keys().collect();
                keys.sort();
                Value::Object(
                    keys.into_iter()
                        .map(|key| (key.clone(), canonical(&fields[key])))
                        .collect(),
                )
            }
            Value::Array(values) => Value::Array(values.iter().map(canonical).collect()),
            other => other.clone(),
        }
    }
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&canonical(arguments))?)
    ))
}

pub(crate) trait WireFrame: Serialize + DeserializeOwned {
    fn validate(&self) -> anyhow::Result<()>;
}

impl WireFrame for ClientFrame {
    fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::Hello { manifest, .. } => validate_manifest(manifest)?,
            Self::ResultChunk {
                data,
                arguments_hash,
                ..
            } => {
                ensure!(data.len() <= MAX_CHUNK_BYTES, "Chunk exceeds 64 KiB");
                ensure!(valid_hash(arguments_hash), "Invalid arguments hash");
            }
            Self::Receipt { receipt } => {
                ensure!(
                    valid_hash(&receipt.arguments_hash),
                    "Invalid arguments hash"
                );
                if let Some(result) = &receipt.result {
                    ensure!(
                        serde_json::to_vec(result)?.len() <= MAX_OUTPUT_BYTES,
                        "Output exceeds 16 MiB"
                    );
                }
            }
            _ => {}
        }
        Ok(())
    }
}

impl WireFrame for ServerFrame {
    fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope<T> {
    version: u8,
    frame: T,
}

pub(crate) fn encode_frame<T: WireFrame>(frame: &T) -> anyhow::Result<Vec<u8>> {
    frame.validate()?;
    let bytes = serde_json::to_vec(&Envelope { version: 1, frame })?;
    ensure!(bytes.len() <= MAX_FRAME_BYTES, "Frame exceeds 256 KiB");
    Ok(bytes)
}

pub(crate) fn decode_frame<T: WireFrame>(bytes: &[u8]) -> anyhow::Result<T> {
    ensure!(bytes.len() <= MAX_FRAME_BYTES, "Frame exceeds 256 KiB");
    let envelope: Envelope<T> = serde_json::from_slice(bytes)?;
    ensure!(envelope.version == 1, "Unsupported remote protocol version");
    envelope.frame.validate()?;
    Ok(envelope.frame)
}
