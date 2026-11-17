use super::{Session, SessionId, SessionStore, SqliteSessionStore};
use serde_json::json;

fn sample(id: &str) -> Session {
    serde_json::from_value(json!({
        "id": id, "system_prompt":"frozen", "memory_snapshot":"memory",
        "last_message_at":"2026-10-19T10:00:00Z", "input_tokens": 123,
        "telegram_group":{"chat_id":-42,"name":"Group","bot_username":"@bot",
            "participants":{"user:7":{"name":"Ann","username":"@ann"}}},
        "messages":[{"User":{"content":"first"}}, {"Assistant":{"content":"second"}}],
        "compaction":{"through":1,"summary":"summary"}
    }))
    .unwrap()
}

#[tokio::test]
async fn sqlite_roundtrip_selection_deletion_and_space_isolation() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("sessions");
    let store = SqliteSessionStore::open(path.clone()).unwrap();
    let conversation = SessionId::new("cli:main");
    assert_eq!(store.active_id(&conversation).await.unwrap(), conversation);
    assert!(store.list().await.unwrap().is_empty());
    let original = sample("cli:main");
    store.save(&original).await.unwrap();
    let fresh = store.new_session(&conversation).await.unwrap();
    assert_eq!(store.list().await.unwrap()[0].id(), fresh.id());
    assert_eq!(store.active_id(&conversation).await.unwrap(), *fresh.id());
    let selected = store.select(&conversation, original.id()).await.unwrap();
    assert_eq!(
        serde_json::to_value(selected).unwrap(),
        serde_json::to_value(&original).unwrap()
    );
    assert!(
        store
            .select(&conversation, &SessionId::new("missing"))
            .await
            .is_err()
    );
    assert_eq!(
        store.active_id(&conversation).await.unwrap(),
        *original.id()
    );
    drop(store);
    let reopened = SqliteSessionStore::open(path).unwrap();
    assert_eq!(
        serde_json::to_value(reopened.load(original.id()).await.unwrap().unwrap()).unwrap(),
        serde_json::to_value(original).unwrap()
    );
    assert_eq!(reopened.list().await.unwrap().len(), 2);
    reopened.delete(&conversation).await.unwrap();
    assert!(reopened.load(&conversation).await.unwrap().is_none());
    assert_eq!(
        reopened.active_id(&conversation).await.unwrap(),
        conversation
    );
    reopened.delete(&conversation).await.unwrap();
    let other = SqliteSessionStore::open(root.path().join("web")).unwrap();
    assert!(other.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn sqlite_imports_payload_ids_and_mapping_once_without_touching_legacy() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("active")).unwrap();
    let bytes = serde_json::to_vec_pretty(&sample("saved-id")).unwrap();
    let legacy = root.path().join("legacy-filename.json");
    std::fs::write(&legacy, &bytes).unwrap();
    std::fs::write(root.path().join("active/cli:main.json"), "\"saved-id\"").unwrap();
    let store = SqliteSessionStore::open(root.path().into()).unwrap();
    assert_eq!(
        store
            .active_id(&SessionId::new("cli:main"))
            .await
            .unwrap()
            .as_str(),
        "saved-id"
    );
    assert!(
        store
            .load(&SessionId::new("saved-id"))
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(std::fs::read(&legacy).unwrap(), bytes);
    store.delete(&SessionId::new("saved-id")).await.unwrap();
    drop(store);
    // Old files must never resurrect deleted sessions or block a completed migration.
    std::fs::write(&legacy, "malformed stale JSON").unwrap();
    let store = SqliteSessionStore::open(root.path().into()).unwrap();
    assert!(store.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn sqlite_failed_import_rolls_back_and_can_be_retried() {
    for invalid in ["corrupt", "missing_mapping", "conflicting_id"] {
        let root = tempfile::tempdir().unwrap();
        let bytes = serde_json::to_vec(&sample("good")).unwrap();
        std::fs::write(root.path().join("a.json"), &bytes).unwrap();
        let bad = match invalid {
            "corrupt" => {
                let p = root.path().join("z.json");
                std::fs::write(&p, "bad").unwrap();
                p
            }
            "missing_mapping" => {
                std::fs::create_dir(root.path().join("active")).unwrap();
                let p = root.path().join("active/chat.json");
                std::fs::write(&p, "\"missing\"").unwrap();
                p
            }
            _ => {
                let p = root.path().join("z.json");
                std::fs::write(&p, r#"{"id":"good","messages":[]}"#).unwrap();
                p
            }
        };
        assert!(
            SqliteSessionStore::open(root.path().into()).is_err(),
            "{invalid}"
        );
        let db = rusqlite::Connection::open(root.path().join("state.sqlite3")).unwrap();
        let count = db
            .query_row("SELECT count(*) FROM sessions", [], |r| r.get::<_, i64>(0))
            .unwrap();
        assert_eq!(count, 0, "{invalid}");
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM schema_meta WHERE key='legacy_imported'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        drop(db);
        std::fs::remove_file(bad).unwrap();
        let store = SqliteSessionStore::open(root.path().into()).unwrap();
        assert_eq!(store.list().await.unwrap().len(), 1);
        assert_eq!(std::fs::read(root.path().join("a.json")).unwrap(), bytes);
    }
}

#[tokio::test]
async fn sqlite_new_session_is_atomic_when_mapping_write_fails() {
    let root = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(root.path().into()).unwrap();
    let original = sample("old");
    store.save(&original).await.unwrap();
    let conversation = SessionId::new("chat");
    store.select(&conversation, original.id()).await.unwrap();
    let db = rusqlite::Connection::open(root.path().join("state.sqlite3")).unwrap();
    db.execute_batch("CREATE TRIGGER reject_mapping BEFORE INSERT ON active_sessions BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
    assert!(store.new_session(&conversation).await.is_err());
    assert_eq!(store.list().await.unwrap().len(), 1);
    assert_eq!(
        store.active_id(&conversation).await.unwrap().as_str(),
        "old"
    );
}
#[cfg(unix)]
#[test]
fn sqlite_refuses_future_schema_and_symlink_database() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("state.sqlite3");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(
        "PRAGMA user_version=999; CREATE TABLE untouched(value); INSERT INTO untouched VALUES (7);",
    )
    .unwrap();
    drop(db);
    let before = std::fs::read(&path).unwrap();
    assert!(SqliteSessionStore::open(root.path().into()).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let link_dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(&path, link_dir.path().join("state.sqlite3")).unwrap();
    assert!(SqliteSessionStore::open(link_dir.path().into()).is_err());
}
#[cfg(unix)]
#[test]
fn sqlite_database_and_sidecars_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("sessions");
    let _store = SqliteSessionStore::open(dir.clone()).unwrap();
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for name in ["state.sqlite3", "state.sqlite3-wal", "state.sqlite3-shm"] {
        assert_eq!(
            std::fs::metadata(dir.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "{name}"
        );
    }
}
#[cfg(unix)]
#[test]
fn sqlite_import_refuses_symlinked_active_directory() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("chat.json"),
        serde_json::to_vec(&sample("chat")).unwrap(),
    )
    .unwrap();
    std::fs::write(outside.path().join("conversation.json"), "\"chat\"").unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("active")).unwrap();
    assert!(SqliteSessionStore::open(root.path().into()).is_err());
}

#[tokio::test]
async fn sqlite_search_bounds_candidates_bytes_and_individual_payloads() {
    let root = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(root.path().into()).unwrap();
    let mut db = rusqlite::Connection::open(root.path().join("state.sqlite3")).unwrap();
    let tx = db.transaction().unwrap();
    for n in 0..1001 {
        let id = format!("chat-{n:04}");
        tx.execute(
            "INSERT INTO sessions VALUES (?1,?2,1)",
            rusqlite::params![id, serde_json::to_vec(&sample(&id)).unwrap()],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    let found = store.search_sessions(None).await.unwrap();
    assert_eq!(found.scanned, 1000);
    assert_eq!(found.sessions.len(), 1000);
    assert!(found.truncated);
    assert_eq!(
        store
            .search_sessions(Some("chat-1000".into()))
            .await
            .unwrap()
            .sessions
            .len(),
        1
    );
    db.execute("DELETE FROM sessions", []).unwrap();
    for n in 0..5 {
        db.execute(
            "INSERT INTO sessions VALUES (?1,zeroblob(16777216),1)",
            [format!("large-{n}")],
        )
        .unwrap();
    }
    db.execute(
        "INSERT INTO sessions VALUES ('oversized',zeroblob(16777217),2)",
        [],
    )
    .unwrap();
    let found = store.search_sessions(None).await.unwrap();
    assert_eq!(found.scanned, 5); // one oversized + four 16 MiB payloads
    assert_eq!(found.skipped, 5);
    assert!(found.truncated);
}

#[test]
fn sqlite_schema_enforces_run_and_submission_uniqueness_and_foreign_keys() {
    let root = tempfile::tempdir().unwrap();
    let _store = SqliteSessionStore::open(root.path().into()).unwrap();
    let db = rusqlite::Connection::open(root.path().join("state.sqlite3")).unwrap();
    db.execute_batch("PRAGMA foreign_keys=ON; INSERT INTO sessions VALUES ('s',x'7b7d',1);")
        .unwrap();
    db.execute(
        "INSERT INTO runs VALUES ('r1','c','s',0,0,x'7b7d',NULL)",
        [],
    )
    .unwrap();
    assert!(
        db.execute(
            "INSERT INTO runs VALUES ('r2','c','s',0,0,x'7b7d',NULL)",
            []
        )
        .is_err()
    );
    assert!(
        db.execute(
            "INSERT INTO runs VALUES ('r2','other','missing',0,0,x'7b7d',NULL)",
            []
        )
        .is_err()
    );
    db.execute(
        "INSERT INTO tool_executions VALUES ('r1','call',x'7b7d')",
        [],
    )
    .unwrap();
    assert!(
        db.execute(
            "INSERT INTO tool_executions VALUES ('r1','call',x'7b7d')",
            []
        )
        .is_err()
    );
    db.execute(
        "INSERT INTO submissions(id,conversation_id,request_id,payload_hash,sequence,payload) VALUES ('s1','c','request','hash',1,x'7b7d')",
        [],
    )
    .unwrap();
    assert!(
        db.execute(
            "INSERT INTO submissions(id,conversation_id,request_id,payload_hash,sequence,payload) VALUES ('s2','c','request','hash',2,x'7b7d')",
            []
        )
        .is_err()
    );
    db.execute(
        "INSERT INTO submissions(id,conversation_id,request_id,payload_hash,sequence,payload) VALUES ('s3','other','request','hash',1,x'7b7d')",
        [],
    )
    .unwrap();
}

#[test]
fn sqlite_concurrent_first_open_imports_once() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("legacy.json"),
        serde_json::to_vec(&sample("legacy")).unwrap(),
    )
    .unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let barrier = barrier.clone();
                let directory = root.path().to_owned();
                scope.spawn(move || {
                    barrier.wait();
                    SqliteSessionStore::open(directory).unwrap()
                })
            })
            .collect();
        let stores: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(stores.len(), 4);
        let db = rusqlite::Connection::open(root.path().join("state.sqlite3")).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM sessions", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    });
}

