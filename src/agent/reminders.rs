use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
};

use anyhow::{Context, ensure};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Recipient {
    pub interface: String,
    pub chat: String,
}

tokio::task_local! {
    pub(crate) static WAKEUP_CONTEXT: Option<WakeupContext>;
}

#[derive(Clone)]
pub(crate) struct WakeupContext {
    pub recipient: Recipient,
    pub session: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Pending,
    Running,
    Ready,
    Completed,
    Sending,
    Sent,
    Cancelled,
    Uncertain,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DurableWakeup {
    pub request: crate::agent::inbox::SubmissionRequest,
    pub deadline_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reminder {
    pub id: String,
    pub recipient: Recipient,
    pub text: String,
    pub due_at: DateTime<Utc>,
    pub status: Status,
    pub retry_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durable: Option<DurableWakeup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creation_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creation_hash: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Action {
    Create,
    List,
    Reschedule,
    Cancel,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReminderRequest {
    pub action: Action,
    pub id: Option<String>,
    pub text: Option<String>,
    pub due_at: Option<String>,
}

#[derive(Clone)]
pub(crate) struct ReminderStore {
    directory: PathBuf,
    journal: &'static str,
    // A cancelled async caller cannot release ownership while its blocking write lives.
    _scheduler_owner: Option<std::sync::Arc<File>>,
}

impl ReminderStore {
    pub(crate) fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            journal: "reminders",
            _scheduler_owner: None,
        }
    }

    pub(crate) fn wakeups(directory: impl Into<PathBuf>) -> Self {
        Self {
            journal: "wakeups",
            ..Self::new(directory)
        }
    }

    pub(crate) fn with_scheduler_owner(mut self, owner: File) -> Self {
        self._scheduler_owner = Some(std::sync::Arc::new(owner));
        self
    }

    fn open_lock(&self, name: &str) -> anyhow::Result<File> {
        fs::create_dir_all(&self.directory)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        Ok(options.open(self.directory.join(name))?)
    }

    pub(crate) fn scheduler_lock(&self) -> anyhow::Result<Option<File>> {
        let file = self.open_lock(&format!("{}-scheduler.lock", self.journal))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }

    fn read(&self) -> anyhow::Result<Vec<Reminder>> {
        match fs::read(self.directory.join(format!("{}.json", self.journal))) {
            Ok(bytes) => {
                ensure!(bytes.len() <= MAX_BYTES, "Reminders journal exceeds 8 MiB");
                let records: Vec<Reminder> = serde_json::from_slice(&bytes)?;
                let mut ids = std::collections::HashSet::new();
                for record in &records {
                    uuid::Uuid::parse_str(&record.id).context("Invalid reminder id")?;
                    ensure!(ids.insert(&record.id), "Duplicate reminder id");
                    validate_recipient(&record.recipient)?;
                    self.validate_instruction(&record.text)?;
                    if self.journal == "wakeups" {
                        ensure!(record.status != Status::Sent, "Invalid wakeup state: sent");
                        validate_session(
                            record
                                .source_session
                                .as_deref()
                                .context("Missing wakeup source session")?,
                        )?;
                        if let Some(message) = &record.message {
                            validate_text(message)?;
                        }
                        ensure!(
                            !matches!(record.status, Status::Ready | Status::Sending)
                                || record.message.is_some(),
                            "Missing wakeup delivery result"
                        );
                    } else {
                        ensure!(
                            !matches!(
                                record.status,
                                Status::Running | Status::Ready | Status::Completed
                            ),
                            "Invalid legacy reminder state"
                        );
                    }
                }
                Ok(records)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    }

    fn update<T>(
        &self,
        operation: impl FnOnce(&mut Vec<Reminder>) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let lock = self.open_lock(&format!("{}.lock", self.journal))?;
        lock.lock()?;
        let mut records = self.read()?;
        let before = serde_json::to_vec(&records)?;
        let result = operation(&mut records)?;
        let bytes = serde_json::to_vec(&records)?;
        ensure!(
            bytes.len() <= MAX_BYTES,
            "Reminders journal is full (8 MiB); no records were removed"
        );
        if before != bytes {
            let mut file = tempfile::NamedTempFile::new_in(&self.directory)?;
            file.write_all(&bytes)?;
            file.as_file().sync_all()?;
            file.persist(self.directory.join(format!("{}.json", self.journal)))?;
            File::open(&self.directory)?.sync_all()?;
        }
        Ok(result)
    }

    #[cfg(test)]
    pub(crate) fn execute(
        &self,
        recipient: &Recipient,
        request: ReminderRequest,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Value> {
        self.execute_scoped(recipient, None, request, now, None)
    }

    #[cfg(test)]
    pub(crate) fn execute_wakeup(
        &self,
        recipient: &Recipient,
        session: &str,
        request: ReminderRequest,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Value> {
        ensure!(self.journal == "wakeups", "Not a wakeup store");
        validate_session(session)?;
        self.execute_scoped(recipient, Some(session), request, now, None)
    }

    pub(crate) fn execute_wakeup_idempotent(
        &self,
        recipient: &Recipient,
        session: &str,
        request: ReminderRequest,
        now: DateTime<Utc>,
        key: Option<String>,
    ) -> anyhow::Result<Value> {
        ensure!(self.journal == "wakeups", "Not a wakeup store");
        validate_session(session)?;
        self.execute_scoped(recipient, Some(session), request, now, key)
    }

    fn validate_instruction(&self, text: &str) -> anyhow::Result<()> {
        if self.journal == "wakeups" {
            ensure!(
                !text.trim().is_empty() && text.chars().count() <= 6000,
                "Wakeup instruction must contain 1–6000 characters"
            );
            Ok(())
        } else {
            validate_text(text)
        }
    }

    fn execute_scoped(
        &self,
        recipient: &Recipient,
        session: Option<&str>,
        request: ReminderRequest,
        now: DateTime<Utc>,
        creation_key: Option<String>,
    ) -> anyhow::Result<Value> {
        validate_recipient(recipient)?;
        let singular = if self.journal == "wakeups" {
            "wakeup"
        } else {
            "reminder"
        };
        self.update(|records| {
            match request.action {
                Action::List => {
                    ensure!(request.id.is_none() && request.text.is_none() && request.due_at.is_none(), "list takes no fields");
                    Ok(json!({self.journal: records.iter().filter(|r| &r.recipient == recipient && !matches!(r.status, Status::Sent | Status::Completed | Status::Cancelled)).collect::<Vec<_>>() }))
                },
                Action::Create => {
                    ensure!(request.id.is_none(), "create does not take an id");
                    use sha2::Digest;
                    let creation_hash=creation_key.as_ref().map(|_|format!("{:x}",sha2::Sha256::digest(serde_json::to_vec(&json!({"recipient":recipient,"source":session,"text":request.text,"due_at":request.due_at})).unwrap())));
                    if let Some(key)=&creation_key && let Some(old)=records.iter().find(|r|r.creation_key.as_ref()==Some(key)) {
                        ensure!(old.creation_hash==creation_hash,"Wakeup creation identity payload conflict");return Ok(json!({singular:old}));
                    }
                    let text = request.text.context("text required")?;
                    self.validate_instruction(&text)?;
                    let due_at = parse_due(request.due_at, now)?;
                    ensure!(self.journal != "wakeups" || session.is_some(), "Wakeup source session required");
                    let reminder = Reminder { id: uuid::Uuid::new_v4().to_string(), recipient: recipient.clone(), text, due_at, status: Status::Pending, retry_at: None, last_error: None, source_session: session.map(str::to_owned), message: None, durable: None,creation_key,creation_hash };
                    let output = json!({singular: reminder});
                    records.push(reminder);
                    Ok(output)
                },
                Action::Cancel | Action::Reschedule => {
                    ensure!(request.text.is_none(), "text cannot be changed");
                    let id = request.id.context("id required")?;
                    let record = records.iter_mut().find(|r| r.id == id && &r.recipient == recipient).context("Reminder not found in this chat")?;
                    ensure!(record.status == Status::Pending, "Only pending reminders can be cancelled or rescheduled");
                    if matches!(request.action, Action::Cancel) {
                        ensure!(request.due_at.is_none(), "cancel does not take a time");
                        record.status = Status::Cancelled;
                    } else {
                        record.due_at = parse_due(request.due_at, now)?;
                        record.retry_at = None;
                        record.last_error = None;
                    }
                    Ok(json!({singular: record}))
                },
            }
        })
    }

    // ponytail: scan the bounded journal; index pending entries if polling cost becomes material.
    pub(crate) fn due(&self, now: DateTime<Utc>) -> anyhow::Result<Vec<Reminder>> {
        self.update(|records| {
            let mut due: Vec<_> = records.iter().filter(|r| ready(r, now)).cloned().collect();
            due.sort_by_key(|r| r.due_at);
            Ok(due)
        })
    }

    pub(crate) fn claim(&self, id: &str, now: DateTime<Utc>) -> anyhow::Result<Option<Reminder>> {
        self.update(|records| {
            let Some(record) = records
                .iter_mut()
                .find(|r| r.id == id && r.status == Status::Pending && ready(r, now))
            else {
                return Ok(None);
            };
            record.status = if self.journal == "wakeups" {
                Status::Running
            } else {
                Status::Sending
            };
            Ok(Some(record.clone()))
        })
    }

    pub(crate) fn claim_durable(
        &self,
        id: &str,
        now: DateTime<Utc>,
        request: crate::agent::inbox::SubmissionRequest,
    ) -> anyhow::Result<Option<Reminder>> {
        self.update(|records| {
            let Some(record) = records.iter_mut().find(|r| r.id == id) else {
                return Ok(None);
            };
            if record.status == Status::Running && record.durable.is_some() {
                return Ok(Some(record.clone()));
            }
            if record.status != Status::Pending || !ready(record, now) {
                return Ok(None);
            }
            record.durable = Some(DurableWakeup {
                request,
                deadline_at: now + chrono::Duration::minutes(5),
            });
            record.status = Status::Running;
            Ok(Some(record.clone()))
        })
    }

    pub(crate) fn complete_execution(
        &self,
        id: &str,
        message: Option<String>,
    ) -> anyhow::Result<()> {
        if let Some(message) = &message {
            validate_text(message)?;
        }
        self.update(|records| {
            let record = records
                .iter_mut()
                .find(|r| r.id == id)
                .context("Wakeup disappeared")?;
            ensure!(record.status == Status::Running, "Wakeup is not running");
            record.status = if message.is_some() {
                Status::Ready
            } else {
                Status::Completed
            };
            record.message = message;
            record.last_error = None;
            record.retry_at = None;
            Ok(())
        })
    }

    pub(crate) fn claim_delivery(
        &self,
        id: &str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Option<Reminder>> {
        self.update(|records| {
            let Some(record) = records
                .iter_mut()
                .find(|r| r.id == id && r.status == Status::Ready && ready(r, now))
            else {
                return Ok(None);
            };
            record.status = Status::Sending;
            Ok(Some(record.clone()))
        })
    }

    pub(crate) fn finish(
        &self,
        id: &str,
        status: Status,
        error: Option<String>,
        retry_at: Option<DateTime<Utc>>,
    ) -> anyhow::Result<()> {
        self.update(|records| {
            let record = records
                .iter_mut()
                .find(|r| r.id == id)
                .context("Reminder disappeared")?;
            ensure!(
                matches!(record.status, Status::Sending | Status::Running),
                "Task is neither running nor sending"
            );
            record.status = status;
            record.last_error = error;
            record.retry_at = retry_at;
            Ok(())
        })
    }

    pub(crate) fn note_unavailable(&self, id: &str, error: String) -> anyhow::Result<()> {
        self.update(|records| {
            if let Some(record) = records
                .iter_mut()
                .find(|r| r.id == id && matches!(r.status, Status::Pending | Status::Ready))
            {
                record.last_error = Some(error);
            }
            Ok(())
        })
    }

    pub(crate) fn recover(&self) -> anyhow::Result<()> {
        self.update(|records| {
            for record in records.iter_mut().filter(|r| matches!(r.status, Status::Sending | Status::Running)) {
                if record.status==Status::Running && record.durable.is_some() {continue;}
                record.status = Status::Uncertain;
                record.last_error = Some(
                    "Gateway stopped during execution or delivery; outcome unknown, no automatic retry".into(),
                );
            }
            Ok(())
        })
    }
}

fn ready(record: &Reminder, now: DateTime<Utc>) -> bool {
    (matches!(record.status, Status::Pending | Status::Ready)
        || (record.status == Status::Running && record.durable.is_some()))
        && record.due_at <= now
        && record.retry_at.is_none_or(|retry| retry <= now)
}
fn validate_session(session: &str) -> anyhow::Result<()> {
    ensure!(
        !session.is_empty()
            && session.len() <= 255
            && session != "."
            && session != ".."
            && !session.contains(['/', '\\'])
            && !session.chars().any(char::is_control),
        "Invalid wakeup source session"
    );
    Ok(())
}
fn validate_recipient(recipient: &Recipient) -> anyhow::Result<()> {
    ensure!(
        recipient
            .interface
            .strip_prefix("telegram:")
            .is_some_and(|tag| !tag.is_empty())
            && recipient.chat.parse::<i64>().is_ok(),
        "Scheduled tasks require a Telegram source chat"
    );
    Ok(())
}
fn validate_text(text: &str) -> anyhow::Result<()> {
    ensure!(
        !text.trim().is_empty() && text.chars().count() <= 1200,
        "Reminder text must contain 1–1200 characters"
    );
    Ok(())
}
fn parse_due(value: Option<String>, now: DateTime<Utc>) -> anyhow::Result<DateTime<Utc>> {
    let value = value.context("due_at required")?;
    let due = DateTime::parse_from_rfc3339(&value)
        .context("due_at must be RFC 3339 with an explicit offset")?
        .with_timezone(&Utc);
    ensure!(due > now, "Reminder time must be in the future");
    Ok(due)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wakeups_preserve_source_and_ready_results_across_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let store = ReminderStore::wakeups(directory.path());
        let now = Utc::now();
        let owner = recipient("42");
        let created = store.execute_wakeup(&owner, "source-session", request(json!({
            "action":"create", "text":"Check the build", "due_at":(now + chrono::Duration::seconds(1)).to_rfc3339()
        })), now).unwrap();
        let id = created["wakeup"]["id"].as_str().unwrap();
        assert!(
            ReminderStore::new(directory.path())
                .due(now + chrono::Duration::seconds(2))
                .unwrap()
                .is_empty()
        );
        let claimed = store
            .claim(id, now + chrono::Duration::seconds(2))
            .unwrap()
            .unwrap();
        assert_eq!(claimed.source_session.as_deref(), Some("source-session"));
        assert_eq!(claimed.status, Status::Running);
        assert!(
            store
                .claim(id, now + chrono::Duration::seconds(2))
                .unwrap()
                .is_none()
        );
        store
            .complete_execution(id, Some("Build passed".into()))
            .unwrap();
        store.recover().unwrap();
        let reopened = ReminderStore::wakeups(directory.path());
        let ready = reopened.due(now + chrono::Duration::seconds(2)).unwrap();
        assert_eq!(ready[0].status, Status::Ready);
        assert_eq!(ready[0].message.as_deref(), Some("Build passed"));
        assert!(
            reopened
                .claim(id, now + chrono::Duration::seconds(2))
                .unwrap()
                .is_none()
        );
        assert!(
            reopened
                .claim_delivery(id, now + chrono::Duration::seconds(2))
                .unwrap()
                .is_some()
        );
        reopened.recover().unwrap();
        assert!(
            reopened
                .due(now + chrono::Duration::seconds(2))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn wakeups_running_is_uncertain_and_silent_completion_is_terminal() {
        let directory = tempfile::tempdir().unwrap();
        let store = ReminderStore::wakeups(directory.path());
        let now = Utc::now();
        for silent in [false, true] {
            let created = store.execute_wakeup(&recipient("42"), "session", request(json!({
                "action":"create", "text":"Check", "due_at":(now + chrono::Duration::seconds(1)).to_rfc3339()
            })), now).unwrap();
            let id = created["wakeup"]["id"].as_str().unwrap();
            store
                .claim(id, now + chrono::Duration::seconds(2))
                .unwrap()
                .unwrap();
            if silent {
                store.complete_execution(id, None).unwrap();
            }
            store.recover().unwrap();
        }
        assert!(
            store
                .due(now + chrono::Duration::seconds(2))
                .unwrap()
                .is_empty()
        );
        let listed = store
            .execute_wakeup(
                &recipient("42"),
                "session",
                request(json!({"action":"list"})),
                now,
            )
            .unwrap();
        assert_eq!(listed["wakeups"].as_array().unwrap().len(), 1);
        assert_eq!(listed["wakeups"][0]["status"], "uncertain");
    }

    #[test]
    fn wakeups_reject_impossible_persisted_states_without_rewriting_journal() {
        let directory = tempfile::tempdir().unwrap();
        let store = ReminderStore::wakeups(directory.path());
        let now = Utc::now();
        let created = store.execute_wakeup(&recipient("42"), "session", request(json!({"action":"create", "text":"Check", "due_at":(now + chrono::Duration::hours(1)).to_rfc3339()})), now).unwrap();
        let mut record = created["wakeup"].clone();
        record["status"] = json!("sent");
        let bytes = serde_json::to_vec(&json!([record])).unwrap();
        std::fs::write(directory.path().join("wakeups.json"), &bytes).unwrap();
        assert!(store.due(now).is_err());
        assert_eq!(
            std::fs::read(directory.path().join("wakeups.json")).unwrap(),
            bytes
        );
    }

    fn recipient(chat: &str) -> Recipient {
        Recipient {
            interface: "telegram:test".into(),
            chat: chat.into(),
        }
    }
    fn request(value: serde_json::Value) -> ReminderRequest {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn reminders_persist_manage_and_isolate_chats() {
        let directory = tempfile::tempdir().unwrap();
        let store = ReminderStore::new(directory.path());
        let now = Utc::now();
        let due = now + chrono::Duration::minutes(10);
        let owner = recipient("42");
        let created = store
            .execute(
                &owner,
                request(json!({"action":"create","text":"Call doctor","due_at":due.to_rfc3339()})),
                now,
            )
            .unwrap();
        let id = created["reminder"]["id"].as_str().unwrap();
        let reloaded = ReminderStore::new(directory.path());
        assert_eq!(
            reloaded
                .execute(&owner, request(json!({"action":"list"})), now)
                .unwrap()["reminders"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            reloaded
                .execute(&recipient("7"), request(json!({"action":"list"})), now)
                .unwrap()["reminders"],
            json!([])
        );
        assert!(
            reloaded
                .execute(
                    &recipient("7"),
                    request(json!({"action":"cancel","id":id})),
                    now
                )
                .is_err()
        );
        assert!(store.due(now).unwrap().is_empty());
        assert_eq!(store.due(due).unwrap().len(), 1);
        let later = due + chrono::Duration::hours(1);
        store
            .execute(
                &owner,
                request(json!({"action":"reschedule","id":id,"due_at":later.to_rfc3339()})),
                now,
            )
            .unwrap();
        assert!(store.claim(id, due).unwrap().is_none());
        store
            .execute(&owner, request(json!({"action":"cancel","id":id})), now)
            .unwrap();
        assert!(store.claim(id, later).unwrap().is_none());
        assert!(
            store
                .execute(
                    &owner,
                    request(json!({"action":"reschedule","id":id,"due_at":later.to_rfc3339()})),
                    now
                )
                .is_err()
        );
    }

    #[test]
    fn reminders_reject_invalid_data_and_preserve_corrupt_journal() {
        let directory = tempfile::tempdir().unwrap();
        let store = ReminderStore::new(directory.path());
        let now = Utc::now();
        for (text, time) in [
            (" ".into(), (now + chrono::Duration::hours(1)).to_rfc3339()),
            (
                "x".repeat(1201),
                (now + chrono::Duration::hours(1)).to_rfc3339(),
            ),
            ("ok".into(), now.to_rfc3339()),
            ("ok".into(), "2026-11-01T12:00:00".into()),
        ] {
            assert!(
                store
                    .execute(
                        &recipient("42"),
                        request(json!({"action":"create","text":text,"due_at":time})),
                        now
                    )
                    .is_err()
            );
        }
        assert!(
            serde_json::from_value::<ReminderRequest>(json!({"action":"list","chat":"7"})).is_err()
        );
        let path = directory.path().join("reminders.json");
        std::fs::write(&path, "broken").unwrap();
        assert!(
            store
                .execute(&recipient("42"), request(json!({"action":"list"})), now)
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "broken");
    }

    #[test]
    fn reminders_claim_retry_and_recovery_are_durable() {
        let directory = tempfile::tempdir().unwrap();
        let store = ReminderStore::new(directory.path());
        let now = Utc::now();
        let due = now + chrono::Duration::minutes(1);
        let created = store
            .execute(
                &recipient("42"),
                request(json!({"action":"create","text":"Call","due_at":due.to_rfc3339()})),
                now,
            )
            .unwrap();
        let id = created["reminder"]["id"].as_str().unwrap();
        assert!(store.claim(id, due).unwrap().is_some());
        assert!(store.claim(id, due).unwrap().is_none());
        let retry = due + chrono::Duration::minutes(2);
        store
            .finish(id, Status::Pending, Some("rate limit".into()), Some(retry))
            .unwrap();
        assert!(store.due(due).unwrap().is_empty());
        assert!(store.claim(id, retry).unwrap().is_some());
        let lock = store.scheduler_lock().unwrap().unwrap();
        assert!(
            ReminderStore::new(directory.path())
                .scheduler_lock()
                .unwrap()
                .is_none()
        );
        store.recover().unwrap();
        drop(lock);
        assert!(store.due(retry).unwrap().is_empty());
        let listed = store
            .execute(&recipient("42"), request(json!({"action":"list"})), now)
            .unwrap();
        assert_eq!(listed["reminders"][0]["status"], "uncertain");
    }

    #[tokio::test]
    async fn reminders_scheduler_ownership_outlives_detached_blocking_claim() {
        let directory = tempfile::tempdir().unwrap();
        let store = ReminderStore::new(directory.path());
        let now = Utc::now();
        let created = store
            .execute(
                &recipient("42"),
                request(json!({"action":"create","text":"Call","due_at":now.to_rfc3339()})),
                now - chrono::Duration::seconds(1),
            )
            .unwrap();
        let id = created["reminder"]["id"].as_str().unwrap().to_owned();
        let owner = store.scheduler_lock().unwrap().unwrap();
        let owned = store.clone().with_scheduler_owner(owner);
        let lock = store.open_lock("reminders.lock").unwrap();
        lock.lock().unwrap();
        let background = owned.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let task = tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            background.claim(&id, now).unwrap()
        });
        ready.await.unwrap();
        drop(owned);
        assert!(store.scheduler_lock().unwrap().is_none());
        drop(lock);
        assert!(task.await.unwrap().is_some());
        let _owner = store.scheduler_lock().unwrap().unwrap();
        store.recover().unwrap();
        assert_eq!(
            store
                .execute(&recipient("42"), request(json!({"action":"list"})), now)
                .unwrap()["reminders"][0]["status"],
            "uncertain"
        );
    }
}
