use std::{fs, path::Path};

use anyhow::{Context, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params_from_iter, types::Value};

use super::{
    SqliteSessionStore,
    sqlite::{DATABASE, VERSION, import_files},
};

/// Build the complete merged database in staging. The caller holds exclusive
/// store locks and validates all other file conflicts before installing it.
/// Directories are ordered by selector authority; all session histories survive.
pub(crate) fn stage_personal_sessions(
    directories: &[std::path::PathBuf],
    staging: &Path,
) -> anyhow::Result<()> {
    let store = SqliteSessionStore::open(staging.to_owned())?;
    let mut db = store
        .connection
        .lock()
        .map_err(|_| anyhow::anyhow!("Session database lock poisoned"))?;
    let tx = db.transaction()?;
    for directory in directories {
        let path = if directory.exists() {
            fs::canonicalize(directory)?.join(DATABASE)
        } else {
            directory.join(DATABASE)
        };
        if path.try_exists()? {
            let original = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?;
            let version: i64 = original.pragma_query_value(None, "user_version", |r| r.get(0))?;
            ensure!(
                (1..=VERSION).contains(&version),
                "Unsupported owner migration schema {version} in {}",
                path.display()
            );
            // Backup includes committed WAL pages without ever copying a live main file.
            let snapshot = tempfile::NamedTempFile::new_in(staging)?;
            original.backup("main", snapshot.path(), None)?;
            let snapshot =
                Connection::open_with_flags(snapshot.path(), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            for (table, keys, columns) in [
                ("sessions", &["id"][..], 3),
                ("active_sessions", &["conversation_id"][..], 2),
                ("runs", &["id"][..], if version >= 5 { 7 } else { 6 }),
                ("tool_executions", &["run_id", "call_id"][..], 3),
                ("submissions", &["id"][..], if version >= 3 { 9 } else { 6 }),
            ] {
                merge_table(&snapshot, &tx, table, keys, columns)
                    .with_context(|| format!("Owner migration conflict in {} ({table}); back up and reconcile both stores", path.display()))?;
            }
            if version >= 5 {
                let revision: i64 = snapshot.query_row(
                    "SELECT CAST(value AS INTEGER) FROM schema_meta WHERE key='run_view_revision'",
                    [],
                    |r| r.get(0),
                )?;
                tx.execute("UPDATE schema_meta SET value=max(CAST(value AS INTEGER),?1)+1 WHERE key='run_view_revision'",[revision])?;
            }
            if version >= 6 {
                merge_table(&snapshot, &tx, "compaction_jobs", &["session_id"], 5)?;
            }
            if version >= 4 {
                merge_table(&snapshot, &tx, "deliveries", &["id"], 4)?;
            }
            if version >= 3 {
                merge_table(&snapshot, &tx, "inbox_controls", &["conversation_id"], 2)?;
            }
            if version >= 2 {
                merge_table(&snapshot, &tx, "run_controls", &["run_id"], 2)?;
            }
            for (table, keys, columns) in [
                ("web_chat_titles", &["conversation_id"][..], 2),
                ("web_chat_branches", &["session_id"][..], 4),
                ("web_chat_selection", &["root_id"][..], 2),
                ("web_chat_pins", &["root_id"][..], 1),
                ("webhooks", &["id"][..], 6),
                ("webhook_receipts", &["webhook_id", "request_key"][..], 5),
                ("subagent_sessions", &["id"][..], 4),
            ] {
                let definition: Option<String> = snapshot
                    .query_row(
                        "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
                        [table],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(definition) = definition {
                    let exists: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                        [table],
                        |row| row.get(0),
                    )?;
                    if !exists {
                        tx.execute_batch(&definition)?;
                    }
                    merge_table(&snapshot, &tx, table, keys, columns).with_context(|| {
                        format!(
                            "Personal migration conflict in {} ({table})",
                            path.display()
                        )
                    })?;
                }
            }
            if let Some(revision) = snapshot.query_row("SELECT CAST(value AS INTEGER) FROM schema_meta WHERE key='web_titles_revision'", [], |row| row.get::<_,i64>(0)).optional()? {
                tx.execute("INSERT INTO schema_meta VALUES('web_titles_revision',?1) ON CONFLICT(key) DO UPDATE SET value=max(CAST(value AS INTEGER),?1)+1", [revision])?;
            }
            let imported: Option<String> = snapshot
                .query_row(
                    "SELECT value FROM schema_meta WHERE key='legacy_imported'",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            match imported.as_deref() {
                Some("1") => {}
                None => import_files(&tx, directory)?,
                _ => anyhow::bail!("Unknown legacy import marker in {}", path.display()),
            }
        } else {
            import_files(&tx, directory)?;
        }
    }
    tx.commit()?;
    drop(db);
    drop(store); // Last connection checkpoints/removes staging WAL before rename.
    fs::File::open(staging)?.sync_all()?;
    Ok(())
}

fn merge_table(
    source: &Connection,
    destination: &Connection,
    table: &str,
    keys: &[&str],
    columns: usize,
) -> anyhow::Result<()> {
    // All identifiers come from the fixed schema above, never user input.
    let select = format!("SELECT * FROM {table}");
    let placeholders = vec!["?"; columns].join(",");
    let names = if table == "submissions" && columns == 6 {
        "(id,conversation_id,request_id,payload_hash,sequence,payload)"
    } else if table == "runs" && columns == 6 {
        "(id,conversation_id,session_id,revision,terminal,payload)"
    } else {
        ""
    };
    let insert =
        format!("INSERT INTO {table}{names} VALUES ({placeholders}) ON CONFLICT DO NOTHING");
    let where_keys = keys
        .iter()
        .map(|key| format!("{key}=?"))
        .collect::<Vec<_>>()
        .join(" AND ");
    let lookup = format!("SELECT * FROM {table} WHERE {where_keys}");
    let values = |row: &rusqlite::Row<'_>| {
        (0..columns)
            .map(|i| row.get::<_, Value>(i))
            .collect::<rusqlite::Result<Vec<_>>>()
    };
    let mut statement = source.prepare(&select)?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let incoming = values(row)?;
        destination.execute(&insert, params_from_iter(&incoming))?;
        if table == "active_sessions" {
            // An older selector does not supersede the first store's current choice.
            continue;
        }
        let existing = destination
            .query_row(
                &lookup,
                params_from_iter(incoming.iter().take(keys.len())),
                values,
            )
            .optional()?;
        let existing = existing.context("Conflicting unique key")?;
        let compare = if table == "sessions" { 2 } else { columns };
        ensure!(
            existing[..compare] == incoming[..compare],
            "Different records for key {:?}",
            &incoming[..keys.len()]
        );
        if table == "sessions" {
            destination.execute(
                "UPDATE sessions SET updated_at=max(updated_at,?2) WHERE id=?1",
                [&incoming[0], &incoming[2]],
            )?;
        }
    }
    Ok(())
}

pub(crate) fn checkpoint_owner_destination(directory: &Path) -> anyhow::Result<()> {
    let path = if directory.exists() {
        fs::canonicalize(directory)?.join(DATABASE)
    } else {
        directory.join(DATABASE)
    };
    if path.try_exists()? {
        let db = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        let busy: i64 = db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
        ensure!(
            busy == 0,
            "Owner session database is busy; stop writers before migration"
        );
        db.close().map_err(|(_, error)| error)?;
        for name in ["state.sqlite3-wal", "state.sqlite3-shm"] {
            ensure!(
                !directory.join(name).try_exists()?,
                "Owner SQLite sidecar is still open; stop writers before migration"
            );
        }
    }
    Ok(())
}
