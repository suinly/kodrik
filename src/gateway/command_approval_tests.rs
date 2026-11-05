async fn seed_command_approval(
    store: &SqliteSessionStore,
    conversation: &str,
    cmd: &str,
) -> crate::agent::command_audit::CommandApproval {
    seed_approval_from(store, conversation, conversation, cmd).await
}

async fn seed_approval_from(
    store: &SqliteSessionStore,
    conversation: &str,
    channel: &str,
    cmd: &str,
) -> crate::agent::command_audit::CommandApproval {
    use crate::agent::{
        message::ToolCall,
        run_state::{Checkpoint, RunPhase},
    };
    let mut session = Session::new(SessionId::new(conversation));
    let call = ToolCall {
        call_id: "shell-call".into(),
        name: "exec_command".into(),
        arguments: serde_json::json!({"cmd":cmd, "workdir":"/workspace"}),
    };
    session.push(Message::ToolCall(call.clone()));
    if conversation != channel {
        let mut history = Checkpoint::new(store, session.id(), &session);
        history.record.conversation_id = channel.into();
        history.record.routing_from = Some(store.active_id(&SessionId::new(channel)).await.unwrap().as_str().to_owned());
        history.save(&session, RunPhase::Completed, None).await.unwrap();
    }
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
    if super::personal_session(&session) {
        store
            .set_command_audit_mode(
                session.id().as_str(),
                crate::agent::command_audit::CommandAuditMode::Review,
            )
            .await
            .unwrap();
    }
    store
        .record_command_audits(&checkpoint.record, &[(call, "destructive".into())])
        .await
        .unwrap();
    store
        .pending_command_approvals(conversation)
        .await
        .unwrap()
        .remove(0)
}

async fn telegram_environment_fixture() -> (
    tempfile::TempDir,
    GatewayRunner,
    SqliteSessionStore,
    Arc<Mutex<Vec<(String, String)>>>,
    crate::web::auth::devices::DeviceIdentity,
    SessionId,
) {
    use crate::web::auth::{AuthStore, Identity, devices::PairingPoll};
    let (dir, gateway, _, sent, _, _) = setup_with_model(vec![], Arc::new(NoModel)).await;
    let config: crate::config::Config = toml::from_str("[owner]\ninterface='test'\nuser_id=42\n[gateway.test]\ntype='telegram'\ntoken='test'\n[companion]\nlearning_enabled=false\nproactive_enabled=false\n[provider.primary]\ntype='openai'\nmodel='test'\napi_key='test'").unwrap();
    let app = crate::bootstrap::build_app_at(&config, &dir.path().join("owner"), false)
        .await
        .unwrap();
    let auth = AuthStore::new(dir.path());
    app.agent_runner.enable_environments(auth.clone()).unwrap();
    let gateway = gateway
        .with_telegram_spaces(&config, dir.path(), app)
        .await
        .unwrap();
    let runner = gateway
        .runner_for(&SessionId::new("telegram:test:42"))
        .unwrap();
    let store = runner.sqlite_store().unwrap();
    let session = store
        .new_session(&SessionId::new("telegram:test:42"))
        .await
        .unwrap();
    let now = chrono::Utc::now().timestamp();
    let pairing = auth.start_device_pairing("Laptop", now).unwrap();
    auth.approve_device_pairing(pairing.request_id, &pairing.code, &Identity::Owner, now)
        .unwrap();
    let PairingPoll::Authorized { device_id, .. } = auth
        .poll_device_pairing(pairing.request_id, &pairing.polling_secret, now)
        .unwrap()
    else {
        panic!()
    };
    (
        dir,
        gateway,
        store,
        sent,
        crate::web::auth::devices::DeviceIdentity { device_id },
        session.id().clone(),
    )
}

fn telegram_environment_manifest(label: &str) -> crate::remote::protocol::EnvironmentManifest {
    crate::remote::protocol::EnvironmentManifest {
        label: label.into(),
        project_root: "/private/project".into(),
        tools: vec![],
        skills: vec![],
        project_instructions: None,
        warnings: vec![],
    }
}

