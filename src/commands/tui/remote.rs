use super::directory_prompt::DirectoryPrompts;
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

#[derive(Debug)]
struct ReconnectAccessChanged;
impl std::fmt::Display for ReconnectAccessChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Local catalog or grant changed; use /attach to review access")
    }
}
impl std::error::Error for ReconnectAccessChanged {}

fn reconnect_terminal(error: &anyhow::Error) -> bool {
    if error.is::<ReconnectAccessChanged>() {
        return true;
    }
    matches!(error.downcast_ref::<tokio_tungstenite::tungstenite::Error>(),
        Some(tokio_tungstenite::tungstenite::Error::Http(response))
        if matches!(response.status().as_u16(), 401 | 403))
}

async fn prepare_reconnect(
    config: &RemoteConfig,
    root: &std::path::Path,
    original: &crate::remote::protocol::EnvironmentManifest,
) -> anyhow::Result<Prepared> {
    let prepared = Prepared(Some(LocalEnvironment::open(config, root).await?));
    let manifest = prepared.0.as_ref().unwrap().manifest()?;
    if serde_json::to_value(&manifest)? != serde_json::to_value(original)?
        || !config.manifest_authorized(&manifest)?
    {
        return Err(ReconnectAccessChanged.into());
    }
    Ok(prepared)
}

