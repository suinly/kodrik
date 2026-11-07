use super::*;
use crate::remote::protocol::RemoteChatSnapshot;
use serde_json::json;

fn snapshot() -> RemoteChatSnapshot {
    serde_json::from_value(json!({
        "session_id":"server-session","revision":5,"environment_revision":1,
        "messages":[
            {"id":"server-session:0","role":"user","version":0,"versions":["server-session","branch"],"parts":[{"type":"text","text":"server question"}]},
            {"id":"server-session:1","role":"assistant","parts":[{"type":"dynamic-tool","toolCallId":"tool","toolName":"read_file","state":"output-available","input":{"path":"file"},"output":{"text":"contents"}},{"type":"text","text":"server answer"}]}
        ],"previous_cursor":null,"versions":null,"environment":null,
        "run":{"busy":true,"model":"server-test-model","phase":"NeedModel","command_audit_mode":"review","command_approvals":[],"live":{"current":{"status":"Active","text":"partial reply"}}}
    })).unwrap()
}

#[test]
fn restored_tool_results_do_not_treat_null_error_as_failure() {
    use crate::agent::message::{ToolCall, ToolResult};

    for (name, output, failed) in [
        (
            "spawn_agent",
            json!({"id":"companion:subagent:one","status":"running","result":null,"error":null}),
            false,
        ),
        (
            "wait_agent",
            json!({"id":"companion:subagent:one","status":"completed","result":"Done","error":null}),
            false,
        ),
        (
            "spawn_agent",
            json!({"error":"Maximum active children reached"}),
            true,
        ),
        (
            "wait_agent",
            json!({"id":"companion:subagent:one","status":"failed","result":null,"error":"Child request budget exhausted"}),
            true,
        ),
        (
            "exec_command",
            json!({"exit_code":2,"timed_out":false,"stderr":"permission denied"}),
            true,
        ),
        (
            "exec_command",
            json!({"exit_code":null,"timed_out":true}),
            true,
        ),
    ] {
        let mut session = Session::new(SessionId::new("tool-history"));
        session.push(Message::ToolCall(ToolCall {
            call_id: "tool".into(),
            name: name.into(),
            arguments: json!({}),
        }));
        session.push(Message::ToolResult(ToolResult {
            call_id: "tool".into(),
            content: output.clone().into(),
        }));
        let mut app = App::default();
        restore_history(&mut app, &session);
        let row = app
            .messages
            .iter()
            .find(|row| matches!(row.role, AppMessageRole::Tool(_)))
            .unwrap();
        assert_eq!(
            matches!(row.role, AppMessageRole::Tool(ToolState::Failed)),
            failed,
            "{name}: {output}: {}",
            row.content
        );
        if !failed {
            assert!(matches!(row.role, AppMessageRole::Tool(ToolState::Success)));
            assert!(!row.content.contains("Tool error"));
        }
        if let Some(error) = output["error"].as_str() {
            assert!(row.content.contains(error), "{}", row.content);
        }
    }
}

#[test]
fn remote_projection_preserves_successful_spawn_with_null_error_and_real_errors() {
    let mut view = snapshot();
    view.messages[1]["parts"] = json!([
        {"type":"dynamic-tool","toolCallId":"spawn","toolName":"spawn_agent","state":"output-available","input":{"task":"Inspect"},"output":{"id":"companion:subagent:one","status":"running","result":null,"error":null}},
        {"type":"dynamic-tool","toolCallId":"failed-spawn","toolName":"spawn_agent","state":"output-error","input":{"task":"Inspect"},"errorText":"Maximum active children reached"}
    ]);
    let mut app = App::default();
    remote::project(&mut app, &view).unwrap();
    let tools: Vec<_> = app
        .messages
        .iter()
        .filter(|row| matches!(row.role, AppMessageRole::Tool(_)))
        .collect();
    assert_eq!(tools.len(), 2);
    assert!(
        matches!(tools[0].role, AppMessageRole::Tool(ToolState::Success)),
        "{}",
        tools[0].content
    );
    assert!(!tools[0].content.contains("Tool error"));
    assert!(matches!(
        tools[1].role,
        AppMessageRole::Tool(ToolState::Failed)
    ));
    assert!(tools[1].content.contains("Maximum active children reached"));
}

#[test]
fn remote_projection_clears_completed_plan_after_new_user_request() {
    for status in ["pending", "in_progress", "completed"] {
        let mut view = snapshot();
        view.messages[1]["parts"] = json!([{"type":"dynamic-tool","toolCallId":"plan","toolName":"update_plan","state":"output-available","input":{"plan":[{"step":"Inspect","status":status}]},"output":"Plan updated"}]);
        let mut app = App::default();
        remote::project(&mut app, &view).unwrap();
        assert!(app.plan.is_some());
        view.run.as_mut().unwrap()["clear_completed_plan"] = json!(true);
        remote::project(&mut app, &view).unwrap();
        assert_eq!(app.plan.is_none(), status == "completed", "queued {status}");
        view.run.as_mut().unwrap()["clear_completed_plan"] = json!(false);
        view.messages.push(
            json!({"id":"next","role":"user","parts":[{"type":"text","text":"Next request"}]}),
        );
        remote::project(&mut app, &view).unwrap();
        assert_eq!(app.plan.is_none(), status == "completed", "{status}");
        remote::project(&mut app, &view).unwrap();
        assert_eq!(
            app.plan.is_none(),
            status == "completed",
            "reloaded {status}"
        );
    }
}

