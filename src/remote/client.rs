use super::{
    config::RemoteConfig,
    local::LocalEnvironment,
    protocol::*,
    service::{HEARTBEAT, LEASE},
    transport::LeaseClock,
};
use crate::agent::session::SessionId;
use anyhow::{Context, ensure};
use futures::{SinkExt, StreamExt};
use std::{
    collections::HashMap,
    io::Read,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    sync::Arc,
    time::Duration,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest, protocol::WebSocketConfig},
};
type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Credential {
    pub device_id: uuid::Uuid,
    pub token: String,
}
impl Credential {
    pub(crate) fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .context("Cannot open device credential; run kodrik connect")?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.len() <= 4096
                && metadata.permissions().mode() & 0o077 == 0,
            "Credential must be a private bounded regular file"
        );
        let mut bytes = Vec::new();
        file.take(4097).read_to_end(&mut bytes)?;
        let credential: Self = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("Invalid device credential"))?;
        ensure!(
            !credential.token.is_empty()
                && credential.token.len() <= 256
                && credential.token.bytes().all(|b| b.is_ascii_graphic()),
            "Invalid device token"
        );
        Ok(credential)
    }
    pub(crate) fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        use std::io::Write;
        let parent = path.parent().context("Credential directory missing")?;
        private_directory(parent)?;
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .context("Cannot save credential; existing credentials are never overwritten")?;
        file.write_all(&serde_json::to_vec(self)?)?;
        file.sync_all()?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    }
}

