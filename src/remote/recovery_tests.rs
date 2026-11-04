use super::{protocol::*, tools::alias};
use crate::agent::{
    Agent, AgentRunner, AssistantMessage, ContextBuilder, LlmContext,
    inbox::{BusyMode, SubmissionRequest, SubmissionStatus},
    llm::LlmProvider,
    message::{Message, ToolCall},
    recovery::RecoveryTarget,
    run_state::{Checkpoint, RunPhase},
    session::{Session, SessionId, SessionStore, SqliteSessionStore},
    tools::{Tool, ToolRegistry},
};
use crate::gateway::{IncomingMessage, InterfaceId};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use uuid::Uuid;

struct Model(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl LlmProvider for Model {
    async fn generate(
        &self,
        _: &LlmContext<'_>,
        _: &[&dyn Tool],
        _: Option<&tokio::sync::mpsc::Sender<crate::agent::AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok((
            vec![Message::Assistant(AssistantMessage {
                content: "server answer".into(),
            })],
            None,
        ))
    }
}

struct LocalModel(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl LlmProvider for LocalModel {
    async fn generate(
        &self,
        context: &LlmContext<'_>,
        tools: &[&dyn Tool],
        _: Option<&tokio::sync::mpsc::Sender<crate::agent::AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        self.0.fetch_add(1, Ordering::SeqCst);
        if context
            .messages
            .iter()
            .any(|m| matches!(m, Message::ToolResult(_)))
            || tools.iter().all(|tool| tool.audit_target().is_none())
        {
            return Ok((
                vec![Message::Assistant(AssistantMessage {
                    content: "server answer".into(),
                })],
                None,
            ));
        }
        let tool = tools
            .iter()
            .find(|tool| tool.audit_target().is_some())
            .unwrap();
        Ok((
            vec![Message::ToolCall(ToolCall {
                call_id: "in-flight".into(),
                name: tool.name(),
                arguments: json!({"cmd":"effect"}),
            })],
            None,
        ))
    }
}

#[tokio::test]
async fn in_flight_disconnect_yields_ownership_and_requires_new_explicit_input() {
    use crate::web::auth::{
        AuthStore, Identity,
        devices::{DeviceIdentity, PairingPoll},
    };
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
    let id = SessionId::new(Uuid::new_v4().to_string());
    store.save(&Session::new(id.clone())).await.unwrap();
    let auth = AuthStore::new(dir.path());
    let now = chrono::Utc::now().timestamp();
    let pair = auth.start_device_pairing("fixture", now).unwrap();
    auth.approve_device_pairing(pair.request_id, &pair.code, &Identity::Owner, now)
        .unwrap();
    let PairingPoll::Authorized { device_id, .. } = auth
        .poll_device_pairing(pair.request_id, &pair.polling_secret, now)
        .unwrap()
    else {
        panic!()
    };
    let count = Arc::new(AtomicUsize::new(0));
    let runner = Arc::new(AgentRunner::new(
        Arc::new(Agent::new(
            Arc::new(LocalModel(count.clone())),
            ToolRegistry::default(),
            ContextBuilder::new("fixture"),
        )),
        Arc::new(store.clone()),
    ));
    runner.enable_environments(auth).unwrap();
    let service = runner.environments().unwrap().clone();
    let manifest = EnvironmentManifest {
        label: "fixture".into(),
        project_root: dir.path().display().to_string(),
        tools: vec![ToolDescriptor {
            id: "exec_command".into(),
            kind: ToolKind::Exec,
            description: "shell".into(),
            parameters: json!({"type":"object"}),
            deferred: false,
        }],
        skills: vec![],
        project_instructions: None,
        warnings: vec![],
    };
    let key = service
        .attach(DeviceIdentity { device_id }, id.clone(), manifest.clone())
        .await
        .unwrap();
    let (peer, mut outgoing) = super::transport::Peer::new();
    service.bind_peer(&key, peer).await.unwrap();
    let unrelated = SessionId::new(Uuid::new_v4().to_string());
    let server_task = {
        let runner = runner.clone();
        tokio::spawn(async move {
            runner
                .run(
                    unrelated.clone(),
                    IncomingMessage::new(
                        InterfaceId("web".into()),
                        unrelated.as_str(),
                        "server only",
                    ),
                    None,
                )
                .await
        })
    };
    let task = {
        let runner = runner.clone();
        let id = id.clone();
        tokio::spawn(async move {
            runner
                .run(
                    id.clone(),
                    IncomingMessage::new(InterfaceId("web".into()), id.as_str(), "run on laptop"),
                    None,
                )
                .await
        })
    };
    let ServerFrame::Invoke { invocation } =
        tokio::time::timeout(std::time::Duration::from_secs(3), outgoing.recv())
            .await
            .unwrap()
            .unwrap()
    else {
        panic!()
    };
    let correction = store
        .submit(request(&id, "queued correction", BusyMode::Steer))
        .await
        .unwrap();
    let follow = store
        .submit(request(&id, "queued follow", BusyMode::FollowUp))
        .await
        .unwrap();
    service.disconnect(&key).await.unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    assert!(
        result
            .as_ref()
            .is_err_and(|error| error.is::<super::tools::EnvironmentUnavailable>()),
        "{result:?}"
    );
    let (run, history) = store.load_run(&invocation.run_id).await.unwrap().unwrap();
    assert!(matches!(run.phase, RunPhase::EnvironmentPaused { .. }));
    assert_eq!(server_task.await.unwrap().unwrap().content, "server answer");
    assert_eq!(count.load(Ordering::SeqCst), 2);
    let output = history
        .messages()
        .iter()
        .find_map(|m| {
            if let Message::ToolResult(r) = m {
                Some(&r.content.output)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(output["outcome"], "unknown");
    assert!(
        store
            .claim_execution(id.as_str(), id.as_str())
            .await
            .is_ok()
    );
    assert!(!store.session_busy(id.as_str()).await.unwrap());
    for submission in [correction, follow] {
        assert_eq!(
            store
                .submission_by_id(&submission.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            SubmissionStatus::Unanswered
        );
    }
    let fresh = service
        .attach(DeviceIdentity { device_id }, id.clone(), manifest)
        .await
        .unwrap();
    assert_ne!(fresh.generation, key.generation);
    runner
        .recover(RecoveryTarget::Conversation(id.clone()))
        .await
        .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 2, "reconnect restarted work");
    let answer = runner
        .run(
            id.clone(),
            IncomingMessage::new(
                InterfaceId("telegram:test".into()),
                id.as_str(),
                "discuss without repeating",
            ),
            None,
        )
        .await
        .unwrap();
    assert_eq!(answer.content, "server answer");
    assert_eq!(count.load(Ordering::SeqCst), 3);
    assert!(matches!(
        store
            .load_run(&invocation.run_id)
            .await
            .unwrap()
            .unwrap()
            .0
            .phase,
        RunPhase::EnvironmentPaused { .. }
    ));
}

fn request(id: &SessionId, text: &str, mode: BusyMode) -> SubmissionRequest {
    SubmissionRequest::new(
        id,
        IncomingMessage::new(InterfaceId("web".into()), id.as_str(), text),
        mode,
    )
}

#[tokio::test]
#[ignore = "subprocess fixture"]
async fn remote_crash_worker() {
    use crate::agent::{run_state::ToolExecution, tools::ReplayPolicy};
    let dir = std::path::PathBuf::from(std::env::var("KODRIK_DURABLE_FIXTURE").unwrap());
    let store = SqliteSessionStore::open(dir.join("sessions")).unwrap();
    let config = crash_config(&dir);
    let env = super::local::LocalEnvironment::open(&config, &dir)
        .await
        .unwrap();
    let manifest = env.manifest().unwrap();
    let id = SessionId::new(Uuid::new_v4().to_string());
    let device = Uuid::new_v4();
    let key = EnvironmentKey {
        connection_id: Uuid::new_v4(),
        generation: Uuid::new_v4(),
    };
    let descriptor = manifest
        .tools
        .iter()
        .find(|tool| tool.id == "exec_command")
        .unwrap();
    let call = ToolCall {
        call_id: "crash-effect".into(),
        name: alias(&key, descriptor),
        arguments: json!({"cmd":"printf x >> effects; printf known","workdir":dir}),
    };
    let mut session = Session::new(id.clone());
    session.push(Message::ToolCall(call.clone()));
    let first = store
        .submit(request(&id, "original", BusyMode::Steer))
        .await
        .unwrap();
    let mut cp = Checkpoint::new(&store, &id, &session);
    cp.record.isolated = Some(false);
    cp.record.submission_ids = vec![first.id];
    cp.record.pending_calls = vec![call.clone()];
    cp.save(
        &session,
        RunPhase::ExecutingTool {
            call_id: call.call_id.clone(),
            replay: ReplayPolicy::Never,
        },
        Some(ToolExecution {
            call: call.clone(),
            replay: ReplayPolicy::Never,
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
        tool_id: "exec_command".into(),
        arguments: call.arguments.clone(),
        deadline: chrono::Utc::now() + chrono::Duration::minutes(5),
    };
    std::fs::write(
        dir.join("invocation.json"),
        serde_json::to_vec(&(device, &invocation)).unwrap(),
    )
    .unwrap();
    store
        .remote_binding(device, key.clone(), id.clone(), manifest)
        .await
        .unwrap();
    store
        .remote_intent(device, invocation.clone())
        .await
        .unwrap();
    store
        .submit(request(&id, "saved correction", BusyMode::Steer))
        .await
        .unwrap();
    crate::agent::durable_fixture::boundary("before_dispatch").unwrap();
    assert!(
        store
            .remote_dispatch(device, invocation.clone())
            .await
            .unwrap()
    );
    store
        .remote_receipt(
            device,
            OperationReceipt {
                invocation: invocation.clone(),
                arguments_hash: arguments_hash(&invocation.arguments).unwrap(),
                state: OperationState::Accepted,
                result: None,
            },
        )
        .await
        .unwrap();
    env.attach(device, key, id).await.unwrap();
    let receipt = env.execute(invocation).await.unwrap();
    store.remote_receipt(device, receipt.clone()).await.unwrap();
    crate::agent::durable_fixture::boundary("after_receipt").unwrap();
    session.push(Message::ToolResult(crate::agent::message::ToolResult {
        call_id: call.call_id.clone(),
        content: receipt.result.clone().unwrap(),
    }));
    cp.record.pending_calls.clear();
    cp.save(
        &session,
        RunPhase::NeedModel,
        Some(ToolExecution {
            call,
            replay: ReplayPolicy::Never,
            result: receipt.result,
        }),
    )
    .await
    .unwrap();
    crate::agent::durable_fixture::boundary("after_result").unwrap();
    store
        .yield_environment_run(&cp.record.id, &session)
        .await
        .unwrap();
    crate::agent::durable_fixture::boundary("after_pause").unwrap();
    panic!("missing crash gate");
}

fn crash_config(dir: &std::path::Path) -> super::config::RemoteConfig {
    super::config::RemoteConfig {
        server: "https://example.invalid/".parse().unwrap(),
        credential_file: dir.join("device"),
        label: "fixture".into(),
        allow_shell: true,
        skill_roots: vec![],
        mcp: Default::default(),
        optional_mcp: vec![],
    }
}

#[tokio::test]
async fn pause_is_atomic_preserves_known_batch_results_and_cross_interface_queue() {
    use crate::agent::{message::ToolResult, run_state::ToolExecution, tools::ReplayPolicy};
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
    let id = SessionId::new(Uuid::new_v4().to_string());
    let other = SessionId::new("telegram:other-interface");
    let unrelated = SessionId::new("unrelated");
    let mut session = Session::new(id.clone());
    let known = ToolCall {
        call_id: "known".into(),
        name: "server_safe".into(),
        arguments: json!({}),
    };
    let lost = ToolCall {
        call_id: "lost".into(),
        name: "local_lost".into(),
        arguments: json!({}),
    };
    session.push(Message::ToolCall(known.clone()));
    session.push(Message::ToolCall(lost.clone()));
    store.save(&session).await.unwrap();
    store.select(&other, &id).await.unwrap();
    let correction = store
        .submit(request(
            &other,
            "cross-interface correction",
            BusyMode::Steer,
        ))
        .await
        .unwrap();
    let mut cp = Checkpoint::new(&store, &id, &session);
    cp.record.pending_calls = vec![known.clone(), lost.clone()];
    cp.save(
        &session,
        RunPhase::ExecutingTool {
            call_id: known.call_id.clone(),
            replay: ReplayPolicy::Safe,
        },
        Some(ToolExecution {
            call: known.clone(),
            replay: ReplayPolicy::Safe,
            result: None,
        }),
    )
    .await
    .unwrap();
    session.push(Message::ToolResult(ToolResult {
        call_id: known.call_id.clone(),
        content: json!({"known":42}).into(),
    }));
    cp.save(
        &session,
        RunPhase::NeedTool {
            call_id: lost.call_id.clone(),
        },
        Some(ToolExecution {
            call: known.clone(),
            replay: ReplayPolicy::Safe,
            result: Some(json!({"known":42}).into()),
        }),
    )
    .await
    .unwrap();
    let untouched = store
        .submit(request(&unrelated, "unrelated queued", BusyMode::Steer))
        .await
        .unwrap();
    let before =
        serde_json::to_value(store.load_run(&cp.record.id).await.unwrap().unwrap()).unwrap();
    store.with_connection(|db|{db.execute_batch("CREATE TRIGGER fail_pause BEFORE UPDATE ON runs WHEN NEW.terminal=1 BEGIN SELECT RAISE(ABORT,'pause storage fault'); END;")?;Ok(())}).await.unwrap();
    assert!(
        store
            .yield_environment_run(&cp.record.id, &session)
            .await
            .is_err()
    );
    assert_eq!(
        serde_json::to_value(store.load_run(&cp.record.id).await.unwrap().unwrap()).unwrap(),
        before
    );
    assert_eq!(
        store
            .submission_by_id(&correction.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        SubmissionStatus::Queued
    );
    assert!(
        store
            .tool_execution(&cp.record.id, "lost")
            .await
            .unwrap()
            .is_none()
    );
    store
        .with_connection(|db| {
            db.execute_batch("DROP TRIGGER fail_pause")?;
            Ok(())
        })
        .await
        .unwrap();
    store
        .yield_environment_run(&cp.record.id, &session)
        .await
        .unwrap();
    let (_, saved) = store.load_run(&cp.record.id).await.unwrap().unwrap();
    assert_eq!(
        store
            .submission_by_id(&correction.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        SubmissionStatus::Unanswered,
        "shared session correction remained runnable"
    );
    assert_eq!(
        store
            .submission_by_id(&untouched.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        SubmissionStatus::Queued
    );
    assert!(
        saved
            .messages()
            .iter()
            .any(|m| matches!(m,Message::User(u) if u.content=="cross-interface correction"))
    );
    let execution = store
        .tool_execution(&cp.record.id, "known")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        execution.replay,
        ReplayPolicy::Safe,
        "pause changed server replay provenance"
    );
    assert_eq!(execution.result.unwrap().output, json!({"known":42}));
    for call in ["known", "lost"] {
        assert_eq!(
            saved
                .messages()
                .iter()
                .filter(|m| matches!(m,Message::ToolResult(r) if r.call_id==call))
                .count(),
            1
        );
    }
    store
        .yield_environment_run(&cp.record.id, &session)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(store.load(&id).await.unwrap().unwrap()).unwrap(),
        serde_json::to_value(saved).unwrap()
    );
    drop(store);
    let reopened = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
    let (record, _) = reopened.load_run(&cp.record.id).await.unwrap().unwrap();
    assert!(matches!(record.phase, RunPhase::EnvironmentPaused { .. }));
    assert!(record.phase.terminal());
    assert!(
        reopened
            .unfinished_runs()
            .await
            .unwrap()
            .iter()
            .all(|run| run.id != cp.record.id)
    );
}

#[test]
fn old_run_phases_keep_serde_and_terminal_semantics() {
    for (value, terminal) in [
        (json!("NeedModel"), false),
        (json!("Completed"), true),
        (json!("Cancelled"), true),
        (json!({"Parked":{"reason":"legacy"}}), false),
        (json!({"Failed":{"reason":"legacy"}}), true),
        (
            json!({"ExecutingTool":{"call_id":"old","replay":"Never"}}),
            false,
        ),
        (json!({"EnvironmentPaused":{"reason":"lease"}}), true),
    ] {
        let phase: RunPhase = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(phase.terminal(), terminal);
        assert_eq!(serde_json::to_value(phase).unwrap(), value);
    }
}

#[tokio::test]
async fn mixed_batch_pause_keeps_interrupted_server_effect_unknown() {
    use crate::agent::{
        run_state::ToolExecution,
        tools::{ExecCommandTool, ReplayPolicy},
    };
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
    let id = SessionId::new(Uuid::new_v4().to_string());
    let key = EnvironmentKey {
        connection_id: Uuid::new_v4(),
        generation: Uuid::new_v4(),
    };
    let descriptor = ToolDescriptor {
        id: "exec_command".into(),
        kind: ToolKind::Exec,
        description: "laptop shell".into(),
        parameters: json!({"type":"object"}),
        deferred: false,
    };
    let server = ToolCall {
        call_id: "server-effect".into(),
        name: "exec_command".into(),
        arguments: json!({"cmd":"printf x >> effects", "workdir":dir.path()}),
    };
    let local = ToolCall {
        call_id: "laptop-not-started".into(),
        name: alias(&key, &descriptor),
        arguments: json!({"cmd":"must not execute"}),
    };
    let mut session = Session::new(id.clone());
    session.push(Message::ToolCall(server.clone()));
    session.push(Message::ToolCall(local.clone()));
    store.save(&session).await.unwrap();
    let mut cp = Checkpoint::new(&store, &id, &session);
    cp.record.isolated = Some(false);
    cp.record.pending_calls = vec![server.clone(), local];
    cp.save(
        &session,
        RunPhase::ExecutingTool {
            call_id: server.call_id.clone(),
            replay: ReplayPolicy::Never,
        },
        Some(ToolExecution {
            call: server.clone(),
            replay: ReplayPolicy::Never,
            result: None,
        }),
    )
    .await
    .unwrap();
    store
        .remote_binding(
            Uuid::new_v4(),
            key,
            id.clone(),
            EnvironmentManifest {
                label: "fixture".into(),
                project_root: dir.path().display().to_string(),
                tools: vec![descriptor],
                skills: vec![],
                project_instructions: None,
                warnings: vec![],
            },
        )
        .await
        .unwrap();
    ExecCommandTool.execute(server.arguments).await.unwrap();
    // Simulate losing the result after a real server effect, before any laptop dispatch.
    let calls = Arc::new(AtomicUsize::new(0));
    let runner = AgentRunner::new(
        Arc::new(Agent::new(
            Arc::new(Model(calls.clone())),
            ToolRegistry::default(),
            ContextBuilder::new("fixture"),
        )),
        Arc::new(store.clone()),
    );
    runner
        .recover(RecoveryTarget::Conversation(id))
        .await
        .unwrap();
    let (run, history) = store.load_run(&cp.record.id).await.unwrap().unwrap();
    assert!(matches!(run.phase, RunPhase::EnvironmentPaused { .. }));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(dir.path().join("effects")).unwrap(), b"x");
    for (call, outcome) in [
        ("server-effect", "unknown"),
        ("laptop-not-started", "unavailable"),
    ] {
        let result = history
            .messages()
            .iter()
            .find_map(|message| match message {
                Message::ToolResult(result) if result.call_id == call => {
                    Some(&result.content.output)
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(result["outcome"], outcome, "wrong evidence for {call}");
    }
}

#[tokio::test]
async fn remote_process_crash_matrix_never_replays_and_preserves_known_results() {
    struct Process(std::process::Child);
    impl Drop for Process {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    for (point, effects, outcome) in [
        ("before_dispatch", 0, "unavailable"),
        ("local_accepted", 0, "unknown"),
        ("local_effect", 1, "unknown"),
        ("after_receipt", 1, "known"),
        ("after_result", 1, "known"),
        ("after_pause", 1, "known"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut process = Process(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "remote::recovery_tests::remote_crash_worker",
                    "--ignored",
                    "--nocapture",
                ])
                .env("KODRIK_DURABLE_FIXTURE", dir.path())
                .env("KODRIK_DURABLE_POINT", point)
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            while !dir.path().join("ready").exists() {
                assert!(
                    process.0.try_wait().unwrap().is_none(),
                    "worker exited at {point}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("crash gate deadline");
        process.0.kill().unwrap();
        process.0.wait().unwrap();
        let (device, invocation): (Uuid, Invocation) =
            serde_json::from_slice(&std::fs::read(dir.path().join("invocation.json")).unwrap())
                .unwrap();
        let store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let runner = AgentRunner::new(
            Arc::new(Agent::new(
                Arc::new(Model(calls.clone())),
                ToolRegistry::default(),
                ContextBuilder::new("fixture"),
            )),
            Arc::new(store.clone()),
        );
        runner
            .recover(RecoveryTarget::Conversation(invocation.session_id.clone()))
            .await
            .unwrap();
        let (run, session) = store.load_run(&invocation.run_id).await.unwrap().unwrap();
        let evidence = store
            .remote_operation(&invocation.run_id, &invocation.call_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !matches!(
                evidence.state,
                OperationState::Accepted | OperationState::Running
            ),
            "crash left live operation evidence at {point}"
        );
        assert!(
            matches!(run.phase, RunPhase::EnvironmentPaused { .. }),
            "{point}: {:?}",
            run.phase
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(
            !store
                .session_busy(invocation.session_id.as_str())
                .await
                .unwrap()
        );
        let output = session
            .messages()
            .iter()
            .find_map(|m| {
                if let Message::ToolResult(r) = m {
                    Some(&r.content.output)
                } else {
                    None
                }
            })
            .unwrap();
        if outcome == "known" {
            assert_eq!(output["stdout"], "known");
        } else {
            assert_eq!(output["outcome"], outcome, "{point}");
        }
        assert_eq!(
            std::fs::read(dir.path().join("effects"))
                .unwrap_or_default()
                .len(),
            effects,
            "{point}"
        );
        // Direct journal retry characterizes laptop dedup; server recovery never sends this call.
        if point != "before_dispatch" {
            let env = super::local::LocalEnvironment::open(&crash_config(dir.path()), dir.path())
                .await
                .unwrap();
            env.attach(
                device,
                invocation.environment.clone(),
                invocation.session_id.clone(),
            )
            .await
            .unwrap();
            let _ = env.execute(invocation.clone()).await.unwrap();
            env.close().await.unwrap();
            assert_eq!(
                std::fs::read(dir.path().join("effects"))
                    .unwrap_or_default()
                    .len(),
                effects,
                "journal retry repeated {point}"
            );
        }
        assert!(
            session
                .messages()
                .iter()
                .any(|m| matches!(m,Message::User(u) if u.content=="saved correction"))
        );
    }
}

#[tokio::test]
async fn late_evidence_reaches_next_context_without_rewriting_history() {
    use crate::agent::{message::ToolResult, run_state::ToolExecution, tools::ReplayPolicy};
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
    let id = SessionId::new(Uuid::new_v4().to_string());
    let mut session = Session::new(id.clone());
    session.initialize_system_prompt("fixture");
    let call = ToolCall {
        call_id: "late".into(),
        name: "local_exec".into(),
        arguments: json!({"cmd":"effect"}),
    };
    session.push(Message::ToolCall(call.clone()));
    let mut cp = Checkpoint::new(&store, &id, &session);
    cp.record.pending_calls = vec![call.clone()];
    cp.save(
        &session,
        RunPhase::ExecutingTool {
            call_id: call.call_id.clone(),
            replay: ReplayPolicy::Never,
        },
        Some(ToolExecution {
            call: call.clone(),
            replay: ReplayPolicy::Never,
            result: None,
        }),
    )
    .await
    .unwrap();
    let device = Uuid::new_v4();
    let invocation = Invocation {
        environment: EnvironmentKey {
            connection_id: Uuid::new_v4(),
            generation: Uuid::new_v4(),
        },
        session_id: id.clone(),
        run_id: cp.record.id.clone(),
        call_id: call.call_id,
        tool_id: "exec_command".into(),
        arguments: call.arguments,
        deadline: chrono::Utc::now() + chrono::Duration::minutes(1),
    };
    store
        .remote_binding(
            device,
            invocation.environment.clone(),
            id.clone(),
            EnvironmentManifest {
                label: "fixture".into(),
                project_root: "/laptop".into(),
                tools: vec![ToolDescriptor {
                    id: "exec_command".into(),
                    kind: ToolKind::Exec,
                    description: "shell".into(),
                    parameters: json!({"type":"object"}),
                    deferred: false,
                }],
                skills: vec![],
                project_instructions: None,
                warnings: vec![],
            },
        )
        .await
        .unwrap();
    store
        .remote_intent(device, invocation.clone())
        .await
        .unwrap();
    store
        .remote_dispatch(device, invocation.clone())
        .await
        .unwrap();
    store
        .yield_environment_run(&cp.record.id, &session)
        .await
        .unwrap();
    let (_, paused) = store.load_run(&cp.record.id).await.unwrap().unwrap();
    assert!(paused.messages().iter().any(|m|matches!(m,Message::ToolResult(ToolResult{content,..}) if content.output["outcome"]=="unknown")));
    let frozen = serde_json::to_value(&paused).unwrap();
    store
        .remote_receipt(
            device,
            OperationReceipt {
                arguments_hash: arguments_hash(&invocation.arguments).unwrap(),
                invocation,
                state: OperationState::Completed,
                result: Some(json!({"stdout":"late private result"}).into()),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(store.load(&id).await.unwrap().unwrap()).unwrap(),
        frozen
    );
    struct Reconciled;
    #[async_trait::async_trait]
    impl LlmProvider for Reconciled {
        async fn generate(
            &self,
            context: &LlmContext<'_>,
            _: &[&dyn Tool],
            _: Option<&tokio::sync::mpsc::Sender<crate::agent::AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            assert!(
                context
                    .instructions
                    .contains("Remote operation reconciliation"),
                "late evidence absent from next context"
            );
            assert!(context.instructions.contains("completed"));
            assert!(!context.instructions.contains("late private result"));
            Ok((
                vec![Message::Assistant(AssistantMessage {
                    content: "reconciled".into(),
                })],
                None,
            ))
        }
    }
    let runner = AgentRunner::new(
        Arc::new(Agent::new(
            Arc::new(Reconciled),
            ToolRegistry::default(),
            ContextBuilder::new("fixture"),
        )),
        Arc::new(store.clone()),
    );
    runner
        .run(
            id.clone(),
            IncomingMessage::new(InterfaceId("web".into()), id.as_str(), "inspect evidence"),
            None,
        )
        .await
        .unwrap();
    let view = store.conversation_view(id.as_str()).await.unwrap();
    assert!(
        view.run
            .unwrap()
            .remote_operations
            .iter()
            .any(|row| row["call_id"] == "late" && row["state"] == "completed"),
        "new run snapshot hid older reconciliation"
    );
}

#[tokio::test]
async fn recovery_of_old_local_generation_pauses_without_model_or_queued_self_start() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().into()).unwrap();
    let id = SessionId::new(Uuid::new_v4().to_string());
    let mut session = Session::new(id.clone());
    session.initialize_system_prompt("test");
    store.save(&session).await.unwrap();
    let first = store
        .submit(request(&id, "original", BusyMode::Steer))
        .await
        .unwrap();
    let key = EnvironmentKey {
        connection_id: Uuid::new_v4(),
        generation: Uuid::new_v4(),
    };
    let tool = ToolDescriptor {
        id: "exec_command".into(),
        kind: ToolKind::Exec,
        description: "laptop shell".into(),
        parameters: json!({"type":"object"}),
        deferred: false,
    };
    let call = ToolCall {
        call_id: "not-dispatched".into(),
        name: alias(&key, &tool),
        arguments: json!({"cmd":"effect","workdir":"/laptop"}),
    };
    store
        .remote_binding(
            Uuid::new_v4(),
            key,
            id.clone(),
            EnvironmentManifest {
                label: "Laptop".into(),
                project_root: "/laptop".into(),
                tools: vec![tool],
                skills: vec![],
                project_instructions: None,
                warnings: vec![],
            },
        )
        .await
        .unwrap();
    session.push(Message::User(crate::agent::UserMessage {
        content: "original".into(),
        files: vec![
            crate::agent::FileAttachment::save(
                dir.path(),
                "missing.txt",
                Some("text/plain"),
                b"lost attachment",
            )
            .unwrap(),
        ],
        images: vec![],
    }));
    session.push(Message::ToolCall(call.clone()));
    let mut cp = Checkpoint::new(&store, &id, &session);
    cp.record.isolated = Some(false);
    cp.record.submission_ids = vec![first.id.clone()];
    cp.record.pending_calls = vec![call];
    cp.save(
        &session,
        RunPhase::NeedTool {
            call_id: "not-dispatched".into(),
        },
        None,
    )
    .await
    .unwrap();
    if let Message::User(user) = &session.messages()[0] {
        std::fs::remove_file(&user.files[0].path).unwrap();
    }
    let correction = store
        .submit(request(&id, "accepted correction", BusyMode::Steer))
        .await
        .unwrap();
    let follow = store
        .submit(request(&id, "accepted follow-up", BusyMode::FollowUp))
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let runner = AgentRunner::new(
        Arc::new(Agent::new(
            Arc::new(Model(calls.clone())),
            ToolRegistry::default(),
            ContextBuilder::new("test"),
        )),
        Arc::new(store.clone()),
    );
    runner
        .recover(RecoveryTarget::Conversation(id.clone()))
        .await
        .unwrap();
    let (run, history) = store.load_run(&cp.record.id).await.unwrap().unwrap();
    assert!(
        serde_json::to_value(&run.phase)
            .unwrap()
            .get("EnvironmentPaused")
            .is_some(),
        "lost local work was not paused: {:?}",
        run.phase
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "recovery silently continued the old attempt"
    );
    assert!(run.phase.terminal());
    assert!(!store.session_busy(id.as_str()).await.unwrap());
    assert!(store.next_submission(&id).await.unwrap().is_none());
    for submission in [first, correction, follow] {
        assert_eq!(
            store
                .submission_by_id(&submission.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            SubmissionStatus::Unanswered
        );
    }
    assert!(
        history
            .messages()
            .iter()
            .any(|m| matches!(m,Message::User(u) if u.content=="accepted correction"))
    );
    assert!(
        history
            .messages()
            .iter()
            .any(|m| matches!(m,Message::User(u) if u.content=="accepted follow-up"))
    );
    let output = history
        .messages()
        .iter()
        .find_map(|m| {
            if let Message::ToolResult(r) = m {
                Some(&r.content.output)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(output["outcome"], "unavailable");
    runner
        .recover(RecoveryTarget::Conversation(id.clone()))
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    store
        .submit(request(&id, "new explicit request", BusyMode::RejectIfBusy))
        .await
        .unwrap();
    assert!(store.next_submission(&id).await.unwrap().is_some());
}
