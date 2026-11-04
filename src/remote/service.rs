use super::protocol::*;
use crate::{
    agent::session::{SessionId, SessionStore, SqliteSessionStore},
    web::auth::{AuthStore, Identity, devices::DeviceIdentity},
};
use anyhow::ensure;
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Mutex, watch},
    time::Instant,
};
use uuid::Uuid;

pub(crate) const HEARTBEAT: Duration = Duration::from_secs(5);
pub(crate) const LEASE: Duration = Duration::from_secs(15);

struct Binding {
    device: Uuid,
    key: EnvironmentKey,
    manifest: EnvironmentManifest,
    expires: Instant,
    peer: Option<Arc<super::transport::Peer>>,
    scope_lease: Arc<super::tools::ScopeLease>,
}

impl Drop for Binding {
    fn drop(&mut self) {
        self.scope_lease.revoke();
    }
}

pub(crate) struct EnvironmentService {
    store: SqliteSessionStore,
    auth: AuthStore,
    owner: bool,
    bindings: Mutex<HashMap<SessionId, Binding>>,
    revision: watch::Sender<u64>,
    closed: AtomicBool,
}

impl EnvironmentService {
    pub(crate) fn new(store: SqliteSessionStore, auth: AuthStore, owner: bool) -> Self {
        Self {
            store,
            auth,
            owner,
            bindings: Mutex::new(HashMap::new()),
            revision: watch::channel(0).0,
            closed: AtomicBool::new(false),
        }
    }