pub(crate) fn private_directory(path: &std::path::Path) -> anyhow::Result<()> {
    // System ancestors may be aliases (macOS /var); reject the actual storage directory.
    if !path.try_exists()? {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0,
        "Credential directory must be private and not a symlink"
    );
    Ok(())
}
pub(crate) struct RemoteClient {
    socket: Option<Socket>,
    device: uuid::Uuid,
    binding: Option<(EnvironmentKey, SessionId)>,
    chat: ChatClient,
}
impl RemoteClient {
    pub(crate) async fn connect(config: &RemoteConfig) -> anyhow::Result<Self> {
        let url = endpoint(&config.server)?;
        let credential = Credential::load(&config.credential_file)?;
        let chat = ChatClient::new(config)?;
        let mut request = url.as_str().into_client_request()?;
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {}", credential.token).parse()?,
        );
        let settings = WebSocketConfig::default()
            .max_message_size(Some(MAX_FRAME_BYTES))
            .max_frame_size(Some(MAX_FRAME_BYTES))
            .write_buffer_size(0)
            .max_write_buffer_size(2 * MAX_FRAME_BYTES);
        // connect_async makes one handshake; redirects are errors and credentials never enter URLs.
        let (socket, _) = tokio::time::timeout(
            LEASE,
            tokio_tungstenite::connect_async_with_config(request, Some(settings), true),
        )
        .await??;
        Ok(Self {
            socket: Some(socket),
            device: credential.device_id,
            binding: None,
            chat,
        })
    }
    pub(crate) fn chat(&self) -> ChatClient {
        self.chat.clone()
    }
    pub(crate) async fn attach(
        &mut self,
        session: SessionId,
        manifest: EnvironmentManifest,
    ) -> anyhow::Result<EnvironmentKey> {
        ensure!(self.binding.is_none(), "Client already attached");
        let socket = self.socket.as_mut().context("Remote client closed")?;
        send(
            socket,
            &ClientFrame::Hello {
                session_id: session.clone(),
                manifest,
            },
        )
        .await?;
        let frame = tokio::time::timeout(HEARTBEAT, receive(socket)).await??;
        if let ServerFrame::Error { message } = &frame {
            let message: String = message
                .chars()
                .flat_map(char::escape_debug)
                .take(1024)
                .collect();
            anyhow::bail!("Environment attachment rejected: {message}");
        }
        let ServerFrame::Welcome {
            environment,
            heartbeat_seconds,
            lease_seconds,
        } = frame
        else {
            anyhow::bail!("Expected remote welcome")
        };
        ensure!(
            heartbeat_seconds == 5 && lease_seconds == 15,
            "Incompatible heartbeat/lease"
        );
        self.binding = Some((environment.clone(), session));
        Ok(environment)
    }
    pub(crate) async fn detach(&mut self, key: &EnvironmentKey) -> anyhow::Result<()> {
        ensure!(
            self.binding.as_ref().is_some_and(|(saved, _)| saved == key),
            "Stale detach"
        );
        let mut socket = self.socket.take().context("Client closed")?;
        self.binding = None;
        send(
            &mut socket,
            &ClientFrame::Close {
                environment: key.clone(),
            },
        )
        .await?;
        tokio::time::timeout(Duration::from_secs(2), socket.close(None)).await??;
        Ok(())
    }
    pub(crate) async fn serve_local(
        &mut self,
        environment: Arc<LocalEnvironment>,
    ) -> anyhow::Result<()> {
        let (_keep_open, stop) = tokio::sync::oneshot::channel();
        self.serve_local_until(environment, stop).await
    }
    pub(crate) async fn serve_local_until(
        &mut self,
        environment: Arc<LocalEnvironment>,
        stop: tokio::sync::oneshot::Receiver<()>,
    ) -> anyhow::Result<()> {
        self.serve_local_until_ready(environment, stop, None).await
    }
    pub(crate) async fn serve_local_until_ready(
        &mut self,
        environment: Arc<LocalEnvironment>,
        stop: tokio::sync::oneshot::Receiver<()>,
        ready: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> anyhow::Result<()> {
        let mut guard = EnvironmentGuard(Some(environment.clone()));
        let result = self.serve_inner(environment.clone(), stop, ready).await;
        self.binding = None;
        let cleanup = environment.close().await;
        guard.0.take();
        result.and(cleanup)
    }
    async fn serve_inner(
        &mut self,
        environment: Arc<LocalEnvironment>,
        mut stop: tokio::sync::oneshot::Receiver<()>,
        ready: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> anyhow::Result<()> {
        let (key, session) = self.binding.clone().context("Client not attached")?;
        environment
            .attach(self.device, key.clone(), session.clone())
            .await?;
        if let Some(ready) = ready {
            let _ = ready.send(());
        }
        let socket = self.socket.take().context("Client closed")?;
        let (mut sink, mut stream) = socket.split();
        let (outgoing, mut queue) = tokio::sync::mpsc::channel::<ClientFrame>(32);
        let mut jobs = tokio::task::JoinSet::new();
        let writer = tokio::spawn(async move {
            while let Some(frame) = queue.recv().await {
                let closing = matches!(frame, ClientFrame::Close { .. });
                tokio::time::timeout(
                    Duration::from_secs(2),
                    sink.send(Message::Binary(encode_frame(&frame)?.into())),
                )
                .await??;
                if closing {
                    break;
                }
            }
            Ok::<_, anyhow::Error>(())
        });
        let mut writer = WriterGuard(writer);
        let mut calls = HashMap::<(String, String), Invocation>::new();
        let mut heartbeat = tokio::time::interval(HEARTBEAT);
        let mut check = tokio::time::interval(Duration::from_millis(250));
        let mut clock = LeaseClock::new();
        let mut recovering = HashMap::new();
        let result=async {
            loop {
                if recovering.is_empty() {
                    for receipt in environment.pending_receipts()? {
                        recovering.insert((receipt.invocation.run_id.clone(),receipt.invocation.call_id.clone()),receipt.invocation.clone());
                        let outgoing=outgoing.clone();
                        jobs.spawn(async move {send_receipt(&outgoing,receipt).await});
                    }
                }
                tokio::select! {
                    biased;
                    _=&mut stop=>{
                        environment.stop_admission()?;
                        tokio::time::timeout(Duration::from_secs(2),outgoing.send(ClientFrame::Close{environment:key.clone()})).await??;
                        tokio::time::timeout(Duration::from_secs(3),&mut writer.0).await???;
                        return Ok(());
                    },
                    result=&mut writer.0=>{result??;anyhow::bail!("Remote writer closed");},
                    _=check.tick()=>ensure!(clock.live(),"Remote lease expired or suspend detected"),
                    _=heartbeat.tick()=>{ensure!(clock.live(),"Remote lease expired");outgoing.try_send(ClientFrame::Heartbeat{environment:key.clone()})?;},
                    frame=stream.next()=>{
                        ensure!(clock.live(),"Remote lease expired or suspend detected");
                        let frame=decode_message(frame.context("Remote socket closed")??)?;
                        match frame {
                            ServerFrame::HeartbeatAck{environment}=>ensure!(environment==key && clock.renew(),"Stale heartbeat ACK"),
                            ServerFrame::Invoke{invocation}=>{
                                ensure!(invocation.environment==key && invocation.session_id==session,"Stale invocation");
                                ensure!(calls.len()<4,"Too many local calls or unacknowledged results");
                                let id=(invocation.run_id.clone(),invocation.call_id.clone());
                                ensure!(!calls.contains_key(&id),"Duplicate in-flight invocation");
                                calls.insert(id,invocation.clone());
                                let environment=environment.clone();let outgoing=outgoing.clone();
                                jobs.spawn(async move {
                                    let receipt=environment.execute(invocation).await?;
                                    send_receipt(&outgoing,receipt).await
                                });
                            },
                            ServerFrame::Cancel{environment:target,run_id,call_id}=>{ensure!(target==key,"Stale cancel");environment.cancel(&run_id,&call_id).await?;},
                            ServerFrame::ReceiptAck{environment:target,run_id,call_id,arguments_hash:hash}=>{
                                let id=(run_id,call_id);
                                let call=calls.remove(&id).or_else(||recovering.remove(&id)).context("Unsolicited receipt ACK")?;
                                ensure!(target==call.environment && hash==arguments_hash(&call.arguments)?,"Changed receipt ACK");environment.acknowledge(call).await?;
                            },
                            ServerFrame::Revoke{..}|ServerFrame::Error{..}=>anyhow::bail!("Remote authority revoked"),
                            ServerFrame::Welcome{..}=>anyhow::bail!("Duplicate welcome"),
                        }
                    },
                    result=jobs.join_next(),if !jobs.is_empty()=>{result.context("Local execution missing")???;},
                }
            }
        }.await;
        // Let admitted operations finish their bounded shutdown before dropping their jobs.
        let cleanup = environment.close().await;
        jobs.abort_all();
        while jobs.join_next().await.is_some() {}
        result.and(cleanup)
    }
}
struct EnvironmentGuard(Option<Arc<LocalEnvironment>>);
impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        if let Some(environment) = self.0.take() {
            // Revoke admission synchronously even if the caller retains another Arc.
            if let Err(error) = environment.stop_admission() {
                log::warn!("Local cancellation failed: {error}");
            }
            tokio::spawn(async move {
                if let Err(error) = environment.close().await {
                    log::warn!("Local cleanup failed: {error}");
                }
            });
        }
    }
}
struct WriterGuard(tokio::task::JoinHandle<anyhow::Result<()>>);
impl Drop for WriterGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn send(socket: &mut Socket, frame: &ClientFrame) -> anyhow::Result<()> {
    tokio::time::timeout(
        Duration::from_secs(2),
        socket.send(Message::Binary(encode_frame(frame)?.into())),
    )
    .await??;
    Ok(())
}
fn decode_message(frame: Message) -> anyhow::Result<ServerFrame> {
    match frame {
        Message::Binary(bytes) => decode_frame(&bytes),
        Message::Text(text) => decode_frame(text.as_bytes()),
        _ => anyhow::bail!("Expected remote protocol frame"),
    }
}
async fn receive(socket: &mut Socket) -> anyhow::Result<ServerFrame> {
    decode_message(socket.next().await.context("Remote socket closed")??)
}
async fn send_receipt(
    outgoing: &tokio::sync::mpsc::Sender<ClientFrame>,
    mut receipt: OperationReceipt,
) -> anyhow::Result<()> {
    if encode_frame(&ClientFrame::Receipt {
        receipt: receipt.clone(),
    })
    .is_err()
    {
        let output = receipt
            .result
            .take()
            .context("Oversized receipt without result")?;
        let bytes = serde_json::to_vec(&output)?;
        ensure!(
            bytes.len() <= MAX_OUTPUT_BYTES,
            "Local result exceeds limit"
        );
        // JSON encodes byte arrays as up to four characters per byte, leaving room for identity.
        let chunks = bytes.chunks(48 * 1024);
        let count = chunks.len();
        for (sequence, data) in chunks.enumerate() {
            outgoing
                .send(ClientFrame::ResultChunk {
                    environment: receipt.invocation.environment.clone(),
                    run_id: receipt.invocation.run_id.clone(),
                    call_id: receipt.invocation.call_id.clone(),
                    arguments_hash: receipt.arguments_hash.clone(),
                    sequence: sequence as u32,
                    final_chunk: sequence + 1 == count,
                    data: data.to_vec(),
                })
                .await?;
        }
    }
    outgoing.send(ClientFrame::Receipt { receipt }).await?;
    Ok(())
}
pub(crate) fn endpoint(url: &reqwest::Url) -> anyhow::Result<reqwest::Url> {
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "Server URL must not contain credentials/query/fragment"
    );
    let host = url.host_str().unwrap_or("");
    let loopback = host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && loopback),
        "Server must use HTTPS except on loopback"
    );
    ensure!(
        url.path().ends_with('/') && !url.path().contains('%') && !url.path().contains("//"),
        "Invalid server prefix"
    );
    let mut endpoint = url.join("api/remote/environment")?;
    endpoint
        .set_scheme(if url.scheme() == "https" { "wss" } else { "ws" })
        .map_err(|_| anyhow::anyhow!("Invalid WebSocket URL"))?;
    Ok(endpoint)
}

