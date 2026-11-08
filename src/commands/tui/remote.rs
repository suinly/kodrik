use super::*;
use crate::remote::protocol::RemoteChatSnapshot;
use crate::remote::{
    client::{ChatClient, RemoteClient},
    config::RemoteConfig,
    local::LocalEnvironment,
    protocol::ChatCommand,
};
use anyhow::{Context, ensure};
use std::path::PathBuf;

#[test]
fn snapshot_waiting_queue_replaces_optimistic_inputs_and_clears_when_consumed() {
    let mut app = App::default();
    app.waiting_messages = vec!["optimistic".into()];
    let mut snapshot: RemoteChatSnapshot = serde_json::from_value(serde_json::json!({
        "session_id":"queue-session", "revision":1, "environment_revision":0,
        "messages":[], "previous_cursor":null, "versions":null, "environment":null,
        "run":{"queued":[{"text":"authoritative", "photos":1}]}
    }))
    .unwrap();
    project(&mut app, &snapshot).unwrap();
    assert_eq!(app.waiting_messages, ["authoritative\n[Images: 1]"]);
    snapshot.run.as_mut().unwrap()["queued"] = serde_json::json!([]);
    project(&mut app, &snapshot).unwrap();
    assert!(app.waiting_messages.is_empty());
}

struct Prepared(Option<Arc<LocalEnvironment>>);
impl Drop for Prepared {
    fn drop(&mut self) {
        if let Some(environment) = self.0.take() {
            let _ = environment.stop_admission();
            tokio::spawn(async move {
                let _ = environment.close().await;
            });
        }
    }
}

pub(super) fn with_scrollback(
    current: &RemoteChatSnapshot,
    mut latest: RemoteChatSnapshot,
) -> RemoteChatSnapshot {
    if current.session_id == latest.session_id
        && current.revision == latest.revision
        && current.messages.len() > latest.messages.len()
    {
        latest.messages = current.messages.clone();
        latest.previous_cursor = current.previous_cursor.clone();
    }
    latest
}

pub(super) fn audit_mode(
    input: &str,
) -> anyhow::Result<crate::agent::command_audit::CommandAuditMode> {
    let parts = input.split_whitespace().collect::<Vec<_>>();
    let ["/audit", mode] = parts.as_slice() else {
        anyhow::bail!("Use /audit trusted|review|ask for the current server session");
    };
    mode.parse()
}

