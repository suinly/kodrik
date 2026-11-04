use super::{config::RemoteConfig, protocol::*, supervisor::Supervisor};
use crate::agent::{
    Image,
    session::SessionId,
    tools::{ApplyPatchTool, ExecCommandTool, Tool, ToolOutput, ViewImageTool},
};
use anyhow::{Context, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Read,
    os::fd::{AsRawFd, FromRawFd},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{Notify, watch};
use uuid::Uuid;

const JOURNAL_BYTES: u64 = 256 * 1024 * 1024;
type CallId = (String, String);
struct Binding {
    device: Uuid,
    key: EnvironmentKey,
    session: SessionId,
}
struct State {
    binding: Option<Binding>,
    active: HashMap<CallId, watch::Sender<bool>>,
    db: Connection,
}
struct ActiveCall<'a> {
    environment: &'a LocalEnvironment,
    id: CallId,
}
impl Drop for ActiveCall<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.environment.state.lock() {
            state.active.remove(&self.id);
        }
        self.environment.finished.notify_waiters();
    }
}
pub(crate) struct LocalEnvironment {
    root: PathBuf,
    skill_roots: Vec<PathBuf>,
    server: String,
    manifest: EnvironmentManifest,
    tools: HashMap<String, Arc<dyn Tool>>,
    supervisor: Arc<Supervisor>,
    state: Mutex<State>,
    closed: AtomicBool,
    finished: Notify,
    connections: Mutex<Vec<rmcp::service::RunningServiceCancellationToken>>,
}

