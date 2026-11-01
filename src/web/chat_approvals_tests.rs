use super::*;
use crate::agent::{
    command_audit::{CommandApproval, CommandAuditMode},
    run_state::{Checkpoint, RunPhase},
    session::{Session, SessionId},
};

async fn pending(store: &SqliteSessionStore, mut session: Session, owner: &str) -> CommandApproval {
    store
        .set_command_audit_mode(owner, CommandAuditMode::Review)
        .await
        .unwrap();
    let call = ToolCall {
        call_id: "shell-approval".into(),
        name: "exec_command".into(),
        arguments: json!({"cmd":format!("printf '%s' '{}'\nprintf done", "<exact> & ".repeat(100)),"workdir":"/tmp/work dir"}),
    };
    session.push(Message::ToolCall(call.clone()));
    let mut checkpoint = Checkpoint::new(store, session.id(), &session);
    checkpoint.record.pending_calls = vec![call.clone()];
    checkpoint
        .save(
            &session,
            RunPhase::NeedTool {
                call_id: call.call_id.clone(),
            },
            None,
        )
        .await
        .unwrap();
    store
        .record_command_audits(&checkpoint.record, &[(call, "destructive".into())])
        .await
        .unwrap();
    store
        .pending_command_approvals(owner)
        .await
        .unwrap()
        .remove(0)
}

