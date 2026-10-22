use std::{fs, os::unix::fs::DirBuilderExt, path::Path};

use anyhow::{Context, ensure};
use rusqlite::{Connection, OpenFlags, types::Value};

/// The owner migration keeps both exclusive ownership locks through installation.
pub(crate) fn stage_owner_datasets(
    source: &Path,
    destination: &Path,
    staging: &Path,
) -> anyhow::Result<()> {
    let source_present = super::database_present(source)?;
    let destination_present = super::database_present(destination)?;
    if !source_present {
        return Ok(());
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(staging)?;
    let original = snapshot(source, staging)?;
    let source_db = readonly(original.path())?;
    let source_empty = empty(&source_db)?;
    if destination_present {
        let target = snapshot(destination, staging)?;
        let target_db = readonly(target.path())?;
        let target_empty = empty(&target_db)?;
        if !source_empty && !target_empty {
            // No automatic merge: a different destination may contain newer edits.
            for sql in [
                "SELECT sequence,name,definition,revision FROM datasets ORDER BY sequence",
                "SELECT sequence,dataset,id,revision,record_date,values_json FROM records ORDER BY sequence",
                "SELECT request_id,payload,response FROM receipts ORDER BY request_id",
            ] {
                let mut a = source_db.prepare(sql)?;
                let mut b = target_db.prepare(sql)?;
                let count = a.column_count();
                let mut a = a.query([])?;
                let mut b = b.query([])?;
                loop {
                    match (a.next()?, b.next()?) {
                        (None, None) => break,
                        (Some(a), Some(b)) => {
                            for i in 0..count {
                                ensure!(
                                    a.get::<_, Value>(i)? == b.get::<_, Value>(i)?,
                                    "Owner dataset migration conflict; back up and reconcile both stores"
                                );
                            }
                        }
                        _ => anyhow::bail!(
                            "Owner dataset migration conflict; back up and reconcile both stores"
                        ),
                    }
                }
            }
            return Ok(());
        }
    }
    if source_empty {
        return Ok(());
    }
    drop(source_db);
    original.as_file().sync_all()?;
    original.persist(staging.join("state.sqlite3"))?;
    fs::File::open(staging)?.sync_all()?;
    Ok(())
}

fn empty(db: &Connection) -> anyhow::Result<bool> {
    Ok(db.query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM datasets)
            AND NOT EXISTS(SELECT 1 FROM records)
            AND NOT EXISTS(SELECT 1 FROM receipts)",
        [],
        |row| row.get(0),
    )?)
}

fn readonly(path: &Path) -> anyhow::Result<Connection> {
    // macOS /var is a symlink; NOFOLLOW must still protect the database leaf.
    let path = fs::canonicalize(path.parent().context("Missing dataset directory")?)?
        .join(path.file_name().context("Missing dataset filename")?);
    Ok(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?)
}

fn snapshot(directory: &Path, staging: &Path) -> anyhow::Result<tempfile::NamedTempFile> {
    ensure!(
        fs::symlink_metadata(directory)?.is_dir(),
        "Invalid dataset migration directory"
    );
    for name in ["state.sqlite3", "state.sqlite3-wal", "state.sqlite3-shm"] {
        match fs::symlink_metadata(directory.join(name)) {
            Ok(metadata) => ensure!(metadata.is_file(), "Invalid dataset migration file"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let db = readonly(&directory.join("state.sqlite3"))?;
    let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    ensure!(
        version == super::VERSION,
        "Unsupported dataset migration schema {version}"
    );
    let snapshot = tempfile::NamedTempFile::new_in(staging)?;
    db.backup("main", snapshot.path(), None)?;
    let copy = readonly(snapshot.path())?;
    let integrity: String = copy.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    ensure!(integrity == "ok", "Dataset snapshot failed integrity check");
    Ok(snapshot)
}