#[derive(Clone)]
pub(crate) struct ChatClient {
    http: reqwest::Client,
    base: reqwest::Url,
}

// The gateway budgets history at 1 MiB; leave room for snapshot metadata and SSE framing.
const MAX_CHAT_BYTES: usize = 2 * 1024 * 1024;
const MAX_COMMAND_BYTES: usize = 64 * 1024;

impl ChatClient {
    pub(crate) fn new(config: &RemoteConfig) -> anyhow::Result<Self> {
        endpoint(&config.server)?;
        let credential = Credential::load(&config.credential_file)?;
        let mut authorization: reqwest::header::HeaderValue =
            format!("Bearer {}", credential.token)
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid device token"))?;
        authorization.set_sensitive(true);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::AUTHORIZATION, authorization);
        Ok(Self {
            http: reqwest::Client::builder()
                .default_headers(headers)
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .connect_timeout(LEASE)
                .read_timeout(Duration::from_secs(30))
                .build()
                .map_err(|_| anyhow::anyhow!("Cannot initialize remote chat client"))?,
            base: config.server.clone(),
        })
    }

    fn url(
        &self,
        session: Option<&SessionId>,
        suffix: Option<&str>,
    ) -> anyhow::Result<reqwest::Url> {
        let mut url = self.base.join("api/remote/chat/sessions")?;
        if let Some(session) = session {
            ensure!(
                !session.as_str().is_empty()
                    && !matches!(session.as_str(), "." | "..")
                    && session.as_str().len() <= 1024,
                "Invalid remote session ID"
            );
            let mut path = url
                .path_segments_mut()
                .map_err(|_| anyhow::anyhow!("Invalid remote chat URL"))?;
            path.push(session.as_str());
            if let Some(suffix) = suffix {
                path.push(suffix);
            }
        }
        Ok(url)
    }

    async fn json<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> anyhow::Result<T> {
        let response = chat_response(request.timeout(LEASE)).await?;
        ensure!(
            response
                .content_length()
                .is_none_or(|size| size <= MAX_CHAT_BYTES as u64),
            "Remote chat response exceeds limit"
        );
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| anyhow::anyhow!("Remote chat response interrupted"))?;
            ensure!(
                chunk.len() <= MAX_CHAT_BYTES - bytes.len(),
                "Remote chat response exceeds limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("Invalid remote chat response"))
    }

    pub(crate) async fn snapshot(&self, session: &SessionId) -> anyhow::Result<RemoteChatSnapshot> {
        let snapshot = self
            .json(self.http.get(self.url(Some(session), None)?))
            .await?;
        checked_snapshot(snapshot, session)
    }

    pub(crate) async fn sessions(&self) -> anyhow::Result<serde_json::Value> {
        self.json(self.http.get(self.url(None, None)?)).await
    }

    pub(crate) async fn sessions_page(&self, offset: usize) -> anyhow::Result<serde_json::Value> {
        let request = self
            .http
            .get(self.url(None, None)?)
            .query(&[("limit", 30), ("offset", offset)]);
        self.json(request).await
    }

    pub(crate) async fn command(
        &self,
        session: &SessionId,
        command: ChatCommand,
    ) -> anyhow::Result<serde_json::Value> {
        let bytes =
            serde_json::to_vec(&command).map_err(|_| anyhow::anyhow!("Invalid remote command"))?;
        ensure!(
            bytes.len()
                <= if matches!(command, ChatCommand::SendImages { .. }) {
                    super::protocol::MAX_IMAGE_COMMAND_BYTES
                } else {
                    MAX_COMMAND_BYTES
                },
            "Remote command exceeds limit"
        );
        // Never retry here: the caller retains request_id to resolve an uncertain send/edit.
        self.json(
            self.http
                .post(self.url(Some(session), Some("commands"))?)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(bytes),
        )
        .await
        .context("Remote command may have been applied; do not blindly retry; retain request_id")
    }

    pub(crate) async fn history(
        &self,
        session: &SessionId,
        cursor: &str,
    ) -> anyhow::Result<RemoteChatSnapshot> {
        ensure!(cursor.len() <= 4096, "Remote history cursor exceeds limit");
        let request = self
            .http
            .get(self.url(Some(session), None)?)
            .query(&[("cursor", cursor)]);
        checked_snapshot(self.json(request).await?, session)
    }

    pub(crate) async fn observe(
        &self,
        session: &SessionId,
        tx: tokio::sync::mpsc::Sender<RemoteChatSnapshot>,
    ) -> anyhow::Result<()> {
        self.observe_mode(session, tx, false).await
    }

    pub(crate) async fn observe_incremental(
        &self,
        session: &SessionId,
        tx: tokio::sync::mpsc::Sender<RemoteChatSnapshot>,
    ) -> anyhow::Result<()> {
        self.observe_mode(session, tx, true).await
    }

    async fn observe_mode(
        &self,
        session: &SessionId,
        tx: tokio::sync::mpsc::Sender<RemoteChatSnapshot>,
        incremental: bool,
    ) -> anyhow::Result<()> {
        let mut url = self.url(Some(session), Some("events"))?;
        if incremental {
            url.query_pairs_mut().append_pair("incremental", "true");
        }
        let response = tokio::select! {
            biased;
            _ = tx.closed() => return Ok(()),
            response = chat_response(self.http.get(url)
                .header(reqwest::header::ACCEPT, "text/event-stream")) => response?,
        };
        ensure!(
            response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(';').next())
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream")),
            "Invalid remote chat event content type"
        );
        let mut stream = response.bytes_stream();
        let mut parser = ChatEvents {
            incremental,
            ..Default::default()
        };
        loop {
            let chunk = tokio::select! {
                biased;
                _ = tx.closed() => return Ok(()),
                chunk = stream.next() => chunk.context("Remote chat event stream closed; resynchronize snapshot")?
                    .map_err(|_| anyhow::anyhow!("Remote chat event stream interrupted; resynchronize snapshot"))?,
            };
            for byte in chunk {
                if let Some(snapshot) = parser.push(byte)? {
                    if tx.send(checked_snapshot(snapshot, session)?).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }
}

async fn chat_response(request: reqwest::RequestBuilder) -> anyhow::Result<reqwest::Response> {
    let response = request
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("Remote chat request failed"))?;
    ensure!(
        response.status().is_success(),
        "Remote chat request denied ({})",
        response.status()
    );
    Ok(response)
}