struct Binding {
    session: SessionId,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
    environment: Arc<LocalEnvironment>,
    directory_prompts: Option<DirectoryPrompts>,
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
            let key = client.attach(session.clone(), manifest).await?;
            let (stop, receiver) = oneshot::channel();
            let (ready, started) = oneshot::channel();
            let (directory_sender, directory_receiver) = mpsc::channel(4);
            environment.set_directory_requests(directory_sender);
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
                session,
                stop: Some(stop),
                task,
                environment: environment.clone(),
                directory_prompts: Some(DirectoryPrompts::new(directory_receiver)),
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
        self.directory_prompts.take();
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

struct Reconnect(tokio::task::JoinHandle<anyhow::Result<Binding>>);
impl Drop for Reconnect {
    fn drop(&mut self) {
        self.0.abort();
    }
}
fn reconnect(
    config: RemoteConfig,
    root: PathBuf,
    session: SessionId,
    manifest: crate::remote::protocol::EnvironmentManifest,
) -> Reconnect {
    Reconnect(tokio::spawn(async move {
        for attempt in 0..15 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            // Rebuild only the previously granted catalog; never prompt or grant in a retry.
            let result = async {
                let prepared = prepare_reconnect(&config, &root, &manifest).await?;
                Binding::open(&config, prepared, session.clone()).await
            }
            .await;
            match result {
                Ok(binding) => return Ok(binding),
                Err(error) if attempt == 14 || reconnect_terminal(&error) => return Err(error),
                Err(error) => log::debug!("Local reconnect failed: {error:#}"),
            }
        }
        unreachable!()
    }))
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
    reconnect: Option<Reconnect>,
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
    async fn receive_update(
        &mut self,
        app: &mut App,
        epoch: u64,
        update: Update,
    ) -> anyhow::Result<()> {
        if !current_observer(self.epoch, epoch) {
            return Ok(());
        }
        let (snapshot, resynchronized) = match update {
            Update::Snapshot(snapshot) => (snapshot, false),
            Update::Resynchronized(snapshot) => (snapshot, true),
            Update::Disconnected(reason) => {
                self.server_connected = false;
                app.set_copy_notice(app.language.pick(&format!(
                    "Server disconnected: {reason}; reconnecting (no local fallback)"
                ), &format!(
                    "Сервер отключён: {reason}; переподключение (без локального резервного режима)"
                )).to_owned());
                return Ok(());
            }
        };
        self.server_connected = true;
        let snapshot = with_scrollback(&self.snapshot, snapshot);
        let accepted = if resynchronized {
            self.projection.resynchronize(app, &snapshot)?
        } else {
            self.projection.accept(app, &snapshot)?
        };
        if !accepted {
            return Ok(());
        }
        self.snapshot = snapshot;
        // Carry only an existing attachment; observation alone must not grant access.
        for _ in 0..4 {
            let Some(id) = self
                .snapshot
                .run
                .as_ref()
                .and_then(|run| run["handoff_session_id"].as_str())
                .filter(|id| *id != self.session.as_str())
                .map(SessionId::new)
            else {
                break;
            };
            if let Some(binding) = self.binding.as_mut() {
                binding.directory_prompts.take();
            }
            let snapshot = self.chat.snapshot(&id).await?;
            let manifest = self
                .binding
                .as_ref()
                .map(|binding| binding.environment.manifest())
                .transpose()?;
            let detached = self.detach().await;
            app.remote_grant = None;
            self.observer.take();
            self.epoch += 1;
            self.session = id;
            self.projection = Projection::default();
            self.snapshot = snapshot;
            app.cancel_message_navigation();
            app.sessions = None;
            self.projection.accept(app, &self.snapshot)?;
            self.observer = Some(observe(
                self.chat.clone(),
                self.session.clone(),
                self.epoch,
                self.updates.clone(),
            ));
            if let Err(error) = detached {
                self.binding_failed = true;
                app.push_kodrik_message(app.language.pick(
                    &format!("Handoff cleanup failed: {error}; local access stopped. Review cleanup before /attach."),
                    &format!("Ошибка отключения при переходе: {error}; локальный доступ остановлен. Проверьте очистку перед /attach."),
                ).to_owned());
            } else if let Some(manifest) = manifest {
                self.binding_failed = true;
                let result = async {
                    let prepared = prepare_reconnect(&self.config, &self.root, &manifest).await?;
                    Binding::open(&self.config, prepared, self.session.clone()).await
                }
                .await;
                match result {
                    Ok(binding) => {
                        self.binding = Some(binding);
                        self.binding_failed = false;
                    }
                    Err(error) => app.push_kodrik_message(app.language.pick(
                        &format!("Handoff attachment failed: {error}; use /attach in this session and continue."),
                        &format!("Не удалось перенести подключение: {error}; используйте /attach в этом диалоге и продолжите задачу."),
                    ).to_owned()),
                }
            }
        }
        Ok(())
    }
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
            Err(error) => {
                app.sessions_error = Some(format!(
                    "{}: {error}",
                    app.language
                        .pick("Could not load sessions", "Не удалось загрузить диалоги")
                ))
            }
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
        self.reconnect.take();
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
            app.push_kodrik_message(
                app.language
                    .pick(
                        "Local environment attached to this session.",
                        "Локальное окружение подключено к этому диалогу.",
                    )
                    .into(),
            );
        } else {
            let literal = |text: &str| {
                text.chars()
                    .flat_map(char::escape_debug)
                    .collect::<String>()
            };
            let language = app.language;
            let details = format!(
                "{}\n{}: {}\n{}: {}\n{}: {} ({})\n{}: {}\n{}: {}\n{}: {}\n{}: {}\n{}\n\n{}",
                language.pick("Grant local environment access?", "Предоставить доступ к локальному окружению?"),
                language.pick("Server", "Сервер"), literal(self.config.server.as_str()),
                language.pick("Project", "Проект"), literal(&manifest.project_root),
                language.pick("Shell", "Оболочка"), language.pick(
                    if self.config.allow_shell { "enabled" } else { "disabled" },
                    if self.config.allow_shell { "включена" } else { "выключена" },
                ), language.pick("not sandboxed", "без песочницы"),
                language.pick("Selected HTTP MCP", "Выбранные HTTP MCP"), self.config.mcp.keys().map(|s| literal(s)).collect::<Vec<_>>().join(", "),
                language.pick("Shared tools", "Общие инструменты"), manifest.tools.iter().map(|tool| literal(&tool.id)).collect::<Vec<_>>().join(", "),
                language.pick("Skills", "Навыки"), manifest.skills.iter().map(|skill| literal(&skill.name)).collect::<Vec<_>>().join(", "),
                language.pick("Project instructions", "Инструкции проекта"), language.pick(
                    if manifest.project_instructions.is_some() { "present" } else { "none" },
                    if manifest.project_instructions.is_some() { "есть" } else { "нет" },
                ),
                manifest.warnings.iter().map(|warning| literal(warning)).collect::<Vec<_>>().join("\n"),
                language.pick(
                    "Tool arguments, file contents, skill instructions and command/MCP outputs may be sent to the server and its model. Shell and MCP can affect resources outside this project; cleanup is best effort, not rollback. Access follows the selected conversation and ends when this TUI closes.",
                    "Аргументы инструментов, содержимое файлов, инструкции навыков и вывод команд/MCP могут отправляться на сервер и его модели. Оболочка и MCP могут воздействовать на ресурсы вне этого проекта; очистка выполняется по возможности, это не откат изменений. Доступ следует за выбранным диалогом и прекращается при закрытии этого TUI.",
                ),
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
        let prepared = self.pending.take().context(app.language.pick(
            "No local access request pending",
            "Нет ожидающего запроса локального доступа",
        ))?;
        app.remote_grant = None;
        if !allow {
            app.remote_connection = Some("detached");
            app.push_kodrik_message(app.language.pick("Local access declined; server chat remains available. Use /attach to review again.", "Локальный доступ отклонён; чат на сервере доступен. Для повторного просмотра используйте /attach.").into());
            return Ok(());
        }
        self.binding_failed = true;
        let manifest = prepared.0.as_ref().unwrap().manifest()?;
        self.config.authorize_manifest(&manifest, || Ok(true))?;
        self.binding = Some(Binding::open(&self.config, prepared, self.session.clone()).await?);
        self.binding_failed = false;
        app.remote_connection = Some("connected");
        app.push_kodrik_message(
            app.language
                .pick(
                    "Local environment attached to this session.",
                    "Локальное окружение подключено к этому диалогу.",
                )
                .into(),
        );
        Ok(())
    }

    async fn older(&mut self, app: &mut App) -> anyhow::Result<()> {
        if let Some(cursor) = self.snapshot.previous_cursor.clone() {
            let mut older = self.chat.history(&self.session, &cursor).await?;
            ensure!(
                older.revision == self.snapshot.revision,
                app.language.pick("History changed; wait for a fresh snapshot before loading older messages", "История изменилась; дождитесь нового снимка перед загрузкой предыдущих сообщений")
            );
            older.messages.extend(self.snapshot.messages.clone());
            ensure!(
                older.messages.len() <= 4000
                    && serde_json::to_vec(&older)?.len() <= 16 * 1024 * 1024,
                app.language.pick(
                    "Scrollback limit reached; older history remains on server",
                    "Достигнут лимит прокрутки; предыдущая история остаётся на сервере"
                )
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
        self.reconnect.take();
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
        let id = SessionId::new(response["id"].as_str().context(app.language.pick(
            "Server returned no session ID",
            "Сервер не вернул ID диалога",
        ))?);
        let snapshot = self.chat.snapshot(&id).await?;
        self.observer.take();
        self.epoch += 1;
        self.session = id;
        self.projection = Projection::default();
        self.snapshot = snapshot;
        self.uncertain = None;
        app.cancel_message_navigation();
        app.sessions = None;
        app.clear_input_images();
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
            anyhow::bail!(app.language.pick(
                "Previous input is unconfirmed; use /retry to check the same request ID first",
                "Предыдущий ввод не подтверждён; сначала проверьте исходный ID запроса через /retry"
            ));
        }
        let editing = app.navigation.editing.as_ref();
        let is_edit = editing.is_some();
        let command = if let Some(edit) = editing {
            let turn = self
                .absolute_turn(edit.user_turn)
                .context(app.language.pick(
                    "User message is not loaded",
                    "Сообщение пользователя не загружено",
                ))?;
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
                app.clear_input_images();
                if is_edit {
                    let id = SessionId::new(response["id"].as_str().context(app.language.pick(
                        "Edit returned no branch",
                        "Сервер не вернул ветку после редактирования",
                    ))?);
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
            "/models" => {
                let catalogue = self
                    .chat
                    .command(&self.session, ChatCommand::Models)
                    .await?;
                let models: Vec<String> = serde_json::from_value(catalogue["models"].clone())?;
                let mut picker =
                    super::models::ModelPicker::new(models, app.model_override.clone());
                if let Some(names) = catalogue.get("model_names") {
                    picker.names = serde_json::from_value(names.clone())?;
                }
                app.model_picker = Some(picker);
                app.sessions = None;
                app.view.clear_selection();
            }
            input if input == "/model" || input.starts_with("/model ") => {
                let model = input
                    .strip_prefix("/model ")
                    .filter(|model| *model != "auto")
                    .map(str::to_owned);
                self.chat
                    .command(&self.session, ChatCommand::SetModel { model })
                    .await?;
                let snapshot = self.chat.snapshot(&self.session).await?;
                self.receive_update(app, self.epoch, Update::Snapshot(snapshot))
                    .await?;
            }
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
            "/audit status" => app.push_kodrik_message(
                app.language
                    .pick(
                        &format!("Current server tool mode: {}", app.audit_mode.code()),
                        &format!(
                            "Текущий режим инструментов на сервере: {}",
                            app.audit_mode.code()
                        ),
                    )
                    .to_owned(),
            ),
            "/sessions" => {
                self.load_sessions(app, true);
            }
            "/sessions retry" => self.load_sessions(app, false),
            "/new" => self.select(app, ChatCommand::New).await?,
            "/detach" => {
                self.detach().await?;
                app.remote_connection = Some("detached");
                app.push_kodrik_message(
                    app.language
                        .pick(
                            "Local access detached; server conversation remains available.",
                            "Локальный доступ отключён; диалог на сервере доступен.",
                        )
                        .into(),
                );
            }
            "/attach" | "/attach yes" => self.attach(app).await?,
            "/attach allow" => self.decide_grant(app, true).await?,
            "/attach deny" => self.decide_grant(app, false).await?,
            "/older" => self.older(app).await?,
            "/retry" => {
                let command = self.uncertain.clone().context(
                    app.language
                        .pick("No unconfirmed input", "Нет неподтверждённого ввода"),
                )?;
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
                                session_id: SessionId::new(response["id"].as_str().context(
                                    app.language.pick(
                                        "Edit returned no branch",
                                        "Сервер не вернул ветку после редактирования",
                                    ),
                                )?),
                            },
                        )
                        .await?;
                    }
                } else {
                    self.uncertain = None;
                    app.push_kodrik_message(
                        app.language
                            .pick(
                                &format!("Original input confirmed: {}", status["status"]),
                                &format!("Исходный ввод подтверждён: {}", status["status"]),
                            )
                            .to_owned(),
                    );
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
                app.clear_input_images();
                app.input_cursor = 0;
            }
            "/approvals" => {
                project(app, &self.chat.snapshot(&self.session).await?)?;
            }
            input if input.starts_with("/approve ") || input.starts_with("/deny ") => {
                let mut parts = input.split_whitespace();
                let allow = parts.next() == Some("/approve");
                let id = parts.next().context(
                    app.language
                        .pick("Approval ID required", "Требуется ID запроса разрешения"),
                )?;
                ensure!(
                    parts.next().is_none(),
                    app.language.pick(
                        "Unexpected approval arguments",
                        "Неожиданные аргументы запроса разрешения"
                    )
                );
                ensure!(
                    !allow
                        || app
                            .approval
                            .as_ref()
                            .is_some_and(|approval| approval.id == id),
                    app.language.pick(
                        "Review the complete pending approval with /approvals before allowing it",
                        "Перед разрешением просмотрите полный ожидающий запрос через /approvals"
                    )
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
                        app.language.pick(
                            "Approval is no longer displayed in this session",
                            "Запрос разрешения больше не отображается в этом диалоге"
                        )
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
                let mode = audit_mode(input).map_err(|_| {
                    anyhow::anyhow!(app.language.pick(
                        "Use /audit trusted|review|ask for the current server session",
                        "Используйте /audit trusted|review|ask для текущего диалога на сервере"
                    ))
                })?;
                self.chat
                    .command(
                        &self.session,
                        ChatCommand::SetAuditMode {
                            mode: mode.code().into(),
                        },
                    )
                    .await?;
            }
            input
                if matches!(
                    input.split_whitespace().next(),
                    Some(
                        "/models"
                            | "/model"
                            | "/audit"
                            | "/sessions"
                            | "/new"
                            | "/detach"
                            | "/attach"
                            | "/older"
                            | "/retry"
                            | "/approvals"
                            | "/approve"
                            | "/deny"
                    )
                ) =>
            {
                anyhow::bail!(
                    "{}: {}",
                    app.language.pick(
                        "Unknown remote command or arguments",
                        "Неизвестная удалённая команда или аргументы"
                    ),
                    input.split_whitespace().next().unwrap_or("/")
                )
            }
            _ => self.send(app, input).await?,
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) async fn run(config: &RemoteConfig) -> anyhow::Result<()> {
    run_with(config, None, None).await
}

pub(crate) async fn run_with(
    config: &RemoteConfig,
    config_path: Option<&Path>,
    resume: Option<&str>,
) -> anyhow::Result<()> {
    let language = load_language(
        &crate::config::data_dir()?.join("tui-language"),
        Language::from_environment(),
    )?;
    let config = &config.pin_credential()?;
    let chat = ChatClient::new(config)?;
    let session = if let Some(id) = resume {
        SessionId::new(id)
    } else {
        SessionId::new(
            chat.command(&SessionId::new("new"), ChatCommand::New)
                .await?["id"]
                .as_str()
                .context(language.pick(
                    "Server returned no session ID",
                    "Сервер не вернул ID диалога",
                ))?,
        )
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
        reconnect: None,
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
        language,
        ..Default::default()
    };
    remote.projection.accept(&mut app, &remote.snapshot)?;
    let mut tui = Tui::enter_remote()?;
    let mut reporter = herdr::Reporter::start_remote(config_path);
    let result = event_loop(&mut remote, &mut app, &mut tui, &mut rx, &mut reporter).await;
    remote.observer.take();
    let cleanup = remote.detach().await;
    let terminal = tui.exit();
    reporter.finish().await;
    result.and(cleanup).and(terminal)
}

async fn event_loop(
    remote: &mut Remote,
    app: &mut App,
    tui: &mut Tui,
    updates: &mut mpsc::Receiver<(u64, Update)>,
    reporter: &mut herdr::Reporter,
) -> anyhow::Result<()> {
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    let mut copies = JoinSet::new();
    let mut deferred = std::collections::VecDeque::new();
    let shutdown = crate::commands::shutdown_signal();
    tokio::pin!(shutdown);
    remote.connection_status(app);
    reporter.update(app);
    tui.draw(app)?;
    let language = app.language;
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
                    if deferred.len() >= 64 || matches!(&event, Event::Paste(text) if text.len()>64*1024) { break Some(Err(anyhow::anyhow!(language.pick("Pending UI input limit reached", "Достигнут лимит ожидающего ввода")))); }
                    deferred.push_back(event);
                }
            }
        }
    };
    match startup {
        None => return Ok(()),
        Some(Err(error)) => app.push_kodrik_message(app.language.pick(&format!(
            "Local attachment failed: {error}. Server chat remains available; use /attach to retry."
        ), &format!(
            "Не удалось подключить локальное окружение: {error}. Чат на сервере доступен; для повтора используйте /attach."
        )).to_owned()),
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
        reporter.update(app);
        if let Some(binding) = remote.binding.as_mut()
            && binding.session != remote.session
        {
            binding.directory_prompts.take();
        }
        let prompts = remote
            .binding
            .as_mut()
            .filter(|binding| !binding.task.is_finished())
            .and_then(|binding| binding.directory_prompts.as_mut());
        let prompt = if let Some(prompts) = prompts {
            if prompts.refresh(deferred.is_empty()) {
                // Do not replay terminal input queued before the permission review was visible.
                if !drain_before_directory_prompt(app)? {
                    prompts.active.take();
                }
                app.view.clear_selection();
            }
            prompts.active.as_ref()
        } else {
            None
        };
        if app.should_quit {
            break;
        }
        let directory_review = if let Some(prompt) = prompt {
            Some(tui.draw_directory_prompt(prompt, app.language)?)
        } else {
            tui.draw(app)?;
            None
        };
        let language = app.language;
        tokio::select! {
            _=&mut shutdown=>app.should_quit=true,
            result=async {match remote.session_list.as_mut() {
                Some(request)=>(&mut request.task).await,
                None=>std::future::pending().await,
            }}=>remote.receive_sessions(app,result),
            result=async {match remote.reconnect.as_mut() {
                Some(retry)=>(&mut retry.0).await,
                None=>std::future::pending().await,
            }}=>{
                remote.reconnect.take();
                match result.map_err(anyhow::Error::from).and_then(|result|result) {
                    Ok(binding)=>{
                        remote.binding=Some(binding);
                        remote.binding_failed=false;
                        app.push_kodrik_message(app.language.pick("Local environment reconnected with a fresh generation; interrupted work was not replayed or resumed.", "Локальное окружение переподключено с новым поколением; прерванная работа не повторялась и не возобновлялась.").into());
                    },
                    Err(error)=>app.push_kodrik_message(app.language.pick(&format!("Automatic local reconnect stopped: {error}; use /attach to review access."), &format!("Автоматическое переподключение локального окружения остановлено: {error}; проверьте доступ через /attach.")).to_owned()),
                }
            },
            _=tick.tick()=>{
                app.expire_copy_notice(std::time::Instant::now());
                app.animation_frame=app.animation_frame.wrapping_add(1);
                if remote.binding.as_ref().is_some_and(|binding|binding.task.is_finished()){
                    let manifest=remote.binding.as_ref().unwrap().environment.manifest()?;
                    let session=remote.binding.as_ref().unwrap().session.clone();
                    let result=remote.detach().await;
                    remote.binding_failed=true;
                    report_disconnect(app,result);
                    remote.reconnect=Some(reconnect(remote.config.clone(),remote.root.clone(),session,manifest));
                }
            },
            Some((epoch,update))=updates.recv()=>{
                remote.receive_update(app,epoch,update).await?;
            },
            Some(copy)=copies.join_next(),if !copies.is_empty()=>{
                if let Ok((id,result))=copy {complete_copy(app,id,result,|sequence|tui.copy_to_terminal(sequence));}
            },
            event=async {match deferred.pop_front(){Some(event)=>Ok(event),None=>next_terminal_event().await}}=>{
                let event = event?;
                if let Some(can_allow) = directory_review {
                    if is_exit_key(&event) {
                        app.should_quit = true;
                    } else if let Some(prompts) = remote.binding.as_mut()
                        .filter(|binding| !binding.task.is_finished())
                        .and_then(|binding| binding.directory_prompts.as_mut())
                    {
                        prompts.input(&event, can_allow);
                    }
                    continue;
                }
                match event {
                Event::Key(key) if matches!(key.kind,KeyEventKind::Press|KeyEventKind::Repeat)=>{
                    if app.tool_details_key(key) { continue; }
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
                        if key.code==KeyCode::Esc && app.remote_grant.is_none() && app.sessions.is_none() && app.model_picker.is_none() && app.audit_picker.is_none() && app.language_choice.is_none() && app.approval.is_none() && app.navigation.selected.is_none() && app.navigation.editing.is_none() && app.command_suggestion().is_none() && matches!(app.status,Status::Working) {
                            remote.chat.command(&remote.session,ChatCommand::Stop).await?;app.status=Status::Stopping;return Ok(());
                        }
                        let turn=app.selected_user_turn();
                        let granting=app.remote_grant.is_some();
                        if let Some(input)=app.update(key){remote.input(app,input).await?;}
                        if !granting && key.code==KeyCode::PageUp && app.navigation.selected.is_none() {remote.older(app).await?;}
                        if let Some(id)=app.navigation.pending_version.take() && let Some(turn)=turn {
                            let user_turn=remote.absolute_turn(turn).context(app.language.pick("User message is not loaded", "Сообщение пользователя не загружено"))?;
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
                                        break Some(Err(anyhow::anyhow!(language.pick("Pending UI input limit reached; action may be unconfirmed", "Достигнут лимит ожидающего ввода; действие может быть не подтверждено"))));
                                    }
                                    deferred.push_back(event);
                                }
                            }
                        }
                    };
                    let Some(operation)=operation else {app.should_quit=true;continue;};
                    if let Err(error)=operation {app.status=Status::Error;let message=app.language.pick(&format!("Remote action not confirmed: {error}. Uncertain input: /retry checks its original ID."), &format!("Удалённое действие не подтверждено: {error}. Для неподтверждённого ввода /retry проверяет исходный ID запроса.")).to_owned();if app.navigation.editing.is_some() {app.set_copy_notice(app.language.pick("Remote action not confirmed", "Удалённое действие не подтверждено").into());}app.push_kodrik_message(message);}
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
    }
    Ok(())
}