#[test]
fn server_projection_preserves_draft_versions_tools_and_live_status() {
    let mut app = App {
        input: "unsent draft".into(),
        input_cursor: 3,
        ..Default::default()
    };
    remote::project(&mut app, &snapshot()).unwrap();
    assert_eq!(app.session.unwrap().as_str(), "server-session");
    assert_eq!(app.input, "unsent draft");
    assert_eq!(app.input_cursor, 3);
    assert_eq!(
        app.messages
            .iter()
            .filter(|m| m.role == AppMessageRole::User)
            .count(),
        1
    );
    assert!(app.messages.iter().any(|m| m.content == "server answer"));
    assert!(app.messages.iter().any(|m| m.content == "partial reply"));
    assert!(
        app.messages
            .iter()
            .any(|m| matches!(m.role, AppMessageRole::Tool(ToolState::Success)))
    );
    assert_eq!(app.versions[&0].ids, ["server-session", "branch"]);
    assert!(matches!(app.status, Status::Working));
}

#[test]
fn native_controls_remain_usable_with_large_approval_and_unavailable_review() {
    let mut view = snapshot();
    let patch = format!("{}final patch marker", "exact private patch\n".repeat(3000));
    let arguments = serde_json::to_string(&json!({"patch":patch})).unwrap();
    view.run.as_mut().unwrap()["command_approvals"] = json!([{"id":"large-call","run_id":"run","call_id":"call","tool_name":"apply_patch","arguments_json":arguments,"cmd":"","workdir":null,"reason":"ask","reason_code":"ask"}]);
    let mut app = App::default();
    app.language_choice = Some(1);
    remote::project(&mut app, &view).unwrap();
    assert!(
        app.language_choice.is_none(),
        "approval must replace the language picker"
    );
    assert!(matches!(app.status, Status::Working));
    assert_eq!(app.approval.as_ref().unwrap().id, "large-call");
    assert_eq!(
        app.approval.as_ref().unwrap().arguments_json.as_deref(),
        Some(arguments.as_str())
    );
    assert!(
        app.messages
            .iter()
            .any(|row| row.role == AppMessageRole::Approval
                && row.content.contains("exact private patch")
                && row.content.contains("final patch marker"))
    );
    view.run.as_mut().unwrap()["command_approvals"] = json!([]);
    view.run.as_mut().unwrap()["approval_unavailable"] =
        json!({"id":"large-call","tool_name":"apply_patch"});
    remote::project(&mut app, &view).unwrap();
    assert!(matches!(app.status, Status::Working));
    assert!(app.approval.is_none());
    assert!(
        app.messages
            .iter()
            .any(|row| row.content.contains("/deny large-call")
                && row.content.contains("Allow is unavailable"))
    );
}

#[test]
fn environment_pause_keeps_ui_writable_and_explains_new_message() {
    let mut snapshot = snapshot();
    snapshot.run = Some(
        json!({"busy":false,"phase":{"EnvironmentPaused":{"reason":"disconnected"}},"command_approvals":[],"command_audit_mode":"review"}),
    );
    let mut app = App::default();
    remote::project(&mut app, &snapshot).unwrap();
    assert!(!matches!(
        app.status,
        Status::Working | Status::Recovering | Status::Stopping
    ));
    assert!(
        app.messages
            .iter()
            .any(|m| m.content.contains("new message") || m.content.contains("новое сообщение"))
    );
    app.insert_input("continue");
    assert_eq!(app.input, "continue");
}

#[test]
fn remote_history_exposes_origin_and_marks_unsaved_file_from_old_generation() {
    let mut view = snapshot();
    view.messages[1]["parts"][0]["origin"] = json!({"label":"Laptop\u{1b}[31m","project_root":"/only-on-laptop/project","tool_id":"read_file","environment":{"connection_id":"connection","generation":"old"}});
    let mut app = App::default();
    remote::project(&mut app, &view).unwrap();
    let text = app
        .messages
        .iter()
        .map(|row| row.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!text.contains("/only-on-laptop/project"));
    assert!(!text.contains("old"));
    assert!(text.contains("read_file · Laptop\\u{1b}[31m"));
    assert!(text.contains("file remains on the disconnected device"));
    assert!(!text.contains('\u{1b}'));
}

#[test]
fn remote_tool_display_uses_short_name_and_device_without_internal_ids() {
    let mut view = snapshot();
    let part = &mut view.messages[1]["parts"][0];
    part["toolName"] = json!("local__exec__719e6926f958f45893bc00ee50e2e1ae");
    part["origin"] = json!({"label":"macbook-pro","project_root":"/Volumes/SamsungT5/Projects/Personal/kodrik","tool_id":"exec_command","environment":{"generation":"27a00aff-d9da-43ee-874d-1174f180c23c"}});
    part["output"] = json!({"exit_code":0,"stdout":"","stderr":""});
    part["input"] = json!({"cmd":"cargo test -q"});
    let mut app = App::default();
    remote::project(&mut app, &view).unwrap();
    let text = app
        .messages
        .iter()
        .map(|row| row.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("exec_command · macbook-pro: cargo test -q — exit 0"),
        "{text}"
    );
    for hidden in ["local__", "27a00aff", "/Volumes/", "[Device:"] {
        assert!(!text.contains(hidden), "{text}");
    }
}