#[tokio::test]
async fn sqlite_cancelled_worker_keeps_migration_lock_until_write_finishes() {
    let root = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(root.path().into()).unwrap();
    let connection = store.connection.clone();
    let references = std::sync::Arc::downgrade(&connection);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocker = std::thread::spawn(move || {
        let _guard = connection.lock().unwrap();
        ready_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    ready_rx.recv().unwrap();
    let task = tokio::spawn(async move { store.save(&sample("queued")).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while references.strong_count() < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    let protected = super::ownership_lock(root.path(), true).is_err();
    release_tx.send(()).unwrap();
    blocker.join().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while references.strong_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        protected,
        "A cancelled async caller must not expose its queued write to owner migration"
    );
    let reopened = SqliteSessionStore::open(root.path().into()).unwrap();
    assert!(
        reopened
            .load(&SessionId::new("queued"))
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn preparing_session_keeps_history_unchanged_until_first_save() {
    let root = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(root.path().into()).unwrap();
    let conversation = SessionId::new("chat");
    let original = sample("old");
    store.save(&original).await.unwrap();
    store.select(&conversation, original.id()).await.unwrap();
    let draft = store.prepare_session(&conversation).await.unwrap();
    assert_ne!(draft.id(), original.id());
    assert_eq!(store.list().await.unwrap().len(), 1);
    assert!(store.load(draft.id()).await.unwrap().is_none());
    drop(store);
    let store = SqliteSessionStore::open(root.path().into()).unwrap();
    assert_eq!(store.active_id(&conversation).await.unwrap(), *draft.id());
    let mut draft = draft;
    draft.push(crate::agent::message::Message::User(
        crate::agent::UserMessage {
            browser_client_id: None,
            browser_context: None,
            content: "hello".into(),
            images: vec![],
            files: vec![],
        },
    ));
    store.save(&draft).await.unwrap();
    assert_eq!(store.list().await.unwrap().len(), 2);
    store.delete(draft.id()).await.unwrap();
    assert_eq!(store.active_id(&conversation).await.unwrap(), conversation);
}

#[tokio::test]
async fn session_model_setter_persists_resets_and_rejects_busy_or_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().into()).unwrap();
    store.save(&sample("target")).await.unwrap();
    store.save(&sample("other")).await.unwrap();
    assert!(
        store
            .set_session_model("target", Some("provider/model".into()))
            .await
            .unwrap()
    );
    let saved = store
        .load(&SessionId::new("target"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.model_override(), Some("provider/model"));
    assert_eq!(saved.messages().len(), 2);
    assert!(
        store
            .load(&SessionId::new("other"))
            .await
            .unwrap()
            .unwrap()
            .model_override()
            .is_none()
    );
    for model in [String::new(), "x".repeat(513), "bad\nmodel".into()] {
        assert!(
            store
                .set_session_model("target", Some(model))
                .await
                .is_err()
        );
    }
    assert!(store.set_session_model("missing", None).await.is_err());
    store
        .with_connection(|db| {
            db.execute(
                "INSERT INTO runs VALUES ('model-run','web:target','target',0,0,x'7b7d',NULL)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(!store.set_session_model("target", None).await.unwrap());
    store.with_connection(|db| {
        db.execute("DELETE FROM runs", [])?;
        db.execute("INSERT INTO active_sessions VALUES ('channel','target')", [])?;
        db.execute("INSERT INTO submissions(id,conversation_id,request_id,payload_hash,sequence,payload) VALUES ('queued-model','channel','req','hash',1,x'7b7d')", [])?;
        Ok(())
    }).await.unwrap();
    assert!(!store.set_session_model("target", None).await.unwrap());
    assert_eq!(
        store
            .load(&SessionId::new("target"))
            .await
            .unwrap()
            .unwrap()
            .model_override(),
        Some("provider/model")
    );
    store
        .with_connection(|db| {
            db.execute("DELETE FROM submissions", [])?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(store.set_session_model("target", None).await.unwrap());
    assert!(
        store
            .load(&SessionId::new("target"))
            .await
            .unwrap()
            .unwrap()
            .model_override()
            .is_none()
    );
}

#[tokio::test]
async fn local_mapped_draft_model_survives_reopen_reset_and_first_save() {
    let root = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(root.path().into()).unwrap();
    store.save(&sample("old")).await.unwrap();
    let conversation = SessionId::new("cli:main");
    let draft = store.prepare_session(&conversation).await.unwrap();
    let id = draft.id().clone();
    assert!(
        store
            .set_session_model(id.as_str(), Some("chosen".into()))
            .await
            .unwrap()
    );
    assert!(
        store
            .with_connection({
                let id = id.clone();
                move |db| {
                    Ok(db.query_row(
                        "SELECT NOT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
                        [id.as_str()],
                        |r| r.get::<_, bool>(0),
                    )?)
                }
            })
            .await
            .unwrap()
    );
    assert_eq!(store.list().await.unwrap().len(), 1);
    drop(store);
    let store = SqliteSessionStore::open(root.path().into()).unwrap();
    assert_eq!(
        store.load(&id).await.unwrap().unwrap().model_override(),
        Some("chosen")
    );
    assert_eq!(store.active_id(&conversation).await.unwrap(), id);
    assert!(store.set_session_model(id.as_str(), None).await.unwrap());
    assert!(
        store
            .load(&id)
            .await
            .unwrap()
            .is_none_or(|s| s.model_override().is_none())
    );
    assert!(
        store
            .with_connection({
                let id = id.clone();
                move |db| {
                    Ok(db.query_row(
                        "SELECT NOT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
                        [id.as_str()],
                        |r| r.get::<_, bool>(0),
                    )?)
                }
            })
            .await
            .unwrap()
    );
    assert_eq!(store.list().await.unwrap().len(), 1);
    assert!(
        store
            .set_session_model("unknown", Some("chosen".into()))
            .await
            .is_err()
    );
    assert!(
        store
            .set_session_model(id.as_str(), Some("chosen".into()))
            .await
            .unwrap()
    );
    let mut session = store.load(&id).await.unwrap().unwrap();
    session.push(crate::agent::message::Message::User(
        crate::agent::UserMessage {
            browser_client_id: None,
            browser_context: None,
            content: "first".into(),
            images: vec![],
            files: vec![],
        },
    ));
    store.save(&session).await.unwrap();
    assert!(!store.is_draft(id.as_str()).await.unwrap());
    assert_eq!(store.list().await.unwrap().len(), 2);
    assert_eq!(
        store.load(&id).await.unwrap().unwrap().model_override(),
        Some("chosen")
    );
    store
        .with_connection(|db| {
            let count: i64 = db.query_row("SELECT count(*) FROM draft_models", [], |r| r.get(0))?;
            assert_eq!(count, 0);
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn v16_migration_adds_draft_models_without_changing_saved_sessions() {
    let root = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(root.path().into()).unwrap();
    store.save(&sample("saved")).await.unwrap();
    let draft = store
        .prepare_session(&SessionId::new("cli:main"))
        .await
        .unwrap();
    store
        .with_connection(|db| {
            db.execute_batch("DROP TABLE draft_models; PRAGMA user_version=16;")?;
            Ok(())
        })
        .await
        .unwrap();
    drop(store);
    let store = SqliteSessionStore::open(root.path().into()).unwrap();
    assert_eq!(store.list().await.unwrap().len(), 1);
    assert!(
        store
            .set_session_model(draft.id().as_str(), Some("chosen".into()))
            .await
            .unwrap()
    );
    assert_eq!(
        store
            .load(draft.id())
            .await
            .unwrap()
            .unwrap()
            .model_override(),
        Some("chosen")
    );
    assert_eq!(
        store
            .load(&SessionId::new("saved"))
            .await
            .unwrap()
            .unwrap()
            .messages()
            .len(),
        2
    );
}
