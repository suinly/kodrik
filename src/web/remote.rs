use super::{
    Settings,
    auth::{AuthStore, devices::DeviceIdentity},
};
use crate::remote::{
    protocol::*,
    service::{EnvironmentService, HEARTBEAT, LEASE},
    transport::{LeaseClock, Peer, ResultAssembly},
};
use anyhow::{Context, ensure};
use axum::{
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use futures::{SinkExt, StreamExt};
use std::{collections::HashMap, sync::Arc, time::Duration};

#[derive(Clone)]
struct RemoteState {
    settings: Settings,
    auth: AuthStore,
    service: Option<Arc<EnvironmentService>>,
}
pub(super) fn routes(
    settings: Settings,
    auth: AuthStore,
    service: Option<Arc<EnvironmentService>>,
) -> Router {
    Router::new()
        .route("/api/remote/environment", get(upgrade))
        .with_state(RemoteState {
            settings,
            auth,
            service,
        })
}
async fn upgrade(
    State(state): State<RemoteState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if headers.get_all(header::AUTHORIZATION).iter().count() != 1 {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_owned)
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let auth = state.auth.clone();
    let identity = tokio::task::spawn_blocking(move || {
        auth.device_identity(&token, chrono::Utc::now().timestamp())
    })
    .await;
    let Ok(Ok(Some(device))) = identity else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if headers.contains_key(header::ORIGIN)
        && !super::server::valid_origin(&state.settings, &headers)
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(service) = state.service else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    upgrade
        .max_message_size(MAX_FRAME_BYTES)
        .max_frame_size(MAX_FRAME_BYTES)
        .write_buffer_size(0)
        .max_write_buffer_size(2 * MAX_FRAME_BYTES)
        .on_upgrade(move |socket| async move {
            if let Err(error) = serve(socket, device, service).await {
                log::warn!("Remote environment disconnected: {error:#}");
            }
        })
}
async fn send(socket: &mut WebSocket, frame: &ServerFrame) -> anyhow::Result<()> {
    tokio::time::timeout(
        Duration::from_secs(2),
        socket.send(Message::Binary(encode_frame(frame)?.into())),
    )
    .await??;
    Ok(())
}
async fn receive(socket: &mut WebSocket) -> anyhow::Result<ClientFrame> {
    let frame = socket.next().await.context("Remote socket closed")??;
    match frame {
        Message::Binary(bytes) => decode_frame(&bytes),
        Message::Text(text) => decode_frame(text.as_bytes()),
        _ => anyhow::bail!("Expected remote protocol frame"),
    }
}
async fn serve(
    mut socket: WebSocket,
    device: DeviceIdentity,
    service: Arc<EnvironmentService>,
) -> anyhow::Result<()> {
    let ClientFrame::Hello {
        session_id,
        manifest,
    } = tokio::time::timeout(HEARTBEAT, receive(&mut socket)).await??
    else {
        anyhow::bail!("Expected environment hello")
    };
    let key = service
        .attach(device.clone(), session_id.clone(), manifest)
        .await?;
    let (peer, mut outgoing) = Peer::new();
    let result=async {
        service.bind_peer(&key,peer.clone()).await?;
        send(&mut socket,&ServerFrame::Welcome{environment:key.clone(),heartbeat_seconds:HEARTBEAT.as_secs(),lease_seconds:LEASE.as_secs()}).await?;
        let mut closed=peer.closed.subscribe();
        let mut expiry=tokio::time::Instant::now()+LEASE;
        let mut clock=LeaseClock::new();
        let mut check=tokio::time::interval(Duration::from_millis(250));
        let mut assemblies:HashMap<(String,String),ResultAssembly>=HashMap::new();
        let mut assembled=HashMap::new();
        loop {
            tokio::select! {
                biased;
                _=async {let _=closed.wait_for(|closed|*closed).await;}=>anyhow::bail!("Environment revoked"),
                _=tokio::time::sleep_until(expiry)=>anyhow::bail!("Heartbeat expired"),
                _=check.tick()=>{ensure!(service.status(&session_id).await.is_some_and(|s|s.state=="connected" && s.environment.as_ref()==Some(&key)),"Environment authority lost");},
                frame=receive(&mut socket)=>{
                    ensure!(clock.live(),"Lease expired or suspend gap detected");
                    let frame=frame?;
                    match frame {
                        ClientFrame::Heartbeat{environment}=>{ensure!(environment==key && clock.renew(),"Stale heartbeat");service.heartbeat(&key).await?;expiry=tokio::time::Instant::now()+LEASE;send(&mut socket,&ServerFrame::HeartbeatAck{environment:key.clone()}).await?;},
                        ClientFrame::Close{environment}=>{ensure!(environment==key,"Stale close");break;},
                        ClientFrame::Hello{..}=>anyhow::bail!("Duplicate hello"),
                        ClientFrame::ResultChunk{ref run_id,ref call_id,..}=>{
                            let id=(run_id.clone(),call_id.clone());
                            ensure!(!assembled.contains_key(&id),"Chunks already completed");
                            if !assemblies.contains_key(&id) {
                                ensure!(assemblies.len()+assembled.len()<4,"Too many assembled results");
                                let saved=service.result_call(device.device_id,run_id,call_id).await?;
                                assemblies.insert(id.clone(),ResultAssembly::new(saved.invocation)?);
                            }
                            if let Some(output)=assemblies.get_mut(&id).unwrap().push(frame)? {
                                ensure!(!assembled.contains_key(&id),"Duplicate completed chunks");
                                assembled.insert(id.clone(),output);assemblies.remove(&id);
                            }
                        },
                        ClientFrame::Receipt{mut receipt}=>{
                            let id=(receipt.invocation.run_id.clone(),receipt.invocation.call_id.clone());
                            ensure!(!assemblies.contains_key(&id),"Incomplete result chunks");
                            if let Some(output)=assembled.remove(&id) {ensure!(receipt.result.is_none() && receipt.state==OperationState::Completed,"Conflicting chunked result");receipt.result=Some(output);}
                            let final_evidence=matches!(receipt.state,OperationState::Completed|OperationState::Stopped|OperationState::Rejected|OperationState::Unknown);
                            service.receipt(device.device_id,receipt.clone()).await?;
                            if final_evidence {
                                send(&mut socket,&ServerFrame::ReceiptAck{environment:receipt.invocation.environment.clone(),run_id:receipt.invocation.run_id.clone(),call_id:receipt.invocation.call_id.clone(),arguments_hash:receipt.arguments_hash.clone()}).await?;
                                peer.finish(receipt)?;
                            }
                        },
                    }
                },
                frame=outgoing.recv()=>{
                    ensure!(clock.live(),"Lease expired or suspend gap detected");
                    let Some(frame)=frame else {break;};
                    if let ServerFrame::Invoke{invocation}=&frame {service.authorize(invocation).await?;}
                    send(&mut socket,&frame).await?;
                },
            }
        }
        Ok::<_,anyhow::Error>(())
    }.await;
    peer.close();
    let cleanup = service.disconnect(&key).await;
    let _ = tokio::time::timeout(Duration::from_millis(250), socket.close()).await;
    result.and(cleanup).with_context(|| {
        format!(
            "session_id={} connection_id={} generation={}",
            session_id.as_str(),
            key.connection_id,
            key.generation
        )
    })
}
