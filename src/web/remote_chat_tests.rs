use super::*;
use crate::{
    agent::{
        Agent, AssistantMessage, ContextBuilder, UserMessage,
        message::{Message, ToolCall, ToolResult},
        tools::ToolRegistry,
    },
    remote::protocol::ChatCommand,
};

struct Echo;
#[async_trait::async_trait]
impl crate::agent::llm::LlmProvider for Echo {
    fn identity(&self) -> String {
        "test-provider:server-model".into()
    }

    async fn generate(
        &self,
        context: &crate::agent::LlmContext<'_>,
        _: &[&dyn crate::agent::tools::Tool],
        _: Option<&mpsc::Sender<crate::agent::AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        let text = context
            .messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::User(u) => Some(u.content.clone()),
                _ => None,
            })
            .unwrap_or_default();
        Ok((
            vec![Message::Assistant(AssistantMessage {
                content: format!("echo {text}"),
            })],
            None,
        ))
    }
}

async fn fixture() -> (tempfile::TempDir, Chat, WebState, SessionId) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteSessionStore::open(dir.path().join("sessions")).unwrap());
    let runner = AgentRunner::new(
        Arc::new(Agent::new(
            Arc::new(Echo),
            ToolRegistry::default(),
            ContextBuilder::new("private system instructions"),
        )),
        store.clone(),
    );
    let chat = Chat::new(&runner, dir.path()).await.unwrap();
    let state = WebState {
        settings: Settings {
            trusted_proxies: vec![],
            listen: "127.0.0.1:0".parse().unwrap(),
            base_url: "http://127.0.0.1/kodrik/".parse().unwrap(),
        },
        auth: AuthStore::new(dir.path()),
        chat: Some(chat.clone()),
        streams: Arc::new(Semaphore::new(64)),
    };
    let id = SessionId::new(uuid::Uuid::new_v4().to_string());
    let mut session = Session::new(id.clone());
    session.initialize_system_prompt("secret personality and provider snapshot");
    session.push(Message::User(UserMessage {
        content: "from telegram".into(),
        files: vec![],
        images: vec![],
    }));
    chat.sessions.save(&session).await.unwrap();
    (dir, chat, state, id)
}