fn checked_snapshot(
    snapshot: RemoteChatSnapshot,
    session: &SessionId,
) -> anyhow::Result<RemoteChatSnapshot> {
    ensure!(
        &snapshot.session_id == session,
        "Remote chat snapshot session mismatch"
    );
    Ok(snapshot)
}

// Native events use only data fields; other SSE fields/comments do not carry state.
#[derive(Default)]
struct ChatEvents {
    line: Vec<u8>,
    data: Vec<u8>,
    skip_lf: bool,
    started: bool,
    incremental: bool,
    assembly: super::chat_sync::Assembly,
}

impl ChatEvents {
    fn push(&mut self, byte: u8) -> anyhow::Result<Option<RemoteChatSnapshot>> {
        if std::mem::take(&mut self.skip_lf) && byte == b'\n' {
            return Ok(None);
        }
        if !matches!(byte, b'\r' | b'\n') {
            ensure!(
                self.line.len() + self.data.len() < MAX_CHAT_BYTES,
                "Remote chat event exceeds limit"
            );
            self.line.push(byte);
            return Ok(None);
        }
        self.skip_lf = byte == b'\r';
        let mut line = std::str::from_utf8(&self.line)
            .map_err(|_| anyhow::anyhow!("Invalid remote chat event UTF-8"))?;
        if !self.started {
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
            self.started = true;
        }
        let snapshot = if line.is_empty() && !self.data.is_empty() {
            self.data.pop(); // SSE joins data lines with LF and drops the final LF.
            let snapshot = if self.incremental {
                let mut frame: super::chat_sync::Frame = serde_json::from_slice(&self.data)
                    .map_err(|_| anyhow::anyhow!("Invalid remote history frame"))?;
                let metadata = frame.metadata.take();
                if self.assembly.apply(frame)? {
                    let mut metadata = metadata.context("Missing remote history metadata")?;
                    metadata["messages"] = serde_json::from_str(self.assembly.text())
                        .map_err(|_| anyhow::anyhow!("Invalid remote history messages"))?;
                    Some(
                        serde_json::from_value(metadata)
                            .map_err(|_| anyhow::anyhow!("Invalid remote chat event snapshot"))?,
                    )
                } else {
                    None
                }
            } else {
                Some(
                    serde_json::from_slice(&self.data)
                        .map_err(|_| anyhow::anyhow!("Invalid remote chat event snapshot"))?,
                )
            };
            self.data.clear();
            snapshot
        } else {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            if field == "data" {
                let value = value.strip_prefix(' ').unwrap_or(value);
                ensure!(
                    self.data.len() + value.len() < MAX_CHAT_BYTES,
                    "Remote chat event exceeds limit"
                );
                self.data.extend_from_slice(value.as_bytes());
                self.data.push(b'\n');
            }
            None
        };
        self.line.clear();
        Ok(snapshot)
    }
}

