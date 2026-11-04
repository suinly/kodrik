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
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
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
pub(crate) struct RemoteClient {
    socket: Option<Socket>,
    device: uuid::Uuid,
    binding: Option<(EnvironmentKey, SessionId)>,
}
impl RemoteClient {
    pub(crate) async fn connect(config: &RemoteConfig) -> anyhow::Result<Self> {
        let url = endpoint(&config.server)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&config.credential_file)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.len() <= 4096
                && metadata.permissions().mode() & 0o077 == 0,
            "Credential must be a private bounded regular file"
        );
        let mut bytes = Vec::new();
        file.take(4097).read_to_end(&mut bytes)?;
        let credential: Credential = serde_json::from_slice(&bytes)?;
        ensure!(!credential.token.is_empty(), "Empty device token");
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
        })
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
        let mut guard = EnvironmentGuard(Some(environment.clone()));
        let result = self.serve_inner(environment.clone()).await;
        self.binding = None;
        let cleanup = environment.close().await;
        guard.0.take();
        result.and(cleanup)
    }
    async fn serve_inner(&mut self, environment: Arc<LocalEnvironment>) -> anyhow::Result<()> {
        let (key, session) = self.binding.clone().context("Client not attached")?;
        environment
            .attach(self.device, key.clone(), session.clone())
            .await?;
        let socket = self.socket.take().context("Client closed")?;
        let (mut sink, mut stream) = socket.split();
        let (outgoing, mut queue) = tokio::sync::mpsc::channel::<ClientFrame>(32);
        let mut jobs = tokio::task::JoinSet::new();
        let writer = tokio::spawn(async move {
            while let Some(frame) = queue.recv().await {
                tokio::time::timeout(
                    Duration::from_secs(2),
                    sink.send(Message::Binary(encode_frame(&frame)?.into())),
                )
                .await??;
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
        jobs.abort_all();
        while jobs.join_next().await.is_some() {}
        result
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
