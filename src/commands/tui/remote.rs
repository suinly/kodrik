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
    async fn open(config: &RemoteConfig, root: &Path, session: SessionId) -> anyhow::Result<Self> {
        let environment = LocalEnvironment::open(config, root).await?;
        let result = async {
            let manifest = environment.manifest()?;
            crate::commands::connect::authorize(config, &manifest)?;
            let mut client = RemoteClient::connect(config).await?;
            client.attach(session, manifest).await?;
            let (stop, receiver) = oneshot::channel();
            let local = environment.clone();
            let task = tokio::spawn(async move { client.serve_local_until(local, receiver).await });
            Ok(Self {
                stop: Some(stop),
                task,
                environment: environment.clone(),
            })
        }
        .await;
        if result.is_err() {
            let _ = tokio::time::timeout(Duration::from_secs(8), environment.close()).await;
        }
        result
    }
    async fn close(mut self) -> anyhow::Result<()> {
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
enum Update {
    Snapshot(RemoteChatSnapshot),
    Resynchronized(RemoteChatSnapshot),
    Disconnected,
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
            if let Ok(snapshot) = chat.snapshot(&session).await {
                if updates
                    .send((epoch, Update::Resynchronized(snapshot)))
                    .await
                    .is_err()
                {
                    return;
                }
                let (tx, mut rx) = mpsc::channel(2);
                let stream = chat.observe(&session, tx);
                tokio::pin!(stream);
                loop {
                    tokio::select! {
                        _=&mut stream=>break,
                        Some(snapshot)=rx.recv()=>{if updates.send((epoch,Update::Snapshot(snapshot))).await.is_err(){return;}}
                    }
                }
            }
            if updates.send((epoch, Update::Disconnected)).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }))
}