#[test]
fn stale_remote_snapshots_cannot_rewind_history_or_cross_sessions() {
    let mut state = remote::Projection::default();
    let mut app = App::default();
    let snapshot = snapshot();
    assert!(state.accept(&mut app, &snapshot).unwrap());
    let mut stale = snapshot.clone();
    stale.revision = 4;
    assert!(!state.accept(&mut app, &stale).unwrap());
    stale.revision = 5;
    stale.environment_revision = 2;
    assert!(state.accept(&mut app, &stale).unwrap());
    stale.session_id = SessionId::new("other");
    stale.revision = 100;
    assert!(!state.accept(&mut app, &stale).unwrap());
}

#[test]
fn remote_controls_do_not_become_user_messages_and_status_is_visible() {
    let mut app = App::default();
    remote::project(&mut app, &snapshot()).unwrap();
    app.input = "/attach".into();
    app.input_cursor = 7;
    assert_eq!(
        app.update(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .as_deref(),
        Some("/attach")
    );
    assert!(
        !app.messages
            .iter()
            .any(|row| row.role == AppMessageRole::User && row.content == "/attach")
    );
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| ui::render(&mut app, frame)).unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("server-test-model"));
    assert!(text.contains("detached"));
}

#[test]
fn server_snapshot_does_not_overwrite_local_connection_display() {
    let mut view = snapshot();
    view.environment=Some(serde_json::from_value(json!({"session_id":"server-session","environment":null,"label":"Work laptop","project_root":"/projects/demo","state":"connected","reason":null})).unwrap());
    let mut app = App {
        remote_environment: Some("demo".into()),
        remote_connection: Some("detached"),
        ..Default::default()
    };
    remote::project(&mut app, &view).unwrap();
    let status = app.remote_environment.unwrap();
    assert_eq!(status, "demo");
    assert_eq!(app.remote_connection, Some("detached"));
}

#[test]
fn projected_user_parts_stay_one_turn_and_equal_snapshot_has_no_duplicate_live_or_approval() {
    let mut view = snapshot();
    view.messages[0]["parts"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"text","text":"second part"}));
    view.messages[0]["parts"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"file","filename":"image.png"}));
    view.run.as_mut().unwrap()["command_approvals"] = json!([{"id":"approval","run_id":"run","call_id":"call","cmd":"echo hi","workdir":null,"reason":"ask","reason_code":""}]);
    let mut app = App::default();
    remote::project(&mut app, &view).unwrap();
    app.approval_choice = Some(2);
    remote::project(&mut app, &view).unwrap();
    assert_eq!(app.approval_choice, Some(2));
    assert_eq!(
        app.messages
            .iter()
            .filter(|row| row.role == AppMessageRole::User)
            .count(),
        1
    );
    assert_eq!(
        app.messages
            .iter()
            .filter(|row| row.content == "partial reply")
            .count(),
        1
    );
    assert_eq!(
        app.messages
            .iter()
            .filter(|row| row.role == AppMessageRole::Approval)
            .count(),
        1
    );
}

