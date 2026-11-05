use crate::agent::{
    AssistantMessage, UserMessage,
    command_audit::CommandAuditMode,
    message::Message,
    run_state::{Checkpoint, RunPhase},
    session::{Session, SessionId, SessionStore, SqliteSessionStore},
};

#[tokio::test]
async fn push_outbox_queues_new_approvals_once_and_follows_audit_mode_changes() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().into()).unwrap();
    store
        .with_connection(|db| {
            db.execute(
                "INSERT INTO web_push_subscriptions VALUES('device','hash','{}','ru')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    for (id, mode, verdict, expected) in [
        ("review-risk", CommandAuditMode::Review, "destructive", true),
        ("review-safe", CommandAuditMode::Review, "safe", false),
        ("ask", CommandAuditMode::Ask, "ask", true),
        ("trusted", CommandAuditMode::Trusted, "destructive", false),
        (
            "webhook-risk",
            CommandAuditMode::Review,
            "destructive",
            true,
        ),
        (
            "telegram:main:-123",
            CommandAuditMode::Review,
            "destructive",
            false,
        ),
        (
            "telegram:main:42:guest",
            CommandAuditMode::Review,
            "destructive",
            false,
        ),
    ] {
        store.set_default_command_audit_mode(mode).await.unwrap();
        let mut session = Session::new(SessionId::new(id));
        if id == "webhook-risk" {
            session.webhook_instruction = Some("Webhook owner instruction".into());
        }
        let call = crate::agent::message::ToolCall {
            call_id: "call".into(),
            name: "exec_command".into(),
            arguments: serde_json::json!({"cmd":"secret command"}),
        };
        session.push(Message::ToolCall(call.clone()));
        store.save(&session).await.unwrap();
        let mut cp = Checkpoint::new(&store, session.id(), &session);
        cp.record.pending_calls = vec![call.clone()];
        cp.save(
            &session,
            RunPhase::NeedTool {
                call_id: call.call_id.clone(),
            },
            None,
        )
        .await
        .unwrap();
        let audits = [(call, verdict.into())];
        store
            .record_command_audits(&cp.record, &audits)
            .await
            .unwrap();
        let target = id.to_owned();
        let queued: Vec<String> = store
            .with_connection(move |db| {
                Ok(db
                    .prepare("SELECT event_id FROM web_push_queue WHERE session_id=?1")?
                    .query_map([target], |row| row.get(0))?
                    .collect::<Result<_, _>>()?)
            })
            .await
            .unwrap();
        assert_eq!(queued.len(), usize::from(expected), "{id}");
        if expected {
            let approval = store.pending_command_approvals(id).await.unwrap().remove(0);
            assert_eq!(queued, [format!("approval:{}", approval.id)]);
        }
        // Delivery removes queue rows; replaying an audit must not notify again.
        store
            .with_connection(|db| {
                db.execute("DELETE FROM web_push_queue", [])?;
                Ok(())
            })
            .await
            .unwrap();
        store
            .record_command_audits(&cp.record, &audits)
            .await
            .unwrap();
        assert_eq!(
            store
                .with_connection(|db| Ok(db.query_row(
                    "SELECT count(*) FROM web_push_queue",
                    [],
                    |r| r.get::<_, i64>(0)
                )?))
                .await
                .unwrap(),
            0
        );
        if id == "webhook-risk" {
            // Enabling approval notifications must not enable ordinary webhook completion pushes.
            session.push(Message::Assistant(AssistantMessage {
                content: "done".into(),
            }));
            cp.save(&session, RunPhase::Completed, None).await.unwrap();
            let event = cp.record.id.clone();
            let count: i64 = store
                .with_connection(move |db| {
                    Ok(db.query_row(
                        "SELECT count(*) FROM web_push_queue WHERE event_id=?1",
                        [event],
                        |r| r.get(0),
                    )?)
                })
                .await
                .unwrap();
            assert_eq!(count, 0);
        }
    }
    store
        .set_command_audit_mode("review-safe", CommandAuditMode::Ask)
        .await
        .unwrap();
    let store = SqliteSessionStore::open(dir.path().into()).unwrap();
    assert_eq!(
        store
            .with_connection(|db| Ok(db.query_row(
                "SELECT session_id FROM web_push_queue",
                [],
                |r| r.get::<_, String>(0)
            )?))
            .await
            .unwrap(),
        "review-safe"
    );
    store
        .with_connection(|db| {
            db.execute("DELETE FROM web_push_queue", [])?;
            Ok(())
        })
        .await
        .unwrap();
    store
        .set_command_audit_mode("review-safe", CommandAuditMode::Ask)
        .await
        .unwrap();
    assert_eq!(
        store
            .with_connection(|db| Ok(db.query_row(
                "SELECT count(*) FROM web_push_queue",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn push_api_requires_authentication_and_the_configured_origin() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let dir = tempfile::tempdir().unwrap();
    let settings = super::Settings {
        base_url: "https://example.com/kodrik/".parse().unwrap(),
        listen: "127.0.0.1:0".parse().unwrap(),
        trusted_proxies: vec![],
    };
    let chat = super::chat_tests::chat(dir.path()).await;
    let auth = super::auth::AuthStore::new(dir.path());
    let token = auth
        .test_session(super::auth::Identity::Owner, chrono::Utc::now().timestamp())
        .unwrap()
        .unwrap();
    let app = super::server::router_with_chat(settings, dir.path(), Some(chat));
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/kodrik/api/chat/push")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kodrik/api/chat/push")
                .header("cookie", format!("kodrik_web_session={token}"))
                .header("origin", "https://evil.example")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn push_outbox_commits_only_completed_personal_answers_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    store.with_connection(|db| {
        db.execute("INSERT INTO web_push_subscriptions(endpoint,session_hash,subscription,locale) VALUES('device','hash','{}','ru')", [])?;
        Ok(())
    }).await.unwrap();
    for (id, phase) in [
        ("personal", RunPhase::Completed),
        (
            "failed",
            RunPhase::Failed {
                reason: "failed".into(),
            },
        ),
        ("cancelled", RunPhase::Cancelled),
        ("companion:subagent:child", RunPhase::Completed),
        ("telegram:main:-123", RunPhase::Completed),
        ("telegram:main:42:guest", RunPhase::Completed),
    ] {
        let mut session = Session::new(SessionId::new(id));
        session.push(Message::User(UserMessage {
            content: "question".into(),
            images: vec![],
            files: vec![],
        }));
        store.save(&session).await.unwrap();
        let mut cp = Checkpoint::new(&store, session.id(), &session);
        cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
        session.push(Message::Assistant(AssistantMessage {
            content: "answer".into(),
        }));
        cp.save(&session, phase, None).await.unwrap();
        store
            .commit_step(cp.record.revision - 1, &session, &cp.record, None)
            .await
            .unwrap();
    }
    let queued = store
        .with_connection(|db| {
            Ok(db
                .prepare("SELECT session_id FROM web_push_queue")?
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap();
    assert_eq!(queued, ["personal"]);
    drop(store);
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    assert_eq!(
        store
            .with_connection(|db| Ok(db.query_row(
                "SELECT count(*) FROM web_push_queue",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
        1
    );
    store.delete(&SessionId::new("personal")).await.unwrap();
    assert_eq!(
        store
            .with_connection(|db| Ok(db.query_row(
                "SELECT count(*) FROM web_push_queue",
                [],
                |r| r.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn push_outbox_includes_explicit_web_messages_once_and_ignores_history_on_subscribe() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    let mut session = Session::new(SessionId::new("target"));
    session.push(Message::Assistant(AssistantMessage {
        content: "old".into(),
    }));
    store.save(&session).await.unwrap();
    store
        .with_connection(|db| {
            db.execute(
                "INSERT INTO web_push_subscriptions VALUES('device','hash','{}','ru')",
                [],
            )?;
            assert_eq!(
                db.query_row("SELECT count(*) FROM web_push_queue", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            Ok(())
        })
        .await
        .unwrap();
    let receipt = store
        .queue_notification(
            "run",
            "call",
            "source",
            "web",
            "target",
            "New explicit message".into(),
        )
        .await
        .unwrap();
    assert!(
        store
            .deliver_local_notification(&receipt)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .deliver_local_notification(&receipt)
            .await
            .unwrap()
            .is_some()
    );
    let queued = store
        .with_connection(|db| {
            Ok(db
                .prepare("SELECT event_id,session_id FROM web_push_queue")?
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap();
    assert_eq!(queued, [("run:call".into(), "target".into())]);
}

#[tokio::test]
async fn push_state_survives_personal_store_migration() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("old");
    let destination = dir.path().join("new");
    let store = SqliteSessionStore::open_async(source.clone())
        .await
        .unwrap();
    store
        .save(&Session::new(SessionId::new("personal")))
        .await
        .unwrap();
    store
        .with_connection(|db| {
            db.execute(
                "INSERT INTO schema_meta VALUES('web_push_vapid','stable-key')",
                [],
            )?;
            db.execute(
                "INSERT INTO web_push_subscriptions VALUES('device','hash','{}','ru')",
                [],
            )?;
            db.execute("INSERT INTO web_push_bindings VALUES('device','hash')", [])?;
            db.execute(
                "INSERT INTO web_push_presence VALUES('hash','tab','personal',100)",
                [],
            )?;
            db.execute(
                "INSERT INTO web_push_queue VALUES('event','device','personal',1,110,100)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    drop(store);
    crate::agent::session::stage_personal_sessions(&[source], &destination).unwrap();
    let store = SqliteSessionStore::open_async(destination).await.unwrap();
    store
        .with_connection(|db| {
            assert_eq!(
                db.query_row(
                    "SELECT value FROM schema_meta WHERE key='web_push_vapid'",
                    [],
                    |r| r.get::<_, String>(0)
                )?,
                "stable-key"
            );
            assert_eq!(
                db.query_row("SELECT attempts FROM web_push_queue", [], |r| r
                    .get::<_, i64>(0))?,
                1
            );
            assert_eq!(
                db.query_row("SELECT session_hash FROM web_push_bindings", [], |r| r
                    .get::<_, String>(
                    0
                ))?,
                "hash"
            );
            assert_eq!(
                db.query_row("SELECT session_id FROM web_push_presence", [], |r| r
                    .get::<_, String>(0))?,
                "personal"
            );
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn push_presence_v7_upgrade_preserves_durable_delivery_state() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    store
        .save(&Session::new(SessionId::new("personal")))
        .await
        .unwrap();
    store.with_connection(|db| {
        db.execute_batch("INSERT INTO schema_meta VALUES('web_push_vapid','stable-key');
            INSERT INTO web_push_subscriptions VALUES('device','hash','{}','ru');
            INSERT INTO web_push_queue VALUES('event','device','personal',2,110,100);
            DROP TABLE command_audits;
            DROP TABLE remote_tool_attachments;
            DROP TABLE remote_operations;
            DROP TABLE remote_bindings;
            DROP TABLE command_audit_modes;
            DROP TABLE command_audit_profile;
            DROP TABLE web_push_presence;
            CREATE TABLE web_push_presence(endpoint TEXT NOT NULL REFERENCES web_push_subscriptions(endpoint) ON DELETE CASCADE,
                tab_id TEXT NOT NULL,session_id TEXT NOT NULL,updated_at INTEGER NOT NULL,PRIMARY KEY(endpoint,tab_id));
            INSERT INTO web_push_presence VALUES('device','tab','personal',100);
            PRAGMA user_version=7;")?;
        Ok(())
    }).await.unwrap();
    drop(store);
    let store = SqliteSessionStore::open_async(dir.path().into())
        .await
        .unwrap();
    store
        .with_connection(|db| {
            assert_eq!(
                db.query_row("SELECT attempts FROM web_push_queue", [], |r| r
                    .get::<_, i64>(0))?,
                2
            );
            assert_eq!(
                db.query_row(
                    "SELECT value FROM schema_meta WHERE key='web_push_vapid'",
                    [],
                    |r| r.get::<_, String>(0)
                )?,
                "stable-key"
            );
            assert_eq!(
                db.query_row("SELECT count(*) FROM web_push_presence", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            db.execute(
                "INSERT INTO web_push_presence VALUES('unsubscribed-session','tab','personal',200)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}