fn is_exit_key(event: &Event) -> bool {
    matches!(event, Event::Key(key) if key.kind == KeyEventKind::Press && key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c'|'C'|'с'|'С')))
}

fn drain_before_directory_prompt(app: &mut App) -> anyhow::Result<bool> {
    for _ in 0..64 {
        if !event::poll(Duration::ZERO)? {
            return Ok(true);
        }
        if is_exit_key(&event::read()?) {
            app.should_quit = true;
            return Ok(false);
        }
    }
    // If the backlog keeps growing, fail closed rather than accept presubmitted keys.
    Ok(!event::poll(Duration::ZERO)?)
}

fn report_disconnect(app: &mut App, result: anyhow::Result<()>) {
    let reason = match result {
        Ok(()) => app
            .language
            .pick(
                "Connection ended without an error",
                "Соединение завершено без ошибки",
            )
            .into(),
        Err(error) => format!("{error:#}"),
    };
    let reason: String = reason.chars().flat_map(char::escape_debug).collect();
    app.push_kodrik_message(app.language.pick(&format!("Local environment disconnected: {reason}; reconnecting if the original grant remains valid (or use /attach). Server chat remains available."), &format!("Локальное окружение отключено: {reason}; переподключение, если исходное разрешение ещё действует (или используйте /attach). Чат на сервере доступен.")).to_owned());
}

#[cfg(test)]
mod binding_tests {
    use super::*;

    #[tokio::test]
    async fn handoff_update_follows_target_and_fences_source_without_granting_access() {
        use axum::{Json, Router, routing::get};
        use serde_json::json;
        let directory = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = RemoteConfig {
            credential: None,
            server: format!("http://{}/", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
            credential_file: directory.path().join("private/device"),
            label: "test".into(),
            allow_shell: false,
            skill_roots: vec![],
            mcp: Default::default(),
            optional_mcp: vec![],
        };
        crate::remote::client::Credential {
            device_id: uuid::Uuid::new_v4(),
            token: "test".into(),
        }
        .save(&config.credential_file)
        .unwrap();
        let target: RemoteChatSnapshot = serde_json::from_value(json!({
            "session_id":"target", "revision":1,"environment_revision":0,"messages":[],
            "previous_cursor":null,"versions":null,"environment":null,
            "run":{"busy":true,"live":{"current":{"status":"Active","text":"target working"}}}
        }))
        .unwrap();
        let server = tokio::spawn(
            axum::serve(
                listener,
                Router::new().route(
                    "/api/remote/chat/sessions/target",
                    get(move || {
                        let target = target.clone();
                        async move { Json(target) }
                    }),
                ),
            )
            .into_future(),
        );
        let source: RemoteChatSnapshot = serde_json::from_value(json!({
            "session_id":"source","revision":9,"environment_revision":0,"messages":[],
            "previous_cursor":null,"versions":null,"environment":null,
            "run":{"busy":false,"phase":"Completed","handoff_session_id":"target"}
        }))
        .unwrap();
        let (tx, _rx) = mpsc::channel(4);
        let mut remote = Remote {
            config: config.clone(),
            chat: ChatClient::new(&config).unwrap(),
            session: SessionId::new("source"),
            binding: None,
            reconnect: None,
            pending: None,
            server_connected: true,
            binding_failed: false,
            observer: None,
            updates: tx,
            epoch: 3,
            projection: Projection::default(),
            snapshot: source.clone(),
            uncertain: None,
            session_list: None,
            sessions_offset: 0,
            root: directory.path().into(),
        };
        let mut app = App::default();
        remote
            .receive_update(&mut app, 3, Update::Snapshot(source.clone()))
            .await
            .unwrap();
        assert_eq!(remote.session.as_str(), "target");
        assert_eq!(remote.epoch, 4);
        assert!(matches!(app.status, Status::Working));
        assert!(
            app.messages
                .iter()
                .any(|row| row.content == "target working")
        );
        assert!(remote.binding.is_none() && remote.pending.is_none());
        remote
            .receive_update(&mut app, 3, Update::Resynchronized(source))
            .await
            .unwrap();
        remote
            .receive_update(&mut app, 3, Update::Disconnected("old stream".into()))
            .await
            .unwrap();
        assert_eq!(remote.session.as_str(), "target");
        assert!(remote.server_connected);
        assert!(matches!(app.status, Status::Working));
        server.abort();
    }

    #[tokio::test]
    async fn handoff_update_reconnects_existing_device_to_target() {
        handoff_device_case(false).await;
    }

    #[tokio::test]
    async fn handoff_attachment_failure_keeps_target_chat_available() {
        handoff_device_case(true).await;
    }

    async fn handoff_device_case(fail_attachment: bool) {
        use axum::{Json, Router, routing::get};
        use serde_json::json;
        let directory = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = RemoteConfig {
            credential: None,
            server: format!("http://{}/", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
            credential_file: directory.path().join("private/device"),
            label: "test".into(),
            allow_shell: false,
            skill_roots: vec![],
            mcp: Default::default(),
            optional_mcp: vec![],
        };
        crate::remote::client::Credential {
            device_id: uuid::Uuid::new_v4(),
            token: "test".into(),
        }
        .save(&config.credential_file)
        .unwrap();
        let target: RemoteChatSnapshot = serde_json::from_value(json!({
            "session_id":"target", "revision":1,"environment_revision":0,"messages":[],
            "previous_cursor":null,"versions":null,"environment":null,
            "run":{"busy":true,"live":{"current":{"status":"Active","text":"target working"}}}
        }))
        .unwrap();
        let (attached, mut attachments) = mpsc::unbounded_channel();
        let server = tokio::spawn(
            axum::serve(
                listener,
                Router::new()
                    .route(
                        "/api/remote/environment",
                        get(move |upgrade: axum::extract::WebSocketUpgrade| {
                            let attached = attached.clone();
                            async move {
                                upgrade.on_upgrade(move |mut socket| async move {
                                    use crate::remote::protocol::*;
                                    let frame = socket.recv().await.unwrap().unwrap();
                                    let ClientFrame::Hello { session_id, .. } =
                                        decode_frame(&frame.into_data()).unwrap()
                                    else {
                                        panic!()
                                    };
                                    attached.send(session_id).unwrap();
                                    socket
                                        .send(axum::extract::ws::Message::Binary(
                                            encode_frame(&ServerFrame::Welcome {
                                                environment: EnvironmentKey {
                                                    connection_id: uuid::Uuid::new_v4(),
                                                    generation: uuid::Uuid::new_v4(),
                                                },
                                                heartbeat_seconds: 5,
                                                lease_seconds: 15,
                                            })
                                            .unwrap()
                                            .into(),
                                        ))
                                        .await
                                        .unwrap();
                                    while let Some(Ok(frame)) = socket.recv().await {
                                        if matches!(
                                            decode_frame::<ClientFrame>(&frame.into_data()),
                                            Ok(ClientFrame::Close { .. })
                                        ) {
                                            break;
                                        }
                                    }
                                })
                            }
                        }),
                    )
                    .route(
                        "/api/remote/chat/sessions/target",
                        get(move || {
                            let target = target.clone();
                            async move { Json(target) }
                        }),
                    ),
            )
            .into_future(),
        );
        let source: RemoteChatSnapshot = serde_json::from_value(json!({
            "session_id":"source","revision":9,"environment_revision":0,"messages":[],
            "previous_cursor":null,"versions":null,"environment":null,
            "run":{"busy":false,"phase":"Completed","handoff_session_id":"target"}
        }))
        .unwrap();
        let environment = LocalEnvironment::open(&config, directory.path())
            .await
            .unwrap();
        config
            .authorize_manifest(&environment.manifest().unwrap(), || Ok(true))
            .unwrap();
        let binding = Binding::open(
            &config,
            Prepared(Some(environment)),
            SessionId::new("source"),
        )
        .await
        .unwrap();
        assert_eq!(attachments.recv().await.unwrap().as_str(), "source");
        let (tx, _rx) = mpsc::channel(4);
        let mut remote = Remote {
            config: config.clone(),
            chat: ChatClient::new(&config).unwrap(),
            session: SessionId::new("source"),
            binding: Some(binding),
            reconnect: None,
            pending: None,
            server_connected: true,
            binding_failed: false,
            observer: None,
            updates: tx,
            epoch: 3,
            projection: Projection::default(),
            snapshot: source.clone(),
            uncertain: None,
            session_list: None,
            sessions_offset: 0,
            root: directory.path().into(),
        };
        if fail_attachment {
            remote.root = directory.path().join("missing-project");
        }
        let mut app = App::default();
        remote
            .receive_update(&mut app, 3, Update::Snapshot(source.clone()))
            .await
            .unwrap();
        assert_eq!(remote.session.as_str(), "target");
        assert_eq!(remote.epoch, 4);
        assert!(matches!(app.status, Status::Working));
        assert!(
            app.messages
                .iter()
                .any(|row| row.content == "target working")
        );
        if fail_attachment {
            assert!(remote.binding.is_none());
            assert!(remote.binding_failed);
            assert!(
                app.messages
                    .iter()
                    .any(|row| row.content.contains("/attach"))
            );
        } else {
            assert_eq!(remote.binding.as_ref().unwrap().session.as_str(), "target");
            assert_eq!(attachments.recv().await.unwrap().as_str(), "target");
        }
        assert!(remote.pending.is_none());
        remote
            .receive_update(&mut app, 3, Update::Resynchronized(source))
            .await
            .unwrap();
        remote
            .receive_update(&mut app, 3, Update::Disconnected("old stream".into()))
            .await
            .unwrap();
        assert_eq!(remote.session.as_str(), "target");
        assert!(remote.server_connected);
        assert!(matches!(app.status, Status::Working));
        remote.detach().await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn reconnect_opens_fresh_binding_and_drop_cancels_pending_retry() {
        use crate::remote::protocol::*;
        use futures::{SinkExt, StreamExt};
        let directory = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = RemoteConfig {
            credential: None,
            server: format!("http://{}/", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
            credential_file: directory.path().join("private/device"),
            label: "test".into(),
            allow_shell: false,
            skill_roots: vec![],
            mcp: Default::default(),
            optional_mcp: vec![],
        };
        crate::remote::client::Credential {
            device_id: uuid::Uuid::new_v4(),
            token: "fixture".into(),
        }
        .save(&config.credential_file)
        .unwrap();
        let environment = LocalEnvironment::open(&config, directory.path())
            .await
            .unwrap();
        let manifest = environment.manifest().unwrap();
        config.authorize_manifest(&manifest, || Ok(true)).unwrap();
        environment.close().await.unwrap();
        let cancelled = reconnect(
            config.clone(),
            directory.path().into(),
            SessionId::new("same"),
            manifest.clone(),
        );
        drop(cancelled);
        assert!(
            tokio::time::timeout(Duration::from_millis(2200), listener.accept())
                .await
                .is_err()
        );
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let hello = socket.next().await.unwrap().unwrap().into_data();
            assert!(
                matches!(decode_frame::<ClientFrame>(&hello).unwrap(), ClientFrame::Hello { session_id, .. } if session_id.as_str()=="same")
            );
            let key = EnvironmentKey {
                connection_id: uuid::Uuid::new_v4(),
                generation: uuid::Uuid::new_v4(),
            };
            socket
                .send(tokio_tungstenite::tungstenite::Message::Binary(
                    encode_frame(&ServerFrame::Welcome {
                        environment: key,
                        heartbeat_seconds: 5,
                        lease_seconds: 15,
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            while let Some(Ok(frame)) = socket.next().await {
                if matches!(
                    decode_frame::<ClientFrame>(&frame.into_data()),
                    Ok(ClientFrame::Close { .. })
                ) {
                    break;
                }
            }
        });
        let mut retry = reconnect(
            config,
            directory.path().into(),
            SessionId::new("same"),
            manifest,
        );
        let mut binding = tokio::time::timeout(Duration::from_secs(8), &mut retry.0)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        binding.close().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn reconnect_rejects_catalog_changes_without_granting_access() {
        let directory = tempfile::tempdir().unwrap();
        let config = RemoteConfig {
            credential: None,
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
        let original = environment.manifest().unwrap();
        config.authorize_manifest(&original, || Ok(true)).unwrap();
        environment.close().await.unwrap();
        std::fs::write(directory.path().join("AGENTS.md"), "changed instructions").unwrap();
        let result = prepare_reconnect(&config, directory.path(), &original).await;
        assert!(result.is_err());
        let changed = LocalEnvironment::open(&config, directory.path())
            .await
            .unwrap();
        assert!(
            !config
                .manifest_authorized(&changed.manifest().unwrap())
                .unwrap()
        );
        changed.close().await.unwrap();
    }

    #[tokio::test]
    async fn bilingual_remote_commands_and_consent_render_without_granting_access() {
        use ratatui::{Terminal, backend::TestBackend};
        for language in [Language::En, Language::Ru] {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(directory.path().join("AGENTS.md"), "project policy").unwrap();
            let config = RemoteConfig {
                credential: None,
                server: "http://localhost/".parse().unwrap(),
                credential_file: directory.path().join("private/device"),
                label: "test".into(),
                allow_shell: false,
                skill_roots: vec![],
                mcp: Default::default(),
                optional_mcp: vec![],
            };
            crate::remote::client::Credential {
                device_id: uuid::Uuid::new_v4(),
                token: "test".into(),
            }
            .save(&config.credential_file)
            .unwrap();
            let snapshot: RemoteChatSnapshot = serde_json::from_value(serde_json::json!({
                "session_id":"localized", "revision":1, "environment_revision":0,
                "messages":[], "previous_cursor":null, "versions":null, "environment":null, "run":null
            })).unwrap();
            let (tx, _rx) = mpsc::channel(4);
            let mut remote = Remote {
                config: config.clone(),
                chat: ChatClient::new(&config).unwrap(),
                session: SessionId::new("localized"),
                binding: None,
                reconnect: None,
                pending: None,
                server_connected: true,
                binding_failed: false,
                observer: None,
                updates: tx,
                epoch: 0,
                projection: Projection::default(),
                snapshot,
                uncertain: None,
                session_list: None,
                sessions_offset: 0,
                root: directory.path().into(),
            };
            let mut app = App {
                language,
                ..Default::default()
            };
            for (command, en, ru) in [
                (
                    "/retry",
                    "No unconfirmed input",
                    "Нет неподтверждённого ввода",
                ),
                (
                    "/attach deny",
                    "No local access request pending",
                    "Нет ожидающего запроса локального доступа",
                ),
                ("/audit invalid", "Use /audit", "Используйте /audit"),
                (
                    "/retry extra",
                    "Unknown remote command",
                    "Неизвестная удалённая команда",
                ),
                (
                    "/approve",
                    "Unknown remote command",
                    "Неизвестная удалённая команда",
                ),
                (
                    "/approve id extra",
                    "Unexpected approval arguments",
                    "Неожиданные аргументы запроса разрешения",
                ),
                (
                    "/approve id",
                    "Review the complete",
                    "Перед разрешением просмотрите",
                ),
                (
                    "/audit trusted wrong id",
                    "Approval is no longer",
                    "Запрос разрешения больше не",
                ),
            ] {
                let error = remote
                    .input(&mut app, command.into())
                    .await
                    .unwrap_err()
                    .to_string();
                assert!(error.contains(language.pick(en, ru)), "{command}: {error}");
            }
            remote
                .input(&mut app, "/audit status".into())
                .await
                .unwrap();
            assert!(app.messages.last().unwrap().content.contains(language.pick(
                "Current server tool mode",
                "Текущий режим инструментов на сервере"
            )));
            remote.attach(&mut app).await.unwrap();
            assert!(remote.binding.is_none());
            let details = &app.remote_grant.as_ref().unwrap().details;
            for (en, ru) in [
                (
                    "Grant local environment access?",
                    "Предоставить доступ к локальному окружению?",
                ),
                ("Server:", "Сервер:"),
                ("Project:", "Проект:"),
                (
                    "Shell: disabled (not sandboxed)",
                    "Оболочка: выключена (без песочницы)",
                ),
                ("Selected HTTP MCP:", "Выбранные HTTP MCP:"),
                ("Shared tools:", "Общие инструменты:"),
                ("Skills:", "Навыки:"),
                ("Project instructions: present", "Инструкции проекта: есть"),
                (
                    "Tool arguments, file contents",
                    "Аргументы инструментов, содержимое файлов",
                ),
                ("outside this project", "вне этого проекта"),
                ("not rollback", "не откат изменений"),
                (
                    "ends when this TUI closes",
                    "прекращается при закрытии этого TUI",
                ),
            ] {
                assert!(details.contains(language.pick(en, ru)), "{details}");
            }
            let mut terminal = Terminal::new(TestBackend::new(180, 60)).unwrap();
            terminal.draw(|frame| ui::render(&mut app, frame)).unwrap();
            let screen = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(
                screen.contains(language.pick(
                    "Grant local environment access?",
                    "Предоставить доступ к локальному окружению?"
                )),
                "{screen}"
            );
            remote.decide_grant(&mut app, false).await.unwrap();
            assert!(remote.pending.is_none() && remote.binding.is_none());
            assert!(
                app.messages
                    .last()
                    .unwrap()
                    .content
                    .contains(language.pick("Local access declined", "Локальный доступ отклонён"))
            );
            remote.input(&mut app, "/detach".into()).await.unwrap();
            assert!(
                app.messages
                    .last()
                    .unwrap()
                    .content
                    .contains(language.pick("Local access detached", "Локальный доступ отключён"))
            );
            remote
                .receive_update(
                    &mut app,
                    0,
                    Update::Disconnected("raw server reason".into()),
                )
                .await
                .unwrap();
            let notice = &app.copy_notice.as_ref().unwrap().0;
            assert!(notice.contains(language.pick("Server disconnected:", "Сервер отключён:")));
            assert!(notice.contains("raw server reason"));
        }
    }

    #[test]
    fn projected_user_photos_do_not_duplicate_inline_image_markers() {
        for language in [Language::En, Language::Ru] {
            let mut app = App {
                language,
                ..Default::default()
            };
            let snapshot: RemoteChatSnapshot = serde_json::from_value(serde_json::json!({
                "session_id":"photos", "revision":1, "environment_revision":0,
                "messages":[
                    {"role":"user","parts":[
                        {"type":"text","text":"Compare [Image 3] and [Image 4]"},
                        {"type":"file","filename":"report.pdf"},
                        {"type":"file","filename":"Фото 1","genericLabel":"photo","genericIndex":1},
                        {"type":"file","filename":"Фото 2","genericLabel":"photo","genericIndex":2}
                    ]},
                    {"role":"user","parts":[
                        {"type":"text","text":""},
                        {"type":"file","filename":"Фото 1","genericLabel":"photo","genericIndex":1}
                    ]}
                ],"previous_cursor":null,"versions":null,"environment":null,"run":null
            }))
            .unwrap();
            project(&mut app, &snapshot).unwrap();
            assert_eq!(
                app.messages[0].content,
                language.pick(
                    "Compare [Image 3] and [Image 4]\n[Attachment: report.pdf — available in server history]",
                    "Compare [Image 3] and [Image 4]\n[Вложение: report.pdf — доступно в истории на сервере]",
                )
            );
            assert!(
                app.messages[1]
                    .content
                    .contains(language.pick("Photo 1", "Фото 1"))
            );
        }
    }

    #[test]
    fn projected_generic_attachment_labels_follow_language_and_keep_real_filenames() {
        for language in [Language::En, Language::Ru] {
            for role in ["user", "assistant"] {
                let mut app = App {
                    language,
                    ..Default::default()
                };
                let snapshot: RemoteChatSnapshot = serde_json::from_value(serde_json::json!({
                    "session_id":"labels", "revision":1, "environment_revision":0,
                    "messages":[{"role":role,"parts":[
                        {"type":"file","filename":"Фото 1","genericLabel":"photo","genericIndex":1},
                        {"type":"file","filename":"Tool image 2","genericLabel":"tool-image","genericIndex":2},
                        {"type":"file","filename":"Фото 3.png"},
                        {"type":"file","filename":"zero.png","genericLabel":"photo","genericIndex":0},
                        {"type":"file","filename":"fraction.png","genericLabel":"photo","genericIndex":1.5},
                        {"type":"file","filename":"unknown.png","genericLabel":"unknown","genericIndex":1}
                    ]}], "previous_cursor":null,"versions":null,"environment":null,"run":null
                })).unwrap();
                project(&mut app, &snapshot).unwrap();
                let text = app
                    .messages
                    .iter()
                    .map(|row| row.content.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                for expected in [
                    language.pick("Attachment: Photo 1 —", "Вложение: Фото 1 —"),
                    language.pick(
                        "Attachment: Tool image 2 —",
                        "Вложение: Изображение инструмента 2 —",
                    ),
                    "Фото 3.png",
                    "zero.png",
                    "fraction.png",
                    "unknown.png",
                ] {
                    assert!(
                        text.contains(expected),
                        "{role}: missing {expected}: {text}"
                    );
                }
            }
        }
    }

    #[test]
    fn bilingual_remote_disconnect_and_history_wrappers() {
        for (language, disconnected, full, attachment, approval) in [
            (
                Language::En,
                "Local environment disconnected:",
                "Full message remains in server history",
                "Attachment:",
                "exceeds the terminal review limit",
            ),
            (
                Language::Ru,
                "Локальное окружение отключено:",
                "Полное сообщение остаётся в истории на сервере",
                "Вложение:",
                "превышает лимит просмотра в терминале",
            ),
        ] {
            let mut app = App {
                language,
                ..Default::default()
            };
            report_disconnect(&mut app, Err(anyhow::anyhow!("raw diagnostic")));
            assert!(
                app.messages
                    .last()
                    .unwrap()
                    .content
                    .starts_with(disconnected)
            );
            assert!(
                app.messages
                    .last()
                    .unwrap()
                    .content
                    .contains("raw diagnostic")
            );
            let snapshot: RemoteChatSnapshot = serde_json::from_value(serde_json::json!({
                "session_id":"localized", "revision":1, "environment_revision":0,
                "messages":[{"role":"user","parts":[{"type":"text","text":"literal [Image 1]","truncated":true},{"type":"file","filename":"photo.png"}]}],
                "previous_cursor":null,"versions":null,"environment":null,
                "run":{"approval_unavailable":{"id":"approval-id"}}
            })).unwrap();
            project(&mut app, &snapshot).unwrap();
            let text = app
                .messages
                .iter()
                .map(|row| row.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            for expected in [
                full,
                attachment,
                approval,
                "literal [Image 1]",
                "photo.png",
                "/deny approval-id",
            ] {
                assert!(text.contains(expected), "missing {expected}: {text}");
            }
        }
    }

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
            credential: None,
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
            session: SessionId::new("same"),
            stop: Some(stop),
            task,
            environment,
            directory_prompts: None,
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

fn attachment_label(part: &serde_json::Value, language: Language) -> String {
    if let Some(index) = part["genericIndex"].as_u64().filter(|index| *index > 0) {
        match part["genericLabel"].as_str() {
            Some("photo") => return format!("{} {index}", language.pick("Photo", "Фото")),
            Some("tool-image") => {
                return format!(
                    "{} {index}",
                    language.pick("Tool image", "Изображение инструмента")
                );
            }
            _ => {}
        }
    }
    part["filename"]
        .as_str()
        .unwrap_or(language.pick("file", "файл"))
        .to_owned()
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
            let parts = row["parts"].as_array().map(Vec::as_slice).unwrap_or(&[]);
            let inline_images: usize = parts
                .iter()
                .filter(|part| part["type"] == "text")
                .filter_map(|part| part["text"].as_str())
                .map(inline_image_count)
                .sum();
            let is_photo = |part: &serde_json::Value| {
                part["type"] == "file"
                    && part["genericLabel"] == "photo"
                    && part["genericIndex"].as_u64().is_some_and(|index| index > 0)
            };
            let photos_have_markers = inline_images > 0
                && inline_images >= parts.iter().filter(|part| is_photo(part)).count();
            let content = parts
                .iter()
                .filter_map(|part| match part["type"].as_str() {
                    Some("text") => Some(format!(
                        "{}{}",
                        part["text"].as_str().unwrap_or(""),
                        if part["truncated"] == true {
                            app.language.pick(
                                "\n[Full message remains in server history]",
                                "\n[Полное сообщение остаётся в истории на сервере]",
                            )
                        } else {
                            ""
                        }
                    )),
                    Some("file") if !(photos_have_markers && is_photo(part)) => Some(
                        app.language
                            .pick(
                                &format!(
                                    "[Attachment: {} — available in server history]",
                                    attachment_label(part, app.language)
                                ),
                                &format!(
                                    "[Вложение: {} — доступно в истории на сервере]",
                                    attachment_label(part, app.language)
                                ),
                            )
                            .to_owned(),
                    ),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            session.push(Message::User(UserMessage {
                browser_client_id: None,
                browser_context: None,
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
                        text.push_str(app.language.pick(
                            "\n[Full message remains in server history]",
                            "\n[Полное сообщение остаётся в истории на сервере]",
                        ));
                    }
                    session.push(if row["role"] == "user" {
                        Message::User(UserMessage {
                            browser_client_id: None,
                            browser_context: None,
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
                                app.language.pick(
                                    "[Local file is not saved on the server]",
                                    "[Локальный файл не сохранён на сервере]",
                                )
                            } else {
                                app.language.pick(
                                    "[The file remains on the disconnected device]",
                                    "[Файл остаётся на отключённом устройстве]",
                                )
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
                    let text = app
                        .language
                        .pick(
                            &format!(
                                "[Attachment: {} — available in server history]",
                                attachment_label(part, app.language)
                            ),
                            &format!(
                                "[Вложение: {} — доступно в истории на сервере]",
                                attachment_label(part, app.language)
                            ),
                        )
                        .to_owned();
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
        app.remove_full_tool(index);
    }
    app.messages
        .retain(|row| row.role != AppMessageRole::Approval);
    app.full_tools.clear();
    refresh_history(app, &session);
    app.versions = versions;
    let run = snapshot.run.as_ref().unwrap_or(&serde_json::Value::Null);
    reconcile_waiting(
        app,
        run["queued"].as_array().map(Vec::as_slice).unwrap_or(&[]),
    );
    if let Some(error) = run["history_error"].as_str() {
        app.set_copy_notice(format!(
            "{}: {error}",
            app.language.pick("History", "История")
        ));
    }
    if run["clear_completed_plan"] == true && app.plan_completed {
        app.plan = None;
        app.plan_completed = false;
    }
    app.model = run["model"]
        .as_str()
        .unwrap_or(app.language.pick("server agent", "агент на сервере"))
        .into();
    app.model_override = run["model_override"].as_str().map(str::to_owned);
    app.context_tokens = run["context_tokens"].as_u64();
    let cost = &run["session_cost"];
    app.session_cost = cost["total_usd"].as_f64().zip(cost["partial"].as_bool());
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
        app.push_kodrik_message(app.language.pick(&format!("Approval {id} exceeds the terminal review limit. Allow is unavailable; review the complete call in Web Chat or reject with /deny {id}."), &format!("Запрос {id} превышает лимит просмотра в терминале. Разрешение недоступно; просмотрите полный вызов в веб-чате или отклоните через /deny {id}.")).to_owned());
    }
    app.audit_session = Some(snapshot.session_id.clone());
    if run["phase"].get("EnvironmentPaused").is_some() {
        app.push_kodrik_message(app.language.pick("Device disconnected. History is saved; send a new message to continue.","Связь с устройством потеряна. История сохранена; для продолжения отправьте новое сообщение.").into());
    }
    if let Some(reason) = run["blocked_reason"].as_str() {
        let translated = match reason {
            "Stopped by user" => "Остановлено пользователем",
            "Stopped by user; queued follow-ups require new input" => {
                "Остановлено пользователем; для продолжения очереди отправьте новое сообщение"
            }
            "Stopped by user; queued follow-ups require new input or continue" => {
                "Остановлено пользователем; для продолжения очереди отправьте новое сообщение или используйте /continue"
            }
            _ => reason,
        };
        app.push_kodrik_message(app.language.pick(reason, translated).into());
    }
    Ok(())
}