impl LocalEnvironment {
    pub(crate) async fn open(config: &RemoteConfig, root: &Path) -> anyhow::Result<Arc<Self>> {
        let root = root.canonicalize()?;
        ensure!(root.is_dir(), "Project root is not a directory");
        let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
        let mut connections = Vec::new();
        tools.insert("apply_patch".into(), Arc::new(ApplyPatchTool));
        tools.insert("view_image".into(), Arc::new(ViewImageTool));
        if config.allow_shell {
            tools.insert("exec_command".into(), Arc::new(ExecCommandTool));
        }
        for (name, server) in &config.mcp {
            match crate::agent::tools::mcp::connect_scoped(
                &HashMap::from([(name.clone(), server.clone())]),
                &mut connections,
            )
            .await
            {
                Ok(connected) => {
                    for tool in connected {
                        if tool.name() != "mcp_search" {
                            ensure!(
                                tools.insert(tool.name(), tool).is_none(),
                                "Duplicate local tool"
                            );
                        }
                    }
                }
                Err(_) if config.optional_mcp.contains(name) => {}
                Err(error) => return Err(error),
            }
        }
        let mut descriptors: Vec<_> = tools
            .iter()
            .map(|(name, tool)| ToolDescriptor {
                id: name.clone(),
                kind: match name.as_str() {
                    "exec_command" => ToolKind::Exec,
                    "apply_patch" => ToolKind::Patch,
                    "view_image" => ToolKind::Image,
                    _ => ToolKind::Mcp,
                },
                description: tool.description(),
                parameters: tool.parameters(),
                deferred: tool.deferred(),
            })
            .collect();
        descriptors.push(ToolDescriptor {id:"read_file".into(),kind:ToolKind::ReadFile,description:"Read a UTF-8 file inside the selected project or skill roots, at most 1 MiB".into(),parameters:json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}),deferred:false});
        descriptors.sort_by(|a, b| a.id.cmp(&b.id));
        let mut skill_roots = Vec::new();
        let mut skills = Vec::new();
        for root in &config.skill_roots {
            let root = root.canonicalize()?;
            let mut entries = std::fs::read_dir(&root)?.collect::<Result<Vec<_>, _>>()?;
            ensure!(entries.len() <= 256, "Too many skill directories");
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let path = entry.path().join("SKILL.md");
                if !path.try_exists()? {
                    continue;
                }
                let bytes = read_scoped(&[root.clone()], &path, 1024 * 1024)?;
                let metadata = crate::agent::skills::metadata(&path, std::str::from_utf8(&bytes)?)?;
                skills.push(SkillDescriptor {
                    id: format!("skill_{}", skills.len()),
                    name: metadata["name"].as_str().unwrap().into(),
                    description: metadata["description"].as_str().unwrap().into(),
                    path: path.to_str().context("Non-UTF8 skill path")?.into(),
                    content_hash: hash_bytes(&bytes),
                });
            }
            skill_roots.push(root);
        }
        let instructions = root.join("AGENTS.md");
        let project_instructions = if instructions.exists() {
            let bytes = read_scoped(&[root.clone()], &instructions, 64 * 1024)?;
            Some(ProjectInstructions {
                path: instructions
                    .to_str()
                    .context("Non-UTF8 instructions path")?
                    .into(),
                content: String::from_utf8(bytes.clone())?,
                content_hash: hash_bytes(&bytes),
            })
        } else {
            None
        };
        let manifest = EnvironmentManifest {
            label: config.label.clone(),
            project_root: root.to_str().context("Non-UTF8 project root")?.into(),
            tools: descriptors,
            skills,
            project_instructions,
        };
        validate_manifest(&manifest)?;
        #[cfg(not(test))]
        let directory = crate::config::data_dir()?.join("remote");
        #[cfg(test)]
        let directory = config
            .credential_file
            .parent()
            .context("Credential parent missing")?
            .join("receipts");
        let db = open_journal(&directory)?;
        Ok(Arc::new(Self {
            root,
            skill_roots,
            server: config.server.to_string(),
            manifest,
            tools,
            supervisor: Arc::new(Supervisor::spawn()?),
            state: Mutex::new(State {
                binding: None,
                active: HashMap::new(),
                db,
            }),
            closed: AtomicBool::new(false),
            finished: Notify::new(),
            connections: Mutex::new(connections),
        }))
    }

    pub(crate) fn manifest(&self) -> anyhow::Result<EnvironmentManifest> {
        Ok(self.manifest.clone())
    }

    pub(crate) async fn attach(
        &self,
        device: Uuid,
        key: EnvironmentKey,
        session: SessionId,
    ) -> anyhow::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Local state poisoned"))?;
        ensure!(
            !self.closed.load(Ordering::SeqCst) && state.binding.is_none(),
            "Local environment already bound or closed"
        );
        state.binding = Some(Binding {
            device,
            key,
            session,
        });
        Ok(())
    }

    pub(crate) async fn execute(
        self: &Arc<Self>,
        call: Invocation,
    ) -> anyhow::Result<OperationReceipt> {
        if call.tool_id == "apply_patch" {
            let environment = self.clone();
            tokio::spawn(async move { environment.execute_inner(call).await }).await?
        } else {
            self.execute_inner(call).await
        }
    }

    async fn execute_inner(&self, call: Invocation) -> anyhow::Result<OperationReceipt> {
        encode_frame(&ServerFrame::Invoke {
            invocation: call.clone(),
        })?;
        let arguments = self.validate_arguments(&call)?;
        let (device, mut cancel, saved) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("Local state poisoned"))?;
            ensure!(
                !self.closed.load(Ordering::SeqCst),
                "Local environment closed"
            );
            let binding = state
                .binding
                .as_ref()
                .context("Local environment is not attached")?;
            ensure!(
                binding.key == call.environment && binding.session == call.session_id,
                "Stale local generation or session"
            );
            ensure!(call.deadline > chrono::Utc::now(), "Invocation expired");
            let device = binding.device.to_string();
            ensure!(state.active.len() < 4, "Too many local operations");
            let transaction = state
                .db
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let old: Option<(String, Vec<u8>)> = transaction
                .query_row(
                    "SELECT device,payload FROM receipts WHERE server=?1 AND run=?2 AND call=?3",
                    params![self.server, call.run_id, call.call_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((owner, bytes)) = old {
                ensure!(
                    owner == device,
                    "Local credential changed for existing call"
                );
                let mut saved: OperationReceipt = serde_json::from_slice(&bytes)?;
                ensure!(
                    serde_json::to_value(&saved.invocation)? == serde_json::to_value(&call)?
                        && saved.arguments_hash == arguments_hash(&call.arguments)?,
                    "Local call identity changed"
                );
                if matches!(
                    saved.state,
                    OperationState::Accepted | OperationState::Running
                ) {
                    saved.state = OperationState::Unknown;
                }
                return Ok(saved);
            }
            let used: i64 = transaction.query_row(
                "SELECT coalesce(sum(length(payload)+reserved),0) FROM receipts",
                [],
                |r| r.get(0),
            )?;
            ensure!(
                (used as u64 + MAX_OUTPUT_BYTES as u64 + MAX_FRAME_BYTES as u64) < JOURNAL_BYTES,
                "Local receipt quota exhausted"
            );
            let saved = OperationReceipt {
                invocation: call.clone(),
                arguments_hash: arguments_hash(&call.arguments)?,
                state: OperationState::Accepted,
                result: None,
            };
            transaction.execute(
                "INSERT INTO receipts VALUES(?1,?2,?3,?4,?5,?6,NULL)",
                params![
                    self.server,
                    device,
                    call.run_id,
                    call.call_id,
                    serde_json::to_vec(&saved)?,
                    MAX_OUTPUT_BYTES as i64
                ],
            )?;
            transaction.commit()?;
            let (sender, receiver) = watch::channel(false);
            state
                .active
                .insert((call.run_id.clone(), call.call_id.clone()), sender);
            (device, receiver, saved)
        };
        let _active = ActiveCall {
            environment: self,
            id: (call.run_id.clone(), call.call_id.clone()),
        };
        let timeout = (call.deadline - chrono::Utc::now())
            .to_std()
            .unwrap_or_default();
        let execution = async {
            match call.tool_id.as_str() {
                "exec_command" => {
                    crate::agent::tools::exec_command::execute(
                        arguments,
                        Some(self.supervisor.clone()),
                    )
                    .await
                }
                "read_file" | "view_image" => {
                    let path = PathBuf::from(arguments["path"].as_str().context("Path required")?);
                    let mut roots = vec![self.root.clone()];
                    roots.extend(self.skill_roots.clone());
                    let image = call.tool_id == "view_image";
                    let file_path = path.clone();
                    let bytes = tokio::task::spawn_blocking(move || {
                        read_scoped(
                            &roots,
                            &file_path,
                            if image { 10 * 1024 * 1024 } else { 1024 * 1024 },
                        )
                    })
                    .await??;
                    if image {
                        let image = Image::from_bytes(&bytes)?;
                        Ok(ToolOutput {
                            output: json!({"path":path,"width":image.width,"height":image.height}),
                            images: vec![image],
                        })
                    } else {
                        Ok(json!({"path":path,"text":String::from_utf8(bytes)?}).into())
                    }
                }
                _ => {
                    self.tools
                        .get(&call.tool_id)
                        .context("Unknown local tool")?
                        .execute(arguments)
                        .await
                }
            }
        };
        // File writes must finish before close returns; cancelling a blocking patch would hide effects.
        let (result, stopped) = if call.tool_id == "apply_patch" {
            (execution.await, false)
        } else {
            tokio::select! {
                result = execution => (result,false),
                _ = cancel.wait_for(|stop|*stop) => (Ok(json!({"interrupted":true,"effects":"unknown"}).into()),true),
                _ = tokio::time::sleep(timeout) => (Ok(json!({"interrupted":true,"reason":"deadline","effects":"unknown"}).into()),true),
            }
        };
        let mut saved = saved;
        saved.state = if stopped {
            OperationState::Unknown
        } else {
            OperationState::Completed
        };
        let result = result.unwrap_or_else(|error| json!({"error":format!("{error:#}")}).into());
        saved.result = if stopped {
            None
        } else {
            Some(if serde_json::to_vec(&result)?.len() <= MAX_OUTPUT_BYTES {
                result
            } else {
                json!({"error":"Local output exceeds 16 MiB"}).into()
            })
        };
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Local state poisoned"))?;
        let persisted = state.db.execute("UPDATE receipts SET payload=?5,reserved=0 WHERE server=?1 AND device=?2 AND run=?3 AND call=?4",params![self.server,device,call.run_id,call.call_id,serde_json::to_vec(&saved)?]);
        state.active.remove(&(call.run_id, call.call_id));
        self.finished.notify_waiters();
        persisted?;
        Ok(saved)
    }

    fn validate_arguments(&self, call: &Invocation) -> anyhow::Result<Value> {
        ensure!(
            self.manifest.tools.iter().any(|t| t.id == call.tool_id),
            "Local tool not allowed"
        );
        let mut args = call.arguments.clone();
        ensure!(args.is_object(), "Tool arguments must be an object");
        if matches!(call.tool_id.as_str(), "exec_command" | "apply_patch") {
            let path = args
                .get("workdir")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(|| self.root.clone());
            let path = if path.is_absolute() {
                path
            } else {
                self.root.join(path)
            };
            ensure!(
                path.canonicalize()?.starts_with(&self.root),
                "workdir must stay within selected project"
            );
            args["workdir"] = json!(path);
        } else if matches!(call.tool_id.as_str(), "read_file" | "view_image") {
            ensure!(
                args.as_object().unwrap().len() == 1,
                "Only path is supported"
            );
            let path = PathBuf::from(args["path"].as_str().context("Path required")?);
            args["path"] = json!(if path.is_absolute() {
                path
            } else {
                self.root.join(path)
            });
        }
        Ok(args)
    }

    pub(crate) async fn cancel(&self, run: &str, call: &str) -> anyhow::Result<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Local state poisoned"))?;
        if let Some(cancel) = state.active.get(&(run.into(), call.into())) {
            cancel.send_replace(true);
        }
        Ok(())
    }

    pub(crate) async fn acknowledge(&self, call: Invocation) -> anyhow::Result<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Local state poisoned"))?;
        let binding = state.binding.as_ref().context("No local binding")?;
        ensure!(
            binding.key == call.environment && binding.session == call.session_id,
            "Stale ACK"
        );
        let bytes: Vec<u8> = state.db.query_row(
            "SELECT payload FROM receipts WHERE server=?1 AND device=?2 AND run=?3 AND call=?4",
            params![
                self.server,
                binding.device.to_string(),
                call.run_id,
                call.call_id
            ],
            |r| r.get(0),
        )?;
        let saved: OperationReceipt = serde_json::from_slice(&bytes)?;
        ensure!(
            serde_json::to_value(&saved.invocation)? == serde_json::to_value(&call)?
                && saved.arguments_hash == arguments_hash(&call.arguments)?,
            "Changed receipt ACK"
        );
        state.db.execute("UPDATE receipts SET acknowledged=?5 WHERE server=?1 AND device=?2 AND run=?3 AND call=?4 AND json_extract(payload,'$.state') IN ('completed','stopped','rejected','unknown')",params![self.server,binding.device.to_string(),call.run_id,call.call_id,chrono::Utc::now().timestamp()])?;
        Ok(())
    }

    pub(crate) async fn close(&self) -> anyhow::Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        for connection in self
            .connections
            .lock()
            .map_err(|_| anyhow::anyhow!("Local connections poisoned"))?
            .drain(..)
        {
            connection.cancel();
        }
        {
            let state = self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("Local state poisoned"))?;
            for cancel in state.active.values() {
                cancel.send_replace(true);
            }
        }
        let supervisor = self.supervisor.clone();
        let cleanup = tokio::task::spawn_blocking(move || supervisor.stop_all()).await;
        loop {
            let finished = self.finished.notified();
            tokio::pin!(finished);
            finished.as_mut().enable();
            if self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("Local state poisoned"))?
                .active
                .is_empty()
            {
                break;
            }
            finished.await;
        }
        cleanup??;
        Ok(())
    }
}