#[cfg(test)]
#[path = "chat_client_tests.rs"]
mod chat_client_tests;

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn client_shutdown_preserves_remote_shell_term_grace() {
        for mode in ["detach", "transport", "abort"] {
            for ignores_term in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let config = RemoteConfig {
                    server: format!("http://{}/", listener.local_addr().unwrap())
                        .parse()
                        .unwrap(),
                    credential_file: dir.path().join("private/device"),
                    label: "cancel fixture".into(),
                    allow_shell: true,
                    skill_roots: vec![],
                    mcp: HashMap::new(),
                    optional_mcp: vec![],
                };
                Credential {
                    device_id: uuid::Uuid::new_v4(),
                    token: "fixture".into(),
                }
                .save(&config.credential_file)
                .unwrap();
                let env = LocalEnvironment::open(&config, dir.path()).await.unwrap();
                let key = EnvironmentKey {
                    connection_id: uuid::Uuid::new_v4(),
                    generation: uuid::Uuid::new_v4(),
                };
                let session = SessionId::new("cancel-fixture");
                let trap = if ignores_term {
                    "trap '' TERM"
                } else {
                    "trap 'sleep 0.2; echo flushed > flushed; exit 0' TERM"
                };
                let call = Invocation {
                    environment: key.clone(),
                    session_id: session.clone(),
                    run_id: "run".into(),
                    call_id: "call".into(),
                    tool_id: "exec_command".into(),
                    arguments: json!({"cmd":format!("{trap}; echo $$ > started; while :; do sleep 0.05; done")}),
                    deadline: chrono::Utc::now() + chrono::Duration::seconds(60),
                };
                let (disconnect, disconnected) = tokio::sync::oneshot::channel();
                let server = tokio::spawn(async move {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                    socket.next().await.unwrap().unwrap();
                    for frame in [
                        ServerFrame::Welcome {
                            environment: key,
                            heartbeat_seconds: 5,
                            lease_seconds: 15,
                        },
                        ServerFrame::Invoke { invocation: call },
                    ] {
                        socket
                            .send(Message::Binary(encode_frame(&frame).unwrap().into()))
                            .await
                            .unwrap();
                    }
                    let mut disconnected = disconnected;
                    loop {
                        tokio::select! {
                            _ = &mut disconnected => break,
                            frame = socket.next() => {
                                let Some(Ok(frame)) = frame else { break };
                                if matches!(decode_frame::<ClientFrame>(&frame.into_data()), Ok(ClientFrame::Close { .. })) { break; }
                            }
                        }
                    }
                });
                let mut client = RemoteClient::connect(&config).await.unwrap();
                client
                    .attach(session, env.manifest().unwrap())
                    .await
                    .unwrap();
                let (stop, stopped) = tokio::sync::oneshot::channel();
                let task = tokio::spawn({
                    let env = env.clone();
                    async move { client.serve_local_until(env, stopped).await }
                });
                tokio::time::timeout(Duration::from_secs(3), async {
                    while std::fs::read_to_string(dir.path().join("started"))
                        .unwrap_or_default()
                        .trim()
                        .is_empty()
                    {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap();
                let pid: i32 = std::fs::read_to_string(dir.path().join("started"))
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                let start = std::time::Instant::now();
                if mode == "detach" {
                    stop.send(()).unwrap();
                } else if mode == "transport" {
                    disconnect.send(()).unwrap();
                } else {
                    task.abort();
                }
                let result = tokio::time::timeout(Duration::from_secs(5), task)
                    .await
                    .unwrap();
                if mode == "abort" {
                    assert!(result.unwrap_err().is_cancelled());
                } else if mode == "detach" {
                    result.unwrap().unwrap();
                } else {
                    assert!(result.unwrap().is_err());
                }
                tokio::time::timeout(Duration::from_secs(5), env.close())
                    .await
                    .unwrap()
                    .unwrap();
                let elapsed = start.elapsed();
                server.await.unwrap();
                // SAFETY: query only this fixture's shell, never a service process.
                assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "shell survived {mode}");
                if ignores_term {
                    assert!(
                        elapsed >= Duration::from_millis(1900),
                        "{mode} skipped TERM grace: {elapsed:?}"
                    );
                } else {
                    assert_eq!(
                        std::fs::read_to_string(dir.path().join("flushed")).unwrap_or_default(),
                        "flushed\n",
                        "TERM trap lost for {mode}"
                    );
                }
                assert!(
                    elapsed < Duration::from_secs(5),
                    "{mode} cleanup was unbounded"
                );
            }
        }
    }
}