#[tokio::test]
async fn telegram_environment_status_uses_real_active_session_and_deduplicates_heartbeats() {
    let (_dir, gateway, store, sent, device, session) = telegram_environment_fixture().await;
    let runner = gateway
        .runner_for(&SessionId::new("telegram:test:42"))
        .unwrap();
    let service = runner.environments().unwrap();
    let key = service
        .attach(
            device.clone(),
            session.clone(),
            telegram_environment_manifest("Laptop"),
        )
        .await
        .unwrap();
    let mut shown = Default::default();
    gateway.show_environment_status(&mut shown).await.unwrap();
    service.heartbeat(&key).await.unwrap();
    gateway.show_environment_status(&mut shown).await.unwrap();
    {
        let sent = sent.lock().unwrap();
        assert_eq!(
            sent.len(),
            1,
            "heartbeat must not create another notification"
        );
        assert_eq!(sent[0].0, "42");
        assert!(sent[0].1.contains(session.as_str()));
        assert!(sent[0].1.contains("Laptop"));
    }
    let next = store
        .new_session(&SessionId::new("telegram:test:42"))
        .await
        .unwrap();
    gateway.show_environment_status(&mut shown).await.unwrap();
    assert_eq!(
        sent.lock().unwrap().len(),
        1,
        "old environment must not migrate to the new session"
    );
    service
        .attach(
            device,
            next.id().clone(),
            telegram_environment_manifest("New laptop"),
        )
        .await
        .unwrap();
    gateway.show_environment_status(&mut shown).await.unwrap();
    let sent = sent.lock().unwrap();
    assert_eq!(sent.len(), 2);
    assert!(sent[1].1.contains(next.id().as_str()));
    assert!(!sent[1].1.contains(session.as_str()));
}

#[tokio::test]
async fn telegram_environment_status_disconnect_and_reconnect_are_new_states_not_authority() {
    let (_dir, gateway, _, sent, device, session) = telegram_environment_fixture().await;
    let runner = gateway
        .runner_for(&SessionId::new("telegram:test:42"))
        .unwrap();
    let service = runner.environments().unwrap();
    let key = service
        .attach(
            device.clone(),
            session.clone(),
            telegram_environment_manifest("Laptop"),
        )
        .await
        .unwrap();
    let mut shown = Default::default();
    gateway.show_environment_status(&mut shown).await.unwrap();
    service.revoke(&key, "device closed").await.unwrap();
    gateway.show_environment_status(&mut shown).await.unwrap();
    gateway.show_environment_status(&mut shown).await.unwrap();
    {
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert!(sent[1].1.contains("Отключено"));
        assert!(sent[1].1.contains("недоступны"));
        assert!(sent[1].1.contains("device closed"));
        assert!(sent[1].1.contains("/private/project"));
        assert!(sent[1].1.contains(session.as_str()));
    }
    assert!(
        service.heartbeat(&key).await.is_err(),
        "status must not renew a revoked lease"
    );
    let fresh = service
        .attach(device, session, telegram_environment_manifest("Laptop"))
        .await
        .unwrap();
    assert_ne!(fresh, key);
    gateway.show_environment_status(&mut shown).await.unwrap();
    assert_eq!(
        sent.lock().unwrap().len(),
        3,
        "fresh generation must notify again"
    );
}

