use std::{
    cell::RefCell,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};

use serde_json::{Value, json};

#[derive(Clone)]
pub(crate) struct Journal {
    directory: PathBuf,
    failed: Arc<Mutex<bool>>,
}

impl Journal {
    pub(crate) fn new(directory: impl AsRef<Path>) -> Self {
        Self {
            directory: directory.as_ref().to_owned(),
            failed: Arc::new(Mutex::new(false)),
        }
    }

    pub(crate) async fn run_transition(
        &self,
        run: &str,
        session: &str,
        category: &'static str,
        attempts: u32,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(category, "recovery" | "park" | "retry"),
            "Invalid recovery audit category"
        );
        self.append(json!({"version":1,"timestamp":chrono::Utc::now().to_rfc3339(),"event":category,"run_id":run,"session_id":session,"attempts":attempts})).await
    }
    async fn append(&self, record: Value) -> anyhow::Result<()> {
        let journal = self.clone();
        let result = tokio::task::spawn_blocking(move || {
            // Keep append and failure-latching atomic with respect to sibling writers.
            let mut failed = journal
                .failed
                .lock()
                .map_err(|_| anyhow::anyhow!("Journal writer poisoned"))?;
            anyhow::ensure!(!*failed, "Journal failed earlier in this process");
            let result = journal.append_sync(record);
            if result.is_err() {
                *failed = true;
            }
            result
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|result| result);
        result.map_err(|error| error.context("Action journal unavailable; further actions blocked"))
    }

    fn append_sync(&self, record: Value) -> anyhow::Result<()> {
        let mut bytes = serde_json::to_vec(&record)?;
        anyhow::ensure!(bytes.len() <= 16_384, "Journal record exceeds 16 KiB");
        bytes.push(b'\n');
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.directory)?;
        let day = record["timestamp"]
            .as_str()
            .and_then(|timestamp| timestamp.get(..10))
            .ok_or_else(|| anyhow::anyhow!("Invalid journal timestamp"))?;
        let file = self.directory.join(format!("{day}.jsonl"));
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(file)?;
        // ponytail: one lock per daily file; use a writer queue if fsync throughput matters.
        file.lock()?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        if file.metadata()?.len() > 0 {
            file.seek(SeekFrom::End(-1))?;
            let mut last = [0];
            file.read_exact(&mut last)?;
            anyhow::ensure!(
                last == [b'\n'],
                "Incomplete journal tail; preserve and repair before restarting"
            );
        }
        file.write_all(&bytes)?;
        file.sync_all()?;
        // Include directory entries when the journal hierarchy was just created.
        for directory in self
            .directory
            .ancestors()
            .filter(|path| !path.as_os_str().is_empty())
        {
            File::open(directory)?.sync_all()?;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct Context {
    session_id: String,
    run_id: String,
    trigger: String,
    source_id: Option<String>,
}

tokio::task_local! {
    static CURRENT: RefCell<Context>;
}

impl Context {
    pub(crate) fn new(session_id: &str, trigger: &str) -> Self {
        let context = Self {
            session_id: session_id.to_owned(),
            run_id: CURRENT
                .try_with(|current| current.borrow().run_id.clone())
                .unwrap_or_else(|_| uuid::Uuid::new_v4().to_string()),
            trigger: trigger.to_owned(),
            source_id: CURRENT
                .try_with(|current| current.borrow().source_id.clone())
                .ok()
                .flatten(),
        };
        // Routing may select a new session before the actual run begins.
        let _ = CURRENT.try_with(|current| *current.borrow_mut() = context.clone());
        context
    }

    pub(crate) fn current_or(session_id: &str, trigger: &str) -> Self {
        CURRENT
            .try_with(|current| current.borrow().clone())
            .unwrap_or_else(|_| Self::new(session_id, trigger))
    }

    pub(crate) fn with_run_id(mut self, id: &str) -> Self {
        self.run_id = id.to_owned();
        let _ = CURRENT.try_with(|current| *current.borrow_mut() = self.clone());
        self
    }

    pub(crate) fn with_source_id(mut self, id: &str) -> Self {
        self.source_id = Some(id.to_owned());
        self
    }
}

pub(crate) async fn scope<F: std::future::Future>(context: Context, future: F) -> F::Output {
    CURRENT.scope(RefCell::new(context), future).await
}

pub(crate) struct Span {
    journal: Option<Journal>,
    context: Context,
    kind: &'static str,
    name: String,
    action_id: String,
    call_id: Option<String>,
    started: Instant,
}

impl Span {
    pub(crate) async fn start(
        journal: Option<&Journal>,
        context: &Context,
        kind: &'static str,
        call_id: Option<&str>,
        name: &str,
        details: Value,
    ) -> anyhow::Result<Self> {
        let span = Self {
            journal: journal.cloned(),
            context: context.clone(),
            kind,
            name: name.to_owned(),
            action_id: uuid::Uuid::new_v4().to_string(),
            call_id: call_id.map(str::to_owned),
            started: Instant::now(),
        };
        span.record("started", "started", details).await?;
        Ok(span)
    }

    pub(crate) async fn finish(&self, status: &str, details: Value) -> anyhow::Result<()> {
        self.record("finished", status, details).await
    }

    async fn record(&self, phase: &str, status: &str, details: Value) -> anyhow::Result<()> {
        if let Some(journal) = &self.journal {
            journal.append(json!({
                "version": 1, "timestamp": chrono::Utc::now().to_rfc3339(),
                "event": format!("{}_{phase}", self.kind), "status": status,
                "session_id": self.context.session_id, "run_id": self.context.run_id,
                "trigger": self.context.trigger, "action_id": self.action_id,
                "source_id": self.context.source_id,
                "call_id": self.call_id, "name": self.name,
                "duration_ms": if phase == "finished" { Some(self.started.elapsed().as_millis() as u64) } else { None },
                "details": details,
            })).await?;
        }
        Ok(())
    }
}

// No Drop record: a cancelled future or crash cannot confirm an external outcome.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn records(directory: &std::path::Path) -> Vec<serde_json::Value> {
        let path = directory.join(format!("{}.jsonl", chrono::Utc::now().format("%Y-%m-%d")));
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn journal_appends_private_correlated_records_and_leaves_interruptions_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::new(dir.path());
        let context = Context::new("test:session", "wakeup");
        let span = Span::start(
            Some(&journal),
            &context,
            "tool",
            Some("call"),
            "exec_command",
            json!({"arguments_bytes":42}),
        )
        .await
        .unwrap();
        span.finish("failed", json!({"exit_code":7,"timed_out":false}))
            .await
            .unwrap();
        let interrupted = Span::start(
            Some(&journal),
            &context,
            "tool",
            Some("interrupted"),
            "exec_command",
            json!({}),
        )
        .await
        .unwrap();
        drop(interrupted);
        let rows = records(dir.path());
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["event"], "tool_started");
        assert_eq!(rows[1]["event"], "tool_finished");
        assert_eq!(rows[0]["action_id"], rows[1]["action_id"]);
        assert_eq!(rows[0]["run_id"], rows[2]["run_id"]);
        assert_eq!(rows[1]["status"], "failed");
        assert_eq!(rows[1]["details"]["exit_code"], 7);
        assert_eq!(rows[2]["call_id"], "interrupted");
        use std::os::unix::fs::PermissionsExt;
        let path = dir
            .path()
            .join(format!("{}.jsonl", chrono::Utc::now().format("%Y-%m-%d")));
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn journal_failure_latches_and_partial_records_block_new_actions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blocked");
        std::fs::write(&path, "not a directory").unwrap();
        let journal = Journal::new(&path);
        let context = Context::new("test", "message");
        assert!(
            Span::start(Some(&journal), &context, "run", None, "agent", json!({}))
                .await
                .is_err()
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(
            Span::start(Some(&journal), &context, "run", None, "agent", json!({}))
                .await
                .is_err()
        );
        let journal = Journal::new(&path);
        let file = path.join(format!("{}.jsonl", chrono::Utc::now().format("%Y-%m-%d")));
        std::fs::write(&file, "{\"partial\":").unwrap();
        assert!(
            Span::start(Some(&journal), &context, "run", None, "agent", json!({}))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(file).unwrap(), "{\"partial\":");
    }

    #[tokio::test]
    async fn concurrent_writers_preserve_complete_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..16 {
            let journal = Journal::new(dir.path());
            tasks.spawn(async move {
                let context = Context::new("test", "message");
                let span = Span::start(Some(&journal), &context, "run", None, "agent", json!({}))
                    .await
                    .unwrap();
                span.finish("ok", json!({})).await.unwrap();
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
        assert_eq!(records(dir.path()).len(), 32);
    }

    #[tokio::test]
    async fn queued_writers_cannot_append_after_failure_state_changes() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::new(dir.path());
        let mut tasks = tokio::task::JoinSet::new();
        {
            let mut failed = journal.failed.lock().unwrap();
            for _ in 0..16 {
                let journal = journal.clone();
                tasks.spawn(async move {
                    Span::start(
                        Some(&journal),
                        &Context::new("test", "message"),
                        "run",
                        None,
                        "agent",
                        json!({}),
                    )
                    .await
                });
            }
            *failed = true;
        }
        while let Some(result) = tasks.join_next().await {
            assert!(result.unwrap().is_err());
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn routed_session_and_source_remain_correlated_with_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::new(dir.path());
        scope(
            Context::new("old", "wakeup").with_source_id("task-id"),
            async {
                let selected = Context::new("selected", "wakeup");
                scope(selected.clone(), async {
                    let run =
                        Span::start(Some(&journal), &selected, "run", None, "agent", json!({}))
                            .await
                            .unwrap();
                    run.finish("ok", json!({})).await.unwrap();
                })
                .await;
                let delivery = Context::current_or("fallback", "delivery");
                let span = Span::start(
                    Some(&journal),
                    &delivery,
                    "delivery",
                    None,
                    "sendRichMessage",
                    json!({}),
                )
                .await
                .unwrap();
                span.finish("sent", json!({})).await.unwrap();
            },
        )
        .await;
        let rows = records(dir.path());
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|row| row["run_id"] == rows[0]["run_id"]
            && row["session_id"] == "selected"
            && row["source_id"] == "task-id"));
    }
}