fn hash_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

fn open_journal(directory: &Path) -> anyhow::Result<Connection> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    ensure!(
        std::fs::symlink_metadata(directory)?.is_dir(),
        "Receipt directory cannot be a symlink"
    );
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    let directory = directory.canonicalize()?;
    let path = directory.join("receipts.sqlite3");
    let marker = directory.join("journal-created");
    let mut marker_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&marker)?;
    // SAFETY: lock the live marker descriptor while initializing/checking journal identity.
    ensure!(
        unsafe { libc::flock(marker_file.as_raw_fd(), libc::LOCK_EX) } == 0,
        "Cannot lock receipt identity"
    );
    let mut identity = String::new();
    marker_file
        .by_ref()
        .take(128)
        .read_to_string(&mut identity)?;
    ensure!(
        identity.is_empty() || path.try_exists()?,
        "Local receipt journal was lost; reconcile unknown operations before resetting the remote receipt directory"
    );
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    ensure!(file.metadata()?.is_file(), "Receipt journal must be a file");
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    let db = Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::default() | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    use std::os::unix::fs::MetadataExt;
    let current = std::fs::symlink_metadata(&path)?;
    ensure!(
        current.is_file()
            && current.ino() == file.metadata()?.ino()
            && current.dev() == file.metadata()?.dev(),
        "Receipt journal replaced during open"
    );
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.pragma_update(None, "synchronous", "FULL")?;
    if !identity.is_empty() {
        let saved: String =
            db.query_row("SELECT identity FROM journal_identity", [], |r| r.get(0))?;
        ensure!(
            saved == identity,
            "Receipt journal identity changed; outcomes unknown"
        );
    } else {
        identity = Uuid::new_v4().to_string();
        db.execute_batch("CREATE TABLE journal_identity(identity TEXT NOT NULL)")?;
        db.execute("INSERT INTO journal_identity VALUES(?1)", [&identity])?;
        use std::io::Write;
        marker_file.write_all(identity.as_bytes())?;
        marker_file.sync_all()?;
        std::fs::File::open(&directory)?.sync_all()?;
    }
    let page_size: i64 = db.pragma_query_value(None, "page_size", |r| r.get(0))?;
    db.pragma_update(None, "max_page_count", JOURNAL_BYTES as i64 / page_size)?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS receipts(server TEXT NOT NULL,device TEXT NOT NULL,run TEXT NOT NULL,call TEXT NOT NULL,payload BLOB NOT NULL,reserved INTEGER NOT NULL,acknowledged INTEGER,PRIMARY KEY(server,device,run,call));")?;
    db.execute(
        "DELETE FROM receipts WHERE acknowledged IS NOT NULL AND acknowledged < ?1",
        [chrono::Utc::now().timestamp() - 7 * 24 * 3600],
    )?;
    Ok(db)
}