#[tokio::test]
async fn telegram_environment_status_never_notifies_foreign_group_guest_or_unconfigured_owner() {
    let (_dir, gateway, store, sent, device, session) = telegram_environment_fixture().await;
    let owner = gateway
        .runner_for(&SessionId::new("telegram:test:42"))
        .unwrap()
        .clone();
    owner
        .environments()
        .unwrap()
        .attach(
            device,
            session,
            telegram_environment_manifest("owner secret"),
        )
        .await
        .unwrap();
    let mut gateway = gateway.with_chat_runners(HashMap::from([
        (SessionId::new("telegram:test:7"), owner.clone()),
        (SessionId::new("telegram:test:-42"), owner.clone()),
        (SessionId::new("telegram:test:42:guest"), owner.clone()),
        (SessionId::new("web:42"), owner.clone()),
    ]));
    gateway
        .show_environment_status(&mut Default::default())
        .await
        .unwrap();
    assert!(sent.lock().unwrap().is_empty());
    gateway
        .chat_runners
        .as_mut()
        .unwrap()
        .insert(SessionId::new("telegram:test:42"), owner);
    let mut spaces = gateway.telegram_spaces.as_ref().unwrap().config.clone();
    spaces.owner = None;
    let original = gateway.telegram_spaces.as_ref().unwrap();
    gateway.telegram_spaces = Some(Arc::new(super::super::telegram_spaces::TelegramSpaces {
        config: spaces,
        root: original.root.clone(),
        users: original.users.clone(),
    }));
    gateway
        .show_environment_status(&mut Default::default())
        .await
        .unwrap();
    assert!(sent.lock().unwrap().is_empty());
    assert!(!store.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn telegram_environment_status_escapes_and_bounds_untrusted_label() {
    let (_dir, gateway, _, sent, device, session) = telegram_environment_fixture().await;
    let label = format!("<b>fake</b>\n\t\u{1b}\u{202e}{}", "🙂".repeat(300));
    let mut manifest = telegram_environment_manifest(&label);
    manifest.project_root = format!("/project\n\u{202e}{}", "🙂".repeat(300));
    let service = gateway
        .runner_for(&SessionId::new("telegram:test:42"))
        .unwrap()
        .environments()
        .unwrap();
    let key = service
        .attach(device, session.clone(), manifest)
        .await
        .unwrap();
    service
        .revoke(&key, &format!("closed\n\u{202e}{}", "🙂".repeat(300)))
        .await
        .unwrap();
    gateway
        .show_environment_status(&mut Default::default())
        .await
        .unwrap();
    let sent = sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    let text = &sent[0].1;
    assert!(text.contains(session.as_str()));
    assert!(!text.contains('\u{1b}') && !text.contains('\u{202e}') && !text.contains('\t'));
    assert!(text.contains("/project\\n\\u{202e}"));
    assert!(text.contains("closed\\n\\u{202e}"));
    assert!(text.encode_utf16().count() < 2000);
    assert!(text.contains('…'));
}

#[tokio::test]
async fn telegram_environment_status_failure_is_not_retried_and_recipient_denial_is_respected() {
    struct FailedStatus {
        allowed: bool,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl Interface for FailedStatus {
        fn id(&self) -> InterfaceId {
            InterfaceId("telegram:test".into())
        }
        fn allows_recipient(&self, chat: &str) -> bool {
            self.allowed && chat == "42"
        }
        async fn run(&self, _: mpsc::Sender<Event>) -> anyhow::Result<()> {
            Ok(())
        }
        async fn send(&self, _: &str, _: &str) -> anyhow::Result<MessageId> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            anyhow::bail!("unknown delivery outcome")
        }
    }
    let (_dir, mut gateway, store, _, device, session) = telegram_environment_fixture().await;
    let service = gateway
        .runner_for(&SessionId::new("telegram:test:42"))
        .unwrap()
        .environments()
        .unwrap()
        .clone();
    let key = service
        .attach(
            device,
            session.clone(),
            telegram_environment_manifest("Laptop"),
        )
        .await
        .unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    gateway.interfaces = vec![Arc::new(FailedStatus {
        allowed: false,
        calls: calls.clone(),
    })];
    let mut shown = Default::default();
    gateway.show_environment_status(&mut shown).await.unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    gateway.interfaces = vec![Arc::new(FailedStatus {
        allowed: true,
        calls: calls.clone(),
    })];
    gateway.show_environment_status(&mut shown).await.unwrap();
    service.heartbeat(&key).await.unwrap();
    gateway.show_environment_status(&mut shown).await.unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(
        store
            .pending_command_approvals(session.as_str())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn telegram_approvals_ignore_web_and_tui_without_telegram_history() {
    let (_dir, gateway, store, _, menus, _) = setup_with_model(vec![], Arc::new(NoModel)).await;
    for session in ["web-only", "cli:interactive:1"] {
        seed_command_approval(&store, session, "rm private.txt").await;
    }
    let gateway = gateway.clone().with_chat_runners(HashMap::from([(
        SessionId::new("telegram:test:42"), gateway.agent_runner.clone(),
    )]));
    gateway.show_command_approvals(&mut Default::default()).await.unwrap();
    assert!(menus.lock().unwrap().is_empty());
}

#[tokio::test]
async fn telegram_approvals_wait_until_the_source_is_not_visible_in_web() {
    let (_dir, gateway, store, _, menus, _) = setup_with_model(vec![], Arc::new(NoModel)).await;
    let approval = seed_approval_from(&store, "web-source", "telegram:test:42", "rm old.txt").await;
    let gateway = gateway.clone().with_chat_runners(HashMap::from([(
        SessionId::new("telegram:test:42"), gateway.agent_runner.clone(),
    )]));
    store.with_connection(|db| {
        db.execute("INSERT INTO web_push_presence VALUES('browser','tab','web-source',?1)", [chrono::Utc::now().timestamp()])?;
        Ok(())
    }).await.unwrap();
    let mut shown = Default::default();
    gateway.show_command_approvals(&mut shown).await.unwrap();
    assert!(menus.lock().unwrap().is_empty(), "visible Web Chat must suppress Telegram");
    assert!(shown.is_empty(), "suppression must not mark the request sent");
    store.with_connection(|db| {
        db.execute("UPDATE web_push_presence SET updated_at=?1", [chrono::Utc::now().timestamp()-46])?;
        Ok(())
    }).await.unwrap();
    gateway.show_command_approvals(&mut shown).await.unwrap();
    assert_eq!(menus.lock().unwrap().len(), 1);
    assert!(menus.lock().unwrap()[0].1.contains(&approval.tool_name));
}

#[tokio::test]
async fn refused_telegram_deletions_do_not_starve_other_resolved_requests() {
    struct RefusedDeletes {
        sent: std::sync::atomic::AtomicUsize,
        attempts: Arc<Mutex<Vec<String>>>,
    }
    #[async_trait::async_trait]
    impl Interface for RefusedDeletes {
        fn id(&self) -> InterfaceId { InterfaceId("telegram:test".into()) }
        fn allows_recipient(&self, chat: &str) -> bool { chat == "42" }
        async fn run(&self, _: mpsc::Sender<Event>) -> anyhow::Result<()> { Ok(()) }
        async fn send(&self, _: &str, _: &str) -> anyhow::Result<MessageId> {
            Ok(MessageId(self.sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst).to_string()))
        }
        async fn delete(&self, chat: &str, id: &MessageId) -> anyhow::Result<()> {
            assert_eq!(chat, "42");
            self.attempts.lock().unwrap().push(id.0.clone());
            anyhow::bail!("temporary refusal")
        }
    }
    let (_dir, mut gateway, store, _, _, _) = setup_with_model(vec![], Arc::new(NoModel)).await;
    let attempts = Arc::new(Mutex::new(vec![]));
    gateway.interfaces = vec![Arc::new(RefusedDeletes { sent: Default::default(), attempts: attempts.clone() })];
    let gateway = gateway.clone().with_chat_runners(HashMap::from([(
        SessionId::new("telegram:test:42"), gateway.agent_runner.clone(),
    )]));
    let mut approvals = vec![];
    for i in 0..6 {
        let session = format!("background-{i}");
        let approval = seed_approval_from(&store, &session, "telegram:test:42", "rm old.txt").await;
        approvals.push((session, approval.id));
    }
    let mut shown = Default::default();
    gateway.show_command_approvals(&mut shown).await.unwrap();
    gateway.show_command_approvals(&mut shown).await.unwrap();
    for (session, id) in approvals {
        store.decide_command_approval(&session, &id, true).await.unwrap();
    }
    gateway.show_command_approvals(&mut shown).await.unwrap();
    gateway.show_command_approvals(&mut shown).await.unwrap();
    let attempted: std::collections::HashSet<_> = attempts.lock().unwrap().iter().cloned().collect();
    assert_eq!(attempted.len(), 6, "every resolved request gets a deletion attempt");
    assert_eq!(shown.len(), 6, "failed deletions stay tracked for retry");
}

#[tokio::test]
async fn telegram_removes_the_sent_request_after_a_web_decision() {
    let (_dir, gateway, store, _, menus, _) = setup_with_model(vec![], Arc::new(NoModel)).await;
    let approval = seed_approval_from(&store, "web-source", "telegram:test:42", "rm old.txt").await;
    let gateway = gateway.clone().with_chat_runners(HashMap::from([(
        SessionId::new("telegram:test:42"), gateway.agent_runner.clone(),
    )]));
    let mut shown = Default::default();
    gateway.show_command_approvals(&mut shown).await.unwrap();
    assert_eq!(menus.lock().unwrap().len(), 1);
    store.decide_command_approval("web-source", &approval.id, true).await.unwrap();
    gateway.show_command_approvals(&mut shown).await.unwrap();
    gateway.show_command_approvals(&mut shown).await.unwrap();
    assert!(shown.is_empty());
    let menus = menus.lock().unwrap();
    assert_eq!(menus.len(), 2, "delete exactly once after the durable decision");
    assert_eq!(menus[1].1, "deleted:42:1");
}

#[tokio::test]
async fn telegram_background_approvals_scan_personal_store_once_and_resolve_old_session() {
    let (_dir, gateway, store, _, menus, _) = setup_with_model(vec![], Arc::new(NoModel)).await;
    let old = seed_approval_from(&store, "old-personal-session", "telegram:test:42", "rm old.txt").await;
    seed_command_approval(&store, "telegram:test:-100", "rm group.txt").await;
    seed_command_approval(&store, "telegram:test:42:guest", "rm guest.txt").await;
    seed_command_approval(&store, "companion:private", "rm internal.txt").await;
    let gateway = gateway.clone().with_chat_runners(HashMap::from([
        (
            SessionId::new("telegram:test:42"),
            gateway.agent_runner.clone(),
        ),
        (
            SessionId::new("telegram:test:7"),
            gateway.agent_runner.clone(),
        ),
    ]));
    let mut shown = Default::default();
    gateway.show_command_approvals(&mut shown).await.unwrap();
    gateway.show_command_approvals(&mut shown).await.unwrap();
    {
        let menus = menus.lock().unwrap();
        assert_eq!(
            menus.len(),
            1,
            "one notification per personal store, not per binding"
        );
        assert!(menus[0].1.contains("old-personal-session"));
        assert!(menus[0].1.contains("rm old.txt"));
    }
    gateway
        .handle_command_approval(&picker_action(&format!("ca:a:{}", old.id), "old"))
        .await
        .unwrap();
    assert!(
        store
            .pending_command_approvals("old-personal-session")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .pending_command_approvals("telegram:test:-100")
            .await
            .unwrap()
            .len(),
        1
    );
    gateway.show_command_approvals(&mut shown).await.unwrap();
    assert!(shown.is_empty());
}

#[tokio::test]
async fn telegram_background_approvals_cannot_cross_user_stores() {
    let (_dir, gateway, store, _, _, _) = setup_with_model(vec![], Arc::new(NoModel)).await;
    let (_other_dir, other_gateway, _, _) = setup(vec![]).await;
    let old = seed_command_approval(&store, "old-webhook-session", "rm secret.txt").await;
    let group = seed_command_approval(&store, "telegram:test:-100", "rm group.txt").await;
    let guest = seed_command_approval(&store, "telegram:test:42:guest", "rm guest.txt").await;
    let gateway = gateway.clone().with_chat_runners(HashMap::from([
        (
            SessionId::new("telegram:test:42"),
            gateway.agent_runner.clone(),
        ),
        (
            SessionId::new("telegram:test:7"),
            other_gateway.agent_runner.clone(),
        ),
    ]));
    let mut foreign = picker_action(&format!("ca:a:{}", old.id), "foreign");
    foreign.source.conversation_id = "7".into();
    assert!(gateway.handle_command_approval(&foreign).await.is_err());
    for id in [&group.id, &guest.id] {
        assert!(
            gateway
                .handle_command_approval(&picker_action(&format!("ca:a:{id}"), "invalid-source"))
                .await
                .is_err()
        );
    }
    assert_eq!(
        store
            .pending_command_approvals("old-webhook-session")
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn telegram_webhook_approval_requires_telegram_history_not_owner_fallback() {
    let (_dir, mut gateway, store, sent, menus, _) =
        setup_with_model(vec![], Arc::new(NoModel)).await;
    let id = "59ab817a-af67-4fb8-b58d-161644c24f34";
    let approval = seed_command_approval(&store, id, "rm webhook.txt").await;
    use crate::agent::inbox::{BusyMode, SubmissionRequest};
    let mut input = IncomingMessage::new(InterfaceId("webhook".into()), id, "private event body");
    input.request_id = "webhook:hook-123:delivery-456".into();
    let submission = store
        .submit(SubmissionRequest::new(
            &SessionId::new(id),
            input,
            BusyMode::FollowUp,
        ))
        .await
        .unwrap();
    let (mut run, run_session) = store.load_run(&approval.run_id).await.unwrap().unwrap();
    run.submission_ids.push(submission.id);
    let mut checkpoint =
        crate::agent::run_state::Checkpoint::new(&store, &SessionId::new(id), &run_session);
    checkpoint.record = run;
    checkpoint
        .save(
            &run_session,
            crate::agent::run_state::RunPhase::NeedTool {
                call_id: approval.call_id.clone(),
            },
            None,
        )
        .await
        .unwrap();
    let mut session = store.load(&SessionId::new(id)).await.unwrap().unwrap();
    session.webhook_instruction = Some("## Recorded delivery source\n{\"webhook_id\":\"hook-123\",\"webhook_name\":\"Site errors\"}\n\n## Owner instruction\nPRIVATE INSTRUCTION".into());
    store.save(&session).await.unwrap();
    gateway
        .show_command_approvals(&mut Default::default())
        .await
        .unwrap();
    assert!(
        menus.lock().unwrap().is_empty(),
        "no guessed Telegram recipient"
    );
    let message = picker_action(&format!("ca:a:{}", approval.id), "webhook");
    assert!(gateway.handle_command_approval(&message).await.is_err());
    gateway.approval_recipient = Some(SessionId::new("telegram:test:42"));
    gateway.approval_web_base = Some(reqwest::Url::parse("https://example.com/kodrik/").unwrap());
    gateway = gateway.clone().with_chat_runners(HashMap::from([(
        SessionId::new("telegram:test:42"), gateway.agent_runner.clone(),
    )]));
    gateway.show_command_approvals(&mut Default::default()).await.unwrap();
    assert!(menus.lock().unwrap().is_empty(), "webhook without Telegram history stays in Web");
    assert!(gateway.handle_command_approval(&message).await.is_err());
    assert!(sent.lock().unwrap().is_empty());
    assert_eq!(store.pending_command_approvals(id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn telegram_unbound_approval_discovery_never_leaks_other_sources_to_first_chat() {
    let (_dir, gateway, store, sent, _, _) = setup_with_model(vec![], Arc::new(NoModel)).await;
    seed_command_approval(&store, "telegram:test:7", "rm seven.txt").await;
    seed_command_approval(&store, "telegram:test:42", "rm forty-two.txt").await;
    seed_command_approval(&store, "private-webhook-source", "rm webhook-secret.txt").await;
    gateway
        .show_command_approvals(&mut Default::default())
        .await
        .unwrap();
    let sent = sent.lock().unwrap();
    assert_eq!(
        sent.len(),
        2,
        "a non-Telegram source needs an explicit recipient"
    );
    assert!(
        sent.iter()
            .any(|(chat, text)| chat == "7" && text.contains("rm seven.txt"))
    );
    assert!(
        sent.iter()
            .any(|(chat, text)| chat == "42" && text.contains("rm forty-two.txt"))
    );
    assert!(!sent.iter().any(|(_, text)| text.contains("webhook-secret")));
}

#[tokio::test]
async fn telegram_background_approval_scan_keeps_delivery_cap_and_context_budget() {
    let (_dir, gateway, store, _, menus, _) = setup_with_model(vec![], Arc::new(NoModel)).await;
    for index in 0..7 {
        seed_approval_from(&store, &format!("background-{index}"), "telegram:test:42", "rm queued.txt").await;
    }
    let gateway = gateway.clone().with_chat_runners(HashMap::from([(
        SessionId::new("telegram:test:42"),
        gateway.agent_runner.clone(),
    )]));
    let mut shown = Default::default();
    gateway.show_command_approvals(&mut shown).await.unwrap();
    assert_eq!(menus.lock().unwrap().len(), 5);
    gateway.show_command_approvals(&mut shown).await.unwrap();
    assert_eq!(menus.lock().unwrap().len(), 7);
    gateway.show_command_approvals(&mut shown).await.unwrap();
    assert_eq!(menus.lock().unwrap().len(), 7);
    let approval = seed_approval_from(&store, "oversized-context", "telegram:test:42", "rm small.txt").await;
    let mut session = store
        .load(&SessionId::new("oversized-context"))
        .await
        .unwrap()
        .unwrap();
    session.webhook_instruction = Some(format!(
        "## Recorded delivery source\n{}",
        serde_json::json!({"webhook_id":"hook", "webhook_name":"🙂".repeat(2200)})
    ));
    store.save(&session).await.unwrap();
    gateway.show_command_approvals(&mut shown).await.unwrap();
    let menu = menus.lock().unwrap().last().unwrap().clone();
    assert!(menu.1.encode_utf16().count() <= 4000);
    assert!(
        menu.2.is_empty(),
        "context must be included in Telegram's complete-review budget"
    );
    assert!(
        gateway
            .handle_command_approval(&picker_action(
                &format!("ca:a:{}", approval.id),
                "oversized-context"
            ))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn telegram_audit_commands_toggle_only_the_private_active_session_without_model_work() {
    let (_dir, gateway, store, sent, _, _) = setup_with_model(vec![], Arc::new(NoModel)).await;
    let conversation = SessionId::new("telegram:test:42");
    for text in ["/audit status", "/audit off", "/audit status"] {
        assert!(
            gateway
                .handle_session_command(&command(text))
                .await
                .unwrap()
        );
    }
    let session = store.active_id(&conversation).await.unwrap();
    assert!(
        store.load(&session).await.unwrap().is_some(),
        "a first-message setting must persist in a real session"
    );
    assert!(!store.command_audit_enabled(session.as_str()).await.unwrap());
    assert!(
        sent.lock()
            .unwrap()
            .last()
            .unwrap()
            .1
            .contains("/audit review")
    );
    for text in ["/audit on extra", "/audit maybe"] {
        assert!(
            gateway
                .handle_session_command(&command(text))
                .await
                .unwrap()
        );
        assert!(!store.command_audit_enabled(session.as_str()).await.unwrap());
    }
    for guest in [false, true] {
        let mut message = command("/audit on");
        message.is_group = !guest;
        if guest {
            message.guest_query_id = Some("guest".into());
        }
        assert!(gateway.handle_session_command(&message).await.unwrap());
        assert!(!store.command_audit_enabled(session.as_str()).await.unwrap());
    }
    assert!(
        gateway
            .handle_session_command(&command("/audit on"))
            .await
            .unwrap()
    );
    assert!(store.command_audit_enabled(session.as_str()).await.unwrap());
    assert_eq!(store.active_id(&conversation).await.unwrap(), session);
    for (mode, label) in [
        (
            crate::agent::command_audit::CommandAuditMode::Trusted,
            "Без проверок",
        ),
        (
            crate::agent::command_audit::CommandAuditMode::Review,
            "Проверка ИИ",
        ),
        (
            crate::agent::command_audit::CommandAuditMode::Ask,
            "Спрашивать всегда",
        ),
    ] {
        assert!(
            gateway
                .handle_session_command(&command(&format!("/audit {}", mode.code())))
                .await
                .unwrap()
        );
        assert_eq!(
            store.command_audit_mode(session.as_str()).await.unwrap(),
            mode
        );
        let sent = sent.lock().unwrap();
        assert!(
            sent.last()
                .unwrap()
                .1
                .starts_with(&format!("◈ Режим инструментов диалога: {label}."))
        );
        assert!(!sent.last().unwrap().1.contains("Command checks"));
    }
}

#[tokio::test]
async fn telegram_audit_commands_use_only_the_senders_store() {
    let (_dir, gateway, owner_store, _, _, _) = setup_with_model(vec![], Arc::new(NoModel)).await;
    let (_other_dir, other_gateway, other_store, _) = setup(vec![]).await;
    let gateway = gateway.clone().with_chat_runners(HashMap::from([
        (
            SessionId::new("telegram:test:42"),
            gateway.agent_runner.clone(),
        ),
        (
            SessionId::new("telegram:test:7"),
            other_gateway.agent_runner.clone(),
        ),
    ]));
    let mut message = command("/audit off");
    message.source.conversation_id = "7".into();
    assert!(gateway.handle_session_command(&message).await.unwrap());
    let selected = other_store
        .active_id(&SessionId::new("telegram:test:7"))
        .await
        .unwrap();
    assert!(
        !other_store
            .command_audit_enabled(selected.as_str())
            .await
            .unwrap()
    );
    assert!(owner_store.list().await.unwrap().is_empty());
    assert!(
        owner_store
            .command_audit_enabled("telegram:test:42")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn telegram_disable_audit_callback_requires_live_reviewable_request_in_the_senders_store() {
    let (_dir, gateway, store, _, menus, _) = setup_with_model(vec![], Arc::new(NoModel)).await;
    let approval = seed_command_approval(&store, "telegram:test:42", "rm important.txt").await;
    gateway
        .show_command_approvals(&mut Default::default())
        .await
        .unwrap();
    assert_eq!(
        menus.lock().unwrap()[0].2[1][0].data,
        format!("ca:s:{}", approval.id)
    );
    let message = picker_action(&format!("ca:s:{}", approval.id), "disable");
    let mut foreign = message.clone();
    foreign.source.conversation_id = "7".into();
    assert!(gateway.handle_command_approval(&foreign).await.is_err());
    for guest in [false, true] {
        let mut invalid = message.clone();
        invalid.is_group = !guest;
        if guest {
            invalid.guest_query_id = Some("guest".into());
        }
        assert!(gateway.handle_command_approval(&invalid).await.is_err());
    }
    assert!(
        store
            .command_audit_enabled("telegram:test:42")
            .await
            .unwrap()
    );
    gateway.handle_command_approval(&message).await.unwrap();
    assert!(
        !store
            .command_audit_enabled("telegram:test:42")
            .await
            .unwrap()
    );
    assert!(
        store
            .pending_command_approvals("telegram:test:42")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(gateway.handle_command_approval(&message).await.is_err());
    store
        .set_command_audit_enabled("telegram:test:42", true)
        .await
        .unwrap();
    store
        .decide_command_approval("telegram:test:42", &approval.id, false)
        .await
        .unwrap();
    assert!(gateway.handle_command_approval(&message).await.is_err());
    assert!(
        store
            .command_audit_enabled("telegram:test:42")
            .await
            .unwrap()
    );
    let (_dir, gateway, store, _, _, _) = setup_with_model(vec![], Arc::new(NoModel)).await;
    let long = seed_command_approval(&store, "telegram:test:42", &"x".repeat(4500)).await;
    assert!(
        gateway
            .handle_command_approval(&picker_action(&format!("ca:s:{}", long.id), "long"))
            .await
            .is_err()
    );
    assert!(
        store
            .command_audit_enabled("telegram:test:42")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn telegram_command_approval_is_durable_explicit_and_single_use() {
    for action in ["a", "r"] {
        let (_dir, gateway, store, _, menus, _) = setup_with_model(vec![], Arc::new(NoModel)).await;
        let approval = seed_command_approval(&store, "telegram:test:42", "rm important.txt").await;
        let mut shown = HashMap::new();
        gateway.show_command_approvals(&mut shown).await.unwrap();
        gateway.show_command_approvals(&mut shown).await.unwrap();
        assert_eq!(
            menus.lock().unwrap().len(),
            1,
            "polling must not repeat approval menus"
        );
        let menu = menus.lock().unwrap()[0].clone();
        assert!(menu.1.contains("rm important.txt"));
        assert!(menu.1.contains("/workspace"));
        assert_eq!(menu.2[0].len(), 2);
        assert!(
            !gateway
                .handle_session_command(&command(&format!("ca:{action}:{}", approval.id)))
                .await
                .unwrap(),
            "ordinary text must not act as a decision"
        );
        assert_eq!(
            store
                .pending_command_approvals("telegram:test:42")
                .await
                .unwrap()
                .len(),
            1
        );
        let message = picker_action(&format!("ca:{action}:{}", approval.id), "approval-click");
        gateway.handle_command_approval(&message).await.unwrap();
        assert!(
            store
                .pending_command_approvals("telegram:test:42")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(gateway.handle_command_approval(&message).await.is_err());
        let id = approval.id;
        let decision: String = store
            .with_connection(move |db| {
                let payload: Vec<u8> = db.query_row(
                    "SELECT payload FROM command_audits WHERE id=?1",
                    [id],
                    |row| row.get(0),
                )?;
                Ok(
                    serde_json::from_slice::<serde_json::Value>(&payload)?["decision"]
                        .as_str()
                        .unwrap()
                        .into(),
                )
            })
            .await
            .unwrap();
        assert_eq!(decision, if action == "a" { "Approved" } else { "Denied" });
        let menus = menus.lock().unwrap();
        assert_eq!(menus.len(), 2);
        assert!(
            menus[1].0 && menus[1].2.is_empty(),
            "decision removes its buttons"
        );
    }
}

#[tokio::test]
async fn telegram_command_approval_rejects_foreign_private_stores_and_unreviewable_commands() {
    let (_dir, gateway, owner_store, _, _, _) = setup_with_model(vec![], Arc::new(NoModel)).await;
    let (_other_dir, other_gateway, other_store, _) = setup(vec![]).await;
    let approval = seed_command_approval(&owner_store, "telegram:test:42", "rm private.txt").await;
    let gateway = gateway.clone().with_chat_runners(HashMap::from([
        (
            SessionId::new("telegram:test:42"),
            gateway.agent_runner.clone(),
        ),
        (
            SessionId::new("telegram:test:7"),
            other_gateway.agent_runner.clone(),
        ),
    ]));
    let mut message = picker_action(&format!("ca:a:{}", approval.id), "foreign");
    message.source.conversation_id = "7".into();
    assert!(gateway.handle_command_approval(&message).await.is_err());
    message.source.conversation_id = "42".into();
    message.is_group = true;
    assert!(gateway.handle_command_approval(&message).await.is_err());
    message.is_group = false;
    message.guest_query_id = Some("guest".into());
    assert!(gateway.handle_command_approval(&message).await.is_err());
    assert_eq!(
        owner_store
            .pending_command_approvals("telegram:test:42")
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        other_store
            .pending_command_approvals("telegram:test:7")
            .await
            .unwrap()
            .is_empty()
    );

    let (_dir, gateway, store, _, menus, _) = setup_with_model(vec![], Arc::new(NoModel)).await;
    let approval = seed_command_approval(&store, "telegram:test:42", &"x".repeat(4500)).await;
    gateway
        .show_command_approvals(&mut Default::default())
        .await
        .unwrap();
    assert!(menus.lock().unwrap()[0].2.is_empty());
    let forged = picker_action(&format!("ca:a:{}", approval.id), "oversized");
    assert!(gateway.handle_command_approval(&forged).await.is_err());
    assert_eq!(
        store
            .pending_command_approvals("telegram:test:42")
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn slow_command_approval_delivery_does_not_block_stop_or_shutdown() {
    struct SlowMenu {
        inputs: tokio::sync::Mutex<mpsc::Receiver<IncomingMessage>>,
        started: Arc<tokio::sync::Notify>,
        sends: Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl Interface for SlowMenu {
        fn id(&self) -> InterfaceId {
            InterfaceId("telegram:test".into())
        }
        fn allows_recipient(&self, chat: &str) -> bool {
            chat == "42"
        }
        async fn run(&self, tx: mpsc::Sender<Event>) -> anyhow::Result<()> {
            while let Some(message) = self.inputs.lock().await.recv().await {
                tx.send(Event::Message(message)).await?;
            }
            Ok(())
        }
        async fn send(&self, _: &str, _: &str) -> anyhow::Result<MessageId> {
            Ok(MessageId("reply".into()))
        }
        async fn send_buttons(
            &self,
            _: &str,
            _: &str,
            _: &[Vec<InlineButton>],
        ) -> anyhow::Result<MessageId> {
            self.sends.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.started.notify_one();
            std::future::pending().await
        }
    }
    let (_dir, mut gateway, store, _, _, _) = setup_with_model(vec![], Arc::new(NoModel)).await;
    let approval = seed_command_approval(&store, "telegram:test:42", "rm important.txt").await;
    let started = Arc::new(tokio::sync::Notify::new());
    let sends = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (inputs, received) = mpsc::channel(1);
    gateway.interfaces = vec![Arc::new(SlowMenu {
        inputs: tokio::sync::Mutex::new(received),
        started: started.clone(),
        sends: sends.clone(),
    })];
    let runner = gateway.agent_runner.clone();
    let mut task = tokio::spawn(async move { gateway.run().await });
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let overlapping_sends = sends.load(std::sync::atomic::Ordering::SeqCst);
    inputs.send(command("/stop")).await.unwrap();
    let stopped = tokio::time::timeout(Duration::from_millis(500), async {
        while !store
            .cancellation_requested(&approval.run_id)
            .await
            .unwrap()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    runner.suspend();
    let shutdown = tokio::time::timeout(Duration::from_millis(500), &mut task).await;
    task.abort();
    assert_eq!(
        overlapping_sends, 1,
        "approval delivery jobs must not overlap"
    );
    assert!(stopped.is_ok(), "slow approval menus must not delay /stop");
    shutdown
        .expect("slow approval menus must not delay shutdown")
        .unwrap()
        .unwrap();
}