    fn changed(&self) {
        self.revision
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    pub(crate) fn revisions(&self) -> watch::Receiver<u64> {
        self.revision.subscribe()
    }

    // Weak ownership ensures the maintenance task cannot keep a gateway alive.
    pub(crate) fn monitor(self: &Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        let service = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = async { let _ = shutdown.wait_for(|stopped| *stopped).await; } => {
                        if let Some(service) = service.upgrade() { service.close().await; }
                        break;
                    }
                    _ = tokio::time::sleep(Duration::from_millis(250)) => {
                        let Some(service) = service.upgrade() else { break; };
                        let _ = service.prune().await;
                    }
                }
            }
        });
    }

    pub(crate) async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.clear().await;
    }

    async fn clear(&self) {
        let mut bindings = self.bindings.lock().await;
        if !bindings.is_empty() {
            for binding in bindings.values() {
                if let Some(peer) = &binding.peer {
                    peer.close();
                }
            }
            bindings.clear();
            self.changed();
        }
    }

    async fn prune(&self) -> anyhow::Result<HashSet<Uuid>> {
        let auth = self.auth.clone();
        let devices = tokio::task::spawn_blocking(move || {
            auth.list_devices(&Identity::Owner, chrono::Utc::now().timestamp())
        })
        .await;
        let allowed: HashSet<_> = match devices {
            Ok(Ok(devices)) => devices.into_iter().map(|device| device.id).collect(),
            _ => {
                self.clear().await;
                anyhow::bail!("Device authority unavailable");
            }
        };
        let mut bindings = self.bindings.lock().await;
        let before = bindings.len();
        let now = Instant::now();
        bindings.retain(|_, binding| {
            let live = binding.scope_lease.live()
                && binding.expires > now
                && allowed.contains(&binding.device);
            if !live && let Some(peer) = &binding.peer {
                peer.close();
            }
            live
        });
        if before != bindings.len() {
            self.changed();
        }
        Ok(allowed)
    }

    async fn personal(&self, session: &SessionId) -> anyhow::Result<()> {
        let session = self
            .store
            .load(session)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Conversation not found"))?;
        ensure!(
            crate::web::chat::personal_session(&session)
                && session.webhook_instruction.is_none()
                && !session
                    .id()
                    .as_str()
                    .starts_with(crate::agent::subagents::PREFIX),
            "Personal owner conversation required"
        );
        Ok(())
    }

    pub(crate) async fn attach(
        &self,
        device: DeviceIdentity,
        session: SessionId,
        manifest: EnvironmentManifest,
    ) -> anyhow::Result<EnvironmentKey> {
        ensure!(self.owner, "Owner environment service required");
        validate_manifest(&manifest)?;
        self.personal(&session).await?;
        let allowed = self.prune().await?;
        ensure!(
            allowed.contains(&device.device_id),
            "Device authority revoked"
        );
        let mut bindings = self.bindings.lock().await;
        ensure!(!self.closed.load(Ordering::SeqCst), "Gateway is stopping");
        ensure!(
            !bindings.contains_key(&session),
            "Conversation already has an environment"
        );
        ensure!(bindings.len() < 16, "Too many environments");
        let key = EnvironmentKey {
            connection_id: Uuid::new_v4(),
            generation: Uuid::new_v4(),
        };
        self.store
            .remote_binding(
                device.device_id,
                key.clone(),
                session.clone(),
                manifest.clone(),
            )
            .await?;
        bindings.insert(
            session,
            Binding {
                device: device.device_id,
                key: key.clone(),
                manifest,
                expires: Instant::now() + LEASE,
                peer: None,
                scope_lease: Arc::new(super::tools::ScopeLease::new()),
            },
        );
        self.changed();
        Ok(key)
    }

    pub(crate) async fn heartbeat(&self, key: &EnvironmentKey) -> anyhow::Result<()> {
        self.prune().await?;
        let mut bindings = self.bindings.lock().await;
        let binding = bindings
            .values_mut()
            .find(|b| b.key == *key)
            .ok_or_else(|| anyhow::anyhow!("Environment unavailable"))?;
        ensure!(
            !self.closed.load(Ordering::SeqCst) && binding.expires > Instant::now(),
            "Environment expired"
        );
        binding.expires = Instant::now() + LEASE;
        ensure!(
            binding.scope_lease.renew(),
            "Environment expired after suspend"
        );
        Ok(())
    }

    pub(crate) async fn revoke(&self, key: &EnvironmentKey, _reason: &str) -> anyhow::Result<()> {
        let mut bindings = self.bindings.lock().await;
        let before = bindings.len();
        bindings.retain(|_, b| {
            if b.key == *key {
                if let Some(peer) = &b.peer {
                    peer.close();
                }
                false
            } else {
                true
            }
        });
        if before != bindings.len() {
            self.changed();
        }
        Ok(())
    }

    pub(crate) async fn revoke_device(&self, device: Uuid) {
        let mut bindings = self.bindings.lock().await;
        let before = bindings.len();
        bindings.retain(|_, b| {
            if b.device == device {
                if let Some(peer) = &b.peer {
                    peer.close();
                }
                false
            } else {
                true
            }
        });
        if before != bindings.len() {
            self.changed();
        }
    }

    pub(crate) async fn status(&self, session: &SessionId) -> Option<EnvironmentStatus> {
        self.prune().await.ok()?;
        let bindings = self.bindings.lock().await;
        let binding = bindings.get(session)?;
        if self.closed.load(Ordering::SeqCst) || binding.expires <= Instant::now() {
            return None;
        }
        Some(EnvironmentStatus {
            session_id: session.clone(),
            environment: Some(binding.key.clone()),
            label: binding.manifest.label.clone(),
            project_root: binding.manifest.project_root.clone(),
            state: "connected".into(),
            reason: None,
        })
    }

    pub(crate) async fn authorize(&self, invocation: &Invocation) -> anyhow::Result<()> {
        ensure!(self.owner, "Owner environment service required");
        self.personal(&invocation.session_id).await?;
        self.prune().await?;
        ensure!(
            invocation.deadline > chrono::Utc::now(),
            "Invocation deadline expired"
        );
        encode_frame(&ServerFrame::Invoke {
            invocation: invocation.clone(),
        })?;
        let bindings = self.bindings.lock().await;
        let binding = bindings
            .get(&invocation.session_id)
            .ok_or_else(|| anyhow::anyhow!("Environment unavailable"))?;
        ensure!(
            !self.closed.load(Ordering::SeqCst) && binding.expires > Instant::now(),
            "Environment expired"
        );
        ensure!(
            binding.key == invocation.environment,
            "Stale environment generation"
        );
        ensure!(
            binding
                .manifest
                .tools
                .iter()
                .any(|tool| tool.id == invocation.tool_id),
            "Tool unavailable"
        );
        Ok(())
    }

    pub(crate) async fn step_scope(
        self: &Arc<Self>,
        session: &SessionId,
    ) -> anyhow::Result<Option<super::tools::EnvironmentScope>> {
        self.prune().await?;
        let bindings = self.bindings.lock().await;
        Ok(bindings
            .get(session)
            .filter(|_| !self.closed.load(Ordering::SeqCst))
            .map(|binding| {
                super::tools::EnvironmentScope::new(
                    self.clone(),
                    session.clone(),
                    binding.key.clone(),
                    binding.manifest.clone(),
                    binding.scope_lease.clone(),
                )
            }))
    }

    pub(crate) async fn bind_peer(
        &self,
        key: &EnvironmentKey,
        peer: Arc<super::transport::Peer>,
    ) -> anyhow::Result<()> {
        let mut bindings = self.bindings.lock().await;
        let binding = bindings
            .values_mut()
            .find(|b| b.key == *key)
            .ok_or_else(|| anyhow::anyhow!("Environment unavailable"))?;
        ensure!(
            binding.peer.is_none() && !self.closed.load(Ordering::SeqCst),
            "Transport already bound or gateway stopping"
        );
        binding.peer = Some(peer);
        Ok(())
    }

    pub(crate) async fn invoke(&self, invocation: Invocation) -> anyhow::Result<OperationReceipt> {
        self.authorize(&invocation).await?;
        let (device, peer) = {
            let bindings = self.bindings.lock().await;
            let binding = bindings
                .get(&invocation.session_id)
                .ok_or_else(|| anyhow::anyhow!("Environment unavailable"))?;
            ensure!(binding.key == invocation.environment, "Stale environment");
            (
                binding.device,
                binding
                    .peer
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("Transport unavailable"))?,
            )
        };
        let mut pending = peer.register(invocation.clone())?;
        let saved = self.store.remote_intent(device, invocation.clone()).await?;
        if saved.state != OperationState::NotSent {
            return Ok(saved);
        }
        self.authorize(&invocation).await?;
        if !self
            .store
            .remote_dispatch(device, invocation.clone())
            .await?
        {
            return self
                .store
                .remote_operation(&invocation.run_id, &invocation.call_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Remote evidence missing"));
        }
        // One-shot durable gate has committed: any delivery uncertainty stays unknown, never retried.
        if peer
            .outgoing
            .try_send(ServerFrame::Invoke {
                invocation: invocation.clone(),
            })
            .is_ok()
        {
            let mut closed = peer.closed.subscribe();
            let timeout = (invocation.deadline - chrono::Utc::now())
                .to_std()
                .unwrap_or_default();
            tokio::select! {
                result=&mut pending.result=>if let Ok(receipt)=result {return Ok(receipt);},
                _=async {let _=closed.wait_for(|closed|*closed).await;}=>{},
                _=tokio::time::sleep(timeout)=>{},
            }
        } else {
            peer.close();
        }
        self.store
            .remote_call_uncertain(device, invocation.clone())
            .await?;
        self.store
            .remote_operation(&invocation.run_id, &invocation.call_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Remote evidence missing"))
    }

    pub(crate) async fn disconnect(&self, key: &EnvironmentKey) -> anyhow::Result<()> {
        self.revoke(key, "Transport disconnected").await?;
        self.store.remote_disconnect(key.clone()).await
    }

    pub(crate) async fn operation(
        &self,
        run: &str,
        call: &str,
    ) -> anyhow::Result<Option<OperationReceipt>> {
        self.store.remote_operation(run, call).await
    }
    pub(crate) async fn receipt(
        &self,
        device: Uuid,
        receipt: OperationReceipt,
    ) -> anyhow::Result<()> {
        self.store.remote_receipt(device, receipt).await
    }
    pub(crate) async fn result_call(
        &self,
        device: Uuid,
        run: &str,
        call: &str,
    ) -> anyhow::Result<OperationReceipt> {
        self.store
            .remote_result_call(device, run.into(), call.into())
            .await
    }
}