#[test]
fn reconnect_accepts_fresh_snapshot_after_gateway_environment_counter_reset() {
    let mut app = App::default();
    let mut projection = remote::Projection::default();
    let mut view = snapshot();
    view.environment_revision = 100;
    projection.accept(&mut app, &view).unwrap();
    view.environment_revision = 1;
    view.environment=Some(serde_json::from_value(json!({"session_id":"server-session","environment":null,"label":"Laptop","project_root":"/tmp/project","state":"disconnected","reason":"gateway restarted"})).unwrap());
    assert!(projection.resynchronize(&mut app, &view).unwrap());
    assert_eq!(app.remote_connection, Some("detached"));
    assert!(app.remote_environment.unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "private PTY subprocess"]
async fn terminal_remote_fixture() {
    use crate::remote::{client::Credential, config::RemoteConfig, protocol::*};
    use axum::{
        Json, Router,
        extract::{
            Path as RoutePath, State, WebSocketUpgrade,
            ws::{Message as WsMessage, WebSocket},
        },
        response::IntoResponse,
        routing::{get, post},
    };
    use std::sync::Mutex;
    let dir = std::path::PathBuf::from(std::env::var_os("KODRIK_TUI_FIXTURE").unwrap());
    let state = Arc::new(Mutex::new(snapshot()));
    if std::env::var("KODRIK_TUI_CASE").as_deref() == Ok("remote-history-error") {
        state.lock().unwrap().previous_cursor = Some("server-session:1".into());
    }
    async fn socket(mut ws: WebSocket) {
        let hello = ws.recv().await.unwrap().unwrap();
        let bytes = match hello {
            WsMessage::Binary(b) => b.to_vec(),
            WsMessage::Text(s) => s.as_bytes().to_vec(),
            _ => panic!("hello"),
        };
        let hello: ClientFrame = decode_frame(&bytes).unwrap();
        std::fs::write("last-hello", serde_json::to_vec(&hello).unwrap()).unwrap();
        if let ClientFrame::Hello { session_id, .. } = &hello {
            eprintln!("fixture-attached-{}", session_id.as_str());
        }
        let environment = EnvironmentKey {
            connection_id: uuid::Uuid::new_v4(),
            generation: uuid::Uuid::new_v4(),
        };
        ws.send(WsMessage::Binary(
            encode_frame(&ServerFrame::Welcome {
                environment: environment.clone(),
                heartbeat_seconds: 5,
                lease_seconds: 15,
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
        while let Some(Ok(frame)) = ws.recv().await {
            if let WsMessage::Binary(bytes) = frame {
                match decode_frame::<ClientFrame>(&bytes).unwrap() {
                    ClientFrame::Heartbeat { .. } => {
                        if ws
                            .send(WsMessage::Binary(
                                encode_frame(&ServerFrame::HeartbeatAck {
                                    environment: environment.clone(),
                                })
                                .unwrap()
                                .into(),
                            ))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    ClientFrame::Close { .. } => {
                        std::fs::write("local-detached", b"1").unwrap();
                        break;
                    }
                    _ => (),
                }
            }
        }
    }
    async fn command(
        State(state): State<Arc<Mutex<RemoteChatSnapshot>>>,
        Json(command): Json<ChatCommand>,
    ) -> axum::response::Response {
        if matches!(&command,ChatCommand::Send{text,..} if text=="slow") {
            std::fs::write("slow-request", b"1").unwrap();
            eprintln!("fixture-slow-request");
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        let mut view = state.lock().unwrap();
        view.revision += 1;
        let value = match command {
            ChatCommand::Send { text, request_id } => {
                view.messages.push(json!({"id":"server-session:2","role":"assistant","parts":[{"type":"text","text":format!("remote echo: {text}")}]}));
                if text == "uncertain" {
                    use std::io::Write;
                    writeln!(
                        std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open("admitted")
                            .unwrap(),
                        "{request_id}"
                    )
                    .unwrap();
                    return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                json!({"queued":true})
            }
            ChatCommand::SubmissionStatus { request_id } => {
                let admitted = std::fs::read_to_string("admitted").unwrap_or_default();
                if admitted.lines().any(|line| line == request_id.to_string()) {
                    json!({"status":"Done","session_id":"server-session"})
                } else {
                    serde_json::Value::Null
                }
            }
            ChatCommand::Stop => {
                view.run = Some(json!({"busy":false,"phase":"Cancelled"}));
                serde_json::Value::Null
            }
            ChatCommand::New => {
                assert!(
                    std::path::Path::new("local-detached").exists(),
                    "New dispatched before detach"
                );
                json!({"id":"new-session"})
            }
            ChatCommand::Select { session_id } => {
                std::fs::write("selected-session", session_id.as_str()).unwrap();
                json!({"id":session_id})
            }
            ChatCommand::SelectVersion {
                session_id,
                user_turn,
            } => {
                assert_eq!(user_turn, 0);
                std::fs::write("selected-version", session_id.as_str()).unwrap();
                json!({"id":session_id})
            }
            ChatCommand::DecideApproval { approval_id, allow } => {
                assert_eq!(approval_id, "approval");
                std::fs::write(
                    "approval-decision",
                    if allow { "allowed" } else { "denied" },
                )
                .unwrap();
                eprintln!("fixture-approval-decided");
                if std::env::var("KODRIK_TUI_CASE").as_deref() == Ok("remote-approval-unavailable")
                {
                    // Exercise PTY backpressure after the parent has observed the decision.
                    eprintln!("{}", " ".repeat(128 * 1024));
                }
                view.run.as_mut().unwrap()["command_approvals"] = json!([]);
                serde_json::Value::Null
            }
            ChatCommand::TrustApproval { approval_id } => {
                assert_eq!(approval_id, "approval");
                std::fs::write("approval-decision", "trusted").unwrap();
                eprintln!("fixture-approval-decided");
                view.run.as_mut().unwrap()["command_approvals"] = json!([]);
                serde_json::Value::Null
            }
            ChatCommand::Edit { text, .. } => {
                if std::env::var("KODRIK_TUI_CASE").as_deref() == Ok("remote-edit-success") {
                    std::fs::write("edited-text", text).unwrap();
                    return Json(json!({"id":"edited-branch"})).into_response();
                }
                return (
                    axum::http::StatusCode::CONFLICT,
                    Json(json!({"error":"fixture edit denied"})),
                )
                    .into_response();
            }
            _ => serde_json::Value::Null,
        };
        Json(value).into_response()
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    if std::env::var("KODRIK_TUI_CASE").is_ok_and(|case| case.starts_with("remote-approval")) {
        state.lock().unwrap().run.as_mut().unwrap()["command_approvals"] = json!([{"id":"approval","run_id":"run","call_id":"call","cmd":"echo hi","workdir":null,"reason":"ask","reason_code":""}]);
    }
    if std::env::var("KODRIK_TUI_CASE").as_deref() == Ok("remote-approval-unavailable") {
        let mut view = state.lock().unwrap();
        view.run.as_mut().unwrap()["command_approvals"] = json!([]);
        view.run.as_mut().unwrap()["approval_unavailable"] =
            json!({"id":"approval","tool_name":"apply_patch"});
    }
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route(
            "/api/remote/chat/sessions",
            get(|axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String,String>>| async move {
                let case=std::env::var("KODRIK_TUI_CASE").unwrap_or_default();
                if case.starts_with("remote-sessions-") && query.contains_key("offset") {
                    let offset=query["offset"].parse::<usize>().unwrap();
                    assert_eq!(query.get("limit").map(String::as_str),Some("30"));
                    if case=="remote-sessions-slow" {
                        std::fs::write("sessions-request",b"1").unwrap();
                        while !std::path::Path::new("sessions-release").exists() {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    }
                    if (case=="remote-sessions-retry" || case=="remote-sessions-pages-retry" && offset==30) && !std::path::Path::new("sessions-failed").exists() {
                        std::fs::write("sessions-failed",b"1").unwrap();
                        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                    if case.starts_with("remote-sessions-pages") {
                        std::fs::write(format!("sessions-page-{offset}"),b"1").unwrap();
                        let rows=(offset..(offset+30).min(35)).map(|index|json!({"id":format!("session-{index}"),"title":format!("Paged conversation {index}")})).collect::<Vec<_>>();
                        return Json(json!(rows)).into_response();
                    }
                }
                Json(json!([{"id":"server-session","title":"server conversation"}])).into_response()
            }),
        )
        .route(
            "/api/remote/chat/sessions/{id}",
            get(
                |RoutePath(id): RoutePath<String>,
                 axum::extract::Query(query): axum::extract::Query<
                    std::collections::HashMap<String, String>,
                >,
                 State(state): State<Arc<Mutex<RemoteChatSnapshot>>>| async move {
                    if query.contains_key("cursor") {
                        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                    let mut view = state.lock().unwrap().clone();
                    view.session_id = SessionId::new(id);
                    Json(view).into_response()
                },
            ),
        )
        .route("/api/remote/chat/sessions/{id}/commands", post(command))
        .route(
            "/api/remote/chat/sessions/{id}/events",
            get(
                |RoutePath(id): RoutePath<String>, State(state): State<Arc<Mutex<RemoteChatSnapshot>>>| async move {
                    let mut metadata = serde_json::to_value(&*state.lock().unwrap()).unwrap();
                    metadata["session_id"] = json!(id);
                    let messages = metadata.as_object_mut().unwrap().remove("messages").unwrap();
                    let text = serde_json::to_string(&messages).unwrap();
                    let mut frames = crate::remote::chat_sync::frames("", &text).unwrap();
                    frames.last_mut().unwrap().metadata = Some(metadata);
                    let data = frames.into_iter().map(|frame| format!("data: {}\n\n", serde_json::to_string(&frame).unwrap())).collect::<String>();
                    (
                        [("content-type", "text/event-stream")],
                        data,
                    )
                        .into_response()
                },
            ),
        )
        .route(
            "/api/remote/environment",
            get(|ws: WebSocketUpgrade| async move {
                if std::env::var("KODRIK_TUI_CASE").as_deref() == Ok("remote-failed-attach") {
                    return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                ws.on_upgrade(socket)
            }),
        )
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    std::env::set_current_dir(&dir).unwrap();
    let config = RemoteConfig {
        server: format!("http://{address}/").parse().unwrap(),
        credential_file: dir.join("remote/device.json"),
        label: "fixture".into(),
        allow_shell: false,
        skill_roots: vec![],
        mcp: Default::default(),
        optional_mcp: vec![],
    };
    Credential {
        device_id: uuid::Uuid::new_v4(),
        token: "fixture".into(),
    }
    .save(&config.credential_file)
    .unwrap();
    let environment = crate::remote::local::LocalEnvironment::open(&config, &dir)
        .await
        .unwrap();
    if std::env::var("KODRIK_TUI_CASE").as_deref() != Ok("remote-first-grant") {
        config
            .authorize_manifest(&environment.manifest().unwrap(), || Ok(true))
            .unwrap();
    }
    environment.close().await.unwrap();
    remote::run(&config).await.unwrap();
    std::fs::write(dir.join("remote-clean-exit"), b"1").unwrap();
    println!("remote-clean-exit");
    server.abort();
}

#[cfg(unix)]
#[test]
fn remote_pty_loads_server_history_sends_steers_stops_and_exits_without_provider() {
    let dir = tempfile::tempdir().unwrap();
    let mut terminal = startup_tests::PrivateTerminal::start_case(dir.path(), "remote");
    terminal.wait_for(b"server answer", 0);
    terminal.send(b"hello\r");
    terminal.wait_for(b"remote echo: hello", 0);
    terminal.send(b"steering\r");
    terminal.wait_for(b"remote echo: steering", 0);
    terminal.send(b"\x1b");
    terminal.wait_for(b"Stopped", 0);
    terminal.send(b"\x03");
    terminal.wait_for(b"remote-clean-exit", 0);
}

#[cfg(unix)]
#[test]
fn first_remote_grant_uses_picker_before_sharing_and_is_cached() {
    let dir = tempfile::tempdir().unwrap();
    let mut terminal = startup_tests::PrivateTerminal::start_case(dir.path(), "remote-first-grant");
    terminal.wait_for(b"Grant local environment access?", 0);
    assert!(!dir.path().join("last-hello").exists());
    assert!(!dir.path().join("remote/device.grant").exists());
    terminal.send(b"\x1b[27u");
    terminal.wait_for(b"Local access declined", 0);
    terminal.send(b"still online\r");
    terminal.wait_for(b"remote echo: still online", 0);
    assert!(!dir.path().join("last-hello").exists());
    let since = terminal.output.len();
    terminal.send(b"/attach\r");
    terminal.wait_for(b"Grant local environment access?", since);
    terminal.send(b"\x1b[A\r");
    terminal.wait_for(b"Local environment attached", 0);
    assert!(dir.path().join("remote/device.grant").exists());
    assert!(dir.path().join("last-hello").exists());
    terminal.send(b"\x03");
    terminal.wait_for(b"remote-clean-exit", 0);
    assert!(dir.path().join("local-detached").exists());
}

#[cfg(unix)]
#[test]
fn remote_start_attaches_and_switch_transfers_binding_without_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let mut terminal = startup_tests::PrivateTerminal::start_case(dir.path(), "remote");
    terminal.wait_for(b"server answer", 0);
    terminal.wait_for(b"Local environment attached", 0);
    terminal.send(b"\x1b");
    terminal.wait_for(b"Stopped", 0);
    terminal.send(b"/new\r");
    terminal.wait_for(b"fixture-attached-new-session", 0);
    assert!(dir.path().join("local-detached").exists());
    let hello: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("last-hello")).unwrap()).unwrap();
    assert_eq!(hello["session_id"], "new-session");
    terminal.send(b"\x03");
    terminal.wait_for(b"remote-clean-exit", 0);
}

#[cfg(unix)]
#[test]
fn failed_environment_attachment_keeps_server_chat_and_terminal_usable() {
    let dir = tempfile::tempdir().unwrap();
    let mut terminal =
        startup_tests::PrivateTerminal::start_case(dir.path(), "remote-failed-attach");
    terminal.wait_for(b"server answer", 0);
    terminal.wait_for(b"Local attachment failed", 0);
    terminal.send(b"still online\r");
    terminal.wait_for(b"remote echo: still online", 0);
    terminal.send(b"\x03");
    terminal.wait_for(b"remote-clean-exit", 0);
}

#[cfg(unix)]
#[test]
fn rejected_remote_edit_preserves_editor_and_original_draft() {
    let dir = tempfile::tempdir().unwrap();
    let mut terminal = startup_tests::PrivateTerminal::start_case(dir.path(), "remote");
    terminal.wait_for(b"server answer", 0);
    terminal.send(b"\x1b");
    terminal.wait_for(b"Stopped", 0);
    terminal.send(b"original draft");
    terminal.send(b"\x1b[27u\x1b[27u");
    terminal.wait_for(b"Enter", 0);
    terminal.send(b"\r");
    terminal.send(b" edited\r");
    terminal.wait_for(b"Remote action not confirmed", 0);
    terminal.send(b"\x1b");
    terminal.wait_for(b"original draft", 0);
    terminal.send(b"\x03\x03");
    terminal.wait_for(b"remote-clean-exit", 0);
}

#[cfg(unix)]
#[test]
fn lost_send_response_checks_original_id_without_resubmitting_effect() {
    let dir = tempfile::tempdir().unwrap();
    let mut terminal = startup_tests::PrivateTerminal::start_case(dir.path(), "remote");
    terminal.wait_for(b"server answer", 0);
    terminal.send(b"uncertain\r");
    terminal.wait_for(b"Remote action not confirmed", 0);
    terminal.send(b"\x03/retry\r");
    terminal.wait_for(b"Original input confirmed", 0);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("admitted"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    terminal.send(b"\x03");
    terminal.wait_for(b"remote-clean-exit", 0);
}

#[cfg(unix)]
#[test]
fn unavailable_remote_approval_cannot_be_allowed_by_typing_its_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut terminal =
        startup_tests::PrivateTerminal::start_case(dir.path(), "remote-approval-unavailable");
    terminal.wait_for(b"Allow is unavailable", 0);
    terminal.send(b"/approve approval\r");
    terminal.wait_for(b"Remote action not confirmed", 0);
    assert!(!dir.path().join("approval-decision").exists());
    terminal.send(b"/deny approval\r");
    terminal.wait_for(b"fixture-approval-decided", 0);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !dir.path().join("approval-decision").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "unreviewable approval could not be rejected"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        std::fs::read_to_string(dir.path().join("approval-decision")).unwrap(),
        "denied"
    );
    terminal.send(b"\x03");
    terminal.wait_for(b"remote-clean-exit", 0);
}

#[cfg(unix)]
#[test]
fn remote_approval_keyboard_sends_server_decision() {
    let dir = tempfile::tempdir().unwrap();
    let mut terminal = startup_tests::PrivateTerminal::start_case(dir.path(), "remote-approval");
    terminal.wait_for(b"echo hi", 0);
    terminal.send(b"\r");
    terminal.wait_for(b"fixture-approval-decided", 0);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !dir.path().join("approval-decision").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "approval decision not dispatched"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        std::fs::read_to_string(dir.path().join("approval-decision")).unwrap(),
        "allowed"
    );
    terminal.send(b"\x03");
    terminal.wait_for(b"remote-clean-exit", 0);
}

#[cfg(unix)]
#[test]
fn remote_session_picker_and_version_navigation_use_server_commands() {
    let dir = tempfile::tempdir().unwrap();
    let mut terminal = startup_tests::PrivateTerminal::start_case(dir.path(), "remote");
    terminal.wait_for(b"server answer", 0);
    terminal.send(b"\x1b");
    terminal.wait_for(b"Stopped", 0);
    terminal.send(b"/sessions\r");
    terminal.wait_for(b"server conversation", 0);
    let since = terminal.output.len();
    terminal.send(b"\r");
    terminal.wait_for(b"fixture-attached-server-session", since);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("selected-session")).unwrap(),
        "server-session"
    );
    terminal.send(b"\x1b[27u\x1b[27u");
    terminal.wait_for(b"versions", 0);
    terminal.send(b"\x1b[C");
    terminal.wait_for(b"fixture-attached-branch", 0);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !dir.path().join("selected-version").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "version command not dispatched"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        std::fs::read_to_string(dir.path().join("selected-version")).unwrap(),
        "branch"
    );
    terminal.send(b"\x03");
    terminal.wait_for(b"remote-clean-exit", 0);
}

#[cfg(unix)]
#[test]
fn remote_sessions_loading_remains_cancellable_and_late_results_stay_closed() {
    let dir = tempfile::tempdir().unwrap();
    let mut terminal =
        startup_tests::PrivateTerminal::start_case(dir.path(), "remote-sessions-slow");
    terminal.wait_for(b"server answer", 0);
    terminal.send(b"\x1b");
    terminal.wait_for(b"Stopped", 0);
    terminal.send(b"/sessions\r");
    terminal.wait_for(b"Loading sessions", 0);
    terminal.send(b"\x1b[27u");
    terminal.send(b"hello\r");
    terminal.wait_for(b"remote echo: hello", 0);
    std::fs::write(dir.path().join("sessions-release"), b"1").unwrap();
    terminal.send(b"\x1b[27u");
    terminal.wait_for(b"Stopped", 0);
    terminal.send(b"/sessions\r");
    terminal.wait_for(b"server conversation", 0);
    terminal.send(b"\x1b[27u");
    terminal.send(b"\x03");
    terminal.wait_for(b"remote-clean-exit", 0);
    assert!(!dir.path().join("selected-session").exists());
}

#[cfg(unix)]
#[test]
fn remote_sessions_paginates_near_end_and_selects_later_page() {
    let dir = tempfile::tempdir().unwrap();
    let mut terminal =
        startup_tests::PrivateTerminal::start_case(dir.path(), "remote-sessions-pages");
    terminal.wait_for(b"server answer", 0);
    terminal.send(b"\x1b");
    terminal.wait_for(b"Stopped", 0);
    terminal.send(b"/sessions\r");
    terminal.wait_for(b"Paged conversation 0", 0);
    assert!(!dir.path().join("sessions-page-30").exists());
    for _ in 0..27 {
        terminal.send(b"\x1b[B");
    }
    terminal.wait_for(b"Paged conversation 27", 0);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !dir.path().join("sessions-page-30").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "next page not prefetched"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    for _ in 0..7 {
        terminal.send(b"\x1b[B");
    }
    terminal.wait_for(b"Paged conversation 34", 0);
    terminal.send(b"\r");
    terminal.wait_for(b"fixture-attached-session-34", 0);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("selected-session")).unwrap(),
        "session-34"
    );
    assert!(!dir.path().join("sessions-page-35").exists());
    terminal.send(b"\x03");
    terminal.wait_for(b"remote-clean-exit", 0);
}

#[cfg(unix)]
#[test]
fn remote_sessions_error_is_retried_inside_the_picker() {
    let dir = tempfile::tempdir().unwrap();
    let mut terminal =
        startup_tests::PrivateTerminal::start_case(dir.path(), "remote-sessions-retry");
    terminal.wait_for(b"server answer", 0);
    terminal.send(b"\x1b");
    terminal.wait_for(b"Stopped", 0);
    terminal.send(b"/sessions\r");
    terminal.wait_for(b"Could not load sessions", 0);
    terminal.send(b"r");
    terminal.wait_for(b"server conversation", 0);
    terminal.send(b"\r");
    terminal.wait_for(b"fixture-attached-server-session", 0);
    terminal.send(b"\x03");
    terminal.wait_for(b"remote-clean-exit", 0);
}

#[cfg(unix)]
#[test]
fn remote_sessions_later_page_failure_keeps_rows_and_retries_same_offset() {
    let dir = tempfile::tempdir().unwrap();
    let mut terminal =
        startup_tests::PrivateTerminal::start_case(dir.path(), "remote-sessions-pages-retry");
    terminal.wait_for(b"server answer", 0);
    terminal.send(b"\x1b");
    terminal.wait_for(b"Stopped", 0);
    terminal.send(b"/sessions\r");
    terminal.wait_for(b"Paged conversation 0", 0);
    for _ in 0..27 {
        terminal.send(b"\x1b[B");
    }
    terminal.wait_for(b"Could not load sessions", 0);
    terminal.wait_for(b"Paged conversation 27", 0);
    terminal.send(b"r");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !dir.path().join("sessions-page-30").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "failed page was not retried"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    terminal.send(b"\r");
    terminal.wait_for(b"fixture-attached-session-27", 0);
    terminal.send(b"\x03");
    terminal.wait_for(b"remote-clean-exit", 0);
}

#[test]
fn remote_audit_rejects_ignored_session_or_approval_arguments() {
    assert!(remote::audit_mode("/audit trusted unrelated approval").is_err());
    assert_eq!(remote::audit_mode("/audit ask").unwrap().code(), "ask");
}

#[test]
fn observer_refresh_keeps_loaded_scrollback_for_the_same_history_revision() {
    let mut current = snapshot();
    current.messages.insert(0,json!({"id":"older","role":"user","user_turn":0,"parts":[{"type":"text","text":"older question"}]}));
    current.previous_cursor = None;
    let latest = snapshot();
    let merged = remote::with_scrollback(&current, latest);
    assert_eq!(merged.messages[0]["id"], "older");
    assert_eq!(merged.messages.len(), 3);
    assert_eq!(merged.previous_cursor, None);
    let mut changed = snapshot();
    changed.revision += 1;
    let replaced = remote::with_scrollback(&current, changed);
    assert_eq!(replaced.messages.len(), 2);
}

#[cfg(unix)]
#[test]
fn remote_trust_shortcut_is_tied_to_displayed_approval_not_generic_mode_change() {
    let dir = tempfile::tempdir().unwrap();
    let mut terminal =
        startup_tests::PrivateTerminal::start_case(dir.path(), "remote-approval-trust");
    terminal.wait_for(b"echo hi", 0);
    terminal.send(b"jj\r");
    terminal.wait_for(b"fixture-approval-decided", 0);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !dir.path().join("approval-decision").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "guarded trust not dispatched"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        std::fs::read_to_string(dir.path().join("approval-decision")).unwrap(),
        "trusted"
    );
    terminal.send(b"\x03");
    terminal.wait_for(b"remote-clean-exit", 0);
}

#[cfg(unix)]
#[test]
fn history_failure_after_attach_still_closes_environment_on_exit() {
    let dir = tempfile::tempdir().unwrap();
    let mut terminal =
        startup_tests::PrivateTerminal::start_case(dir.path(), "remote-history-error");
    terminal.wait_for(b"server answer", 0);
    terminal.wait_for(b"Local environment attached", 0);
    terminal.send(b"/older\r");
    terminal.wait_for(b"Remote action not confirmed", 0);
    terminal.send(b"\x03");
    terminal.wait_for(b"remote-clean-exit", 0);
    assert!(dir.path().join("local-detached").exists());
}

#[cfg(unix)]
#[test]
fn remote_copy_uses_laptop_terminal_clipboard_without_server_command() {
    let dir = tempfile::tempdir().unwrap();
    let mut terminal = startup_tests::PrivateTerminal::start_case(dir.path(), "remote");
    terminal.wait_for(b"server answer", 0);
    terminal.send(b"draft");
    terminal.wait_for(b"draft", 0);
    terminal.send(b"\x1b[<0;4;22M\x1b[<32;8;22M\x1b[<0;8;22m");
    terminal.wait_for(b"\x1b]52;", 0);
    terminal.wait_for(b"Copy requested from terminal", 0);
    // Ctrl+C first clears the draft, just as in standalone TUI.
    terminal.send(b"\x03\x03");
    terminal.wait_for(b"remote-clean-exit", 0);
}

#[cfg(unix)]
#[test]
fn remote_local_controls_never_become_model_input() {
    let dir = tempfile::tempdir().unwrap();
    let mut terminal = startup_tests::PrivateTerminal::start_case(dir.path(), "remote");
    terminal.wait_for(b"server answer", 0);
    terminal.send(b"/audit\r");
    terminal.wait_for(b"Tool mode", 0);
    terminal.send(b"\x1b[27u");
    terminal.send(b"/audit status\r");
    terminal.wait_for(b"Current server tool mode", 0);
    terminal.send(b"/language en\r");
    terminal.wait_for(b"Interface language: English", 0);
    terminal.send(b"/attach no\r");
    terminal.wait_for(b"Unknown remote command", 0);
    terminal.send(b"\x03");
    terminal.wait_for(b"remote-clean-exit", 0);
}

#[cfg(unix)]
#[test]
fn successful_remote_edit_selects_server_branch_and_transfers_access() {
    let dir = tempfile::tempdir().unwrap();
    let mut terminal =
        startup_tests::PrivateTerminal::start_case(dir.path(), "remote-edit-success");
    terminal.wait_for(b"server answer", 0);
    terminal.send(b"\x1b");
    terminal.wait_for(b"Stopped", 0);
    terminal.send(b"\x1b[27u\x1b[27u");
    terminal.wait_for(b"versions", 0);
    terminal.send(b"\r\x05 changed\r");
    terminal.wait_for(b"fixture-attached-edited-branch", 0);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("selected-session")).unwrap(),
        "edited-branch"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("edited-text")).unwrap(),
        "server question changed"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let hello: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("last-hello")).unwrap()).unwrap();
        if hello["session_id"] == "edited-branch" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "edited branch not attached"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    terminal.send(b"\x03");
    terminal.wait_for(b"remote-clean-exit", 0);
}

