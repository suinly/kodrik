use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, UNIX_EPOCH},
};

use anyhow::{Context, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::{Session, SessionId, SessionStore};

pub(super) const VERSION: i64 = 6;
pub(crate) const DATABASE: &str = "state.sqlite3";

#[derive(Clone)]
pub struct SqliteSessionStore {
    pub(super) connection: Arc<Mutex<Connection>>,
    _ownership: Arc<File>,
    pub(super) directory: PathBuf,
    pub(crate) compaction_running: Arc<Mutex<std::collections::HashSet<String>>>,
    pub(crate) compaction_errors: Arc<Mutex<std::collections::HashMap<String, String>>>,
}

pub(crate) struct SearchSessions {
    pub sessions: Vec<Session>,
    pub scanned: usize,
    pub skipped: usize,
    pub truncated: bool,
}

impl SqliteSessionStore {
    pub(crate) async fn select_idle_session(
        &self,
        conversation: &SessionId,
        id: &SessionId,
    ) -> anyhow::Result<Session> {
        let (conversation, id) = (conversation.clone(), id.clone());
        self.with_connection(move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let session = load(&tx, id.as_str())?.context("Session not found")?;
            let current = crate::agent::inbox::active_session(&tx, conversation.as_str())?;
            let source_pending: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM runs WHERE conversation_id=?1 AND terminal=0)
                 OR EXISTS(SELECT 1 FROM submissions WHERE conversation_id=?1 AND status='Queued')",
                [conversation.as_str()],
                |r| r.get(0),
            )?;
            ensure!(
                !source_pending
                    && !crate::agent::inbox::session_busy_in(&tx, &current)?
                    && !crate::agent::inbox::session_busy_in(&tx, id.as_str())?,
                crate::agent::inbox::AdmissionRejected("Session is busy")
            );
            select(&tx, conversation.as_str(), id.as_str())?;
            tx.commit()?;
            Ok(session)
        })
        .await
    }

    /// Blocking initialization; async callers must use `open_async`.
    pub fn open(directory: PathBuf) -> anyhow::Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)?;
        ensure!(
            fs::symlink_metadata(&directory)?.is_dir(),
            "Session directory is not a regular directory"
        );
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let ownership = ownership_lock(&directory, false)?;
        // journal_mode cannot reliably upgrade concurrently even with busy_timeout.
        // Serialize initialization/import, not ordinary operations on open stores.
        let initialization = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory.join("init.lock"))?;
        initialization.lock()?;
        let path = directory.join(DATABASE);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        ensure!(
            file.metadata()?.is_file(),
            "Session database is not a regular file"
        );
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        for name in ["state.sqlite3-wal", "state.sqlite3-shm"] {
            let sidecar = directory.join(name);
            match OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&sidecar)
            {
                Ok(file) => {
                    ensure!(
                        file.metadata()?.is_file(),
                        "Invalid SQLite sidecar {}",
                        sidecar.display()
                    );
                    // The last connection can unlink its sidecars while a new store opens.
                    // Set permissions through the handle, not a now-stale pathname.
                    file.set_permissions(fs::Permissions::from_mode(0o600))?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let mut db = Connection::open(&path)?;
        db.busy_timeout(Duration::from_secs(5))?;
        let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        ensure!(
            version <= VERSION,
            "Unsupported session schema version {version}; expected at most {VERSION}"
        );
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "FULL")?;
        db.pragma_update(None, "foreign_keys", "ON")?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        ensure!(
            version <= VERSION,
            "Unsupported session schema version {version}"
        );
        if version == 0 {
            let count: i64 = tx.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )?;
            ensure!(count == 0, "Unrecognized unversioned session database");
            tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY, payload BLOB NOT NULL, updated_at INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS sessions_recent ON sessions(updated_at DESC, id);
             CREATE TABLE IF NOT EXISTS active_sessions (
                conversation_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE);
             CREATE TABLE IF NOT EXISTS runs (
                id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL,
                session_id TEXT NOT NULL REFERENCES sessions(id), revision INTEGER NOT NULL,
                terminal INTEGER NOT NULL CHECK(terminal IN (0,1)), payload BLOB NOT NULL);
             CREATE UNIQUE INDEX IF NOT EXISTS one_live_run ON runs(conversation_id) WHERE terminal=0;
             CREATE TABLE IF NOT EXISTS tool_executions (
                run_id TEXT NOT NULL REFERENCES runs(id), call_id TEXT NOT NULL,
                payload BLOB NOT NULL, PRIMARY KEY(run_id, call_id));
             CREATE TABLE IF NOT EXISTS submissions (
                id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL, request_id TEXT NOT NULL,
                payload_hash TEXT NOT NULL, sequence INTEGER NOT NULL, payload BLOB NOT NULL,
                UNIQUE(conversation_id, request_id), UNIQUE(conversation_id, sequence));
             CREATE TABLE IF NOT EXISTS schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             PRAGMA user_version=1;"
        )?;
        }
        if version < 2 {
            tx.execute_batch("CREATE TABLE run_controls (run_id TEXT PRIMARY KEY, cancel_reason TEXT NOT NULL); PRAGMA user_version=2;")?;
        }
        if version < 3 {
            tx.execute_batch(
                "ALTER TABLE submissions ADD COLUMN status TEXT NOT NULL DEFAULT 'Queued';
                ALTER TABLE submissions ADD COLUMN run_id TEXT;
                ALTER TABLE submissions ADD COLUMN session_id TEXT;
                CREATE INDEX submissions_pending ON submissions(conversation_id,status,sequence);
                CREATE TABLE inbox_controls(conversation_id TEXT PRIMARY KEY,reason TEXT NOT NULL);
                PRAGMA user_version=3;",
            )?;
        }
        if version < 4 {
            tx.execute_batch("CREATE TABLE deliveries(id TEXT PRIMARY KEY,run_id TEXT NOT NULL,conversation_id TEXT NOT NULL,payload BLOB NOT NULL); CREATE INDEX deliveries_conversation ON deliveries(conversation_id); PRAGMA user_version=4;")?;
        }
        if version < 5 {
            crate::agent::run_view::migrate(&tx)?;
        }
        if version < 6 {
            crate::agent::compact::migrate(&tx)?;
        }
        tx.commit()?;
        import_legacy(&mut db, &directory)?;
        // SQLite derives sidecar permissions from the already-private main database.
        File::open(&directory)?.sync_all()?;
        Ok(Self {
            connection: Arc::new(Mutex::new(db)),
            compaction_errors: Default::default(),
            compaction_running: Default::default(),
            _ownership: Arc::new(ownership),
            directory,
        })
    }

    pub async fn open_async(directory: PathBuf) -> anyhow::Result<Self> {
        tokio::task::spawn_blocking(move || Self::open(directory)).await?
    }

    pub(crate) async fn with_connection<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Connection) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        let store = self.clone();
        crate::agent::run_state::spawn_owned_blocking(move || {
            let mut db = store
                .connection
                .lock()
                .map_err(|_| anyhow::anyhow!("Session database lock poisoned"))?;
            let result = operation(&mut db);
            drop(db);
            // Retain the complete store (including migration ownership) even
            // when the async caller is cancelled before this worker finishes.
            drop(store);
            result
        })
        .await?
    }

    /// Select bounded payloads in a read transaction; never materialize the whole history.
    pub(crate) async fn search_sessions(
        &self,
        id: Option<String>,
    ) -> anyhow::Result<SearchSessions> {
        self.with_connection(move |db| {
            const MAX_SESSION: u64 = 16 * 1024 * 1024;
            const MAX_TOTAL: u64 = 64 * 1024 * 1024;
            const MAX_COUNT: usize = 1000;
            let tx = db.transaction()?;
            let mut query = tx.prepare(
                "SELECT id, length(payload) FROM sessions
                 WHERE (?1 IS NULL OR id=?1) AND substr(id,1,10) != 'companion:'
                 ORDER BY updated_at DESC, id LIMIT 1001",
            )?;
            let candidates = query
                .query_map([id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut result = SearchSessions {
                sessions: Vec::new(),
                scanned: 0,
                skipped: 0,
                truncated: candidates.len() > MAX_COUNT,
            };
            let mut bytes = 0;
            for (id, size) in candidates.into_iter().take(MAX_COUNT) {
                if bytes >= MAX_TOTAL {
                    result.truncated = true;
                    break;
                }
                result.scanned += 1;
                if size > MAX_SESSION {
                    result.skipped += 1;
                    continue;
                }
                if size > MAX_TOTAL - bytes {
                    result.skipped += 1;
                    result.truncated = true;
                    continue;
                }
                let payload: Vec<u8> =
                    tx.query_row("SELECT payload FROM sessions WHERE id=?1", [&id], |r| {
                        r.get(0)
                    })?;
                bytes += payload.len() as u64;
                match decode(&id, &payload) {
                    Ok(session) => result.sessions.push(session),
                    Err(error) => {
                        result.skipped += 1;
                        log::warn!("Skipping invalid session {id}: {error}");
                    }
                }
            }
            Ok(result)
        })
        .await
    }
}

fn valid_id(id: &str) -> anyhow::Result<()> {
    ensure!(
        !id.is_empty()
            && id != "."
            && id != ".."
            && !id.contains(['/', '\\'])
            && !id.chars().any(char::is_control),
        "Invalid session ID"
    );
    Ok(())
}

pub(crate) fn ownership_lock(directory: &Path, exclusive: bool) -> anyhow::Result<File> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.join("store.lock"))?;
    let result = if exclusive {
        lock.try_lock()
    } else {
        lock.try_lock_shared()
    };
    result.with_context(|| format!("Session store {} is in use or migrating; stop other Kodrik processes before owner migration", directory.display()))?;
    Ok(lock)
}

