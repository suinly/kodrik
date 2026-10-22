//! Public-store integration tests: the child imports real legacy files and is killed
//! without dropping its SQLite connections. No production credentials or home state.
use super::session::{Session, SessionId, SessionStore, SqliteSessionStore};
use std::{
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};
fn sample() -> Session {
    serde_json::from_value(
        serde_json::json!({"id":"fixture", "messages":[{"User":{"content":"preserve me"}}]}),
    )
    .unwrap()
}
struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[tokio::test]
async fn migration_child() {
    let Ok(dir) = std::env::var("KODRIK_STORAGE_FIXTURE") else {
        return;
    };
    let store = SqliteSessionStore::open_async(dir.clone().into())
        .await
        .unwrap();
    assert!(
        store
            .load(&SessionId::new("fixture"))
            .await
            .unwrap()
            .is_some()
    );
    std::fs::write(Path::new(&dir).join("ready"), "import committed").unwrap();
    std::future::pending::<()>().await;
}
#[tokio::test]
async fn killed_importer_preserves_legacy_and_online_and_closed_backups_restore() {
    let root = tempfile::tempdir().unwrap();
    let original = serde_json::to_vec(&sample()).unwrap();
    std::fs::write(root.path().join("legacy.json"), &original).unwrap();
    let mut worker = Worker(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "agent::storage_recovery_tests::migration_child"])
            .env("KODRIK_STORAGE_FIXTURE", root.path())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    tokio::time::timeout(Duration::from_secs(15), async {
        while !root.path().join("ready").exists() {
            assert!(worker.0.try_wait().unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    worker.0.kill().unwrap();
    worker.0.wait().unwrap();
    assert_eq!(
        std::fs::read(root.path().join("legacy.json")).unwrap(),
        original
    );
    let store = SqliteSessionStore::open_async(root.path().into())
        .await
        .unwrap();
    let snapshot = tempfile::tempdir().unwrap();
    let db = rusqlite::Connection::open(root.path().join("state.sqlite3")).unwrap();
    db.backup("main", snapshot.path().join("state.sqlite3"), None)
        .unwrap();
    store.delete(&SessionId::new("fixture")).await.unwrap();
    let restored = SqliteSessionStore::open_async(snapshot.path().into())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(
            restored
                .load(&SessionId::new("fixture"))
                .await
                .unwrap()
                .unwrap()
        )
        .unwrap(),
        serde_json::to_value(sample()).unwrap()
    );
    drop(restored);
    drop(db);
    drop(store);
    let closed = tempfile::tempdir().unwrap();
    std::fs::copy(
        root.path().join("state.sqlite3"),
        closed.path().join("state.sqlite3"),
    )
    .unwrap();
    let restored = SqliteSessionStore::open_async(closed.path().into())
        .await
        .unwrap();
    assert!(restored.list().await.unwrap().is_empty());
    let reopened = SqliteSessionStore::open_async(root.path().into())
        .await
        .unwrap();
    assert!(
        reopened.list().await.unwrap().is_empty(),
        "stale JSON must not resurrect deleted history"
    );
}
#[test]
fn corrupt_and_future_databases_are_not_replaced() {
    for future in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.sqlite3");
        if future {
            let db = rusqlite::Connection::open(&path).unwrap();
            db.execute_batch("PRAGMA user_version=999; CREATE TABLE evidence(value); INSERT INTO evidence VALUES('keep');").unwrap();
        } else {
            std::fs::write(&path, b"corrupted SQLite evidence").unwrap();
        }
        let original = std::fs::read(&path).unwrap();
        std::fs::write(
            root.path().join("legacy.json"),
            serde_json::to_vec(&sample()).unwrap(),
        )
        .unwrap();
        assert!(SqliteSessionStore::open(root.path().into()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), original);
    }
}
#[tokio::test]
async fn v1_database_upgrades_without_losing_history() {
    let root = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open_async(root.path().into())
        .await
        .unwrap();
    store.save(&sample()).await.unwrap();
    drop(store);
    let db = rusqlite::Connection::open(root.path().join("state.sqlite3")).unwrap();
    // Reconstruct the v1 shape, retaining the original session payload.
    let drops=db.prepare("SELECT name,type FROM sqlite_master WHERE type IN ('trigger','table','index') AND (name LIKE 'view_%' OR name LIKE 'compaction%')").unwrap()
        .query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).unwrap().collect::<Result<Vec<_>,_>>().unwrap();
    for (name, kind) in drops {
        db.execute_batch(&format!("DROP {kind} IF EXISTS \"{name}\";"))
            .unwrap();
    }
    db.execute_batch("ALTER TABLE runs DROP COLUMN live; DELETE FROM schema_meta WHERE key='run_view_revision'; DROP TABLE run_controls; DROP TABLE inbox_controls; DROP TABLE deliveries; DROP INDEX submissions_pending; ALTER TABLE submissions DROP COLUMN status; ALTER TABLE submissions DROP COLUMN run_id; ALTER TABLE submissions DROP COLUMN session_id; PRAGMA user_version=1;").unwrap();
    drop(db);
    let upgraded = SqliteSessionStore::open_async(root.path().into())
        .await
        .unwrap();
    assert!(
        upgraded
            .load(&SessionId::new("fixture"))
            .await
            .unwrap()
            .is_some()
    );
    let db = rusqlite::Connection::open(root.path().join("state.sqlite3")).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        6
    );
    assert_eq!(
        db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
}
