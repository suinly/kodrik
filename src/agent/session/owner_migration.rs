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
            if version >= 12 {
                merge_table(
                    &snapshot,
                    &tx,
                    "remote_bindings",
                    &["connection_id", "generation"],
                    5,
                )?;
                merge_table(
                    &snapshot,
                    &tx,
                    "remote_operations",
                    &["run_id", "call_id"],
                    6,
                )?;
            }
            if version >= 13 {
                merge_table(
                    &snapshot,
                    &tx,
                    "remote_tool_attachments",
                    &["run_id", "call_id"],
                    3,
                )?;
            }
            if version >= 7 {
                for (table, keys, columns) in [
                    ("web_push_subscriptions", &["endpoint"][..], 4),
                    ("web_push_bindings", &["endpoint"][..], 2),
                    ("web_push_queue", &["event_id", "endpoint"][..], 6),
                ] {
                    merge_table(&snapshot, &tx, table, keys, columns)?;
                }
                let key: Option<String> = snapshot
                    .query_row(
                        "SELECT value FROM schema_meta WHERE key='web_push_vapid'",
                        [],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(key) = key {
                    let existing: Option<String> = tx
                        .query_row(
                            "SELECT value FROM schema_meta WHERE key='web_push_vapid'",
                            [],
                            |row| row.get(0),
                        )
                        .optional()?;
                    ensure!(
                        existing.as_ref().is_none_or(|existing| existing == &key),
                        "Conflicting web push signing keys; back up and reconcile both stores"
                    );
                    tx.execute(
                        "INSERT OR IGNORE INTO schema_meta VALUES('web_push_vapid',?1)",
                        [key],
                    )?;
                }
            }
            if version >= 8 {
                merge_table(
                    &snapshot,
                    &tx,
                    "web_push_presence",
                    &["session_hash", "tab_id"],
                    4,
                )?;
            }
            if version >= 9 {
                merge_table(&snapshot, &tx, "command_audits", &["id"], 5)?;
            }
            if version >= 11 {
                merge_table(&snapshot, &tx, "command_audit_modes", &["session_id"], 3)?;
                merge_table(&snapshot, &tx, "command_audit_profile", &["id"], 2)?;
            } else {
                // Adapt legacy settings without changing the read-only snapshot or bypassing conflict checks.
                let mode = if version == 10 {
                    "CASE WHEN EXISTS(SELECT 1 FROM command_audit_disabled d WHERE d.session_id=s.id AND d.generation=coalesce(json_extract(CAST(s.payload AS TEXT),'$.generation'),'')) THEN 'trusted' ELSE 'review' END"
                } else if version == 9 {
                    "'review'"
                } else {
                    "'trusted'"
                };
                snapshot.execute_batch(&format!("CREATE TEMP VIEW command_audit_modes AS SELECT s.id AS session_id,coalesce(json_extract(CAST(s.payload AS TEXT),'$.generation'),'') AS generation,{mode} AS mode FROM sessions s"))?;
                merge_table(&snapshot, &tx, "command_audit_modes", &["session_id"], 3)?;
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
                ("webhooks", &["id"][..], 7),
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
                    if table == "webhooks" {
                        crate::web::webhook_store::upgrade_audit_mode(&tx)?;
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
    let select = if table == "webhooks" {
        let has_mode: bool = source.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('webhooks') WHERE name='command_audit_mode')",
            [], |row| row.get(0),
        )?;
        format!(
            "SELECT id,name,instruction,token_hash,enabled,created_at,{} FROM webhooks",
            if has_mode {
                "command_audit_mode"
            } else {
                "'review'"
            }
        )
    } else {
        format!("SELECT * FROM {table}")
    };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::session::{Session, SessionId, SessionStore};

    #[tokio::test]
    async fn owner_migration_preserves_webhook_modes_across_old_and_new_schemas() {
        for legacy_first in [false, true] {
            let old = tempfile::tempdir().unwrap();
            let new = tempfile::tempdir().unwrap();
            let destination = tempfile::tempdir().unwrap();
            for (directory, legacy) in [(old.path(), true), (new.path(), false)] {
                let store = SqliteSessionStore::open(directory.into()).unwrap();
                store.with_connection(move |db| {
                    db.execute_batch("CREATE TABLE webhooks (id TEXT PRIMARY KEY,name TEXT NOT NULL,instruction TEXT NOT NULL,token_hash TEXT NOT NULL,enabled INTEGER NOT NULL,created_at TEXT NOT NULL);
                        INSERT INTO webhooks VALUES('shared','Shared','Investigate','hash',1,'2026-01-01');")?;
                    if legacy {
                        db.execute_batch("INSERT INTO webhooks VALUES('legacy','Old','Old instruction','old-token',0,'2026-01-01');")?;
                    } else {
                        db.execute_batch("ALTER TABLE webhooks ADD COLUMN command_audit_mode TEXT NOT NULL DEFAULT 'review';
                            INSERT INTO webhooks VALUES('ask','Ask','Ask instruction','ask-token',0,'2026-01-01','ask');
                            INSERT INTO webhooks VALUES('trusted','Trusted','Trusted instruction','trusted-token',1,'2026-01-01','trusted');")?;
                    }
                    Ok(())
                }).await.unwrap();
            }
            let directories = if legacy_first {
                vec![old.path().into(), new.path().into()]
            } else {
                vec![new.path().into(), old.path().into()]
            };
            stage_personal_sessions(&directories, destination.path()).unwrap();
            let migrated = SqliteSessionStore::open(destination.path().into()).unwrap();
            let modes = migrated
                .with_connection(|db| {
                    Ok(db
                        .prepare("SELECT id,command_audit_mode FROM webhooks ORDER BY id")?
                        .query_map([], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?)
                })
                .await
                .unwrap();
            assert_eq!(
                modes,
                vec![
                    ("ask".into(), "ask".into()),
                    ("legacy".into(), "review".into()),
                    ("shared".into(), "review".into()),
                    ("trusted".into(), "trusted".into())
                ]
            );
            let settings = migrated
                .with_connection(|db| {
                    Ok(db
                        .prepare(
                            "SELECT id,instruction,token_hash,enabled FROM webhooks ORDER BY id",
                        )?
                        .query_map([], |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, bool>(3)?,
                            ))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?)
                })
                .await
                .unwrap();
            assert_eq!(
                settings,
                vec![
                    (
                        "ask".into(),
                        "Ask instruction".into(),
                        "ask-token".into(),
                        false
                    ),
                    (
                        "legacy".into(),
                        "Old instruction".into(),
                        "old-token".into(),
                        false
                    ),
                    ("shared".into(), "Investigate".into(), "hash".into(), true),
                    (
                        "trusted".into(),
                        "Trusted instruction".into(),
                        "trusted-token".into(),
                        true
                    ),
                ]
            );
        }
    }

    #[tokio::test]
    async fn owner_migration_preserves_command_audit_modes_and_profile() {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(source.path().into()).unwrap();
        let mut expected = Vec::new();
        for mode in ["ask", "review", "trusted"] {
            let session = Session::new(SessionId::new(mode));
            store.save(&session).await.unwrap();
            expected.push((
                mode.to_owned(),
                session.generation().to_owned(),
                mode.to_owned(),
            ));
        }
        store
            .with_connection(|db| {
                db.execute("UPDATE command_audit_modes SET mode=session_id", [])?;
                db.execute("INSERT INTO command_audit_profile VALUES(1,'ask')", [])?;
                Ok(())
            })
            .await
            .unwrap();
        drop(store);
        stage_personal_sessions(&[source.path().into()], destination.path()).unwrap();
        let migrated = SqliteSessionStore::open(destination.path().into()).unwrap();
        let actual = migrated.with_connection(|db| {
            let mut query = db.prepare("SELECT session_id,generation,mode FROM command_audit_modes ORDER BY session_id")?;
            let modes = query.query_map([], |row| Ok((row.get::<_,String>(0)?, row.get::<_,String>(1)?, row.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let profile: String = db.query_row("SELECT mode FROM command_audit_profile WHERE id=1", [], |row| row.get(0))?;
            Ok((modes, profile))
        }).await.unwrap();
        assert_eq!(actual, (expected, "ask".into()));
    }

    #[tokio::test]
    async fn owner_migration_converts_legacy_command_audit_settings() {
        for version in [8, 9, 10] {
            let source = tempfile::tempdir().unwrap();
            let destination = tempfile::tempdir().unwrap();
            let store = SqliteSessionStore::open(source.path().into()).unwrap();
            let mut expected = Vec::new();
            for id in ["disabled", "review", "stale"] {
                let session = Session::new(SessionId::new(id));
                store.save(&session).await.unwrap();
                expected.push((
                    id.to_owned(),
                    session.generation().to_owned(),
                    if version < 9 || version == 10 && id == "disabled" {
                        "trusted"
                    } else {
                        "review"
                    }
                    .to_owned(),
                ));
            }
            store.with_connection(move |db| {
                db.execute_batch("DROP TABLE command_audit_modes; DROP TABLE command_audit_profile;")?;
                if version < 9 {
                    db.execute_batch("DROP TABLE command_audits;")?;
                }
                if version == 10 {
                    db.execute_batch("CREATE TABLE command_audit_disabled (session_id TEXT PRIMARY KEY, generation TEXT NOT NULL);
                        INSERT INTO command_audit_disabled SELECT id,json_extract(CAST(payload AS TEXT),'$.generation') FROM sessions WHERE id='disabled';
                        INSERT INTO command_audit_disabled VALUES('stale','old-generation');")?;
                }
                db.pragma_update(None, "user_version", version)?;
                Ok(())
            }).await.unwrap();
            drop(store);
            stage_personal_sessions(&[source.path().into()], destination.path()).unwrap();
            let migrated = SqliteSessionStore::open(destination.path().into()).unwrap();
            let actual = migrated.with_connection(|db| {
                let mut query = db.prepare("SELECT session_id,generation,mode FROM command_audit_modes ORDER BY session_id")?;
                let modes = query.query_map([], |row| Ok((row.get::<_,String>(0)?, row.get::<_,String>(1)?, row.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
                let profile_count: i64 = db.query_row("SELECT count(*) FROM command_audit_profile", [], |row| row.get(0))?;
                Ok((modes, profile_count))
            }).await.unwrap();
            assert_eq!(actual, (expected, 0), "schema {version}");
        }
    }

    #[tokio::test]
    async fn owner_migration_rejects_conflicting_command_audit_settings() {
        for profile in [false, true] {
            let first = tempfile::tempdir().unwrap();
            let second = tempfile::tempdir().unwrap();
            let destination = tempfile::tempdir().unwrap();
            let session = Session::new(SessionId::new("shared"));
            for (directory, mode) in [(first.path(), "review"), (second.path(), "trusted")] {
                let store = SqliteSessionStore::open(directory.into()).unwrap();
                store.save(&session).await.unwrap();
                store
                    .with_connection(move |db| {
                        if profile {
                            db.execute("INSERT INTO command_audit_profile VALUES(1,?1)", [mode])?;
                        } else {
                            db.execute(
                                "UPDATE command_audit_modes SET mode=?1 WHERE session_id='shared'",
                                [mode],
                            )?;
                        }
                        Ok(())
                    })
                    .await
                    .unwrap();
            }
            let error = stage_personal_sessions(
                &[first.path().into(), second.path().into()],
                destination.path(),
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("Different records for key"),
                "{error:#}"
            );
        }
    }
}
