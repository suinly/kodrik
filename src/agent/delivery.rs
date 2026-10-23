use super::{
    run_state::{RunPhase, RunRecord},
    session::{Session, SqliteSessionStore},
};
use crate::{
    Interface, MessageId,
    interfaces::{OutgoingMessage, SendRejection},
};
use anyhow::{Context, ensure};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub(crate) struct StorageFailure(anyhow::Error);
impl std::fmt::Display for StorageFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Delivery storage failed: {:#}", self.0)
    }
}
impl std::error::Error for StorageFailure {}
pub(crate) fn storage_failure(error: anyhow::Error) -> anyhow::Error {
    StorageFailure(error).into()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
enum Status {
    Pending,
    Sending,
    Sent,
    Uncertain,
    Rejected,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
enum Payload {
    Text(String),
    Rich(OutgoingMessage),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Receipt {
    id: String,
    run_id: String,
    conversation_id: String,
    interface: String,
    chat: String,
    status: Status,
    payload: Payload,
    retry_at: Option<DateTime<Utc>>,
    message_id: Option<String>,
    preview: Option<String>,
    #[serde(default)]
    cancel_requested: bool,
    #[serde(default)]
    notification: bool,
}
tokio::task_local! {
    pub(crate) static CURRENT_DELIVERY: Option<(SqliteSessionStore,String,String)>;
}
fn read(db: &Connection, id: &str) -> anyhow::Result<Option<Receipt>> {
    let bytes: Option<Vec<u8>> = db
        .query_row("SELECT payload FROM deliveries WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .optional()?;
    bytes
        .map(|v| serde_json::from_slice(&v).map_err(Into::into))
        .transpose()
}
fn write(db: &Connection, r: &Receipt) -> anyhow::Result<()> {
    db.execute("INSERT INTO deliveries VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload",params![r.id,r.run_id,r.conversation_id,serde_json::to_vec(r)?])?;
    Ok(())
}

fn cancel_receipts(db: &Connection, ids: Vec<String>) -> anyhow::Result<()> {
    for id in ids {
        let mut receipt = read(db, &id)?.context("Delivery missing")?;
        receipt.cancel_requested = true;
        if receipt.status == Status::Pending {
            receipt.status = Status::Rejected;
        }
        write(db, &receipt)?;
    }
    Ok(())
}
pub(crate) fn checkpoint_final(
    db: &Connection,
    run: &RunRecord,
    session: &Session,
) -> anyhow::Result<()> {
    if !matches!(run.phase, RunPhase::Completed) {
        return Ok(());
    }
    let Some((interface, chat)) = run.conversation_id.rsplit_once(':') else {
        return Ok(());
    };
    if !interface.starts_with("telegram:") || chat.parse::<i64>().is_err() {
        return Ok(());
    }
    let explicit:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM deliveries WHERE run_id=?1 AND json_extract(payload,'$.status')!='Rejected' AND coalesce(json_extract(payload,'$.notification'),0)=0)",[&run.id],|r|r.get(0))?;
    if explicit {
        return Ok(());
    }
    let Some(text) = session.messages().iter().rev().find_map(|m| {
        if let super::message::Message::Assistant(a) = m {
            Some(a.content.clone())
        } else {
            None
        }
    }) else {
        return Ok(());
    };
    write(
        db,
        &Receipt {
            id: run.id.clone(),
            run_id: run.id.clone(),
            conversation_id: run.conversation_id.clone(),
            interface: interface.into(),
            chat: chat.into(),
            status: Status::Pending,
            payload: Payload::Text(text),
            retry_at: None,
            message_id: None,
            preview: None,
            cancel_requested: false,
            notification: false,
        },
    )
}
impl SqliteSessionStore {
    pub(crate) async fn pending_notifications(&self) -> anyhow::Result<Vec<String>> {
        self.with_connection(|db| {
            Ok(db.prepare("SELECT id FROM deliveries WHERE json_extract(payload,'$.notification')=1 AND json_extract(payload,'$.status') IN ('Pending','Sending') ORDER BY rowid")?
                .query_map([], |row| row.get(0))?.collect::<Result<Vec<_>,_>>()?)
        }).await
    }

    pub(crate) async fn notification_destination(
        &self,
        id: &str,
    ) -> anyhow::Result<(String, String)> {
        let id = id.to_owned();
        self.with_connection(move |db| {
            let receipt = read(db, &id)?.context("Delivery missing")?;
            ensure!(receipt.notification, "Not a notification");
            Ok((receipt.interface, receipt.chat))
        })
        .await
    }

    pub(crate) async fn reject_notification(&self, id: &str) -> anyhow::Result<()> {
        let _ownership = self.execution_lock("delivery", id).await?;
        let id = id.to_owned();
        self.with_connection(move |db| {
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let mut receipt = read(&tx, &id)?.context("Delivery missing")?;
            ensure!(receipt.notification, "Not a notification");
            match receipt.status {
                Status::Pending => receipt.status = Status::Rejected,
                Status::Sending => receipt.status = Status::Uncertain,
                _ => return Ok(()),
            }
            receipt.cancel_requested = true;
            write(&tx, &receipt)?;
            tx.commit()?;
            Ok(())
        })
        .await
        .map_err(storage_failure)
    }

    pub(crate) async fn notification_result(&self, id: &str) -> anyhow::Result<Option<MessageId>> {
        let id = id.to_owned();
        self.with_connection(move |db| {
            let receipt = read(db, &id)?.context("Delivery missing")?;
            ensure!(
                matches!(
                    receipt.status,
                    Status::Pending | Status::Sending | Status::Sent
                ),
                "Notification outcome is {:?}; do not resend it with a new call",
                receipt.status
            );
            Ok(if receipt.status == Status::Sent {
                receipt.message_id.map(MessageId)
            } else {
                None
            })
        })
        .await
    }

    pub(crate) async fn deliver_web_notification(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<MessageId>> {
        let receipt = self
            .with_connection({
                let id = id.to_owned();
                move |db| read(db, &id)?.context("Delivery missing")
            })
            .await
            .map_err(storage_failure)?;
        ensure!(
            receipt.notification && receipt.interface == "web",
            "Not a web notification"
        );
        if receipt.status == Status::Sent {
            return Ok(receipt.message_id.map(MessageId));
        }
        let ownership = if receipt.chat == "new" {
            None
        } else {
            match self.execution_lock("session", &receipt.chat).await {
                Ok(ownership) => Some(ownership),
                Err(error)
                    if matches!(
                        error.downcast_ref::<std::fs::TryLockError>(),
                        Some(std::fs::TryLockError::WouldBlock)
                    ) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(storage_failure(error)),
            }
        };
        let id = id.to_owned();
        self.with_connection(move |db| {
            // Keep ownership inside the blocking write even if its async caller is cancelled.
            let _ownership = ownership;
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let mut receipt = read(&tx, &id)?.context("Delivery missing")?;
            ensure!(receipt.notification && receipt.interface == "web", "Not a web notification");
            if receipt.status == Status::Sent {
                return Ok(receipt.message_id.map(MessageId));
            }
            if receipt.status != Status::Pending || receipt.cancel_requested {
                return Ok(None);
            }
            let Payload::Text(body) = &receipt.payload else {
                anyhow::bail!("Web notifications require text");
            };
            let mut session = if receipt.chat == "new" {
                Session::new(super::session::SessionId::new(uuid::Uuid::new_v4().to_string()))
            } else {
                let bytes: Option<Vec<u8>> = tx.query_row("SELECT payload FROM sessions WHERE id=?1", [&receipt.chat], |row| row.get(0)).optional()?;
                let Some(bytes) = bytes else {
                    receipt.status = Status::Rejected;
                    write(&tx, &receipt)?;
                    tx.commit()?;
                    return Ok(None);
                };
                if super::inbox::session_busy_in(&tx, &receipt.chat)? {
                    return Ok(None);
                }
                serde_json::from_slice::<Session>(&bytes)?
            };
            session.push(super::message::Message::Assistant(super::AssistantMessage { content: body.clone() }));
            receipt.message_id = Some(format!("{}:{}", session.id().as_str(), session.messages().len()-1));
            receipt.status = Status::Sent;
            tx.execute("INSERT INTO sessions(id,payload,updated_at) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload,updated_at=excluded.updated_at",
                params![session.id().as_str(),serde_json::to_vec(&session)?,Utc::now().timestamp_nanos_opt().context("Session timestamp overflow")?])?;
            write(&tx, &receipt)?;
            tx.commit()?;
            Ok(receipt.message_id.map(MessageId))
        }).await.map_err(storage_failure)
    }

    pub(crate) async fn cancel_session_deliveries(&self, session: &str) -> anyhow::Result<()> {
        let session = session.to_owned();
        self.with_connection(move |db| {
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let ids = tx.prepare("SELECT d.id FROM deliveries d JOIN runs r ON r.id=d.run_id WHERE r.session_id=?1 AND json_extract(d.payload,'$.status') IN ('Pending','Sending')")?
                .query_map([session], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            cancel_receipts(&tx, ids)?;
            tx.commit()?;
            Ok(())
        }).await
    }

    pub(crate) async fn cancel_pending_deliveries(&self, conversation: &str) -> anyhow::Result<()> {
        let conversation = conversation.to_owned();
        self.with_connection(move|db|{
            let tx=db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let ids=tx.prepare("SELECT id FROM deliveries WHERE conversation_id=?1 AND json_extract(payload,'$.status') IN ('Pending','Sending')")?.query_map([conversation],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?;
            cancel_receipts(&tx, ids)?;
            tx.commit()?;Ok(())
        }).await
    }
    pub(crate) async fn pending_delivery_conversations(&self) -> anyhow::Result<Vec<String>> {
        self.with_connection(|db|Ok(db.prepare("SELECT DISTINCT conversation_id FROM deliveries WHERE coalesce(json_extract(payload,'$.notification'),0)=0 AND json_extract(payload,'$.status') IN ('Pending','Sending')")?.query_map([],|r|r.get(0))?.collect::<Result<Vec<_>,_>>()?)).await
    }
    pub(crate) async fn pending_deliveries(
        &self,
        conversation: &str,
    ) -> anyhow::Result<Vec<String>> {
        let conversation = conversation.to_owned();
        self.with_connection(move|db|Ok(db.prepare("SELECT id FROM deliveries WHERE conversation_id=?1 AND coalesce(json_extract(payload,'$.notification'),0)=0 AND json_extract(payload,'$.status') IN ('Pending','Sending') ORDER BY rowid")?.query_map([conversation],|r|r.get(0))?.collect::<Result<Vec<_>,_>>()?)).await
    }
    pub(crate) async fn queue_explicit(
        &self,
        run: &str,
        call: &str,
        conversation: &str,
        interface: &str,
        chat: &str,
        message: &OutgoingMessage,
    ) -> anyhow::Result<String> {
        self.queue_delivery(
            run,
            call,
            conversation,
            interface,
            chat,
            Payload::Rich(message.clone()),
            false,
        )
        .await
    }

    pub(crate) async fn queue_notification(
        &self,
        run: &str,
        call: &str,
        conversation: &str,
        interface: &str,
        chat: &str,
        text: String,
    ) -> anyhow::Result<String> {
        self.queue_delivery(
            run,
            call,
            conversation,
            interface,
            chat,
            Payload::Text(text),
            true,
        )
        .await
    }

    async fn queue_delivery(
        &self,
        run: &str,
        call: &str,
        conversation: &str,
        interface: &str,
        chat: &str,
        payload: Payload,
        notification: bool,
    ) -> anyhow::Result<String> {
        let id = format!("{run}:{call}");
        let r = Receipt {
            id: id.clone(),
            run_id: run.into(),
            conversation_id: conversation.into(),
            interface: interface.into(),
            chat: chat.into(),
            status: Status::Pending,
            payload,
            retry_at: None,
            message_id: None,
            preview: None,
            cancel_requested: false,
            notification,
        };
        self.with_connection(move |db| {
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if let Some(old) = read(&tx, &r.id)? {
                ensure!(
                    serde_json::to_vec(&old.payload)? == serde_json::to_vec(&r.payload)?
                        && old.interface == r.interface
                        && old.chat == r.chat
                        && old.notification == r.notification,
                    "Delivery identity conflict"
                );
            } else {
                write(&tx, &r)?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
        .map_err(storage_failure)?;
        Ok(id)
    }
    pub(crate) async fn deliver_receipt(
        &self,
        interface: &dyn Interface,
        id: &str,
        now: DateTime<Utc>,
        preview: Option<&MessageId>,
    ) -> anyhow::Result<Option<MessageId>> {
        self.deliver_receipt_owned(interface, id, now, preview, None)
            .await
    }
    pub(crate) async fn deliver_receipt_owned(
        &self,
        interface: &dyn Interface,
        id: &str,
        now: DateTime<Utc>,
        preview: Option<&MessageId>,
        ownership: Option<std::sync::Arc<super::session::ExecutionOwnership>>,
    ) -> anyhow::Result<Option<MessageId>> {
        let lock = if let Some(lock) = ownership {
            lock
        } else {
            match self.execution_lock("delivery", id).await {
                Ok(lock) => lock,
                Err(error)
                    if matches!(
                        error.downcast_ref::<std::fs::TryLockError>(),
                        Some(std::fs::TryLockError::WouldBlock)
                    ) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(storage_failure(error)),
            }
        };
        super::run_state::CURRENT_EXECUTION
            .scope(Some(lock), async {
                let id = id.to_owned();
                let preview = preview.map(|id| id.0.clone());
                let Some(mut receipt) = self
                    .with_connection({
                        let id = id.clone();
                        move |db| read(db, &id)
                    })
                    .await
                    .map_err(storage_failure)?
                else {
                    return Ok(None);
                };
                if receipt.interface != interface.id().0
                    || !interface.allows_recipient(&receipt.chat)
                {
                    return Ok(None);
                }
                if receipt.status == Status::Sent {
                    return Ok(receipt.message_id.map(MessageId));
                }
                if receipt.status == Status::Sending {
                    receipt.status = Status::Uncertain;
                    self.with_connection(move |db| write(db, &receipt))
                        .await
                        .map_err(storage_failure)?;
                    return Ok(None);
                }
                if receipt.status != Status::Pending || receipt.retry_at.is_some_and(|at| at > now)
                {
                    return Ok(None);
                }
                let claimed = self
                    .with_connection({
                        let id = id.clone();
                        move |db| {
                            let tx = db.transaction_with_behavior(
                                rusqlite::TransactionBehavior::Immediate,
                            )?;
                            let mut r = read(&tx, &id)?.context("Delivery missing")?;
                            if r.status != Status::Pending || r.cancel_requested {
                                return Ok(None);
                            }
                            r.status = Status::Sending;
                            if preview.is_some() {
                                r.preview = preview;
                            }
                            write(&tx, &r)?;
                            tx.commit()?;
                            Ok(Some(r))
                        }
                    })
                    .await
                    .map_err(storage_failure)?;
                let Some(claimed) = claimed else {
                    return Ok(None);
                };
                receipt = claimed;
                let started = tokio::time::Instant::now();
                let sent = tokio::time::timeout(std::time::Duration::from_secs(15), async {
                    match &receipt.payload {
                        Payload::Text(text) => match &receipt.preview {
                            Some(id) => interface
                                .edit(&receipt.chat, &MessageId(id.clone()), text)
                                .await
                                .map(|()| MessageId(id.clone())),
                            None => interface.send(&receipt.chat, text).await,
                        },
                        Payload::Rich(message) => {
                            interface.send_content(&receipt.chat, message).await
                        }
                    }
                })
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("Delivery timed out; outcome unknown")));
                let mut deferred = false;
                match &sent {
                    Ok(id) => {
                        receipt.status = Status::Sent;
                        receipt.message_id = Some(id.0.clone());
                        receipt.retry_at = None;
                    }
                    Err(error) => match error.downcast_ref::<SendRejection>() {
                        Some(rejection)
                            if rejection.error_code == 429 && rejection.retry_after.is_some() =>
                        {
                            receipt.status = Status::Pending;
                            let seconds = i64::try_from(rejection.retry_after.unwrap().max(1))
                                .context("Retry-After exceeds supported duration")?;
                            receipt.retry_at = Some(
                                (now + chrono::Duration::from_std(started.elapsed())?)
                                    .checked_add_signed(
                                        chrono::Duration::try_seconds(seconds)
                                            .context("Retry-After exceeds supported duration")?,
                                    )
                                    .context("Retry-After overflows timestamp")?,
                            );
                            deferred = true;
                        }
                        Some(_) => receipt.status = Status::Rejected,
                        None => receipt.status = Status::Uncertain,
                    },
                }
                self.with_connection(move |db| {
                    let tx =
                        db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                    let latest = read(&tx, &receipt.id)?.context("Delivery missing")?;
                    receipt.cancel_requested |= latest.cancel_requested;
                    if receipt.cancel_requested && receipt.status == Status::Pending {
                        receipt.status = Status::Rejected;
                    }
                    write(&tx, &receipt)?;
                    tx.commit()?;
                    Ok(())
                })
                .await
                .map_err(storage_failure)?;
                if deferred { Ok(None) } else { sent.map(Some) }
            })
            .await
    }
}

#[cfg(test)]
mod session_cancellation_tests {
    use super::*;
    use crate::agent::{
        AssistantMessage,
        message::Message,
        run_state::Checkpoint,
        session::{SessionId, SessionStore},
    };

    #[tokio::test]
    async fn stopping_an_old_session_cancels_its_receipts_without_touching_new_channel_work() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open_async(dir.path().into())
            .await
            .unwrap();
        let channel = SessionId::new("telegram:main:42");
        let mut runs = Vec::new();
        let mut sessions = Vec::new();
        for text in ["old reply", "new reply"] {
            let mut session = Session::new(SessionId::new(uuid::Uuid::new_v4().to_string()));
            session.push(Message::Assistant(AssistantMessage {
                content: text.into(),
            }));
            store.save(&session).await.unwrap();
            store.select(&channel, session.id()).await.unwrap();
            let mut checkpoint = Checkpoint::new(&store, &channel, &session);
            checkpoint
                .save(&session, RunPhase::Completed, None)
                .await
                .unwrap();
            sessions.push(session.id().clone());
            runs.push(checkpoint.record.id);
        }
        let old_run = runs[0].clone();
        store
            .with_connection(move |db| {
                let pending = read(db, &old_run)?.unwrap();
                for (suffix, status) in [("sending", Status::Sending), ("sent", Status::Sent)] {
                    let mut receipt = pending.clone();
                    receipt.id = format!("{old_run}:{suffix}");
                    receipt.status = status;
                    write(db, &receipt)?;
                }
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(store.active_id(&channel).await.unwrap(), sessions[1]);
        store
            .cancel_session_deliveries(sessions[0].as_str())
            .await
            .unwrap();
        store
            .with_connection(move |db| {
                let old = read(db, &runs[0])?.unwrap();
                assert_eq!(old.status, Status::Rejected);
                assert!(old.cancel_requested);
                let sending = read(db, &format!("{}:sending", runs[0]))?.unwrap();
                assert_eq!(sending.status, Status::Sending);
                assert!(sending.cancel_requested);
                let sent = read(db, &format!("{}:sent", runs[0]))?.unwrap();
                assert_eq!(sent.status, Status::Sent);
                assert!(!sent.cancel_requested);
                let new = read(db, &runs[1])?.unwrap();
                assert_eq!(new.status, Status::Pending);
                assert!(!new.cancel_requested);
                Ok(())
            })
            .await
            .unwrap();
    }
}
impl super::AgentRunner {
    pub(crate) async fn deliver_pending(
        &self,
        interface: &dyn Interface,
        conversation: &str,
    ) -> anyhow::Result<Vec<(MessageId, String)>> {
        let Some(store) = self.sessions.runtime_store() else {
            return Ok(vec![]);
        };
        let mut delivered = vec![];
        for id in store.pending_deliveries(conversation).await? {
            match store
                .deliver_receipt(interface, &id, Utc::now(), None)
                .await
            {
                Ok(Some(message_id)) => {
                    let receipt = store
                        .with_connection(move |db| read(db, &id))
                        .await
                        .map_err(storage_failure)?
                        .context("Delivery missing")?;
                    if let Some((run, session)) = store
                        .load_run(&receipt.run_id)
                        .await
                        .map_err(storage_failure)?
                        && run.phase.terminal()
                        && let Some(text) = session.messages().iter().rev().find_map(|m| {
                            if let super::message::Message::Assistant(a) = m {
                                Some(a.content.clone())
                            } else {
                                None
                            }
                        })
                    {
                        delivered.push((message_id, text));
                    }
                }
                Ok(None) => {}
                Err(error) if error.is::<StorageFailure>() => return Err(error),
                Err(error) => log::warn!("Delivery {id} not confirmed: {error:#}"),
            }
        }
        Ok(delivered)
    }
    pub(crate) async fn deliver_final(
        &self,
        interface: &dyn Interface,
        conversation: &super::session::SessionId,
        request: &str,
        preview: Option<&MessageId>,
        reply: &super::tools::telegram_reply::ReplyContext,
    ) -> anyhow::Result<Option<MessageId>> {
        let store = self
            .sessions
            .runtime_store()
            .context("Durable delivery requires SQLite")?;
        let Some(record) = store.submission(conversation.as_str(), request).await? else {
            return Ok(None);
        };
        let Some(run) = record.run_id else {
            return Ok(None);
        };
        let explicit=store.with_connection({let run=run.clone();move|db|{
            let bytes:Option<Vec<u8>>=db.query_row("SELECT payload FROM deliveries WHERE run_id=?1 AND json_extract(payload,'$.status')='Sent' AND coalesce(json_extract(payload,'$.notification'),0)=0 ORDER BY rowid DESC LIMIT 1",[run],|r|r.get(0)).optional()?;
            Ok(bytes.map(|bytes|serde_json::from_slice::<Receipt>(&bytes)).transpose()?.and_then(|r|r.message_id).map(MessageId))
        }}).await.map_err(storage_failure)?;
        if explicit.is_some() {
            return Ok(explicit);
        }
        store
            .deliver_receipt_owned(
                interface,
                &run,
                Utc::now(),
                preview,
                reply.take_final_delivery().await,
            )
            .await
    }
}
