async fn seed_command_approval(
    store: &SqliteSessionStore,
    conversation: &str,
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
        .set_command_audit_mode(
            session.id().as_str(),
            crate::agent::command_audit::CommandAuditMode::Review,
        )
        .await
        .unwrap();
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
                .starts_with(&format!("◈ Режим команд диалога: {label}."))
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
        !owner_store
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
        let mut shown = std::collections::HashSet::new();
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