fn decode(id: &str, bytes: &[u8]) -> anyhow::Result<Session> {
    let session: Session = serde_json::from_slice(bytes)?;
    ensure!(session.id().as_str() == id, "Session ID mismatch");
    Ok(session)
}

fn load(db: &Connection, id: &str) -> anyhow::Result<Option<Session>> {
    valid_id(id)?;
    let bytes: Option<Vec<u8>> = db
        .query_row("SELECT payload FROM sessions WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .optional()?;
    bytes.map(|bytes| decode(id, &bytes)).transpose()
}

fn save(db: &Connection, session: &Session) -> anyhow::Result<()> {
    valid_id(session.id().as_str())?;
    db.execute(
        "INSERT INTO sessions(id,payload,updated_at) VALUES (?1,?2,?3)
         ON CONFLICT(id) DO UPDATE SET payload=excluded.payload, updated_at=excluded.updated_at",
        params![
            session.id().as_str(),
            serde_json::to_vec(session)?,
            chrono::Utc::now()
                .timestamp_nanos_opt()
                .context("Session timestamp overflow")?
        ],
    )?;
    Ok(())
}

pub(crate) fn select(db: &Connection, conversation: &str, id: &str) -> anyhow::Result<()> {
    valid_id(conversation)?;
    let old: Option<String> = db
        .query_row(
            "SELECT session_id FROM active_sessions WHERE conversation_id=?1",
            [conversation],
            |r| r.get(0),
        )
        .optional()?;
    if old.as_deref().unwrap_or(conversation) != id {
        db.execute("UPDATE compaction_jobs SET status='stale',payload=CAST(json_set(payload,'$.status','stale') AS BLOB) WHERE session_id IN (?1,?2) AND status IN ('preparing','ready')",params![old.as_deref().unwrap_or(conversation),id])?;
    }
    db.execute("INSERT INTO active_sessions VALUES (?1,?2) ON CONFLICT(conversation_id) DO UPDATE SET session_id=excluded.session_id", [conversation, id])?;
    Ok(())
}

fn json_files(directory: &Path) -> anyhow::Result<Vec<PathBuf>> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) => ensure!(
            metadata.is_dir(),
            "Legacy session directory is not a regular directory: {}",
            directory.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    }
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.path().extension().is_some_and(|ext| ext == "json") {
            ensure!(
                entry.file_type()?.is_file(),
                "Legacy session is not a regular file: {}",
                entry.path().display()
            );
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn import_legacy(db: &mut Connection, directory: &Path) -> anyhow::Result<()> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if tx
        .query_row(
            "SELECT value FROM schema_meta WHERE key='legacy_imported'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .is_some()
    {
        return Ok(());
    }
    import_files(&tx, directory)?;
    tx.execute("INSERT INTO schema_meta VALUES ('legacy_imported','1')", [])?;
    tx.commit()?;
    Ok(())
}

pub(super) fn import_files(tx: &Connection, directory: &Path) -> anyhow::Result<()> {
    for path in json_files(directory)? {
        (|| -> anyhow::Result<()> {
            let session: Session = serde_json::from_slice(&fs::read(&path)?)?;
            valid_id(session.id().as_str())?;
            let payload = serde_json::to_vec(&session)?;
            let previous: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT payload FROM sessions WHERE id=?1",
                    [session.id().as_str()],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(previous) = previous {
                ensure!(
                    previous == payload,
                    "Conflicting legacy session ID {}",
                    session.id().as_str()
                );
            } else {
                let modified = fs::metadata(&path)?
                    .modified()?
                    .duration_since(UNIX_EPOCH)?
                    .as_nanos();
                tx.execute(
                    "INSERT INTO sessions VALUES (?1,?2,?3)",
                    params![session.id().as_str(), payload, i64::try_from(modified)?],
                )?;
            }
            Ok(())
        })()
        .with_context(|| format!("Cannot import legacy session {}", path.display()))?;
    }
    for path in json_files(&directory.join("active"))? {
        (|| -> anyhow::Result<()> {
            let conversation = path
                .file_stem()
                .and_then(|name| name.to_str())
                .context("Invalid active mapping filename")?;
            let id: SessionId = serde_json::from_slice(&fs::read(&path)?)?;
            valid_id(id.as_str())?;
            ensure!(
                load(tx, id.as_str())?.is_some(),
                "Active mapping refers to missing session {}",
                id.as_str()
            );
            let existing: Option<String> = tx
                .query_row(
                    "SELECT session_id FROM active_sessions WHERE conversation_id=?1",
                    [conversation],
                    |r| r.get(0),
                )
                .optional()?;
            ensure!(
                existing.is_none_or(|old| old == id.as_str()),
                "Conflicting active mapping for {conversation}"
            );
            select(tx, conversation, id.as_str())
        })()
        .with_context(|| format!("Cannot import active mapping {}", path.display()))?;
    }
    Ok(())
}

#[async_trait::async_trait]
impl SessionStore for SqliteSessionStore {
    fn runtime_store(&self) -> Option<&Self> {
        Some(self)
    }
    async fn active_id(&self, conversation: &SessionId) -> anyhow::Result<SessionId> {
        let conversation = conversation.clone();
        self.with_connection(move |db| {
            valid_id(conversation.as_str())?;
            let id: Option<String> = db
                .query_row(
                    "SELECT session_id FROM active_sessions WHERE conversation_id=?1",
                    [conversation.as_str()],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(id.map(SessionId::new).unwrap_or(conversation))
        })
        .await
    }

    async fn new_session(&self, conversation: &SessionId) -> anyhow::Result<Session> {
        let conversation = conversation.clone();
        self.with_connection(move |db| {
            let session = Session::new(SessionId::new(uuid::Uuid::new_v4().to_string()));
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            save(&tx, &session)?;
            select(&tx, conversation.as_str(), session.id().as_str())?;
            tx.commit()?;
            Ok(session)
        })
        .await
    }

    async fn list(&self) -> anyhow::Result<Vec<Session>> {
        self.with_connection(|db| {
            let mut query =
                db.prepare("SELECT id,payload FROM sessions ORDER BY updated_at DESC,id")?;
            let mut rows = query.query([])?;
            let mut sessions = Vec::new();
            while let Some(row) = rows.next()? {
                sessions.push(decode(
                    &row.get::<_, String>(0)?,
                    &row.get::<_, Vec<u8>>(1)?,
                )?);
            }
            Ok(sessions)
        })
        .await
    }

    async fn select(&self, conversation: &SessionId, id: &SessionId) -> anyhow::Result<Session> {
        let (conversation, id) = (conversation.clone(), id.clone());
        self.with_connection(move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let session = load(&tx, id.as_str())?.context("Session not found")?;
            select(&tx, conversation.as_str(), id.as_str())?;
            tx.commit()?;
            Ok(session)
        })
        .await
    }

    async fn load(&self, id: &SessionId) -> anyhow::Result<Option<Session>> {
        let id = id.clone();
        self.with_connection(move |db| load(db, id.as_str())).await
    }

    async fn save(&self, session: &Session) -> anyhow::Result<()> {
        let session = session.clone();
        self.with_connection(move |db| save(db, &session)).await
    }

    async fn delete(&self, id: &SessionId) -> anyhow::Result<()> {
        let id = id.clone();
        self.with_connection(move |db| {
            valid_id(id.as_str())?;
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let live: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE session_id=?1 AND terminal=0)", [id.as_str()], |r|r.get(0))?;
            ensure!(!live, "Session has an unfinished run; recovery required before deletion");
            tx.execute("DELETE FROM tool_executions WHERE run_id IN (SELECT id FROM runs WHERE session_id=?1)", [id.as_str()])?;
            tx.execute("DELETE FROM run_controls WHERE run_id IN (SELECT id FROM runs WHERE session_id=?1)", [id.as_str()])?;
            tx.execute("DELETE FROM runs WHERE session_id=?1", [id.as_str()])?;
            tx.execute("DELETE FROM compaction_jobs WHERE session_id=?1", [id.as_str()])?;
            tx.execute("DELETE FROM sessions WHERE id=?1", [id.as_str()])?;
            tx.commit()?;
            Ok(())
        })
        .await
    }
}