struct Remote {
    chat: ChatClient,
    session: SessionId,
    binding: Option<Binding>,
    observer: Option<Observer>,
    updates: mpsc::Sender<(u64, Update)>,
    epoch: u64,
    projection: Projection,
    snapshot: RemoteChatSnapshot,
    uncertain: Option<ChatCommand>,
    root: PathBuf,
}
impl Remote {
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
        if let Some(binding) = self.binding.take() {
            binding.close().await?;
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
        self.projection.accept(app, &self.snapshot)?;
        self.observer = Some(observe(
            self.chat.clone(),
            self.session.clone(),
            self.epoch,
            self.updates.clone(),
        ));
        app.push_kodrik_message(
            "Local access is detached. Use /attach then /attach yes to grant this session access."
                .into(),
        );
        Ok(())
    }
    async fn send(&mut self, app: &mut App, text: String) -> anyhow::Result<()> {
        ensure!(
            self.uncertain.is_none(),
            "Previous input is unconfirmed; use /retry to check the same request ID first"
        );
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
                if is_edit {
                    let id =
                        SessionId::new(response["id"].as_str().context("Edit returned no branch")?);
                    self.select(app, ChatCommand::Select { session_id: id })
                        .await?;
                }
                Ok(())
            }
            Err(error) => {
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
    async fn input(
        &mut self,
        app: &mut App,
        input: String,
        config: &RemoteConfig,
        tui: &mut Tui,
    ) -> anyhow::Result<()> {
        if app.navigation.editing.is_some()
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
            "/audit"=>{
                app.audit_picker=Some(approvals::AuditPicker{session:None,choice:approvals::AUDIT_MODES.iter().position(|mode|*mode==app.audit_mode).unwrap_or(1)});
                app.sessions=None;app.view.clear_selection();
            }
            "/audit status"=>app.push_kodrik_message(format!("Current server tool mode: {}",app.audit_mode.code())),
            "/sessions"=>{
                let sessions=self.chat.sessions().await?;
                app.sessions=Some(sessions.as_array().context("Invalid session list")?.iter().filter_map(|row|Some((SessionId::new(row["id"].as_str()?),row["title"].as_str().unwrap_or("Conversation").to_owned()))).collect());app.session_index=0;
            }
            "/new"=>self.select(app,ChatCommand::New).await?,
            "/detach"=>{self.detach().await?;app.push_kodrik_message("Local access detached; server conversation remains available.".into());},
            "/attach"=>app.push_kodrik_message(format!("Server: {}\nSession: {}\nDevice: {}\nProject: {}\nCapabilities: file read, patch, image; shell: {}; HTTP MCP: {}\nType /attach yes to confirm. Exact tools/skills/instructions are disclosed before a new manifest grant. Shell/MCP are not sandboxed; cleanup is best effort.",config.server,self.session.as_str(),config.label,self.root.display(),config.allow_shell,config.mcp.keys().cloned().collect::<Vec<_>>().join(", "))),
            "/attach yes"=>{
                self.detach().await?;
                tui.exit()?;
                let result=Binding::open(config,&self.root,self.session.clone()).await;
                tui.resume()?;
                self.binding=Some(result?);
                app.push_kodrik_message("Local environment attached to this session only.".into());
            }
            "/older"=>self.older(app).await?,
            "/retry"=>{
                let command=self.uncertain.clone().context("No unconfirmed input")?;
                let request_id=match &command {ChatCommand::Send{request_id,..}|ChatCommand::Edit{request_id,..}=>*request_id,_=>unreachable!()};
                let status=self.chat.command(&self.session,ChatCommand::SubmissionStatus{request_id}).await?;
                if status.is_null(){
                    let response=self.chat.command(&self.session,command.clone()).await?;
                    self.uncertain=None;
                    if matches!(command,ChatCommand::Edit{..}){self.select(app,ChatCommand::Select{session_id:SessionId::new(response["id"].as_str().context("Edit returned no branch")?)}).await?;}
                }else{
                    self.uncertain=None;
                    app.push_kodrik_message(format!("Original input confirmed: {}",status["status"]));
                    if matches!(command,ChatCommand::Edit{..}) && let Some(id)=status["session_id"].as_str(){self.select(app,ChatCommand::Select{session_id:SessionId::new(id)}).await?;}
                }
                app.input.clear();app.input_cursor=0;
            }
            "/approvals"=>{project(app,&self.chat.snapshot(&self.session).await?)?;},
            input if input.starts_with("/approve ")||input.starts_with("/deny ")=>{
                let mut parts=input.split_whitespace();let allow=parts.next()==Some("/approve");let id=parts.next().context("Approval ID required")?;
                ensure!(parts.next().is_none(),"Unexpected approval arguments");
                self.chat.command(&self.session,ChatCommand::DecideApproval{approval_id:id.into(),allow}).await?;
            }
            input if input.starts_with("/audit ")=>{
                let parts=input.split_whitespace().collect::<Vec<_>>();
                if let ["/audit","trusted",session,approval_id]=parts.as_slice(){
                    ensure!(*session==self.session.as_str() && app.approval.as_ref().is_some_and(|approval|approval.id==*approval_id),"Approval is no longer displayed in this session");
                    self.chat.command(&self.session,ChatCommand::TrustApproval{approval_id:(*approval_id).into()}).await?;
                    return Ok(());
                }
                let mode=audit_mode(input)?;
                self.chat.command(&self.session,ChatCommand::SetAuditMode{mode:mode.code().into()}).await?;
            }
            input if input.starts_with('/')=>anyhow::bail!("Unknown remote command or arguments: {}",input.split_whitespace().next().unwrap_or("/")),
            _=>self.send(app,input).await?,
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
        observer: Some(observe(chat.clone(), session.clone(), 0, tx.clone())),
        epoch: 0,
        chat,
        session,
        binding: None,
        updates: tx,
        projection: Projection::default(),
        snapshot,
        uncertain: None,
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
    let result = event_loop(config, &mut remote, &mut app, &mut tui, &mut rx).await;
    remote.observer.take();
    let cleanup = remote.detach().await;
    let terminal = tui.exit();
    result.and(cleanup).and(terminal)
}

async fn event_loop(
    config: &RemoteConfig,
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
    while !app.should_quit {
        tui.draw(app)?;
        tokio::select! {
            _=&mut shutdown=>app.should_quit=true,
            _=tick.tick()=>{
                app.animation_frame=app.animation_frame.wrapping_add(1);
                if remote.binding.as_ref().is_some_and(|binding|binding.task.is_finished()){
                    let _=remote.detach().await;
                    app.push_kodrik_message("Laptop disconnected; use /attach and confirm again for a new lease. Server chat remains available.".into());
                }
            },
            Some((epoch,update))=updates.recv()=>{
                if !current_observer(remote.epoch,epoch) {continue;}
                match update {
                Update::Snapshot(snapshot)=>{
                    let snapshot=with_scrollback(&remote.snapshot,snapshot);
                    if remote.projection.accept(app,&snapshot)? {remote.snapshot=snapshot;}
                }
                Update::Resynchronized(snapshot)=>{
                    let snapshot=with_scrollback(&remote.snapshot,snapshot);
                    if remote.projection.resynchronize(app,&snapshot)? {remote.snapshot=snapshot;}
                }
                Update::Disconnected=>{
                    app.set_copy_notice("Server disconnected; reconnecting (no local fallback)".into());
                }
            }},
            Some(copy)=copies.join_next(),if !copies.is_empty()=>{
                if let Ok((id,result))=copy {complete_copy(app,id,result,|sequence|tui.copy_to_terminal(sequence));}
            },
            event=async {match deferred.pop_front(){Some(event)=>Ok(event),None=>next_terminal_event().await}}=>match event? {
                Event::Key(key) if matches!(key.kind,KeyEventKind::Press|KeyEventKind::Repeat)=>{
                    match selection_command(app,key) {
                        Some(SelectionCommand::Clear)=>continue,
                        Some(SelectionCommand::Copy(text))=>{let id=app.view.selection_id();copies.spawn(async move{(id,clipboard::copy(text).await)});continue;},
                        None=>(),
                    }
                    let operation=async {
                        if key.code==KeyCode::Enter && let Some((id,_))=app.sessions.as_ref().and_then(|rows|rows.get(app.session_index)) {
                            return remote.select(app,ChatCommand::Select{session_id:id.clone()}).await;
                        }
                        if key.code==KeyCode::Esc && app.sessions.is_none() && app.audit_picker.is_none() && app.approval.is_none() && app.navigation.selected.is_none() && app.navigation.editing.is_none() && app.command_suggestion().is_none() && matches!(app.status,Status::Working) {
                            remote.chat.command(&remote.session,ChatCommand::Stop).await?;app.status=Status::Stopping;return Ok(());
                        }
                        let turn=app.selected_user_turn();
                        if let Some(input)=app.update(key){remote.input(app,input,config,tui).await?;}
                        if key.code==KeyCode::PageUp && app.navigation.selected.is_none() {remote.older(app).await?;}
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
                Event::Paste(text)=>{if app.sessions.is_none(){app.insert_input(&text);}},
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
                        let mut text = format!(
                            "[Device: {} · {} · {} · {}]",
                            field("label"),
                            field("project_root"),
                            field("tool_id"),
                            key["generation"]
                                .as_str()
                                .unwrap_or("")
                                .chars()
                                .flat_map(char::escape_debug)
                                .collect::<String>()
                        );
                        if field("tool_id") == "read_file" && part["attachment_saved"] != true {
                            let connected = snapshot.environment.as_ref().is_some_and(|status| {
                                status.state == "connected"
                                    && status.environment.as_ref().is_some_and(|environment| {
                                        serde_json::to_value(environment).ok().as_ref() == Some(key)
                                    })
                            });
                            text.push_str(if connected {
                                "\n[Local file is not saved on the server]"
                            } else {
                                "\n[The file remains on the disconnected device]"
                            });
                        }
                        session.push(Message::Assistant(AssistantMessage { content: text }));
                    }
                    let call_id = part["toolCallId"].as_str().unwrap_or("").to_owned();
                    session.push(Message::ToolCall(ToolCall {
                        call_id: call_id.clone(),
                        name: part["toolName"].as_str().unwrap_or("tool").into(),
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
    app.model = run["model"].as_str().unwrap_or("server agent").into();
    app.remote_environment = Some(snapshot.environment.as_ref().map_or_else(
        || format!("{} · laptop detached", snapshot.session_id.as_str()),
        |environment| {
            format!(
                "{} · {} · {} · {}",
                snapshot.session_id.as_str(),
                environment.label,
                environment.project_root,
                environment.state
            )
        },
    ));
    app.status = if run["busy"] == true {
        Status::Working
    } else if run["phase"] == "Cancelled" {
        Status::Stopped
    } else {
        Status::Ready
    };
    app.compacting = run["compaction"].is_object();
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
    app.audit_session = Some(snapshot.session_id.clone());
    if run["phase"].get("EnvironmentPaused").is_some() {
        app.push_kodrik_message(app.language.pick("Laptop disconnected. History is saved; send a new message to continue.","Связь с ноутбуком потеряна. История сохранена; для продолжения отправьте новое сообщение.").into());
    }
    if let Some(reason) = run["blocked_reason"].as_str() {
        app.push_kodrik_message(reason.into());
    }
    Ok(())
}