#[cfg(unix)]
#[test]
fn exit_during_unanswered_http_request_closes_local_binding_promptly() {
    let dir = tempfile::tempdir().unwrap();
    let mut terminal = startup_tests::PrivateTerminal::start_case(dir.path(), "remote");
    terminal.wait_for(b"server answer", 0);
    terminal.wait_for(b"Local environment attached", 0);
    terminal.send(b"slow\r");
    terminal.wait_for(b"fixture-slow-request", 0);
    assert!(dir.path().join("slow-request").exists());
    terminal.send(b"\x03");
    terminal.wait_for(b"remote-clean-exit", 0);
    assert!(dir.path().join("local-detached").exists());
}

#[test]
fn old_observer_cannot_reset_revision_after_same_session_selection() {
    assert!(!remote::current_observer(2, 1));
    assert!(remote::current_observer(2, 2));
}

#[test]
fn remote_compaction_only_runs_while_preparing_and_unfinished() {
    let mut app = App::default();
    let mut view = snapshot();
    for (status, finished, active) in [
        ("preparing", false, true),
        ("preparing", true, false),
        ("ready", false, false),
        ("applied", true, false),
        ("failed", false, false),
        ("cancelled", false, false),
        ("stale", false, false),
    ] {
        view.run.as_mut().unwrap()["compaction"] = json!({
            "status":status,"progress":{"finished_at":if finished {json!("2026-11-07T10:00:00Z")} else {json!(null)}}
        });
        remote::project(&mut app, &view).unwrap();
        assert_eq!(app.compacting, active, "{status}, finished={finished}");
    }
    view.run.as_mut().unwrap()["compaction"] = json!(null);
    remote::project(&mut app, &view).unwrap();
    assert!(!app.compacting);
}