#[tokio::test]
async fn native_sessions_pages_validate_bounds_and_default_to_thirty() {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;
    let (dir, chat, state, _) = fixture().await;
    chat.sessions
        .with_connection(|db| {
            for index in 0..35 {
                let id = format!("page-{index:02}");
                let payload = json!({"id":id,"messages":[]});
                db.execute(
                    "INSERT INTO sessions VALUES(?1,?2,?3)",
                    rusqlite::params![id, serde_json::to_vec(&payload)?, index],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let now = chrono::Utc::now().timestamp();
    let pairing = state.auth.start_device_pairing("Laptop", now).unwrap();
    state
        .auth
        .approve_device_pairing(pairing.request_id, &pairing.code, &Identity::Owner, now)
        .unwrap();
    let crate::web::auth::devices::PairingPoll::Authorized { token, .. } = state
        .auth
        .poll_device_pairing(pairing.request_id, &pairing.polling_secret, now)
        .unwrap()
    else {
        panic!()
    };
    let app = crate::web::server::router_with_chat(state.settings.clone(), dir.path(), Some(chat));
    for (query, status, ids) in [
        ("", 200, None),
        ("?limit=2&offset=30", 200, Some(vec!["page-05", "page-04"])),
        ("?limit=100&offset=36", 200, Some(vec![])),
        ("?offset=9223372036854775807", 200, Some(vec![])),
        ("?limit=0", 400, None),
        ("?limit=101", 400, None),
        ("?limit=-1", 400, None),
        ("?offset=-1", 400, None),
        ("?offset=9223372036854775808", 400, None),
        ("?offset=18446744073709551616", 400, None),
        ("?limit=abc", 400, None),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/kodrik/api/remote/chat/sessions{query}"))
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{query}");
        if status == 200 {
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                    .unwrap();
            let rows = body.as_array().unwrap();
            if query.is_empty() {
                assert_eq!(rows.len(), 30);
            }
            if let Some(ids) = ids {
                assert_eq!(
                    rows.iter()
                        .map(|row| row["id"].as_str().unwrap())
                        .collect::<Vec<_>>(),
                    ids,
                    "{query}"
                );
            }
        }
    }
}

#[tokio::test]
async fn native_sessions_group_order_and_titles_do_not_deserialize_histories() {
    let (_dir, chat, _state, _) = fixture().await;
    chat.sessions.with_connection(|db| {
        db.execute("DELETE FROM sessions", [])?;
        for (id, updated, extra) in [
            ("pinned", 1, json!({})),
            ("root", 2, json!({})),
            ("selected", 3, json!({})),
            ("active", 100, json!({})),
            ("fallback", 4, json!({})),
            ("fallback-child", 50, json!({})),
            ("tie-a", 20, json!({})),
            ("tie-b", 20, json!({})),
            ("tie-b-child", 19, json!({})),
            ("telegram:-42:owner", 15, json!({})),
            ("telegram:42", 14, json!({"messages":[{"User":{"content":"🌍".repeat(100)}}]})),
            ("blank", 10, json!({"messages":[{"User":{"content":"\n\u{2003}"}}]})),
            ("companion:private", 999, json!({})),
            ("owner:guest", 999, json!({})),
            ("telegram:owner:-42", 999, json!({})),
            ("metadata-group", 999, json!({"telegram_group":{}})),
            ("webhook", 999, json!({"webhook_instruction":"secret"})),
        ] {
            let mut payload = json!({"id":id,"messages":[{"ToolResult":{"invalid":"history"}},{"User":{"content":" \n\t"}},{"User":{"content":format!("  {id}\n  title 🌍  "),"images":"invalid attachment"}}],"compaction":"invalid unused metadata"});
            payload.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            db.execute("INSERT INTO sessions VALUES(?1,?2,?3)", rusqlite::params![id,serde_json::to_vec(&payload)?,updated])?;
        }
        for (id, root) in [("selected", "root"), ("active", "root"), ("fallback-child", "fallback"), ("tie-b-child", "tie-b")] {
            db.execute("INSERT INTO web_chat_branches VALUES(?1,?2,?2,0)", [id,root])?;
        }
        db.execute("INSERT INTO web_chat_selection VALUES('root','selected'),('fallback','pinned')", [])?;
        db.execute("INSERT INTO web_chat_pins VALUES('pinned')", [])?;
        db.execute("INSERT INTO web_chat_titles VALUES('root','Saved root title')", [])?;
        Ok(())
    }).await.unwrap();
    let rows = chat.remote_sessions(30, 0).await.unwrap();
    assert_eq!(
        rows.as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "pinned",
            "selected",
            "fallback-child",
            "tie-a",
            "tie-b",
            "telegram:-42:owner",
            "telegram:42",
            "blank"
        ]
    );
    assert_eq!(rows[0]["pinned"], true);
    assert_eq!(rows[1]["root_id"], "root");
    assert_eq!(rows[1]["title"], "Saved root title");
    assert_eq!(rows[2]["title"], "fallback-child title 🌍");
    assert_eq!(rows[6]["title"], "🌍".repeat(80));
    assert_eq!(rows[7]["title"], "Новый диалог");
    assert_eq!(
        chat.remote_sessions(2, 0).await.unwrap(),
        json!(rows.as_array().unwrap()[..2])
    );
    assert_eq!(
        chat.remote_sessions(2, 2).await.unwrap(),
        json!(rows.as_array().unwrap()[2..4])
    );
}

#[tokio::test]
async fn queued_request_hides_only_completed_plans_in_web_and_remote_snapshots() {
    use crate::agent::{
        inbox::{BusyMode, SubmissionRequest},
        run_state::{Checkpoint, RunPhase},
    };
    for status in ["pending", "in_progress", "completed"] {
        let (_dir, chat, _state, id) = fixture().await;
        let mut session = chat.sessions.load(&id).await.unwrap().unwrap();
        session.push(Message::ToolCall(ToolCall {
            call_id: "plan".into(),
            name: "update_plan".into(),
            arguments: json!({"plan":[{"step":"Inspect","status":status}]}),
        }));
        session.push(Message::ToolResult(ToolResult {
            call_id: "plan".into(),
            content: json!("Plan updated").into(),
        }));
        let mut cp = Checkpoint::new(&chat.sessions, &id, &session);
        cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
        assert!(!chat.snapshot(id.as_str()).await.unwrap()["plan"].is_null());
        chat.sessions
            .submit(SubmissionRequest::new(
                &id,
                crate::gateway::IncomingMessage::new(
                    crate::gateway::InterfaceId("web".into()),
                    id.as_str(),
                    "Next request",
                ),
                BusyMode::Steer,
            ))
            .await
            .unwrap();
        let web = chat.snapshot(id.as_str()).await.unwrap();
        assert_eq!(web["queued"].as_array().unwrap().len(), 1);
        assert_eq!(web["plan"].is_null(), status == "completed", "{status}");
        let native = chat.remote_snapshot(&id).await.unwrap();
        assert_eq!(native.run.unwrap()["clear_completed_plan"], true);
        assert!(!chat.sessions.load(&id).await.unwrap().unwrap().messages().iter().any(|message| matches!(message, Message::User(user) if user.content == "Next request")));
    }
}

#[tokio::test]
async fn native_snapshot_reads_shared_history_without_private_snapshots() {
    let (_dir, chat, _state, id) = fixture().await;
    let snapshot = chat.remote_snapshot(&id).await.unwrap();
    let text = serde_json::to_string(&snapshot).unwrap();
    assert!(text.contains("from telegram"));
    assert!(!text.contains("secret personality"));
    assert_eq!(snapshot.messages[0]["role"], "user");
    assert!(
        chat.remote_snapshot(&SessionId::new("companion:private"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn native_large_pending_approval_preserves_controls_and_exact_arguments() {
    use crate::agent::{
        command_audit::CommandAuditMode,
        run_state::{Checkpoint, RunPhase},
    };
    use crate::remote::protocol::{EnvironmentKey, EnvironmentManifest, ToolDescriptor, ToolKind};
    let (_dir, chat, _state, id) = fixture().await;
    let mut session = chat.sessions.load(&id).await.unwrap().unwrap();
    let key = EnvironmentKey {
        connection_id: uuid::Uuid::new_v4(),
        generation: uuid::Uuid::new_v4(),
    };
    let descriptor = ToolDescriptor {
        id: "apply_patch".into(),
        kind: ToolKind::Patch,
        description: "Patch".into(),
        parameters: json!({"type":"object"}),
        deferred: false,
    };
    let name = crate::remote::tools::alias(&key, &descriptor);
    chat.sessions
        .remote_binding(
            uuid::Uuid::new_v4(),
            key.clone(),
            id.clone(),
            EnvironmentManifest {
                label: "Review laptop".into(),
                project_root: "/laptop-only".into(),
                tools: vec![descriptor],
                skills: vec![],
                project_instructions: None,
                warnings: vec![],
            },
        )
        .await
        .unwrap();
    let arguments = json!({"patch":format!("*** Begin Patch\n{}\n*** End Patch", "private-line\n".repeat(4000))});
    let call = ToolCall {
        call_id: "large-patch".into(),
        name,
        arguments: arguments.clone(),
    };
    session.push(Message::ToolCall(call.clone()));
    let mut cp = Checkpoint::new(&chat.sessions, &id, &session);
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
    chat.sessions
        .set_command_audit_mode(id.as_str(), CommandAuditMode::Ask)
        .await
        .unwrap();
    chat.sessions
        .record_command_audits(&cp.record, &[(call, "ask".into())])
        .await
        .unwrap();
    let pending = chat
        .sessions
        .pending_command_approvals(id.as_str())
        .await
        .unwrap();
    let view = chat.remote_snapshot(&id).await.unwrap();
    let run = view.run.unwrap();
    assert_eq!(run["busy"], true);
    assert_eq!(run["command_audit_mode"], "ask");
    assert_eq!(run["command_approvals"][0]["id"], pending[0].id);
    assert_eq!(
        run["command_approvals"][0]["target"]["environment"],
        json!(key)
    );
    assert_eq!(
        serde_json::from_str::<Value>(
            run["command_approvals"][0]["arguments_json"]
                .as_str()
                .unwrap()
        )
        .unwrap(),
        arguments
    );
    assert!(serde_json::to_vec(&run).unwrap().len() < 512 * 1024);
    chat.remote_command(
        &id,
        ChatCommand::DecideApproval {
            approval_id: pending[0].id.clone(),
            allow: true,
        },
    )
    .await
    .unwrap();
    assert!(
        chat.sessions
            .pending_command_approvals(id.as_str())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn native_oversized_approval_is_explicit_and_never_partial() {
    use crate::agent::{
        command_audit::CommandAuditMode,
        run_state::{Checkpoint, RunPhase},
    };
    let (_dir, chat, _state, id) = fixture().await;
    let mut session = chat.sessions.load(&id).await.unwrap().unwrap();
    let call = ToolCall {
        call_id: "oversize".into(),
        name: "apply_patch".into(),
        arguments: json!({"patch":"x".repeat(600*1024)}),
    };
    session.push(Message::ToolCall(call.clone()));
    let mut cp = Checkpoint::new(&chat.sessions, &id, &session);
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
    chat.sessions
        .set_command_audit_mode(id.as_str(), CommandAuditMode::Ask)
        .await
        .unwrap();
    chat.sessions
        .record_command_audits(&cp.record, &[(call, "ask".into())])
        .await
        .unwrap();
    let approval = chat
        .sessions
        .pending_command_approvals(id.as_str())
        .await
        .unwrap()
        .remove(0);
    let snapshot = chat.remote_snapshot(&id).await.unwrap();
    let run = snapshot.run.as_ref().unwrap();
    assert_eq!(run["busy"], true);
    assert_eq!(run["pending_approval_count"], 1);
    assert_eq!(run["command_approvals"], json!([]));
    assert_eq!(run["approval_unavailable"]["id"], approval.id);
    assert!(serde_json::to_vec(&snapshot).unwrap().len() < 2 * 1024 * 1024);
    chat.remote_command(
        &id,
        ChatCommand::DecideApproval {
            approval_id: approval.id,
            allow: false,
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn native_large_live_output_keeps_busy_phase_and_stop_controls() {
    use crate::agent::{
        run_state::{Checkpoint, RunPhase},
        run_view::PartialStatus,
    };
    let (_dir, chat, _state, id) = fixture().await;
    let session = chat.sessions.load(&id).await.unwrap().unwrap();
    let mut cp = Checkpoint::new(&chat.sessions, &id, &session);
    cp.save(&session, RunPhase::NeedModel, None).await.unwrap();
    chat.sessions
        .save_live(
            &cp.record.id,
            cp.record.model_attempts,
            "long live text".repeat(20000),
            PartialStatus::Active,
        )
        .await
        .unwrap();
    let snapshot = chat.remote_snapshot(&id).await.unwrap();
    let run = snapshot.run.as_ref().unwrap();
    assert_eq!(run["busy"], true);
    assert_eq!(run["phase"], "NeedModel");
    assert_eq!(run["model"], "test-provider:server-model");
    assert!(serde_json::to_vec(&snapshot).unwrap().len() < 2 * 1024 * 1024);
    chat.remote_command(&id, ChatCommand::Stop).await.unwrap();
    assert!(
        chat.sessions
            .load_run(&cp.record.id)
            .await
            .unwrap()
            .unwrap()
            .0
            .cancel_requested
    );
}

#[tokio::test]
async fn native_snapshot_reports_server_provider_identity() {
    let (_dir, chat, _state, id) = fixture().await;
    let snapshot = chat.remote_snapshot(&id).await.unwrap();
    assert_eq!(snapshot.run.unwrap()["model"], "test-provider:server-model");
}

#[tokio::test]
async fn native_trust_stale_approval_does_not_change_audit_mode() {
    use crate::agent::command_audit::CommandAuditMode;
    let (_dir, chat, _state, id) = fixture().await;
    chat.sessions
        .set_command_audit_mode(id.as_str(), CommandAuditMode::Ask)
        .await
        .unwrap();
    let command = serde_json::from_value(json!({
        "kind":"trust_approval", "approval_id":"stale-approval"
    }))
    .unwrap();
    assert!(chat.remote_command(&id, command).await.is_err());
    assert_eq!(
        chat.sessions.command_audit_mode(id.as_str()).await.unwrap(),
        CommandAuditMode::Ask
    );
}

#[tokio::test]
async fn native_submission_status_is_null_for_unknown_request() {
    let (_dir, chat, _state, id) = fixture().await;
    let status = chat
        .remote_command(
            &id,
            ChatCommand::SubmissionStatus {
                request_id: uuid::Uuid::new_v4(),
            },
        )
        .await
        .unwrap();
    assert_eq!(status, Value::Null);
}

#[tokio::test]
async fn native_replies_and_edits_do_not_queue_web_completion_pushes() {
    let (_dir, chat, _state, id) = fixture().await;
    chat.sessions
        .with_connection(|db| {
            db.execute(
                "INSERT INTO web_push_subscriptions VALUES('device','hash','{}','ru')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    for command in [
        ChatCommand::Send {
            request_id: uuid::Uuid::new_v4(),
            text: "from remote TUI".into(),
        },
        ChatCommand::Edit {
            request_id: uuid::Uuid::new_v4(),
            user_turn: 0,
            text: "edited from remote TUI".into(),
        },
    ] {
        let target = match &command {
            ChatCommand::Edit { request_id, .. } => SessionId::new(request_id.to_string()),
            _ => id.clone(),
        };
        chat.remote_command(&id, command).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while chat.sessions.session_busy(target.as_str()).await.unwrap()
                || chat.runs.lock().await.contains(target.as_str())
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let session = chat.sessions.load(&target).await.unwrap().unwrap();
        assert!(matches!(
            session.messages().last(),
            Some(Message::Assistant(_))
        ));
        let queued: i64 = chat
            .sessions
            .with_connection(|db| {
                Ok(db.query_row("SELECT count(*) FROM web_push_queue", [], |row| row.get(0))?)
            })
            .await
            .unwrap();
        assert_eq!(queued, 0, "TUI replies must not notify web subscribers");
    }
    // The same conversation must still notify for a subsequent browser message.
    admit_message(
        chat.clone(),
        Input {
            id: id.as_str().into(),
            request_id: uuid::Uuid::new_v4().to_string(),
            text: "from browser".into(),
        },
        vec![],
        false,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while chat.sessions.session_busy(id.as_str()).await.unwrap()
            || chat.runs.lock().await.contains(id.as_str())
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let queued: i64 = chat
        .sessions
        .with_connection(|db| {
            Ok(db.query_row("SELECT count(*) FROM web_push_queue", [], |row| row.get(0))?)
        })
        .await
        .unwrap();
    assert_eq!(queued, 1, "browser replies must still notify");
}

#[tokio::test]
async fn native_submission_status_finds_send_retries_without_exposing_content() {
    let (_dir, chat, _state, uuid_id) = fixture().await;
    let legacy_id = SessionId::new("telegram:1234");
    chat.sessions
        .save(&Session::new(legacy_id.clone()))
        .await
        .unwrap();
    for id in [uuid_id, legacy_id] {
        let request_id = uuid::Uuid::new_v4();
        let command = ChatCommand::Send {
            request_id,
            text: "original private input with secret-token".into(),
        };
        let first = chat.remote_command(&id, command.clone()).await.unwrap();
        let retry = chat.remote_command(&id, command).await.unwrap();
        assert_eq!(first["submission_id"], retry["submission_id"]);
        tokio::time::timeout(Duration::from_secs(5), async {
            while chat.sessions.session_busy(id.as_str()).await.unwrap()
                || chat.runs.lock().await.contains(id.as_str())
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let status = chat
            .remote_command(&id, ChatCommand::SubmissionStatus { request_id })
            .await
            .unwrap();
        assert_eq!(
            status,
            json!({"status":"Done","session_id":id.as_str()}),
            "status must expose only metadata for {}",
            id.as_str()
        );
        let session = chat.sessions.load(&id).await.unwrap().unwrap();
        assert_eq!(
            session.messages().iter().filter(|message| matches!(message,
                Message::User(user) if user.content == "original private input with secret-token"
            )).count(),
            1
        );
        assert!(session.messages().iter().any(|message| matches!(message,
            Message::Assistant(reply) if reply.content.contains("echo original private input")
        )));
    }
}

#[tokio::test]
async fn native_snapshot_projects_compaction_from_durable_run_view() {
    use crate::agent::run_state::{Checkpoint, RunPhase};
    let (_dir, chat, _state, id) = fixture().await;
    let session = chat.sessions.load(&id).await.unwrap().unwrap();
    let mut checkpoint = Checkpoint::new(&chat.sessions, &id, &session);
    checkpoint
        .save(&session, RunPhase::NeedModel, None)
        .await
        .unwrap();
    let run_id = checkpoint.record.id.clone();
    let job = json!({
        "id":"compact-test", "run_id":run_id, "conversation_id":id.as_str(),
        "status":"preparing", "snapshot":{
            "session":session, "through":1, "history_tokens":123,
            "threshold":100, "model_identity":"test-provider:server-model"
        },
        "candidate":null, "error":null, "model_calls":1,
        "known_input_tokens":123, "unknown_calls":0
    });
    let session_id = id.as_str().to_owned();
    chat.sessions
        .with_connection(move |db| {
            db.execute(
                "INSERT INTO compaction_jobs VALUES(?1,'compact-test',?2,'preparing',?3)",
                rusqlite::params![session_id, run_id, serde_json::to_vec(&job)?],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let snapshot = chat.remote_snapshot(&id).await.unwrap();
    let compaction = &snapshot.run.as_ref().unwrap()["compaction"];
    assert_eq!(compaction["status"], "preparing");
    assert_eq!(compaction["model"], "test-provider:server-model");
    assert_eq!(compaction["input_tokens"], 123);
    assert!(compaction.get("snapshot").is_none());
}

#[test]
fn native_large_group_keeps_latest_work_visible_within_budget() {
    let mut parts = (0..12)
        .map(|i| {
            json!({"type":"dynamic-tool","toolCallId":format!("call-{i}"),
            "toolName":"exec_command","state":"output-available","output":"x".repeat(10000)})
        })
        .collect::<Vec<_>>();
    parts.push(json!({"type":"text","text":"Latest progress"}));
    let mut row = json!({"id":"session:1","role":"assistant","parts":parts});
    service::project_group(&mut row, "session");
    assert_eq!(
        row["parts"].as_array().unwrap().last().unwrap()["text"],
        "Latest progress"
    );
    assert!(
        row["parts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|part| part["toolCallId"] == "call-11")
    );
    assert_eq!(row["parts"][0]["truncated"], true);
    assert!(serde_json::to_vec(&row).unwrap().len() <= 64 * 1024);
    row["parts"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"text","text":"Final answer"}));
    service::project_group(&mut row, "session");
    assert_eq!(
        row["parts"].as_array().unwrap().last().unwrap()["text"],
        "Final answer"
    );
}

#[test]
fn native_group_truncation_preserves_absolute_user_turn() {
    let mut row = json!({"id":"session:30", "role":"user", "user_turn":15,
        "parts":(0..8).map(|_| json!({"type":"text","text":"x".repeat(10000)})).collect::<Vec<_>>()});
    service::project_group(&mut row, "session");
    assert_eq!(row["parts"][0]["truncated"], true);
    assert_eq!(row["user_turn"], 15);
}

#[tokio::test]
async fn native_send_retries_do_not_duplicate_and_edit_keeps_branches() {
    let (_dir, chat, _state, id) = fixture().await;
    let request_id = uuid::Uuid::new_v4();
    let command = ChatCommand::Send {
        request_id,
        text: "hello".into(),
    };
    let first = chat.remote_command(&id, command.clone()).await.unwrap();
    let second = chat.remote_command(&id, command).await.unwrap();
    assert_eq!(first["submission_id"], second["submission_id"]);
    assert!(
        chat.remote_command(
            &id,
            ChatCommand::Send {
                request_id,
                text: "different".into()
            }
        )
        .await
        .is_err()
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while chat.sessions.session_busy(id.as_str()).await.unwrap()
            || chat.runs.lock().await.contains(id.as_str())
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let request_id = uuid::Uuid::new_v4();
    let edit = ChatCommand::Edit {
        request_id,
        user_turn: 1,
        text: "edited".into(),
    };
    let first = chat.remote_command(&id, edit.clone()).await.unwrap();
    let second = chat.remote_command(&id, edit).await.unwrap();
    assert_eq!(first["id"], second["id"]);
    let selected: Option<String> = chat
        .sessions
        .with_connection(|db| {
            use rusqlite::OptionalExtension;
            Ok(db
                .query_row(
                    "SELECT session_id FROM web_chat_selection LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .optional()?)
        })
        .await
        .unwrap();
    assert!(selected.is_none(), "native edit changed browser selection");
    let branch = SessionId::new(first["id"].as_str().unwrap());
    tokio::time::timeout(Duration::from_secs(5), async {
        while chat.sessions.session_busy(branch.as_str()).await.unwrap()
            || chat.runs.lock().await.contains(branch.as_str())
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let edit_status = chat
        .remote_command(&id, ChatCommand::SubmissionStatus { request_id })
        .await
        .unwrap();
    assert_eq!(
        edit_status,
        json!({"status":"Done","session_id":branch.as_str()})
    );
    assert_eq!(
        chat.remote_command(&branch, ChatCommand::SubmissionStatus { request_id })
            .await
            .unwrap(),
        edit_status,
        "edit status must also resolve on its own branch"
    );
    let snapshot = chat.remote_snapshot(&branch).await.unwrap();
    assert!(
        snapshot
            .messages
            .iter()
            .any(|m| m["versions"].as_array().is_some_and(|ids| ids.len() == 2))
    );
    assert!(
        chat.remote_command(
            &id,
            ChatCommand::SelectVersion {
                session_id: branch.clone(),
                user_turn: 1
            }
        )
        .await
        .is_ok()
    );
    let unrelated = SessionId::new(uuid::Uuid::new_v4().to_string());
    chat.sessions
        .save(&Session::new(unrelated.clone()))
        .await
        .unwrap();
    assert_eq!(
        chat.remote_command(&unrelated, ChatCommand::SubmissionStatus { request_id })
            .await
            .unwrap(),
        Value::Null,
        "an unrelated conversation must not reveal an edit submission"
    );
    assert!(
        chat.remote_command(
            &id,
            ChatCommand::SelectVersion {
                session_id: unrelated,
                user_turn: 1
            }
        )
        .await
        .is_err()
    );
    let original = chat.sessions.load(&id).await.unwrap().unwrap();
    assert_eq!(
        original
            .messages()
            .iter()
            .filter(|m| matches!(m,Message::User(u) if u.content=="hello"))
            .count(),
        1
    );
}

#[tokio::test]
async fn native_full_result_is_chunked_without_system_snapshots() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let (dir, chat, state, id) = fixture().await;
    let mut session = chat.sessions.load(&id).await.unwrap().unwrap();
    session.push(Message::Assistant(AssistantMessage {
        content: "я".repeat(80000),
    }));
    chat.sessions.save(&session).await.unwrap();
    let now = chrono::Utc::now().timestamp();
    let pairing = state.auth.start_device_pairing("Laptop", now).unwrap();
    state
        .auth
        .approve_device_pairing(pairing.request_id, &pairing.code, &Identity::Owner, now)
        .unwrap();
    let crate::web::auth::devices::PairingPoll::Authorized { token, .. } = state
        .auth
        .poll_device_pairing(pairing.request_id, &pairing.polling_secret, now)
        .unwrap()
    else {
        panic!()
    };
    let app = crate::web::server::router_with_chat(state.settings.clone(), dir.path(), Some(chat));
    let mut offset = 0;
    let mut text = String::new();
    loop {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/kodrik/api/remote/chat/sessions/{}/history/1?offset={offset}",
                        id.as_str()
                    ))
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let bytes = axum::body::to_bytes(response.into_body(), 512 * 1024)
            .await
            .unwrap();
        let chunk: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(chunk["data"].as_str().unwrap().len() <= 64 * 1024);
        text.push_str(chunk["data"].as_str().unwrap());
        let Some(next) = chunk["next_offset"].as_u64() else {
            break;
        };
        assert!(next > offset);
        offset = next;
    }
    let row: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        row["parts"][0]["text"].as_str().unwrap().chars().count(),
        80000
    );
    assert!(!text.contains("secret personality"));
}

#[tokio::test]
async fn native_attachment_references_include_tool_images_without_inline_blobs() {
    use axum::{body::Body, http::Request};
    use base64::{Engine, engine::general_purpose::STANDARD};
    use tower::ServiceExt;
    let (dir, chat, state, id) = fixture().await;
    let image = crate::agent::Image {
        data_url: format!(
            "data:image/png;base64,{}",
            STANDARD.encode(b"image fixture")
        ),
        width: 1,
        height: 1,
    };
    let file =
        crate::agent::FileAttachment::save(dir.path(), "notes.txt", None, b"private file").unwrap();
    let mut session = chat.sessions.load(&id).await.unwrap().unwrap();
    session.push(Message::User(UserMessage {
        content: "attachments".into(),
        images: vec![image.clone()],
        files: vec![file],
    }));
    session.push(Message::ToolCall(ToolCall {
        call_id: "picture".into(),
        name: "view_image".into(),
        arguments: json!({}),
    }));
    session.push(Message::ToolResult(ToolResult {
        call_id: "picture".into(),
        content: crate::agent::message::ToolOutput {
            output: json!({"ok":true}),
            images: vec![image],
        },
    }));
    chat.sessions.save(&session).await.unwrap();
    let web = chat.snapshot(id.as_str()).await.unwrap();
    let web_images: Vec<_> = web["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|row| row["parts"].as_array().unwrap())
        .filter(|part| {
            part["url"]
                .as_str()
                .is_some_and(|url| url.contains("/tool_images/"))
        })
        .collect();
    assert_eq!(web_images.len(), 1, "result image lost from Web history");
    assert_eq!(web_images[0]["toolCallId"], "picture");
    let snapshot = chat.remote_snapshot(&id).await.unwrap();
    let text = serde_json::to_string(&snapshot).unwrap();
    assert!(!text.contains("base64"));
    assert!(!text.contains("private file"));
    let urls: Vec<_> = snapshot
        .messages
        .iter()
        .flat_map(|m| m["parts"].as_array().unwrap())
        .filter_map(|p| p["url"].as_str())
        .collect();
    assert_eq!(urls.len(), 3, "tool output image lost from native history");
    let now = chrono::Utc::now().timestamp();
    let pairing = state.auth.start_device_pairing("Laptop", now).unwrap();
    state
        .auth
        .approve_device_pairing(pairing.request_id, &pairing.code, &Identity::Owner, now)
        .unwrap();
    let crate::web::auth::devices::PairingPoll::Authorized { token, .. } = state
        .auth
        .poll_device_pairing(pairing.request_id, &pairing.polling_secret, now)
        .unwrap()
    else {
        panic!()
    };
    let app = crate::web::server::router_with_chat(state.settings.clone(), dir.path(), Some(chat));
    for url in urls {
        let url = state.settings.base_url.join(url).unwrap();
        assert!(url.path().starts_with("/kodrik/api/remote/chat/"));
        let request = |authorized: bool| {
            let mut builder = Request::builder().uri(url.path());
            if authorized {
                builder = builder.header("Authorization", format!("Bearer {token}"));
            }
            builder.body(Body::empty()).unwrap()
        };
        assert_eq!(
            app.clone().oneshot(request(false)).await.unwrap().status(),
            401
        );
        let response = app.clone().oneshot(request(true)).await.unwrap();
        assert_eq!(response.status(), 200);
        let bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert!(bytes == b"image fixture"[..] || bytes == b"private file"[..]);
    }
}

#[tokio::test]
async fn native_history_bounds_preserve_tool_groups_and_durable_content() {
    let (_dir, chat, _state, id) = fixture().await;
    let mut session = chat.sessions.load(&id).await.unwrap().unwrap();
    for i in 0..220 {
        session.push(Message::Assistant(AssistantMessage {
            content: format!("message {i}"),
        }));
        session.push(Message::User(UserMessage {
            content: format!("user {i}"),
            files: vec![],
            images: vec![],
        }));
    }
    session.push(Message::ToolCall(ToolCall {
        call_id: "large".into(),
        name: "test".into(),
        arguments: json!({}),
    }));
    session.push(Message::ToolResult(ToolResult {
        call_id: "large".into(),
        content: json!({"payload":"x".repeat(2*1024*1024)}).into(),
    }));
    chat.sessions.save(&session).await.unwrap();
    let snapshot = chat.remote_snapshot(&id).await.unwrap();
    assert!(serde_json::to_vec(&snapshot).unwrap().len() <= 1024 * 1024);
    assert!(snapshot.messages.len() <= 200);
    assert!(snapshot.previous_cursor.is_some());
    let cursor = snapshot.previous_cursor.unwrap();
    let previous = chat.remote_history(&id, &cursor).await.unwrap();
    assert_ne!(previous.previous_cursor, Some(cursor));
    for row in snapshot
        .messages
        .iter()
        .chain(&previous.messages)
        .filter(|row| row["role"] == "user")
    {
        let text = row["parts"][0]["text"].as_str().unwrap();
        let expected = if text == "from telegram" {
            0
        } else {
            text.strip_prefix("user ")
                .unwrap()
                .parse::<usize>()
                .unwrap()
                + 1
        };
        assert_eq!(row["user_turn"], expected, "wrong absolute turn for {text}");
    }
    assert_eq!(
        snapshot
            .messages
            .iter()
            .rev()
            .find(|row| row["role"] == "user")
            .unwrap()["user_turn"],
        220
    );
    let mut oldest = previous;
    while let Some(cursor) = oldest.previous_cursor.clone() {
        oldest = chat.remote_history(&id, &cursor).await.unwrap();
    }
    assert_eq!(oldest.messages[0]["user_turn"], 0);
    let text = serde_json::to_string(&snapshot.messages).unwrap();
    assert!(text.contains("truncated"));
    assert!(text.contains("/api/remote/chat/sessions/"));
    assert!(text.contains("/history/"));
    let tool = snapshot
        .messages
        .iter()
        .flat_map(|m| m["parts"].as_array().unwrap())
        .find(|p| p["toolCallId"] == "large")
        .unwrap();
    assert_eq!(tool["toolName"], "test");
    assert_eq!(tool["state"], "output-available");
    assert_eq!(tool["truncated"], true);
    assert_eq!(
        chat.sessions
            .load(&id)
            .await
            .unwrap()
            .unwrap()
            .messages()
            .len(),
        443
    );
}

#[tokio::test]
async fn historical_remote_origins_cover_older_pages_after_new_run() {
    use crate::{
        agent::run_state::{Checkpoint, RunPhase},
        remote::protocol::*,
    };
    let (_dir, chat, _state, id) = fixture().await;
    let key = EnvironmentKey {
        connection_id: uuid::Uuid::new_v4(),
        generation: uuid::Uuid::new_v4(),
    };
    let descriptor = ToolDescriptor {
        id: "view_image".into(),
        kind: ToolKind::Image,
        description: "image".into(),
        parameters: json!({"type":"object"}),
        deferred: false,
    };
    let alias = crate::remote::tools::alias(&key, &descriptor);
    let manifest = EnvironmentManifest {
        label: "<Laptop & private>".into(),
        project_root: "/only-on-laptop/project".into(),
        tools: vec![descriptor],
        skills: vec![],
        project_instructions: None,
        warnings: vec![],
    };
    let device = uuid::Uuid::new_v4();
    chat.sessions
        .remote_binding(device, key.clone(), id.clone(), manifest)
        .await
        .unwrap();
    let mut session = chat.sessions.load(&id).await.unwrap().unwrap();
    let mut cp = Checkpoint::new(&chat.sessions, &id, &session);
    for index in 0..270 {
        session.push(Message::User(UserMessage {
            content: format!("turn {index}"),
            files: vec![],
            images: vec![],
        }));
        session.push(Message::ToolCall(ToolCall {
            call_id: format!("image-{index}"),
            name: alias.clone(),
            arguments: json!({}),
        }));
        session.push(Message::ToolResult(ToolResult {
            call_id: format!("image-{index}"),
            content: crate::agent::tools::ToolOutput {
                output: json!({"path":"/only-on-laptop/image.png"}),
                images: vec![crate::agent::Image {
                    data_url: "data:image/png;base64,aW1hZ2U=".into(),
                    width: 1,
                    height: 1,
                }],
            },
        }));
    }
    cp.save(&session, RunPhase::Completed, None).await.unwrap();
    let run = cp.record.id.clone();
    let environment = key.clone();
    let session_id = id.clone();
    chat.sessions
        .with_connection(move |db| {
            for index in 0..270 {
                let call = format!("image-{index}");
                let receipt = OperationReceipt {
                    invocation: Invocation {
                        environment: environment.clone(),
                        session_id: session_id.clone(),
                        run_id: run.clone(),
                        call_id: call.clone(),
                        tool_id: "view_image".into(),
                        arguments: json!({}),
                        deadline: chrono::Utc::now(),
                    },
                    arguments_hash: arguments_hash(&json!({}))?,
                    state: OperationState::Completed,
                    result: Some(json!({"ok":true}).into()),
                };
                db.execute(
                    "INSERT INTO remote_operations VALUES(?1,?2,?3,?4,?5,?6)",
                    rusqlite::params![
                        run,
                        call,
                        device.to_string(),
                        environment.connection_id.to_string(),
                        environment.generation.to_string(),
                        serde_json::to_vec(&receipt)?
                    ],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let mut newer = Checkpoint::new(&chat.sessions, &id, &session);
    newer
        .save(&session, RunPhase::Completed, None)
        .await
        .unwrap();
    let expected = json!({"label":"<Laptop & private>","project_root":"/only-on-laptop/project","environment":key,"tool_id":"view_image"});
    let web = chat.snapshot(id.as_str()).await.unwrap();
    let first = &web["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["parts"][0]["toolCallId"] == "image-0")
        .unwrap()["parts"];
    assert_eq!(first[0]["origin"], expected);
    assert_eq!(first[1]["origin"], expected);
    let page = chat.remote_snapshot(&id).await.unwrap();
    let older = chat
        .remote_history(&id, page.previous_cursor.as_deref().unwrap())
        .await
        .unwrap();
    let tool_parts: Vec<_> = older
        .messages
        .iter()
        .flat_map(|row| row["parts"].as_array().unwrap())
        .filter(|part| part["toolCallId"].as_str().is_some())
        .collect();
    assert!(!tool_parts.is_empty());
    for part in tool_parts {
        assert_eq!(part["origin"], expected);
    }
    assert!(
        !serde_json::to_string(&older)
            .unwrap()
            .contains("data:image")
    );
}

#[tokio::test]
async fn saved_local_text_survives_disconnect_and_download_requires_session_auth() {
    use crate::{
        agent::run_state::{Checkpoint, RunPhase},
        remote::protocol::*,
    };
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;
    let (dir, chat, state, id) = fixture().await;
    let key = EnvironmentKey {
        connection_id: uuid::Uuid::new_v4(),
        generation: uuid::Uuid::new_v4(),
    };
    let descriptor = ToolDescriptor {
        id: "read_file".into(),
        kind: ToolKind::ReadFile,
        description: "Read".into(),
        parameters: json!({"type":"object"}),
        deferred: false,
    };
    let name = crate::remote::tools::alias(&key, &descriptor);
    let manifest = EnvironmentManifest {
        label: "Laptop".into(),
        project_root: "/only-on-laptop".into(),
        tools: vec![descriptor],
        skills: vec![],
        project_instructions: None,
        warnings: vec![],
    };
    let device = uuid::Uuid::new_v4();
    chat.sessions
        .remote_binding(device, key.clone(), id.clone(), manifest)
        .await
        .unwrap();
    let call = ToolCall {
        call_id: "selected".into(),
        name,
        arguments: json!({"path":"notes.txt","save_attachment":true}),
    };
    let mut session = chat.sessions.load(&id).await.unwrap().unwrap();
    session.push(Message::ToolCall(call.clone()));
    let mut cp = Checkpoint::new(&chat.sessions, &id, &session);
    cp.record.pending_calls = vec![call.clone()];
    cp.save(
        &session,
        RunPhase::ExecutingTool {
            call_id: call.call_id.clone(),
            replay: crate::agent::tools::ReplayPolicy::Never,
        },
        Some(crate::agent::run_state::ToolExecution {
            call: call.clone(),
            replay: crate::agent::tools::ReplayPolicy::Never,
            result: None,
        }),
    )
    .await
    .unwrap();
    let invocation = Invocation {
        environment: key.clone(),
        session_id: id.clone(),
        run_id: cp.record.id.clone(),
        call_id: call.call_id.clone(),
        tool_id: "read_file".into(),
        arguments: call.arguments.clone(),
        deadline: chrono::Utc::now() + chrono::Duration::minutes(1),
    };
    chat.sessions
        .remote_intent(device, invocation.clone())
        .await
        .unwrap();
    chat.sessions
        .remote_dispatch(device, invocation.clone())
        .await
        .unwrap();
    let output: crate::agent::tools::ToolOutput =
        json!({"path":"/fake-server-secret","text":"saved laptop text"}).into();
    chat.sessions
        .remote_receipt(
            device,
            OperationReceipt {
                arguments_hash: arguments_hash(&invocation.arguments).unwrap(),
                invocation,
                state: OperationState::Completed,
                result: Some(output.clone()),
            },
        )
        .await
        .unwrap();
    let result_index = session.messages().len();
    session.push(Message::ToolResult(ToolResult {
        call_id: call.call_id,
        content: output,
    }));
    cp.save(&session, RunPhase::Completed, None).await.unwrap();
    chat.sessions.remote_disconnect(key).await.unwrap();
    let snapshot = chat.remote_snapshot(&id).await.unwrap();
    let parts: Vec<_> = snapshot
        .messages
        .iter()
        .flat_map(|row| row["parts"].as_array().unwrap())
        .collect();
    let file = parts
        .iter()
        .find(|part| part["type"] == "file")
        .expect("saved file missing");
    assert_eq!(file["filename"], "notes.txt");
    assert_eq!(file["origin"]["label"], "Laptop");
    assert_eq!(
        file["url"],
        format!(
            "./api/remote/chat/sessions/{}/attachments/{result_index}/tool_files/0",
            id.as_str()
        )
    );
    assert!(parts.iter().any(|part| part["attachment_saved"] == true));
    let path = format!(
        "/kodrik/api/remote/chat/sessions/{}/attachments/{result_index}/tool_files/0",
        id.as_str()
    );
    let now = chrono::Utc::now().timestamp();
    let pairing = state.auth.start_device_pairing("download", now).unwrap();
    state
        .auth
        .approve_device_pairing(pairing.request_id, &pairing.code, &Identity::Owner, now)
        .unwrap();
    let crate::web::auth::devices::PairingPoll::Authorized { token, .. } = state
        .auth
        .poll_device_pairing(pairing.request_id, &pairing.polling_secret, now)
        .unwrap()
    else {
        panic!()
    };
    let app = crate::web::server::router_with_chat(
        state.settings.clone(),
        dir.path(),
        Some(chat.clone()),
    );
    let response = app
        .clone()
        .oneshot(Request::builder().uri(&path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&path)
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(
        &to_bytes(response.into_body(), 1024).await.unwrap()[..],
        b"saved laptop text"
    );
    let wrong = path.replace("tool_files/0", "tool_files/1");
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(wrong)
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    // A private child session containing copied result JSON is not a file-read authority.
    let child = SessionId::new("companion:subagent:private");
    chat.sessions
        .save(&Session::new(child.clone()))
        .await
        .unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .uri(path.replace(id.as_str(), child.as_str()))
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn incremental_events_keep_complete_history_and_transmit_only_changes() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let (dir, chat, state, id) = fixture().await;
    let mut session = chat.sessions.load(&id).await.unwrap().unwrap();
    for i in 0..30 {
        session.push(Message::Assistant(AssistantMessage {
            content: format!("step {i}: {}", "large progress 🌍".repeat(2000)),
        }));
    }
    chat.sessions.save(&session).await.unwrap();
    let now = chrono::Utc::now().timestamp();
    let pairing = state.auth.start_device_pairing("Laptop", now).unwrap();
    state
        .auth
        .approve_device_pairing(pairing.request_id, &pairing.code, &Identity::Owner, now)
        .unwrap();
    let crate::web::auth::devices::PairingPoll::Authorized { token, .. } = state
        .auth
        .poll_device_pairing(pairing.request_id, &pairing.polling_secret, now)
        .unwrap()
    else {
        panic!()
    };
    let app = crate::web::server::router_with_chat(
        state.settings.clone(),
        dir.path(),
        Some(chat.clone()),
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/kodrik/api/remote/chat/sessions/{}/events?incremental=true",
                    id.as_str()
                ))
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut stream = response.into_body().into_data_stream();
    let mut receiver = crate::remote::chat_sync::Assembly::default();
    async fn receive(
        stream: &mut (impl futures::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Unpin),
        receiver: &mut crate::remote::chat_sync::Assembly,
    ) -> (usize, Value) {
        let mut bytes = 0;
        loop {
            let chunk = tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            bytes += chunk.len();
            let text = std::str::from_utf8(&chunk).unwrap();
            let mut frame: crate::remote::chat_sync::Frame =
                serde_json::from_str(text.trim().strip_prefix("data: ").unwrap()).unwrap();
            let metadata = frame.metadata.take();
            if receiver.apply(frame).unwrap() {
                return (bytes, metadata.unwrap());
            }
        }
    }
    let (initial_bytes, _) = receive(&mut stream, &mut receiver).await;
    assert!(initial_bytes > 1024 * 1024);
    let rows: Vec<Value> = serde_json::from_str(receiver.text()).unwrap();
    assert_eq!(rows[1]["parts"].as_array().unwrap().len(), 30);
    assert!(!receiver.text().contains("truncated"));
    session.push(Message::Assistant(AssistantMessage {
        content: "Newest answer".into(),
    }));
    chat.sessions.save(&session).await.unwrap();
    let (changed_bytes, metadata) = receive(&mut stream, &mut receiver).await;
    assert!(
        changed_bytes < 4096,
        "resent history: {changed_bytes} bytes"
    );
    assert_eq!(metadata["session_id"], id.as_str());
    let rows: Vec<Value> = serde_json::from_str(receiver.text()).unwrap();
    assert_eq!(rows[1]["parts"].as_array().unwrap().len(), 31);
    assert_eq!(rows[1]["parts"][30]["text"], "Newest answer");
    // A result updates an existing call rather than appending a duplicate call.
    session.push(Message::ToolCall(ToolCall {
        call_id: "call".into(),
        name: "test".into(),
        arguments: json!({}),
    }));
    chat.sessions.save(&session).await.unwrap();
    receive(&mut stream, &mut receiver).await;
    session.push(Message::ToolResult(ToolResult {
        call_id: "call".into(),
        content: json!({"value":42}).into(),
    }));
    chat.sessions.save(&session).await.unwrap();
    let (bytes, _) = receive(&mut stream, &mut receiver).await;
    assert!(bytes < 4096);
    let rows: Vec<Value> = serde_json::from_str(receiver.text()).unwrap();
    let parts = rows[1]["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 32);
    assert_eq!(parts[31]["output"]["value"], 42);
}

#[tokio::test]
async fn native_events_send_initial_snapshot_and_close_after_revoke() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let (dir, chat, state, id) = fixture().await;
    chat.runner.enable_environments(state.auth.clone()).unwrap();
    let environments = chat.runner.environments().unwrap().clone();
    let now = chrono::Utc::now().timestamp();
    let pairing = state.auth.start_device_pairing("Laptop", now).unwrap();
    state
        .auth
        .approve_device_pairing(pairing.request_id, &pairing.code, &Identity::Owner, now)
        .unwrap();
    let crate::web::auth::devices::PairingPoll::Authorized { device_id, token } = state
        .auth
        .poll_device_pairing(pairing.request_id, &pairing.polling_secret, now)
        .unwrap()
    else {
        panic!()
    };
    let app = crate::web::server::router_with_chat(state.settings.clone(), dir.path(), Some(chat));
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/kodrik/api/remote/chat/sessions/{}/events",
                    id.as_str()
                ))
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut stream = response.into_body().into_data_stream();
    let initial = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let initial = std::str::from_utf8(&initial).unwrap();
    assert!(initial.contains("from telegram"));
    assert!(initial.contains("revision"));
    let key = environments
        .attach(
            crate::web::auth::devices::DeviceIdentity { device_id },
            id.clone(),
            crate::remote::protocol::EnvironmentManifest {
                label: "Connected laptop".into(),
                project_root: "/project".into(),
                tools: vec![],
                skills: vec![],
                project_instructions: None,
                warnings: vec![],
            },
        )
        .await
        .unwrap();
    let updated = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        std::str::from_utf8(&updated)
            .unwrap()
            .contains("Connected laptop")
    );
    environments.revoke(&key, "closed").await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        std::str::from_utf8(&closed)
            .unwrap()
            .contains("\"state\":\"disconnected\"")
    );
    state.auth.revoke_device(device_id).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn native_http_requires_bearer_rechecks_revocation_and_keeps_prefix() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let (dir, chat, state, id) = fixture().await;
    let now = chrono::Utc::now().timestamp();
    let pairing = state.auth.start_device_pairing("Laptop", now).unwrap();
    state
        .auth
        .approve_device_pairing(pairing.request_id, &pairing.code, &Identity::Owner, now)
        .unwrap();
    let crate::web::auth::devices::PairingPoll::Authorized { device_id, token } = state
        .auth
        .poll_device_pairing(pairing.request_id, &pairing.polling_secret, now)
        .unwrap()
    else {
        panic!()
    };
    let app = crate::web::server::router_with_chat(state.settings.clone(), dir.path(), Some(chat));
    for (path, expected) in [
        (format!("api/remote/chat/sessions/{}", id.as_str()), 200),
        ("api/remote/chat/sessions".into(), 200),
        ("api/profile".into(), 401),
        ("api/chat/sessions".into(), 401),
        ("api/remote/chat/sessions/companion:private".into(), 404),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/kodrik/{path}"))
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{path}");
    }
    let path = format!("/kodrik/api/remote/chat/sessions/{}", id.as_str());
    let response = app
        .clone()
        .oneshot(Request::builder().uri(&path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    state.auth.revoke_device(device_id).unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .uri(path)
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn native_snapshot_retains_total_run_time_after_completion() {
    use crate::agent::run_state::{Checkpoint, RunPhase};
    let (_dir, chat, _state, id) = fixture().await;
    let session = chat.sessions.load(&id).await.unwrap().unwrap();
    let mut checkpoint = Checkpoint::new(&chat.sessions, &id, &session);
    let start = chrono::Utc::now() - chrono::Duration::seconds(125);
    checkpoint.record.started_at = start;
    checkpoint
        .save(&session, RunPhase::NeedModel, None)
        .await
        .unwrap();
    let active = chat.remote_snapshot(&id).await.unwrap();
    assert_eq!(active.run.as_ref().unwrap()["started_at"], json!(start));
    assert!(active.run.as_ref().unwrap()["finished_at"].is_null());
    checkpoint
        .save(&session, RunPhase::Completed, None)
        .await
        .unwrap();
    let done = chat.remote_snapshot(&id).await.unwrap();
    let end: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(done.run.as_ref().unwrap()["finished_at"].clone()).unwrap();
    assert!((end - start).num_seconds() >= 125);
    assert_eq!(
        done.run.as_ref().unwrap()["finished_at"],
        chat.remote_snapshot(&id).await.unwrap().run.unwrap()["finished_at"]
    );
}

#[tokio::test]
async fn native_image_send_preserves_vision_payload_and_deduplicates() {
    let (_dir, chat, _state, id) = fixture().await;
    let command = ChatCommand::SendImages {
        request_id: uuid::Uuid::new_v4(),
        text: String::new(),
        images: vec![
            crate::agent::Image::from_bytes(&crate::agent::image::tests::sample(
                image::ImageFormat::Png,
            ))
            .unwrap(),
        ],
    };
    let first = chat.remote_command(&id, command.clone()).await.unwrap();
    let second = chat.remote_command(&id, command).await.unwrap();
    assert_eq!(first["submission_id"], second["submission_id"]);
    tokio::time::timeout(Duration::from_secs(5), async {
        while chat.sessions.session_busy(id.as_str()).await.unwrap()
            || chat.runs.lock().await.contains(id.as_str())
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let saved = chat.sessions.load(&id).await.unwrap().unwrap();
    let images: Vec<_> = saved
        .messages()
        .iter()
        .filter_map(|m| match m {
            crate::agent::message::Message::User(m) if !m.images.is_empty() => Some(&m.images),
            _ => None,
        })
        .collect();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0][0].width, 2);
}