struct Binding {
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
    environment: Arc<LocalEnvironment>,
}
impl Drop for Binding {
    fn drop(&mut self) {
        let _ = self.environment.stop_admission();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}
impl Binding {
    async fn open(
        config: &RemoteConfig,
        mut prepared: Prepared,
        session: SessionId,
    ) -> anyhow::Result<Self> {
        let environment = prepared
            .0
            .as_ref()
            .context("Local environment missing")?
            .clone();
        let result = async {
            let manifest = environment.manifest()?;
            ensure!(
                config.manifest_authorized(&manifest)?,
                "Local environment needs approval"
            );
            let mut client = RemoteClient::connect(config).await?;
            let key = client.attach(session, manifest).await?;
            let (stop, receiver) = oneshot::channel();
            let (ready, started) = oneshot::channel();
            let local = environment.clone();
            let task = tokio::spawn(async move {
                client
                    .serve_local_until_ready(local, receiver, Some(ready))
                    .await
                    .with_context(|| {
                        format!(
                            "connection_id={} generation={}",
                            key.connection_id, key.generation
                        )
                    })
            });
            let binding = Self {
                stop: Some(stop),
                task,
                environment: environment.clone(),
            };
            tokio::time::timeout(Duration::from_secs(8), started).await??;
            Ok(binding)
        }
        .await;
        if result.is_ok() {
            prepared.0.take();
        }
        result
    }
    async fn close(&mut self) -> anyhow::Result<()> {
        self.environment.stop_admission()?;
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        match tokio::time::timeout(Duration::from_secs(8), &mut self.task).await {
            Ok(result) => result?,
            Err(_) => {
                self.task.abort();
                let _ = (&mut self.task).await;
                anyhow::bail!("Local cleanup timed out; server lease will expire")
            }
        }
    }
}

struct Observer(tokio::task::JoinHandle<()>);
impl Drop for Observer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct SessionListRequest {
    offset: usize,
    task: tokio::task::JoinHandle<anyhow::Result<serde_json::Value>>,
}
impl Drop for SessionListRequest {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn session_rows(value: serde_json::Value) -> anyhow::Result<Vec<(SessionId, String)>> {
    let rows = value.as_array().context("Invalid session list")?;
    ensure!(rows.len() <= 30, "Session page exceeds limit");
    rows.iter()
        .map(|row| {
            let id = row["id"].as_str().context("Missing session ID")?;
            ensure!(
                !id.is_empty() && id.len() <= 1024 && !id.chars().any(char::is_control),
                "Invalid session ID"
            );
            let title = row["title"].as_str().context("Missing session title")?;
            Ok((
                SessionId::new(id),
                title.chars().flat_map(char::escape_debug).collect(),
            ))
        })
        .collect()
}
enum Update {
    Snapshot(RemoteChatSnapshot),
    Resynchronized(RemoteChatSnapshot),
    Disconnected(String),
}
pub(super) fn current_observer(expected: u64, received: u64) -> bool {
    expected == received
}
fn observe(
    chat: ChatClient,
    session: SessionId,
    epoch: u64,
    updates: mpsc::Sender<(u64, Update)>,
) -> Observer {
    Observer(tokio::spawn(async move {
        loop {
            let (tx, mut rx) = mpsc::channel(2);
            let stream = chat.observe_incremental(&session, tx);
            tokio::pin!(stream);
            let mut initial = true;
            let error = loop {
                tokio::select! {
                    result=&mut stream=>break result.err().map(|error| error.to_string()).unwrap_or_else(|| "Event stream closed".into()),
                    Some(snapshot)=rx.recv()=>{
                        let update = if initial { Update::Resynchronized(snapshot) } else { Update::Snapshot(snapshot) };
                        initial = false;
                        if updates.send((epoch,update)).await.is_err(){return;}
                    }
                }
            };
            // EOF may win select after the client has queued its final complete update.
            while let Ok(snapshot) = rx.try_recv() {
                let update = if initial {
                    Update::Resynchronized(snapshot)
                } else {
                    Update::Snapshot(snapshot)
                };
                initial = false;
                if updates.send((epoch, update)).await.is_err() {
                    return;
                }
            }
            if updates
                .send((epoch, Update::Disconnected(error)))
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }))
}

struct Remote {
    config: RemoteConfig,
    chat: ChatClient,
    session: SessionId,
    binding: Option<Binding>,
    pending: Option<Prepared>,
    server_connected: bool,
    binding_failed: bool,
    observer: Option<Observer>,
    updates: mpsc::Sender<(u64, Update)>,
    epoch: u64,
    projection: Projection,
    snapshot: RemoteChatSnapshot,
    uncertain: Option<ChatCommand>,
    session_list: Option<SessionListRequest>,
    sessions_offset: usize,
    root: PathBuf,
}
impl Remote {
    fn load_sessions(&mut self, app: &mut App, reset: bool) {
        if reset {
            self.session_list.take();
            self.sessions_offset = 0;
            app.sessions = Some(Vec::new());
            app.session_index = 0;
            app.sessions_more = true;
        } else if self.session_list.is_some() || app.sessions.is_none() || !app.sessions_more {
            return;
        }
        app.sessions_loading = true;
        app.sessions_error = None;
        let chat = self.chat.clone();
        let offset = self.sessions_offset;
        self.session_list = Some(SessionListRequest {
            offset,
            task: tokio::spawn(async move { chat.sessions_page(offset).await }),
        });
    }

    fn receive_sessions(
        &mut self,
        app: &mut App,
        result: Result<anyhow::Result<serde_json::Value>, tokio::task::JoinError>,
    ) {
        let Some(request) = self.session_list.take() else {
            return;
        };
        app.sessions_loading = false;
        let Some(rows) = app.sessions.as_mut() else {
            return;
        };
        match result
            .map_err(anyhow::Error::from)
            .and_then(|result| result)
            .and_then(session_rows)
        {
            Ok(page) => {
                self.sessions_offset = request.offset + page.len();
                app.sessions_more = page.len() == 30;
                for row in page {
                    if !rows.iter().any(|(id, _)| id == &row.0) {
                        rows.push(row);
                    }
                }
            }
            Err(error) => app.sessions_error = Some(error.to_string()),
        }
    }

    fn connection_status(&self, app: &mut App) {
        app.remote_environment = Some(
            self.root
                .file_name()
                .unwrap_or(self.root.as_os_str())
                .to_string_lossy()
                .chars()
                .flat_map(char::escape_debug)
                .collect(),
        );
        app.remote_connection = Some(if !self.server_connected || self.binding_failed {
            "disconnected"
        } else if self.binding.is_some() {
            "connected"
        } else {
            "detached"
        });
    }

    async fn attach(&mut self, app: &mut App) -> anyhow::Result<()> {
        if self.binding.is_some() || self.pending.is_some() {
            return Ok(());
        }
        self.binding_failed = true;
        let prepared = Prepared(Some(
            LocalEnvironment::open(&self.config, &self.root).await?,
        ));
        let manifest = prepared.0.as_ref().unwrap().manifest()?;
        if self.config.manifest_authorized(&manifest)? {
            self.binding = Some(Binding::open(&self.config, prepared, self.session.clone()).await?);
            self.binding_failed = false;
            app.remote_connection = Some("connected");
            app.push_kodrik_message("Local environment attached to this session.".into());
        } else {
            let literal = |text: &str| {
                text.chars()
                    .flat_map(char::escape_debug)
                    .collect::<String>()
            };
            let details = format!(
                "Grant local environment access?\nServer: {}\nProject: {}\nShell: {} (not sandboxed)\nSelected HTTP MCP: {}\nShared tools: {}\nSkills: {}\nProject instructions: {}\n{}\n\nTool arguments, file contents, skill instructions and command/MCP outputs may be sent to the server and its model. Shell and MCP can affect resources outside this project; cleanup is best effort, not rollback. Access follows the selected conversation and ends when this TUI closes.",
                literal(self.config.server.as_str()),
                literal(&manifest.project_root),
                self.config.allow_shell,
                self.config
                    .mcp
                    .keys()
                    .map(|s| literal(s))
                    .collect::<Vec<_>>()
                    .join(", "),
                manifest
                    .tools
                    .iter()
                    .map(|tool| literal(&tool.id))
                    .collect::<Vec<_>>()
                    .join(", "),
                manifest
                    .skills
                    .iter()
                    .map(|skill| literal(&skill.name))
                    .collect::<Vec<_>>()
                    .join(", "),
                manifest.project_instructions.is_some(),
                manifest
                    .warnings
                    .iter()
                    .map(|warning| literal(warning))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            self.pending = Some(prepared);
            self.binding_failed = false;
            app.remote_grant = Some(super::app::RemoteGrant { details, choice: 1 });
            app.remote_connection = Some("detached");
            app.remote_grant_scroll = 0;
        }
        Ok(())
    }

    async fn decide_grant(&mut self, app: &mut App, allow: bool) -> anyhow::Result<()> {
        let prepared = self
            .pending
            .take()
            .context("No local access request pending")?;
        app.remote_grant = None;
        if !allow {
            app.remote_connection = Some("detached");
            app.push_kodrik_message("Local access declined; server chat remains available. Use /attach to review again.".into());
            return Ok(());
        }
        self.binding_failed = true;
        let manifest = prepared.0.as_ref().unwrap().manifest()?;
        self.config.authorize_manifest(&manifest, || Ok(true))?;
        self.binding = Some(Binding::open(&self.config, prepared, self.session.clone()).await?);
        self.binding_failed = false;
        app.remote_connection = Some("connected");
        app.push_kodrik_message("Local environment attached to this session.".into());
        Ok(())
    }

    async fn older(&mut self, app: &mut App) -> anyhow::Result<()> {
        if let Some(cursor) = self.snapshot.previous_cursor.clone() {
            let mut older = self.chat.history(&self.session, &cursor).await?;
            ensure!(
                older.revision == self.snapshot.revision,
                "History changed; wait for a fresh snapshot before loading older messages"
            );
            older.messages.extend(self.snapshot.messages.clone());
            ensure!(
                older.messages.len() <= 4000
                    && serde_json::to_vec(&older)?.len() <= 16 * 1024 * 1024,
                "Scrollback limit reached; older history remains on server"
            );
            older.run = self.snapshot.run.clone();
            older.environment = self.snapshot.environment.clone();
            older.environment_revision = self.snapshot.environment_revision;
            self.snapshot = older;
            project(app, &self.snapshot)?;
        }
        Ok(())
    }
    async fn detach(&mut self) -> anyhow::Result<()> {
        self.pending.take();
        self.binding_failed = false;
        if let Some(binding) = self.binding.as_mut() {
            let result = binding.close().await;
            self.binding.take();
            result?;
        }
        Ok(())
    }
    async fn select(&mut self, app: &mut App, command: ChatCommand) -> anyhow::Result<()> {
        self.detach().await?;
        let response = self.chat.command(&self.session, command).await?;
        let id = SessionId::new(
            response["id"]
                .as_str()
                .context("Server returned no session ID")?,
        );
        let snapshot = self.chat.snapshot(&id).await?;
        self.observer.take();
        self.epoch += 1;
        self.session = id;
        self.projection = Projection::default();
        self.snapshot = snapshot;
        self.uncertain = None;
        app.cancel_message_navigation();
        app.sessions = None;
        app.input_images.clear();
        self.projection.accept(app, &self.snapshot)?;
        self.observer = Some(observe(
            self.chat.clone(),
            self.session.clone(),
            self.epoch,
            self.updates.clone(),
        ));
        app.remote_connection = Some("detached");
        self.attach(app).await?;
        Ok(())
    }
    async fn send(&mut self, app: &mut App, text: String) -> anyhow::Result<()> {
        if self.uncertain.is_some() {
            app.discard_pending_input(&text, app.input_images.len());
            app.input = text;
            app.input_cursor = app.input.len();
            anyhow::bail!(
                "Previous input is unconfirmed; use /retry to check the same request ID first"
            );
        }
        let editing = app.navigation.editing.as_ref();
        let is_edit = editing.is_some();
        let command = if let Some(edit) = editing {
            let turn = self.absolute_turn(edit.user_turn)?;
            self.detach().await?;
            ChatCommand::Edit {
                request_id: uuid::Uuid::new_v4(),
                user_turn: turn,
                text: text.clone(),
            }
        } else if !app.input_images.is_empty() {
            ChatCommand::SendImages {
                request_id: uuid::Uuid::new_v4(),
                text: text.clone(),
                images: app.input_images.clone(),
            }
        } else {
            ChatCommand::Send {
                request_id: uuid::Uuid::new_v4(),
                text: text.clone(),
            }
        };
        self.uncertain = Some(command.clone());
        let result = self.chat.command(&self.session, command).await;
        match result {
            Ok(response) => {
                self.uncertain = None;
                app.input_images.clear();
                if is_edit {
                    let id =
                        SessionId::new(response["id"].as_str().context("Edit returned no branch")?);
                    self.select(app, ChatCommand::Select { session_id: id })
                        .await?;
                }
                Ok(())
            }
            Err(error) => {
                app.discard_pending_input(&text, app.input_images.len());
                app.input = text;
                app.input_cursor = app.input.len();
                Err(error)
            }
        }
    }
    fn absolute_turn(&self, local: usize) -> anyhow::Result<usize> {
        let row = self
            .snapshot
            .messages
            .iter()
            .filter(|row| row["role"] == "user")
            .nth(local)
            .context("User message is not loaded")?;
        Ok(row["user_turn"]
            .as_u64()
            .map(|turn| turn as usize)
            .unwrap_or(local))
    }
    async fn input(&mut self, app: &mut App, input: String) -> anyhow::Result<()> {
        if app.navigation.editing.is_some()
            && self.pending.is_none()
            && !(self.uncertain.is_some() && input.trim() == "/retry")
        {
            return self.send(app, input).await;
        }
        if handle_language_command(
            app,
            &input,
            &crate::config::data_dir()?.join("tui-language"),
        ) {
            return Ok(());
        }
        match input.trim() {
            "/audit" => {
                app.audit_picker = Some(approvals::AuditPicker {
                    session: None,
                    choice: approvals::AUDIT_MODES
                        .iter()
                        .position(|mode| *mode == app.audit_mode)
                        .unwrap_or(1),
                });
                app.sessions = None;
                app.view.clear_selection();
            }
            "/audit status" => app.push_kodrik_message(format!(
                "Current server tool mode: {}",
                app.audit_mode.code()
            )),
            "/sessions" => {
                self.load_sessions(app, true);
            }
            "/sessions retry" => self.load_sessions(app, false),
            "/new" => self.select(app, ChatCommand::New).await?,
            "/detach" => {
                self.detach().await?;
                app.remote_connection = Some("detached");
                app.push_kodrik_message(
                    "Local access detached; server conversation remains available.".into(),
                );
            }
            "/attach" | "/attach yes" => self.attach(app).await?,
            "/attach allow" => self.decide_grant(app, true).await?,
            "/attach deny" => self.decide_grant(app, false).await?,
            "/older" => self.older(app).await?,
            "/retry" => {
                let command = self.uncertain.clone().context("No unconfirmed input")?;
                let request_id = match &command {
                    ChatCommand::Send { request_id, .. }
                    | ChatCommand::SendImages { request_id, .. }
                    | ChatCommand::Edit { request_id, .. } => *request_id,
                    _ => unreachable!(),
                };
                let status = self
                    .chat
                    .command(&self.session, ChatCommand::SubmissionStatus { request_id })
                    .await?;
                if status.is_null() {
                    let response = self.chat.command(&self.session, command.clone()).await?;
                    self.uncertain = None;
                    if matches!(command, ChatCommand::Edit { .. }) {
                        self.select(
                            app,
                            ChatCommand::Select {
                                session_id: SessionId::new(
                                    response["id"].as_str().context("Edit returned no branch")?,
                                ),
                            },
                        )
                        .await?;
                    }
                } else {
                    self.uncertain = None;
                    app.push_kodrik_message(format!(
                        "Original input confirmed: {}",
                        status["status"]
                    ));
                    if matches!(command, ChatCommand::Edit { .. })
                        && let Some(id) = status["session_id"].as_str()
                    {
                        self.select(
                            app,
                            ChatCommand::Select {
                                session_id: SessionId::new(id),
                            },
                        )
                        .await?;
                    }
                }
                app.input.clear();
                app.input_images.clear();
                app.input_cursor = 0;
            }
            "/approvals" => {
                project(app, &self.chat.snapshot(&self.session).await?)?;
            }
            input if input.starts_with("/approve ") || input.starts_with("/deny ") => {
                let mut parts = input.split_whitespace();
                let allow = parts.next() == Some("/approve");
                let id = parts.next().context("Approval ID required")?;
                ensure!(parts.next().is_none(), "Unexpected approval arguments");
                ensure!(
                    !allow
                        || app
                            .approval
                            .as_ref()
                            .is_some_and(|approval| approval.id == id),
                    "Review the complete pending approval with /approvals before allowing it"
                );
                self.chat
                    .command(
                        &self.session,
                        ChatCommand::DecideApproval {
                            approval_id: id.into(),
                            allow,
                        },
                    )
                    .await?;
            }
            input if input.starts_with("/audit ") => {
                let parts = input.split_whitespace().collect::<Vec<_>>();
                if let ["/audit", "trusted", session, approval_id] = parts.as_slice() {
                    ensure!(
                        *session == self.session.as_str()
                            && app
                                .approval
                                .as_ref()
                                .is_some_and(|approval| approval.id == *approval_id),
                        "Approval is no longer displayed in this session"
                    );
                    self.chat
                        .command(
                            &self.session,
                            ChatCommand::TrustApproval {
                                approval_id: (*approval_id).into(),
                            },
                        )
                        .await?;
                    return Ok(());
                }
                let mode = audit_mode(input)?;
                self.chat
                    .command(
                        &self.session,
                        ChatCommand::SetAuditMode {
                            mode: mode.code().into(),
                        },
                    )
                    .await?;
            }
            input if input.starts_with('/') => anyhow::bail!(
                "Unknown remote command or arguments: {}",
                input.split_whitespace().next().unwrap_or("/")
            ),
            _ => self.send(app, input).await?,
        }
        Ok(())
    }
}

pub(crate) async fn run(config: &RemoteConfig) -> anyhow::Result<()> {
    let chat = ChatClient::new(config)?;
    let sessions = chat.sessions().await?;
    let session = match sessions
        .as_array()
        .and_then(|rows| rows.first())
        .and_then(|row| row["id"].as_str())
    {
        Some(id) => SessionId::new(id),
        None => SessionId::new(
            chat.command(&SessionId::new("new"), ChatCommand::New)
                .await?["id"]
                .as_str()
                .context("Server returned no session ID")?,
        ),
    };
    let snapshot = chat.snapshot(&session).await?;
    let (tx, mut rx) = mpsc::channel(4);
    let mut remote = Remote {
        config: config.clone(),
        observer: Some(observe(chat.clone(), session.clone(), 0, tx.clone())),
        epoch: 0,
        chat,
        session,
        binding: None,
        pending: None,
        server_connected: true,
        binding_failed: false,
        updates: tx,
        projection: Projection::default(),
        snapshot,
        uncertain: None,
        session_list: None,
        sessions_offset: 0,
        root: std::env::current_dir()?.canonicalize()?,
    };
    let mut app = App {
        language: load_language(
            &crate::config::data_dir()?.join("tui-language"),
            Language::from_environment(),
        )?,
        ..Default::default()
    };
    remote.projection.accept(&mut app, &remote.snapshot)?;
    let mut tui = Tui::enter()?;
    let result = event_loop(&mut remote, &mut app, &mut tui, &mut rx).await;
    remote.observer.take();
    let cleanup = remote.detach().await;
    let terminal = tui.exit();
    result.and(cleanup).and(terminal)
}

async fn event_loop(
    remote: &mut Remote,
    app: &mut App,
    tui: &mut Tui,
    updates: &mut mpsc::Receiver<(u64, Update)>,
) -> anyhow::Result<()> {
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    let mut copies = JoinSet::new();
    let mut deferred = std::collections::VecDeque::new();
    let shutdown = crate::commands::shutdown_signal();
    tokio::pin!(shutdown);
    remote.connection_status(app);
    tui.draw(app)?;
    let startup = {
        let attach = remote.attach(app);
        tokio::pin!(attach);
        loop {
            tokio::select! {
                result = &mut attach => break Some(result),
                _ = &mut shutdown => break None,
                event = next_terminal_event() => {
                    let event = event?;
                    if matches!(event, Event::Key(key) if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c'|'C'|'с'|'С'))) { break None; }
                    if deferred.len() >= 64 || matches!(&event, Event::Paste(text) if text.len()>64*1024) { break Some(Err(anyhow::anyhow!("Pending UI input limit reached"))); }
                    deferred.push_back(event);
                }
            }
        }
    };
    match startup {
        None => return Ok(()),
        Some(Err(error)) => app.push_kodrik_message(format!(
            "Local attachment failed: {error}. Server chat remains available; use /attach to retry."
        )),
        Some(Ok(())) => {}
    }
    while !app.should_quit {
        if app.sessions.is_none() {
            remote.session_list.take();
            app.sessions_loading = false;
        } else if !app.sessions_loading
            && app.sessions_error.is_none()
            && app.sessions_more
            && app
                .sessions
                .as_ref()
                .is_some_and(|rows| app.session_index.saturating_add(5) >= rows.len())
        {
            remote.load_sessions(app, false);
        }
        remote.connection_status(app);
        tui.draw(app)?;
        tokio::select! {
            _=&mut shutdown=>app.should_quit=true,
            result=async {match remote.session_list.as_mut() {
                Some(request)=>(&mut request.task).await,
                None=>std::future::pending().await,
            }}=>remote.receive_sessions(app,result),
            _=tick.tick()=>{
                app.expire_copy_notice(std::time::Instant::now());
                app.animation_frame=app.animation_frame.wrapping_add(1);
                if remote.binding.as_ref().is_some_and(|binding|binding.task.is_finished()){
                    let result=remote.detach().await;
                    remote.binding_failed=true;
                    report_disconnect(app,result);
                }
            },
            Some((epoch,update))=updates.recv()=>{
                if !current_observer(remote.epoch,epoch) {continue;}
                match update {
                Update::Snapshot(snapshot)=>{
                    remote.server_connected=true;
                    let snapshot=with_scrollback(&remote.snapshot,snapshot);
                    if remote.projection.accept(app,&snapshot)? {remote.snapshot=snapshot;}
                }
                Update::Resynchronized(snapshot)=>{
                    remote.server_connected=true;
                    let snapshot=with_scrollback(&remote.snapshot,snapshot);
                    if remote.projection.resynchronize(app,&snapshot)? {remote.snapshot=snapshot;}
                }
                Update::Disconnected(reason)=>{
                    remote.server_connected=false;
                    app.set_copy_notice(format!("Server disconnected: {reason}; reconnecting (no local fallback)"));
                }
            }},
            Some(copy)=copies.join_next(),if !copies.is_empty()=>{
                if let Ok((id,result))=copy {complete_copy(app,id,result,|sequence|tui.copy_to_terminal(sequence));}
            },
            event=async {match deferred.pop_front(){Some(event)=>Ok(event),None=>next_terminal_event().await}}=>match event? {
                Event::Key(key) if matches!(key.kind,KeyEventKind::Press|KeyEventKind::Repeat)=>{
                    if super::images::key(key) { super::images::clipboard(app).await; continue; }
                    match selection_command(app,key) {
                        Some(SelectionCommand::Clear)=>continue,
                        Some(SelectionCommand::Copy(text))=>{let id=app.view.selection_id();copies.spawn(async move{(id,clipboard::copy(text).await)});continue;},
                        None=>(),
                    }
                    let operation=async {
                        if app.remote_grant.is_none() && key.code==KeyCode::Enter && key.kind==KeyEventKind::Press && let Some((id,_))=app.sessions.as_ref().and_then(|rows|rows.get(app.session_index)) {
                            return remote.select(app,ChatCommand::Select{session_id:id.clone()}).await;
                        }
                        if key.code==KeyCode::Esc && app.remote_grant.is_none() && app.sessions.is_none() && app.audit_picker.is_none() && app.language_choice.is_none() && app.approval.is_none() && app.navigation.selected.is_none() && app.navigation.editing.is_none() && app.command_suggestion().is_none() && matches!(app.status,Status::Working) {
                            remote.chat.command(&remote.session,ChatCommand::Stop).await?;app.status=Status::Stopping;return Ok(());
                        }
                        let turn=app.selected_user_turn();
                        let granting=app.remote_grant.is_some();
                        if let Some(input)=app.update(key){remote.input(app,input).await?;}
                        if !granting && key.code==KeyCode::PageUp && app.navigation.selected.is_none() {remote.older(app).await?;}
                        if let Some(id)=app.navigation.pending_version.take() && let Some(turn)=turn {
                            let user_turn=remote.absolute_turn(turn)?;
                            remote.select(app,ChatCommand::SelectVersion{session_id:SessionId::new(id),user_turn}).await?;
                        }
                        Ok::<_,anyhow::Error>(())
                    };
                    let operation={
                        tokio::pin!(operation);
                        loop {
                            tokio::select! {
                                result=&mut operation=>break Some(result),
                                _=&mut shutdown=>break None,
                                event=next_terminal_event()=>{
                                    let event=event?;
                                    if matches!(event,Event::Key(key) if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code,KeyCode::Char('c'|'C'|'с'|'С'))) {
                                        break None;
                                    }
                                    // Bound pending UI input while serializing a server action.
                                    if deferred.len()>=64 || matches!(&event,Event::Paste(text) if text.len()>64*1024) {
                                        break Some(Err(anyhow::anyhow!("Pending UI input limit reached; action may be unconfirmed")));
                                    }
                                    deferred.push_back(event);
                                }
                            }
                        }
                    };
                    let Some(operation)=operation else {app.should_quit=true;continue;};
                    if let Err(error)=operation {app.status=Status::Error;app.push_kodrik_message(format!("Remote action not confirmed: {error}. Uncertain input: /retry checks its original ID."));}
                }
                Event::Paste(text)=>{super::images::paste(app, &text).await;},
                Event::Mouse(mouse)=>{
                    if let Some(text)=app.view.mouse(mouse,&mut app.scroll){let id=app.view.selection_id();copies.spawn(async move{(id,clipboard::copy(text).await)});}
                }
                Event::Resize(..)|Event::FocusLost=>{app.view.clear_selection();},
                _=>(),
            }
        }
    }
    Ok(())
}

fn report_disconnect(app: &mut App, result: anyhow::Result<()>) {
    let reason = match result {
        Ok(()) => "Connection ended without an error".into(),
        Err(error) => format!("{error:#}"),
    };
    let reason: String = reason.chars().flat_map(char::escape_debug).collect();
    app.push_kodrik_message(format!("Local environment disconnected: {reason}; use /attach to reconnect. Server chat remains available."));
}

#[cfg(test)]
mod binding_tests {
    use super::*;

    #[test]
    fn disconnect_reports_error_chain_without_terminal_controls() {
        let mut app = App::default();
        report_disconnect(
            &mut app,
            Err(anyhow::anyhow!("socket failed\u{1b}[31m").context("transport stopped")),
        );
        let text = &app.messages.last().unwrap().content;
        assert!(text.contains("transport stopped"), "{text}");
        assert!(text.contains("socket failed"), "{text}");
        assert!(!text.contains('\u{1b}'), "terminal control leaked");
        assert!(text.contains("/attach"));
    }

    #[test]
    fn session_pages_validate_rows_and_escape_titles() {
        use serde_json::json;
        let page = session_rows(json!([{"id":"one","title":"hello\n\u{1b}[31mworld"}])).unwrap();
        assert_eq!(page[0].0.as_str(), "one");
        assert!(!page[0].1.chars().any(char::is_control));
        for value in [
            json!({"sessions":[]}),
            json!([{"title":"missing ID"}]),
            json!([{"id":"","title":"empty ID"}]),
            json!([{"id":"bad\nID","title":"control"}]),
            json!([{"id":"one"}]),
            json!(vec![json!({"id":"one","title":"too many"}); 31]),
        ] {
            assert!(session_rows(value).is_err());
        }
    }

    #[tokio::test]
    async fn cancelled_close_keeps_cleanup_task_joinable() {
        let directory = tempfile::tempdir().unwrap();
        let config = RemoteConfig {
            server: "http://localhost/".parse().unwrap(),
            credential_file: directory.path().join("remote/device.json"),
            label: "test".into(),
            allow_shell: false,
            skill_roots: vec![],
            mcp: Default::default(),
            optional_mcp: vec![],
        };
        let environment = LocalEnvironment::open(&config, directory.path())
            .await
            .unwrap();
        let (stop, stopped) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let (started, closing) = oneshot::channel();
        let local = environment.clone();
        let task = tokio::spawn(async move {
            stopped.await?;
            let _ = started.send(());
            released.await?;
            local.close().await
        });
        let mut binding = Binding {
            stop: Some(stop),
            task,
            environment,
        };
        {
            let close = binding.close();
            tokio::pin!(close);
            tokio::select! {
                result = &mut close => panic!("cleanup completed before release: {result:?}"),
                result = closing => result.unwrap(),
            }
        }
        assert!(binding.stop.is_none());
        assert!(!binding.task.is_finished());
        release.send(()).unwrap();
        binding.close().await.unwrap();
    }
}

#[derive(Default)]
pub(super) struct Projection {
    session: Option<SessionId>,
    revision: Option<(i64, u64)>,
}
impl Projection {
    pub(super) fn resynchronize(
        &mut self,
        app: &mut App,
        snapshot: &RemoteChatSnapshot,
    ) -> anyhow::Result<bool> {
        if self
            .session
            .as_ref()
            .is_some_and(|id| id != &snapshot.session_id)
            || self
                .revision
                .is_some_and(|(history, _)| snapshot.revision < history)
        {
            return Ok(false);
        }
        self.revision = None;
        self.accept(app, snapshot)
    }
    pub(super) fn accept(
        &mut self,
        app: &mut App,
        snapshot: &RemoteChatSnapshot,
    ) -> anyhow::Result<bool> {
        if self
            .session
            .as_ref()
            .is_some_and(|id| id != &snapshot.session_id)
            || self.revision.is_some_and(|(history, environment)| {
                snapshot.revision < history
                    || (snapshot.revision == history
                        && snapshot.environment_revision <= environment)
            })
        {
            return Ok(false);
        }
        project(app, snapshot)?;
        self.session = Some(snapshot.session_id.clone());
        self.revision = Some((snapshot.revision, snapshot.environment_revision));
        Ok(true)
    }
}

pub(super) fn project(app: &mut App, snapshot: &RemoteChatSnapshot) -> anyhow::Result<()> {
    use crate::agent::{
        AssistantMessage, UserMessage,
        message::{ToolCall, ToolResult},
    };
    let mut session = Session::new(snapshot.session_id.clone());
    let mut versions = std::collections::HashMap::new();
    let mut user_turn = 0;
    for row in &snapshot.messages {
        if row["role"] == "user" {
            if let (Some(ids), Some(current)) =
                (row["versions"].as_array(), row["version"].as_u64())
            {
                versions.insert(
                    user_turn,
                    crate::agent::session::branches::MessageVersions {
                        ids: ids
                            .iter()
                            .filter_map(|id| id.as_str().map(str::to_owned))
                            .collect(),
                        current: current as usize,
                    },
                );
            }
            user_turn += 1;
            let content = row["parts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|part| match part["type"].as_str() {
                    Some("text") => Some(format!(
                        "{}{}",
                        part["text"].as_str().unwrap_or(""),
                        if part["truncated"] == true {
                            "\n[Full message remains in server history]"
                        } else {
                            ""
                        }
                    )),
                    Some("file") => Some(format!(
                        "[Attachment: {} — available in server history]",
                        part["filename"].as_str().unwrap_or("file")
                    )),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            session.push(Message::User(UserMessage {
                content,
                files: vec![],
                images: vec![],
            }));
            continue;
        }
        for part in row["parts"].as_array().into_iter().flatten() {
            match part["type"].as_str() {
                Some("text") => {
                    let mut text = part["text"].as_str().unwrap_or("").to_owned();
                    if part["truncated"] == true {
                        text.push_str("\n[Full message remains in server history]");
                    }
                    session.push(if row["role"] == "user" {
                        Message::User(UserMessage {
                            content: text,
                            files: vec![],
                            images: vec![],
                        })
                    } else {
                        Message::Assistant(AssistantMessage { content: text })
                    });
                }
                Some("dynamic-tool") => {
                    let mut name = part["toolName"].as_str().unwrap_or("tool").to_owned();
                    if let Some(origin) = part["origin"].as_object() {
                        let field = |key: &str| {
                            origin
                                .get(key)
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("")
                                .chars()
                                .flat_map(char::escape_debug)
                                .collect::<String>()
                        };
                        let key = &origin["environment"];
                        name = format!("{} · {}", field("tool_id"), field("label"));
                        if field("tool_id") == "read_file" && part["attachment_saved"] != true {
                            let connected = snapshot.environment.as_ref().is_some_and(|status| {
                                status.state == "connected"
                                    && status.environment.as_ref().is_some_and(|environment| {
                                        serde_json::to_value(environment).ok().as_ref() == Some(key)
                                    })
                            });
                            let text = if connected {
                                "[Local file is not saved on the server]"
                            } else {
                                "[The file remains on the disconnected device]"
                            };
                            session.push(Message::Assistant(AssistantMessage {
                                content: text.into(),
                            }));
                        }
                    }
                    let call_id = part["toolCallId"].as_str().unwrap_or("").to_owned();
                    session.push(Message::ToolCall(ToolCall {
                        call_id: call_id.clone(),
                        name,
                        arguments: part["input"].clone(),
                    }));
                    if matches!(
                        part["state"].as_str(),
                        Some("output-available" | "output-error")
                    ) {
                        let output = if part["state"] == "output-error" {
                            serde_json::json!({"error":part["errorText"]})
                        } else {
                            part["output"].clone()
                        };
                        session.push(Message::ToolResult(ToolResult {
                            call_id,
                            content: output.into(),
                        }));
                    }
                }
                Some("file") => {
                    let text = format!(
                        "[Attachment: {} — available in server history]",
                        part["filename"].as_str().unwrap_or("file")
                    );
                    session.push(Message::Assistant(AssistantMessage { content: text }));
                }
                _ => (),
            }
        }
    }
    if let Some(index) = app
        .streaming_message
        .take()
        .filter(|index| *index < app.messages.len())
    {
        app.messages.remove(index);
    }
    app.messages
        .retain(|row| row.role != AppMessageRole::Approval);
    refresh_history(app, &session);
    app.versions = versions;
    let run = snapshot.run.as_ref().unwrap_or(&serde_json::Value::Null);
    reconcile_waiting(
        app,
        run["queued"].as_array().map(Vec::as_slice).unwrap_or(&[]),
    );
    if let Some(error) = run["history_error"].as_str() {
        app.set_copy_notice(error.to_owned());
    }
    if run["clear_completed_plan"] == true && app.plan_completed {
        app.plan = None;
        app.plan_completed = false;
    }
    app.model = run["model"].as_str().unwrap_or("server agent").into();
    if app.remote_environment.is_none() {
        app.remote_environment = Some(String::new());
        app.remote_connection = Some("detached");
    }
    app.status = if run["busy"] == true {
        Status::Working
    } else if run["phase"] == "Completed" {
        Status::Done(String::new())
    } else if run["phase"].get("Failed").is_some() {
        Status::Error
    } else if run["phase"] == "Cancelled" {
        Status::Stopped
    } else {
        Status::Ready
    };
    app.task_started_at = serde_json::from_value(run["started_at"].clone()).ok();
    app.task_finished_at = serde_json::from_value(run["finished_at"].clone()).ok();
    app.compaction_progress = serde_json::from_value(run["compaction"]["progress"].clone()).ok();
    app.compacting = run["compaction"]["status"] == "preparing"
        && run["compaction"]["progress"]["finished_at"].is_null();
    if let Some(live) = run["live"]["current"]
        .as_object()
        .filter(|live| live.get("status").and_then(|v| v.as_str()) != Some("Committed"))
    {
        if let Some(text) = live
            .get("text")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            let index = app.messages.len();
            app.push_kodrik_message(text.into());
            app.streaming_message = Some(index);
        }
    }
    app.audit_mode = serde_json::from_value(run["command_audit_mode"].clone()).unwrap_or_default();
    let approval = run["command_approvals"]
        .as_array()
        .and_then(|approvals| approvals.first())
        .map(|approval| {
            serde_json::from_value::<crate::agent::command_audit::CommandApproval>(approval.clone())
        })
        .transpose()?;
    if let Some(approval) = &approval {
        approvals::show(app, &snapshot.session_id, approval);
        if app.approval.as_ref().map(|a| &a.id) != Some(&approval.id) {
            app.approval_choice = Some(0);
        }
    }
    app.approval = approval;
    if let Some(id) = run["approval_unavailable"]["id"].as_str() {
        let id = id.escape_debug().to_string();
        app.push_kodrik_message(format!("Approval {id} exceeds the terminal review limit. Allow is unavailable; review the complete call in Web Chat or reject with /deny {id}."));
    }
    app.audit_session = Some(snapshot.session_id.clone());
    if run["phase"].get("EnvironmentPaused").is_some() {
        app.push_kodrik_message(app.language.pick("Laptop disconnected. History is saved; send a new message to continue.","Связь с ноутбуком потеряна. История сохранена; для продолжения отправьте новое сообщение.").into());
    }
    if let Some(reason) = run["blocked_reason"].as_str() {
        app.push_kodrik_message(reason.into());
    }
    Ok(())
}
