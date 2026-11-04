use super::protocol::*;
use crate::agent::tools::ToolOutput;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::sync::{mpsc, oneshot, watch};

type CallId = (String, String);
pub(crate) struct LeaseTimer {
    awake: std::time::Duration,
    continuous: std::time::Duration,
    renewed: std::time::Duration,
    invalid: bool,
}
impl LeaseTimer {
    pub(crate) fn new(awake: std::time::Duration, continuous: std::time::Duration) -> Self {
        Self {
            awake,
            continuous,
            renewed: continuous,
            invalid: false,
        }
    }
    pub(crate) fn observe(
        &mut self,
        awake: std::time::Duration,
        continuous: std::time::Duration,
    ) -> bool {
        let elapsed = continuous.saturating_sub(self.continuous);
        let active = awake.saturating_sub(self.awake);
        self.invalid |= continuous < self.continuous
            || awake < self.awake
            || elapsed.saturating_sub(active) > std::time::Duration::from_secs(1)
            || continuous.saturating_sub(self.renewed) >= super::service::LEASE;
        self.awake = awake;
        self.continuous = continuous;
        !self.invalid
    }
    pub(crate) fn expired(&self) -> bool {
        self.invalid
    }
    pub(crate) fn renew(&mut self) {
        if !self.invalid {
            self.renewed = self.continuous;
        }
    }
}
pub(crate) struct LeaseClock {
    start: tokio::time::Instant,
    renewed: tokio::time::Instant,
    timer: LeaseTimer,
}
impl LeaseClock {
    pub(crate) fn new() -> Self {
        Self {
            start: tokio::time::Instant::now(),
            renewed: tokio::time::Instant::now(),
            timer: LeaseTimer::new(std::time::Duration::ZERO, continuous_time()),
        }
    }
    pub(crate) fn live(&mut self) -> bool {
        let live = self.timer.observe(self.start.elapsed(), continuous_time());
        if self.renewed.elapsed() >= super::service::LEASE {
            self.timer.invalid = true;
        }
        live && !self.timer.expired()
    }
    pub(crate) fn renew(&mut self) -> bool {
        let live = self.live();
        self.timer.renew();
        if live {
            self.renewed = tokio::time::Instant::now();
        }
        live
    }
}
#[allow(deprecated)] // libc exposes the native clock; no additional Mach dependency is needed.
fn continuous_time() -> std::time::Duration {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn mach_continuous_time() -> u64;
        }
        let mut scale = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: mach timebase writes this live POD; continuous clock includes suspend.
        let nanos = unsafe {
            libc::mach_timebase_info(&mut scale);
            u128::from(mach_continuous_time()) * u128::from(scale.numer) / u128::from(scale.denom)
        };
        std::time::Duration::from_nanos(nanos.min(u128::from(u64::MAX)) as u64)
    }
    #[cfg(target_os = "linux")]
    {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: boottime writes a live timespec and includes suspend.
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) },
            0
        );
        std::time::Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
    }
}
pub(crate) struct Peer {
    pub(crate) outgoing: mpsc::Sender<ServerFrame>,
    pub(crate) closed: watch::Sender<bool>,
    pending: Mutex<HashMap<CallId, oneshot::Sender<OperationReceipt>>>,
}
pub(crate) struct Pending {
    peer: Arc<Peer>,
    call: Invocation,
    pub(crate) result: oneshot::Receiver<OperationReceipt>,
}
impl Peer {
    pub(crate) fn new() -> (Arc<Self>, mpsc::Receiver<ServerFrame>) {
        let (outgoing, receiver) = mpsc::channel(32);
        (
            Arc::new(Self {
                outgoing,
                closed: watch::channel(false).0,
                pending: Mutex::new(HashMap::new()),
            }),
            receiver,
        )
    }
    pub(crate) fn register(self: &Arc<Self>, call: Invocation) -> anyhow::Result<Pending> {
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| anyhow::anyhow!("Remote pending lock poisoned"))?;
        ensure!(
            !*self.closed.borrow() && pending.len() < 4,
            "Remote connection closed or busy"
        );
        let id = (call.run_id.clone(), call.call_id.clone());
        ensure!(!pending.contains_key(&id), "Remote call already pending");
        let (sender, result) = oneshot::channel();
        pending.insert(id, sender);
        Ok(Pending {
            peer: self.clone(),
            call,
            result,
        })
    }
    pub(crate) fn finish(&self, receipt: OperationReceipt) -> anyhow::Result<()> {
        if let Some(sender) = self
            .pending
            .lock()
            .map_err(|_| anyhow::anyhow!("Remote pending lock poisoned"))?
            .remove(&(
                receipt.invocation.run_id.clone(),
                receipt.invocation.call_id.clone(),
            ))
        {
            let _ = sender.send(receipt);
        }
        Ok(())
    }
    pub(crate) fn close(&self) {
        self.closed.send_replace(true);
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.peer.pending.lock()
            && pending
                .remove(&(self.call.run_id.clone(), self.call.call_id.clone()))
                .is_some()
            && self
                .peer
                .outgoing
                .try_send(ServerFrame::Cancel {
                    environment: self.call.environment.clone(),
                    run_id: self.call.run_id.clone(),
                    call_id: self.call.call_id.clone(),
                })
                .is_err()
        {
            self.peer.close();
        }
    }
}
use anyhow::ensure;

pub(crate) struct ResultAssembly {
    call: Invocation,
    hash: String,
    sequence: u32,
    bytes: Vec<u8>,
    finished: bool,
}
impl ResultAssembly {
    pub(crate) fn new(call: Invocation) -> anyhow::Result<Self> {
        let hash = arguments_hash(&call.arguments)?;
        Ok(Self {
            call,
            hash,
            sequence: 0,
            bytes: Vec::new(),
            finished: false,
        })
    }
    pub(crate) fn push(&mut self, frame: ClientFrame) -> anyhow::Result<Option<ToolOutput>> {
        let ClientFrame::ResultChunk {
            environment,
            run_id,
            call_id,
            arguments_hash,
            sequence,
            final_chunk,
            data,
        } = frame
        else {
            anyhow::bail!("Result chunk required")
        };
        ensure!(
            !self.finished
                && environment == self.call.environment
                && run_id == self.call.run_id
                && call_id == self.call.call_id
                && arguments_hash == self.hash
                && sequence == self.sequence,
            "Result chunk identity/sequence mismatch"
        );
        ensure!(
            data.len() <= MAX_CHUNK_BYTES && self.bytes.len() + data.len() <= MAX_OUTPUT_BYTES,
            "Result exceeds bounded output"
        );
        self.bytes.extend(data);
        self.sequence += 1;
        if final_chunk {
            self.finished = true;
            Ok(Some(serde_json::from_slice(&self.bytes)?))
        } else {
            Ok(None)
        }
    }
}