impl SqliteSessionStore {
    #[cfg(test)]
    pub(crate) async fn commit_step(
        &self,
        expected_revision: i64,
        session: &Session,
        run: &crate::agent::run_state::RunRecord,
        tool: Option<&crate::agent::run_state::ToolExecution>,
    ) -> anyhow::Result<()> {
        self.commit_owned_step(expected_revision, session, run, tool, None)
            .await
    }

    pub(crate) async fn commit_owned_step(
        &self,
        expected_revision: i64,
        session: &Session,
        run: &crate::agent::run_state::RunRecord,
        tool: Option<&crate::agent::run_state::ToolExecution>,
        ownership: Option<Arc<super::ExecutionOwnership>>,
    ) -> anyhow::Result<()> {
        let (session, mut run, tool) = (session.clone(), run.clone(), tool.cloned());
        self.with_connection(move |db| {
            let _ownership = ownership;
            ensure!(run.schema_version == 1 && run.revision == expected_revision + 1, "Invalid run revision");
            ensure!(run.session_id == session.id().as_str(), "Run session binding changed");
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let cancelled: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM run_controls WHERE run_id=?1)", [&run.id], |r|r.get(0))?;
            ensure!(!cancelled || matches!(run.phase, crate::agent::run_state::RunPhase::Cancelled), "Run cancellation requested");
            run.cancel_requested |= cancelled;
            let previous: Option<(i64, Vec<u8>)> = tx.query_row(
                "SELECT revision,payload FROM runs WHERE id=?1", [&run.id], |r| Ok((r.get(0)?,r.get(1)?)),
            ).optional()?;
            let payload = serde_json::to_vec(&run)?;
            let mut history_advanced=false;
            let mut interrupted_attempt=!run.attempt_active && !run.phase.terminal();
            if let Some((revision, previous)) = &previous {
                // Retrying an identical transaction after an unknown commit outcome is harmless.
                if *revision == run.revision && *previous == payload {
                    let saved = load(&tx, &run.session_id)?.context("Run session missing")?;
                    ensure!(serde_json::to_vec(&saved)? == serde_json::to_vec(&session)?, "Duplicate checkpoint session conflict");
                    if let Some(tool) = &tool {
                        let saved: Vec<u8> = tx.query_row("SELECT payload FROM tool_executions WHERE run_id=?1 AND call_id=?2", params![run.id,tool.call.call_id], |r|r.get(0))?;
                        ensure!(saved == serde_json::to_vec(tool)?, "Duplicate checkpoint tool conflict");
                    }
                    return Ok(());
                }
                let old: crate::agent::run_state::RunRecord = serde_json::from_slice(previous)?;
                history_advanced=run.history_len>old.history_len;
                interrupted_attempt |= run.recovery_attempts>old.recovery_attempts;
                ensure!(*revision == expected_revision && !old.phase.terminal(), "Stale or terminal run checkpoint");
                ensure!(old.session_id == run.session_id && old.conversation_id == run.conversation_id, "Run binding changed");
            } else {
                ensure!(expected_revision == 0, "Run checkpoint missing");
                // Terminal preflight failures must not bypass live-run admission either.
                let live: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM runs WHERE terminal=0 AND (conversation_id=?1 OR session_id=?2))",
                    params![run.conversation_id, run.session_id], |r| r.get(0),
                )?;
                ensure!(!live, "Unfinished run owns this conversation or session; recovery required");
                let active: Option<String> = tx.query_row("SELECT session_id FROM active_sessions WHERE conversation_id=?1", [&run.conversation_id], |r|r.get(0)).optional()?;
                let expected_active=run.routing_from.as_deref().unwrap_or(&run.session_id);
                let prebound=if let Some(id)=run.submission_ids.first() {crate::agent::inbox::read_record(&tx,id)?.and_then(|r|r.session_id).is_some_and(|id|id==run.session_id)} else {false};
                ensure!(prebound || active.as_deref().unwrap_or(&run.conversation_id) == expected_active, "Active session changed before run");
                if let Some(id)=run.submission_ids.first() {
                    // A persisted Stop must also fence a worker that has not written its first checkpoint.
                    let stopped:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM inbox_controls WHERE conversation_id=?1) OR EXISTS(SELECT 1 FROM submissions WHERE conversation_id=?1 AND status='Queued' AND json_extract(payload,'$.request.control')=1)",[&run.conversation_id],|r|r.get(0))?;
                    ensure!(!stopped,"Inbox stopped before run admission");
                    let first:String=tx.query_row("SELECT id FROM submissions WHERE conversation_id=?1 AND status='Queued' AND json_extract(payload,'$.request.control')=0 ORDER BY sequence LIMIT 1",[&run.conversation_id],|r|r.get(0))?;
                    ensure!(*id==first,"Submission is not at the head of the inbox");
                }


            }
            if matches!(run.phase,crate::agent::run_state::RunPhase::Cancelled|crate::agent::run_state::RunPhase::Failed{..}) {
                tx.execute("UPDATE compaction_jobs SET status='cancelled',payload=CAST(json_set(payload,'$.status','cancelled') AS BLOB) WHERE run_id=?1 AND status IN ('preparing','ready')",[&run.id])?;
            }
            save(&tx, &session)?;
            if previous.is_none() && run.routing_from.is_some(){select(&tx,&run.conversation_id,&run.session_id)?;}
            tx.execute("INSERT INTO runs(id,conversation_id,session_id,revision,terminal,payload) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO UPDATE SET revision=excluded.revision,terminal=excluded.terminal,payload=excluded.payload",
                params![run.id,run.conversation_id,run.session_id,run.revision,run.phase.terminal(),payload])?;
            if let Some(tool) = &tool {
                let existing: Option<Vec<u8>> = tx.query_row("SELECT payload FROM tool_executions WHERE run_id=?1 AND call_id=?2", params![run.id,tool.call.call_id], |r|r.get(0)).optional()?;
                if let Some(existing) = existing {
                    let old: crate::agent::run_state::ToolExecution = serde_json::from_slice(&existing)?;
                    ensure!(serde_json::to_vec(&old.call)? == serde_json::to_vec(&tool.call)? && old.replay == tool.replay, "Tool identity changed");
                    ensure!(old.result.is_none() || serde_json::to_vec(&old.result)? == serde_json::to_vec(&tool.result)?, "Tool result already committed");
                }
                let count = session.messages().iter().filter(|m| matches!(m, crate::agent::message::Message::ToolResult(r) if r.call_id==tool.call.call_id)).count();
                ensure!(count == usize::from(tool.result.is_some()), "Duplicate or missing tool result in session");
                tx.execute("INSERT INTO tool_executions(run_id,call_id,payload) VALUES(?1,?2,?3) ON CONFLICT(run_id,call_id) DO UPDATE SET payload=excluded.payload", params![run.id,tool.call.call_id,serde_json::to_vec(&tool)?])?;
            }
            crate::agent::inbox::checkpoint_submissions(&tx,&run,&session)?;
            crate::agent::delivery::checkpoint_final(&tx,&run,&session)?;
            crate::agent::run_view::checkpoint_live(&tx,&run,history_advanced,interrupted_attempt)?;
            #[cfg(test)]
            crate::agent::durable_fixture::checkpoint("before", &run, tool.as_ref())?;
            tx.commit()?;
            #[cfg(test)]
            crate::agent::durable_fixture::checkpoint("after", &run, tool.as_ref())?;
            Ok(())
        }).await
    }
}