fn read_scoped(roots: &[PathBuf], path: &Path, limit: usize) -> anyhow::Result<Vec<u8>> {
    let root = roots
        .iter()
        .find(|root| path.starts_with(root))
        .context("Path outside selected roots")?;
    let relative = path.strip_prefix(root)?;
    ensure!(
        relative
            .components()
            .all(|c| matches!(c, Component::Normal(_))),
        "Path traversal rejected"
    );
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(root)?;
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let name = std::ffi::CString::new(component.as_os_str().as_encoded_bytes())?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if components.peek().is_some() {
                libc::O_DIRECTORY
            } else {
                0
            };
        // SAFETY: name is NUL-terminated; parent file owns a live directory descriptor.
        let fd = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: successful openat returns a new descriptor owned by this File.
        file = unsafe { std::fs::File::from_raw_fd(fd) };
    }
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= limit as u64,
        "Not a regular bounded file"
    );
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "File exceeds read limit");
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn journal_readonly_and_full_errors_prevent_effects() {
        for full in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let cfg = RemoteConfig {
                server: reqwest::Url::parse("https://example.invalid/").unwrap(),
                credential_file: dir.path().join("device"),
                label: "fixture".into(),
                allow_shell: true,
                skill_roots: vec![],
                mcp: HashMap::new(),
                optional_mcp: vec![],
            };
            let env = LocalEnvironment::open(&cfg, dir.path()).await.unwrap();
            let call = Invocation {
                environment: EnvironmentKey {
                    connection_id: Uuid::new_v4(),
                    generation: Uuid::new_v4(),
                },
                session_id: SessionId::new(Uuid::new_v4().to_string()),
                run_id: Uuid::new_v4().to_string(),
                call_id: "effect".into(),
                tool_id: "exec_command".into(),
                arguments: json!({"cmd":format!("echo no > effect; #{}","x".repeat(48*1024))}),
                deadline: chrono::Utc::now() + chrono::Duration::seconds(30),
            };
            env.attach(
                Uuid::new_v4(),
                call.environment.clone(),
                call.session_id.clone(),
            )
            .await
            .unwrap();
            {
                let state = env.state.lock().unwrap();
                if full {
                    let pages: i64 = state
                        .db
                        .pragma_query_value(None, "page_count", |r| r.get(0))
                        .unwrap();
                    state
                        .db
                        .pragma_update(None, "max_page_count", pages)
                        .unwrap();
                } else {
                    state.db.pragma_update(None, "query_only", true).unwrap();
                }
            }
            let error = env.execute(call).await.unwrap_err();
            assert_eq!(
                error
                    .downcast_ref::<rusqlite::Error>()
                    .and_then(|e| e.sqlite_error_code()),
                Some(if full {
                    rusqlite::ErrorCode::DiskFull
                } else {
                    rusqlite::ErrorCode::ReadOnly
                })
            );
            assert!(!dir.path().join("effect").exists());
            env.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn closed_environment_disconnects_its_http_mcp_clients() {
        use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::post};
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let router = Router::new().route("/mcp",post(move |Json(body):Json<serde_json::Value>| {
        let tx=tx.clone();async move {
            tx.send(body.clone()).unwrap();
            if body.get("id").is_none() {return StatusCode::ACCEPTED.into_response();}
            let result = match body["method"].as_str().unwrap() {
                "initialize"=>json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}),
                "tools/list"=>json!({"tools":[{"name":"effect","inputSchema":{"type":"object","properties":{}}}]}),
                "tools/call"=>json!({"content":[]}),
                _=>panic!("unexpected method"),
            };
            Json(json!({"jsonrpc":"2.0","id":body["id"],"result":result})).into_response()
        }
    }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = RemoteConfig {
            server: reqwest::Url::parse("https://example.invalid/").unwrap(),
            credential_file: dir.path().join("device"),
            label: "fixture".into(),
            allow_shell: false,
            skill_roots: vec![],
            mcp: HashMap::new(),
            optional_mcp: vec![],
        };
        cfg.mcp.insert(
            "fixture".into(),
            crate::config::McpServerConfig::Http {
                url,
                headers: HashMap::new(),
            },
        );
        let env = LocalEnvironment::open(&cfg, dir.path()).await.unwrap();
        let tool = env
            .tools
            .values()
            .find(|tool| tool.name().starts_with("mcp__"))
            .unwrap()
            .clone();
        env.close().await.unwrap();
        while rx.try_recv().is_ok() {}
        assert!(
            tool.execute(json!({})).await.is_err(),
            "closed environment retained a live MCP transport"
        );
        while let Ok(body) = rx.try_recv() {
            assert_ne!(body["method"], "tools/call");
        }
        server.abort();
    }
}
