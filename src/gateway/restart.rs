use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, ensure};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::{MessageId, gateway::MessageSource};

const VERSION: u8 = 1;
const REQUEST_FILE: &str = "restart-request.json";
const RECEIPT_FILE: &str = "restart-receipt.json";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RestartRequest {
    version: u8,
    request_id: String,
    target_pid: u32,
    created_at: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RestartReceipt {
    version: u8,
    pub(crate) request_id: String,
    pub(crate) old_pid: u32,
    pub(crate) interface: String,
    pub(crate) conversation_id: String,
    pub(crate) message_id: String,
    pub(crate) original_text: String,
    created_at: String,
}

pub(crate) struct RestartCoordinator {
    directory: PathBuf,
    state: Mutex<()>,
    shutdown: watch::Sender<bool>,
}

impl RestartCoordinator {
    pub(crate) fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            state: Mutex::new(()),
            shutdown: watch::channel(false).0,
        }
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    pub(crate) fn request(&self, target_pid: u32) -> anyhow::Result<()> {
        ensure!(target_pid > 0, "Gateway PID must be positive");
        let _guard = self.state.lock().unwrap();
        fs::create_dir_all(&self.directory)?;
        ensure!(
            !self.directory.join(RECEIPT_FILE).exists(),
            "A restart confirmation is still pending"
        );
        let request = RestartRequest {
            version: VERSION,
            request_id: uuid::Uuid::new_v4().to_string(),
            target_pid,
            created_at: Utc::now().to_rfc3339(),
        };
        write_new(&self.directory.join(REQUEST_FILE), &request)
            .context("A restart request is already pending")
    }

    pub(crate) fn complete_response(
        &self,
        source: &MessageSource,
        message_id: &MessageId,
        original_text: &str,
    ) -> anyhow::Result<bool> {
        let _guard = self.state.lock().unwrap();
        let Some(request) = read_optional::<RestartRequest>(&self.directory.join(REQUEST_FILE))?
        else {
            return Ok(false);
        };
        ensure!(
            request.version == VERSION,
            "Unsupported restart request version"
        );
        if request.target_pid != std::process::id() {
            return Ok(false);
        }

        let receipt_path = self.directory.join(RECEIPT_FILE);
        let receipt = RestartReceipt {
            version: VERSION,
            request_id: request.request_id,
            old_pid: request.target_pid,
            interface: source.interface.0.clone(),
            conversation_id: source.conversation_id.clone(),
            message_id: message_id.0.clone(),
            original_text: original_text.to_owned(),
            created_at: Utc::now().to_rfc3339(),
        };
        write_new(&receipt_path, &receipt).context("Cannot persist restart receipt")?;
        if let Err(error) = fs::remove_file(self.directory.join(REQUEST_FILE)) {
            let _ = fs::remove_file(receipt_path);
            return Err(error).context("Cannot consume restart request");
        }
        log::info!(
            "Restart response delivered; stopping gateway PID {}",
            request.target_pid
        );
        self.shutdown.send_replace(true);
        Ok(true)
    }

    pub(crate) fn receipt(&self) -> anyhow::Result<Option<RestartReceipt>> {
        let _guard = self.state.lock().unwrap();
        let receipt = read_optional::<RestartReceipt>(&self.directory.join(RECEIPT_FILE))?;
        if let Some(receipt) = &receipt {
            ensure!(
                receipt.version == VERSION,
                "Unsupported restart receipt version"
            );
        }
        Ok(receipt)
    }

    pub(crate) async fn confirm_once(
        &self,
        interface: &dyn crate::Interface,
    ) -> anyhow::Result<bool> {
        let Some(receipt) = self.receipt()? else {
            return Ok(true);
        };
        if receipt.old_pid == std::process::id() {
            return Ok(false);
        }
        ensure!(
            interface.id().0 == receipt.interface,
            "Restart receipt interface is unavailable"
        );
        let text = match interface.language(&receipt.conversation_id) {
            crate::i18n::Language::En => format!(
                "Gateway restarted successfully. PID: `{}`.",
                std::process::id()
            ),
            crate::i18n::Language::Ru => {
                format!("Шлюз успешно перезапущен. PID: `{}`.", std::process::id())
            }
        };
        interface.send(&receipt.conversation_id, &text).await?;
        let _guard = self.state.lock().unwrap();
        let request_path = self.directory.join(REQUEST_FILE);
        if let Some(request) = read_optional::<RestartRequest>(&request_path)?
            && request.request_id == receipt.request_id
        {
            fs::remove_file(request_path)?;
        }
        match fs::remove_file(self.directory.join(RECEIPT_FILE)) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) async fn confirm(self: Arc<Self>, interface: Arc<dyn crate::Interface>) {
        for delay in [0, 1, 5, 30, 120].into_iter().chain(std::iter::repeat(300)) {
            if delay > 0 {
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
            match self.confirm_once(interface.as_ref()).await {
                Ok(true) => return,
                Ok(false) => return,
                Err(error) => log::warn!("Restart confirmation failed: {error:#}"),
            }
        }
    }

    pub(crate) fn quarantine_receipt(&self, request_id: Option<&str>) -> anyhow::Result<()> {
        let _guard = self.state.lock().unwrap();
        let source = self.directory.join(RECEIPT_FILE);
        if !source.exists() {
            return Ok(());
        }
        let id = request_id
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
            .unwrap_or_else(uuid::Uuid::new_v4);
        fs::rename(
            source,
            self.directory
                .join(format!("restart-receipt.{id}.invalid.json")),
        )?;
        Ok(())
    }
}

fn read_optional<T: for<'de> Deserialize<'de>>(path: &Path) -> anyhow::Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_new(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    let directory = path.parent().context("Restart state has no directory")?;
    fs::create_dir_all(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(&serde_json::to_vec(value)?)?;
    temporary.as_file().sync_all()?;
    temporary.persist_noclobber(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MessageId;
    use crate::{
        Interface,
        gateway::{Event, InterfaceId, MessageSource},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::sync::mpsc;

    fn source() -> MessageSource {
        MessageSource {
            interface: InterfaceId("telegram:main".into()),
            conversation_id: "42".into(),
        }
    }

    struct SendingInterface {
        sends: Arc<AtomicUsize>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl Interface for SendingInterface {
        fn id(&self) -> InterfaceId {
            InterfaceId("telegram:main".into())
        }

        async fn run(&self, _: mpsc::Sender<Event>) -> anyhow::Result<()> {
            Ok(())
        }

        async fn send(&self, chat: &str, text: &str) -> anyhow::Result<MessageId> {
            assert_eq!(chat, "42");
            assert_eq!(
                text,
                format!(
                    "Gateway restarted successfully. PID: `{}`.",
                    std::process::id()
                )
            );
            self.sends.fetch_add(1, Ordering::SeqCst);
            anyhow::ensure!(!self.fail, "temporary failure");
            Ok(MessageId("8".into()))
        }

        async fn edit(&self, _: &str, _: &MessageId, _: &str) -> anyhow::Result<()> {
            panic!("restart confirmation must be a new message")
        }
    }

    fn receipt_for_new_process(directory: &Path) {
        let old_pid = std::process::id() + 1;
        write_new(
            &directory.join(REQUEST_FILE),
            &RestartRequest {
                version: VERSION,
                request_id: "request".into(),
                target_pid: old_pid,
                created_at: Utc::now().to_rfc3339(),
            },
        )
        .unwrap();
        write_new(
            &directory.join(RECEIPT_FILE),
            &RestartReceipt {
                version: VERSION,
                request_id: "request".into(),
                old_pid,
                interface: "telegram:main".into(),
                conversation_id: "42".into(),
                message_id: "7".into(),
                original_text: "Update complete".into(),
                created_at: Utc::now().to_rfc3339(),
            },
        )
        .unwrap();
    }

    #[test]
    fn matching_request_becomes_receipt_before_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        let restart = RestartCoordinator::new(directory.path().into());
        let mut shutdown = restart.subscribe();

        restart.request(std::process::id()).unwrap();
        assert!(
            restart
                .complete_response(&source(), &MessageId("7".into()), "Update complete")
                .unwrap()
        );

        assert!(*shutdown.borrow_and_update());
        assert!(!directory.path().join("restart-request.json").exists());
        let receipt = restart.receipt().unwrap().unwrap();
        assert_eq!(receipt.old_pid, std::process::id());
        assert_eq!(receipt.interface, "telegram:main");
        assert_eq!(receipt.conversation_id, "42");
        assert_eq!(receipt.message_id, "7");
        assert_eq!(receipt.original_text, "Update complete");
    }

    #[test]
    fn request_does_not_overwrite_pending_state() {
        let directory = tempfile::tempdir().unwrap();
        let restart = RestartCoordinator::new(directory.path().into());

        restart.request(std::process::id()).unwrap();
        assert!(restart.request(std::process::id()).is_err());
        assert!(directory.path().join("restart-request.json").exists());
    }

    #[test]
    fn stale_request_cannot_stop_another_process() {
        let directory = tempfile::tempdir().unwrap();
        let restart = RestartCoordinator::new(directory.path().into());
        let shutdown = restart.subscribe();

        restart.request(std::process::id() + 1).unwrap();
        assert!(
            !restart
                .complete_response(&source(), &MessageId("7".into()), "Update complete")
                .unwrap()
        );

        assert!(!*shutdown.borrow());
        assert!(directory.path().join("restart-request.json").exists());
        assert!(restart.receipt().unwrap().is_none());
    }

    #[test]
    fn malformed_request_never_signals_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("restart-request.json"), "not json").unwrap();
        let restart = RestartCoordinator::new(directory.path().into());
        let shutdown = restart.subscribe();

        assert!(
            restart
                .complete_response(&source(), &MessageId("7".into()), "Update complete")
                .is_err()
        );
        assert!(!*shutdown.borrow());
        assert!(restart.receipt().unwrap().is_none());
    }

    #[tokio::test]
    async fn confirmation_sends_new_message_once_and_removes_receipt() {
        let directory = tempfile::tempdir().unwrap();
        receipt_for_new_process(directory.path());
        let restart = RestartCoordinator::new(directory.path().into());
        let sends = Arc::new(AtomicUsize::new(0));
        let interface = SendingInterface {
            sends: sends.clone(),
            fail: false,
        };

        assert!(restart.confirm_once(&interface).await.unwrap());

        assert!(restart.confirm_once(&interface).await.unwrap());
        assert_eq!(sends.load(Ordering::SeqCst), 1);
        assert!(restart.receipt().unwrap().is_none());
        assert!(!directory.path().join(REQUEST_FILE).exists());
    }

    #[tokio::test]
    async fn failed_confirmation_keeps_receipt_for_retry() {
        let directory = tempfile::tempdir().unwrap();
        receipt_for_new_process(directory.path());
        let restart = RestartCoordinator::new(directory.path().into());
        let interface = SendingInterface {
            sends: Arc::default(),
            fail: true,
        };

        assert!(restart.confirm_once(&interface).await.is_err());
        assert!(restart.receipt().unwrap().is_some());
    }
}