impl SqliteSessionStore {
    pub(crate) async fn save_observed(
        &self,
        session: &Session,
        conversation: &SessionId,
        requests: Vec<String>,
    ) -> anyhow::Result<()> {
        let session = session.clone();
        let conversation = conversation.as_str().to_owned();
        self.with_connection(move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            save(&tx, &session)?;
            for request in requests {
                let id: Option<String> = tx
                    .query_row(
                        "SELECT id FROM submissions WHERE conversation_id=?1 AND request_id=?2",
                        params![conversation, request],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(id) = id {
                    let mut record = crate::agent::inbox::read_record(&tx, &id)?.unwrap();
                    ensure!(
                        record.status == crate::agent::inbox::SubmissionStatus::Queued,
                        "Observed submission already processed"
                    );
                    record.status = crate::agent::inbox::SubmissionStatus::Done;
                    record.session_id = Some(session.id().as_str().into());
                    crate::agent::inbox::write_record(&tx, &record)?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }
}

impl SqliteSessionStore {
    pub(crate) async fn commit_control(&self, id: &str) -> anyhow::Result<()> {
        let id = id.to_owned();
        self.with_connection(move|db| {
            let tx=db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut record=crate::agent::inbox::read_record(&tx,&id)?.context("Control submission missing")?;
            if record.status==crate::agent::inbox::SubmissionStatus::Done {return Ok(());}
            let conversation=&record.request.conversation_id;
            let live:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE conversation_id=?1 AND terminal=0)",[conversation],|r|r.get(0))?;
            ensure!(!live,"Control waits for active run cancellation");
            if record.request.message.text.trim()=="/new" {
                let session=Session::new(SessionId::new(uuid::Uuid::new_v4().to_string()));save(&tx,&session)?;select(&tx,conversation,session.id().as_str())?;
                record.session_id=Some(session.id().as_str().into());
            }
            record.status=crate::agent::inbox::SubmissionStatus::Done;
            crate::agent::inbox::write_record(&tx,&record)?;
            tx.execute("INSERT INTO inbox_controls VALUES(?1,'Stopped by user; queued follow-ups require new input') ON CONFLICT(conversation_id) DO UPDATE SET reason=excluded.reason",[conversation])?;
            let ids=tx.prepare("SELECT id FROM submissions WHERE conversation_id=?1 AND status='Queued' AND sequence<?2 AND json_extract(payload,'$.request.mode')='Steer'")?.query_map(params![conversation,record.sequence],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?;
            for id in ids {let mut record=crate::agent::inbox::read_record(&tx,&id)?.unwrap();record.status=crate::agent::inbox::SubmissionStatus::Cancelled;crate::agent::inbox::write_record(&tx,&record)?;}
            let newer:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM submissions WHERE conversation_id=?1 AND sequence>?2 AND status='Queued' AND json_extract(payload,'$.request.control')=0)",params![conversation,record.sequence],|r|r.get(0))?;
            if newer {tx.execute("DELETE FROM inbox_controls WHERE conversation_id=?1",[conversation])?;}
            tx.commit()?;Ok(())
        }).await
    }
}