async fn snapshot(fixture: &Fixture, id: &str) -> Value {
    fixture
        .request(reqwest::Method::GET, &format!("sessions/{id}/run"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn command_audit_mode_requires_session_auth_and_keeps_other_sessions_unchanged() {
    let data = tempfile::tempdir().unwrap();
    let fixture = Fixture::configured(Some(data.path()), "127.0.0.1:0").await;
    let id = fixture.create().await;
    let other = fixture.create().await;
    let store = SqliteSessionStore::open(data.path().join("owner-sessions")).unwrap();
    assert_eq!(
        snapshot(&fixture, &id).await["command_audit_mode"],
        "trusted"
    );
    let session = store.load(&SessionId::new(&id)).await.unwrap().unwrap();
    pending(&store, session, &id).await;
    let before = snapshot(&fixture, &id).await;
    assert_eq!(before["command_audit_mode"], "review");
    assert_eq!(before["command_approvals"].as_array().unwrap().len(), 1);
    let url = format!("{}api/chat/sessions/{id}/command-audit", fixture.base);
    let guest = AuthStore::new(data.path())
        .test_session(
            super::super::auth::Identity::Telegram(7),
            chrono::Utc::now().timestamp(),
        )
        .unwrap()
        .unwrap();
    let guest_cookie = format!("{}={guest}", fixture.cookie.split('=').next().unwrap());
    for (cookie, origin, expected) in [
        (None, fixture.origin.as_str(), 401),
        (Some(fixture.cookie.as_str()), "https://other.invalid", 403),
        (Some(guest_cookie.as_str()), fixture.origin.as_str(), 404),
    ] {
        let mut request = fixture
            .client
            .post(&url)
            .header("Origin", origin)
            .json(&json!({"mode":"trusted"}));
        if let Some(cookie) = cookie {
            request = request.header("Cookie", cookie);
        }
        assert_eq!(request.send().await.unwrap().status(), expected);
    }
    assert_eq!(
        snapshot(&fixture, &id).await["command_audit_mode"],
        "review"
    );
    for body in [
        json!({"mode":"unknown"}),
        json!({"mode":"trusted","session_id":other}),
        json!({"enabled":false}),
    ] {
        assert_eq!(
            fixture
                .request(
                    reqwest::Method::POST,
                    &format!("sessions/{id}/command-audit")
                )
                .json(&body)
                .send()
                .await
                .unwrap()
                .status(),
            422
        );
    }
    assert_eq!(
        fixture
            .request(
                reqwest::Method::POST,
                "sessions/missing-session/command-audit"
            )
            .json(&json!({"mode":"trusted"}))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    for mode in [
        CommandAuditMode::Trusted,
        CommandAuditMode::Ask,
        CommandAuditMode::Review,
    ] {
        assert_eq!(
            fixture
                .request(
                    reqwest::Method::POST,
                    &format!("sessions/{id}/command-audit")
                )
                .json(&json!({"mode":mode}))
                .send()
                .await
                .unwrap()
                .status(),
            204
        );
        let after = snapshot(&fixture, &id).await;
        assert_eq!(after["command_audit_mode"], json!(mode));
        assert!(after["revision"].as_i64().unwrap() > before["revision"].as_i64().unwrap());
        if mode == CommandAuditMode::Trusted {
            assert_eq!(after["command_approvals"], json!([]));
        }
        assert_eq!(
            snapshot(&fixture, &other).await["command_audit_mode"],
            "trusted"
        );
        assert_eq!(
            SqliteSessionStore::open(data.path().join("owner-sessions"))
                .unwrap()
                .command_audit_mode(&id)
                .await
                .unwrap(),
            mode
        );
    }
}

#[tokio::test]
async fn profile_command_audit_default_is_authenticated_isolated_and_only_affects_new_sessions() {
    let data = tempfile::tempdir().unwrap();
    let fixture = Fixture::configured(Some(data.path()), "127.0.0.1:0").await;
    let existing = fixture.create().await;
    let url = format!("{}api/profile/command-audit", fixture.base);
    let guest = AuthStore::new(data.path())
        .test_session(
            super::super::auth::Identity::Telegram(7),
            chrono::Utc::now().timestamp(),
        )
        .unwrap()
        .unwrap();
    let guest_cookie = format!("{}={guest}", fixture.cookie.split('=').next().unwrap());
    for method in [reqwest::Method::GET, reqwest::Method::POST] {
        assert_eq!(
            fixture
                .client
                .request(method, &url)
                .header("Origin", &fixture.origin)
                .json(&json!({"mode":"ask"}))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
    }
    assert_eq!(
        fixture
            .client
            .post(&url)
            .header("Cookie", &fixture.cookie)
            .header("Origin", "https://other.invalid")
            .json(&json!({"mode":"ask"}))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        fixture
            .client
            .post(&url)
            .header("Cookie", &fixture.cookie)
            .header("Origin", &fixture.origin)
            .json(&json!({"mode":"invalid"}))
            .send()
            .await
            .unwrap()
            .status(),
        422
    );
    for cookie in [&fixture.cookie, &guest_cookie] {
        let value: Value = fixture
            .client
            .get(&url)
            .header("Cookie", cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(value, json!({"mode":"trusted"}));
    }
    for (cookie, mode) in [(&fixture.cookie, "ask"), (&guest_cookie, "review")] {
        assert_eq!(
            fixture
                .client
                .post(&url)
                .header("Cookie", cookie)
                .header("Origin", &fixture.origin)
                .json(&json!({"mode":mode}))
                .send()
                .await
                .unwrap()
                .status(),
            204
        );
    }
    assert_eq!(
        snapshot(&fixture, &existing).await["command_audit_mode"],
        "trusted"
    );
    let fresh = fixture.create().await;
    assert_eq!(
        snapshot(&fixture, &fresh).await["command_audit_mode"],
        "ask"
    );
    for (cookie, mode) in [(&fixture.cookie, "ask"), (&guest_cookie, "review")] {
        let value: Value = fixture
            .client
            .get(&url)
            .header("Cookie", cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(value, json!({"mode":mode}));
    }
    let guest_session: Value = fixture
        .client
        .post(format!("{}api/chat/sessions", fixture.base))
        .header("Cookie", &guest_cookie)
        .header("Origin", &fixture.origin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let guest_view: Value = fixture
        .client
        .get(format!(
            "{}api/chat/sessions/{}/run",
            fixture.base,
            guest_session["id"].as_str().unwrap()
        ))
        .header("Cookie", &guest_cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(guest_view["command_audit_mode"], "review");
    assert_eq!(
        SqliteSessionStore::open(data.path().join("owner-sessions"))
            .unwrap()
            .default_command_audit_mode()
            .await
            .unwrap(),
        CommandAuditMode::Ask
    );
}

#[tokio::test]
async fn edited_branch_captures_profile_mode_before_its_first_run_save() {
    let fixture = Fixture::new().await;
    let parent = fixture.create().await;
    let store = SqliteSessionStore::open(fixture.data.path().join("owner-sessions")).unwrap();
    let mut session = store.load(&SessionId::new(&parent)).await.unwrap().unwrap();
    session.push(Message::User(crate::agent::UserMessage {
        content: "Original question".into(),
        files: vec![],
        images: vec![],
    }));
    store.save(&session).await.unwrap();
    let profile_url = format!("{}api/profile/command-audit", fixture.base);
    let profile_mode = |mode| {
        fixture
            .client
            .post(&profile_url)
            .header("Cookie", &fixture.cookie)
            .header("Origin", &fixture.origin)
            .json(&json!({"mode":mode}))
    };
    assert_eq!(profile_mode("ask").send().await.unwrap().status(), 204);
    let branch = uuid::Uuid::new_v4().to_string();
    let lock = store.execution_lock("conversation", &branch).await.unwrap();
    let edit = || {
        fixture
            .request(
                reqwest::Method::POST,
                &format!("sessions/{parent}/messages/0/edit"),
            )
            .json(&json!({"request_id":branch,"text":"Edited question"}))
    };
    assert_eq!(edit().send().await.unwrap().status(), 200);
    assert!(snapshot(&fixture, &branch).await["run_id"].is_null());
    assert_eq!(profile_mode("review").send().await.unwrap().status(), 204);
    assert_eq!(
        snapshot(&fixture, &branch).await["command_audit_mode"],
        "ask"
    );
    assert_eq!(
        snapshot(&fixture, &parent).await["command_audit_mode"],
        "trusted"
    );
    drop(lock);
    // Retry the same edit if the worker retired while its execution lock was held.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            assert_eq!(edit().send().await.unwrap().status(), 200);
            if snapshot(&fixture, &branch).await["phase"] == "Completed" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        snapshot(&fixture, &branch).await["command_audit_mode"],
        "ask"
    );
    assert_eq!(
        snapshot(&fixture, &parent).await["command_audit_mode"],
        "trusted"
    );
}

#[tokio::test]
async fn command_approvals_require_authenticated_same_session_explicit_single_use_decisions() {
    let fixture = Fixture::new().await;
    let parent = fixture.create().await;
    let other = fixture.create().await;
    let store = SqliteSessionStore::open(fixture.data.path().join("owner-sessions")).unwrap();
    let session = store.load(&SessionId::new(&parent)).await.unwrap().unwrap();
    let approval = pending(&store, session, &parent).await;
    let before = snapshot(&fixture, &parent).await;
    assert_eq!(before["command_approvals"], json!([approval]));
    assert_eq!(
        snapshot(&fixture, &other).await["command_approvals"],
        json!([])
    );
    let path = format!("sessions/{parent}/command-approvals/{}", approval.id);
    for (cookie, origin, expected) in [
        (None, fixture.origin.as_str(), 401),
        (Some(fixture.cookie.as_str()), "https://other.invalid", 403),
    ] {
        let mut request = fixture
            .client
            .post(format!("{}api/chat/{path}", fixture.base))
            .header("Origin", origin)
            .json(&json!({"approve":true}));
        if let Some(cookie) = cookie {
            request = request.header("Cookie", cookie);
        }
        assert_eq!(request.send().await.unwrap().status(), expected);
    }
    assert_eq!(
        fixture
            .request(
                reqwest::Method::POST,
                &format!("sessions/{other}/command-approvals/{}", approval.id)
            )
            .json(&json!({"approve":true}))
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    assert_eq!(
        fixture
            .request(reqwest::Method::POST, &path)
            .json(&json!({"approve":"yes"}))
            .send()
            .await
            .unwrap()
            .status(),
        422
    );
    assert_eq!(
        store
            .pending_command_approvals(&parent)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        fixture
            .request(reqwest::Method::POST, &path)
            .json(&json!({"approve":true}))
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    assert_eq!(
        fixture
            .request(reqwest::Method::POST, &path)
            .json(&json!({"approve":true}))
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    let after = snapshot(&fixture, &parent).await;
    assert_eq!(after["command_approvals"], json!([]));
    assert!(after["revision"].as_i64().unwrap() > before["revision"].as_i64().unwrap());
}

#[tokio::test]
async fn command_approvals_for_children_are_visible_and_decidable_only_in_the_parent() {
    let fixture = Fixture::new().await;
    let parent = fixture.create().await;
    let other = fixture.create().await;
    let store = SqliteSessionStore::open(fixture.data.path().join("owner-sessions")).unwrap();
    let session = store.load(&SessionId::new(&parent)).await.unwrap().unwrap();
    let child_id = format!("companion:subagent:{}", uuid::Uuid::new_v4());
    let _child_worker = store.execution_lock("subagent", &child_id).await.unwrap();
    let child_session = session.child(SessionId::new(&child_id));
    store.save(&child_session).await.unwrap();
    let child = json!({"id":child_id,"parent":parent,"generation":session.generation(),"submission":"child-request","status":"running","task":"Inspect files","result":null,"error":null});
    store
        .with_connection(move |db| {
            db.execute(
                "INSERT INTO subagent_sessions VALUES(?1,?2,?3,?4)",
                rusqlite::params![
                    child["id"].as_str().unwrap(),
                    child["parent"].as_str().unwrap(),
                    child["generation"].as_str().unwrap(),
                    serde_json::to_vec(&child)?
                ],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let approval = pending(&store, child_session, &parent).await;
    assert_eq!(
        snapshot(&fixture, &parent).await["command_approvals"],
        json!([approval])
    );
    assert_eq!(
        snapshot(&fixture, &other).await["command_approvals"],
        json!([])
    );
    for (session_id, expected) in [(&other, 409), (&child_id, 404), (&parent, 204)] {
        assert_eq!(
            fixture
                .request(
                    reqwest::Method::POST,
                    &format!("sessions/{session_id}/command-approvals/{}", approval.id)
                )
                .json(&json!({"approve":false}))
                .send()
                .await
                .unwrap()
                .status(),
            expected
        );
    }
    assert_eq!(
        snapshot(&fixture, &parent).await["command_approvals"],
        json!([])
    );
}
