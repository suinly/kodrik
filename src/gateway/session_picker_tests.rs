fn picker_action(data: &str, query: &str) -> IncomingMessage {
    let mut message = command("");
    message.request_id = format!("telegram:test:42:callback:{query}");
    message.callback = Some(Box::new(crate::gateway::SessionCallback {
        id: query.into(),
        message_id: MessageId("20".into()),
        data: data.into(),
    }));
    message
}

#[tokio::test]
async fn session_picker_ack_failure_does_not_stop_the_gateway_or_apply_actions() {
    struct Unavailable;
    #[async_trait::async_trait]
    impl Interface for Unavailable {
        fn id(&self) -> InterfaceId {
            InterfaceId("telegram:test".into())
        }
        async fn run(&self, _: mpsc::Sender<Event>) -> anyhow::Result<()> {
            Ok(())
        }
        async fn send(&self, _: &str, _: &str) -> anyhow::Result<MessageId> {
            panic!("no chat messages on callback failure")
        }
        async fn answer_callback(&self, _: &str, _: &str) -> anyhow::Result<()> {
            anyhow::bail!("Telegram unavailable")
        }
    }
    let (_dir, mut gateway, store, _) = setup(vec![]).await;
    gateway.interfaces.clear();
    let gateway = gateway.add_interface(Unavailable);
    gateway
        .handle_event(Event::Message(picker_action("cs:n", "failed")), None, None)
        .await
        .unwrap();
    assert!(store.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn session_picker_callback_uses_only_the_verified_users_store() {
    let (_dir, gateway, owner_store, _, menus, callbacks) =
        setup_with_model(vec![], Arc::new(NoModel)).await;
    let (_other_dir, other_gateway, other_store, _) = setup(vec![]).await;
    save(&owner_store, "shared-id", "OWNER PRIVATE TOPIC").await;
    save(&other_store, "shared-id", "USER TOPIC").await;
    let owner_runner = gateway.agent_runner.clone();
    let gateway = gateway.with_chat_runners(HashMap::from([
        (SessionId::new("telegram:test:42"), owner_runner),
        (
            SessionId::new("telegram:test:7"),
            other_gateway.agent_runner.clone(),
        ),
    ]));
    gateway
        .handle_event(Event::Message(command("/sessions")), None, None)
        .await
        .unwrap();
    let data = menus.lock().unwrap()[0].2[0][0].data.clone();
    let mut message = picker_action(&data, "foreign-user");
    message.source.conversation_id = "7".into();
    gateway
        .handle_event(Event::Message(message), None, None)
        .await
        .unwrap();
    let menu = menus.lock().unwrap().last().unwrap().clone();
    assert!(menu.1.contains("недоступен"));
    assert!(!format!("{}{:?}", menu.1, menu.2).contains("OWNER PRIVATE"));
    assert!(
        menu.2
            .iter()
            .flatten()
            .any(|button| button.text.contains("USER TOPIC"))
    );
    assert_eq!(
        other_store
            .active_id(&SessionId::new("telegram:test:7"))
            .await
            .unwrap()
            .as_str(),
        "telegram:test:7"
    );
    assert_eq!(
        owner_store
            .active_id(&SessionId::new("telegram:test:42"))
            .await
            .unwrap()
            .as_str(),
        "telegram:test:42"
    );
    assert_eq!(callbacks.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn session_picker_buttons_select_without_replay_and_edit_the_same_menu() {
    let (_dir, gateway, store, sent, menus, callbacks) =
        setup_with_model(vec![], Arc::new(NoModel)).await;
    let long_id = format!("legacy-{}", "a".repeat(180));
    save(&store, &long_id, "Питание").await;
    for id in 0..11 {
        save(&store, &format!("other-{id}"), &format!("Тема {id}")).await;
    }
    gateway
        .handle_event(Event::Message(command("/sessions")), None, None)
        .await
        .unwrap();
    let next = menus.lock().unwrap()[0]
        .2
        .iter()
        .flatten()
        .find(|button| button.text.contains("Далее"))
        .unwrap()
        .data
        .clone();
    gateway
        .handle_event(Event::Message(picker_action(&next, "page")), None, None)
        .await
        .unwrap();
    let menu = menus.lock().unwrap().last().unwrap().clone();
    assert!(menu.0 && menu.1.contains("2/2"));
    let data = menu
        .2
        .iter()
        .flatten()
        .find(|button| button.text.contains("Питание"))
        .unwrap()
        .data
        .clone();
    assert!(data.len() <= 64 && !data.contains(&long_id));
    let before =
        serde_json::to_value(store.load(&SessionId::new(&long_id)).await.unwrap()).unwrap();
    gateway
        .handle_event(Event::Message(picker_action(&data, "select")), None, None)
        .await
        .unwrap();
    assert_eq!(
        store
            .active_id(&SessionId::new("telegram:test:42"))
            .await
            .unwrap()
            .as_str(),
        long_id
    );
    let menus = menus.lock().unwrap();
    assert_eq!(menus.last().unwrap().1, "Выбран диалог: Питание");
    assert!(
        menus.last().unwrap().2.is_empty(),
        "selection must dismiss the keyboard"
    );
    assert_eq!(
        sent.lock().unwrap().len(),
        1,
        "callbacks must edit, not append chat messages"
    );
    assert_eq!(*callbacks.lock().unwrap(), ["page", "select"]);
    assert_eq!(
        serde_json::to_value(store.load(&SessionId::new(&long_id)).await.unwrap()).unwrap(),
        before
    );
    assert!(
        !menus
            .iter()
            .any(|(_, text, _)| text.contains("PRIVATE OLD ANSWER"))
    );
    assert_eq!(
        store
            .with_connection(|db| Ok(db.query_row(
                "SELECT count(*) FROM submissions",
                [],
                |row| row.get::<_, i64>(0)
            )?))
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn session_picker_collapses_branches_and_honors_web_pins_and_selection() {
    let (dir, gateway, store, _, menus, _) = setup_with_model(vec![], Arc::new(NoModel)).await;
    let _chat = crate::web::chat::Chat::new(&gateway.agent_runner, dir.path())
        .await
        .unwrap();
    save(&store, "pinned", "Закреплённый").await;
    save(&store, "root", "Основная версия").await;
    save(&store, "branch", "Другая версия").await;
    save(&store, "latest", "Недавний").await;
    save(&store, "companion:hidden", "SECRET").await;
    store
        .with_connection(|db| {
            db.execute_batch(
                "INSERT INTO web_chat_branches VALUES('branch','root','root',0);
            INSERT INTO web_chat_selection VALUES('root','branch');
            INSERT INTO web_chat_titles VALUES('root','Общее название');
            INSERT INTO web_chat_pins VALUES('pinned');",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    gateway
        .handle_event(Event::Message(command("/sessions")), None, None)
        .await
        .unwrap();
    let menu = menus.lock().unwrap()[0].clone();
    let rows: Vec<_> = menu
        .2
        .iter()
        .flatten()
        .filter(|button| button.data.starts_with("cs:s:"))
        .collect();
    assert_eq!(rows.len(), 3);
    assert!(rows[0].text.starts_with("📌 Закреплённый"));
    assert!(rows[1].text.contains("Недавний"));
    let data = rows
        .iter()
        .find(|row| row.text.contains("Общее название"))
        .unwrap()
        .data
        .clone();
    gateway
        .handle_event(Event::Message(picker_action(&data, "branch")), None, None)
        .await
        .unwrap();
    assert_eq!(
        store
            .active_id(&SessionId::new("telegram:test:42"))
            .await
            .unwrap()
            .as_str(),
        "branch"
    );
}

#[tokio::test]
async fn session_picker_busy_and_deleted_buttons_preserve_selection() {
    use crate::agent::inbox::{BusyMode, SubmissionRequest};
    let (_dir, gateway, store, _, menus, callbacks) =
        setup_with_model(vec![], Arc::new(NoModel)).await;
    save(&store, "current", "Текущий").await;
    save(&store, "target", "Целевой").await;
    let conversation = SessionId::new("telegram:test:42");
    store
        .select(&conversation, &SessionId::new("current"))
        .await
        .unwrap();
    gateway
        .handle_event(Event::Message(command("/sessions")), None, None)
        .await
        .unwrap();
    let data = menus.lock().unwrap()[0]
        .2
        .iter()
        .flatten()
        .find(|row| row.text.contains("Целевой"))
        .unwrap()
        .data
        .clone();
    store.delete(&SessionId::new("target")).await.unwrap();
    gateway
        .handle_event(Event::Message(picker_action(&data, "deleted")), None, None)
        .await
        .unwrap();
    assert!(menus.lock().unwrap().last().unwrap().1.contains("удалён"));
    save(&store, "target", "Целевой").await;
    store
        .submit(SubmissionRequest::new(
            &SessionId::new("target"),
            command("pending web work"),
            BusyMode::FollowUp,
        ))
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(2),
        gateway.handle_event(Event::Message(picker_action(&data, "busy")), None, None),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(menus.lock().unwrap().last().unwrap().1.contains("занят"));
    assert!(
        menus
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .2
            .iter()
            .flatten()
            .any(|row| row.text.starts_with("⏳ Целевой"))
    );
    assert_eq!(
        store.active_id(&conversation).await.unwrap().as_str(),
        "current"
    );
    gateway
        .handle_event(
            Event::Message(picker_action("cs:s:unknown:1", "foreign")),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(callbacks.lock().unwrap().len(), 3);
    assert_eq!(
        store.active_id(&conversation).await.unwrap().as_str(),
        "current"
    );
}

#[tokio::test]
async fn session_picker_new_button_is_durable_and_duplicate_safe() {
    let (_dir, gateway, store, sent, menus, callbacks) =
        setup_with_model(vec![], Arc::new(NoModel)).await;
    gateway
        .handle_event(Event::Message(command("/sessions")), None, None)
        .await
        .unwrap();
    let button = menus.lock().unwrap()[0].2[0][0].clone();
    assert_eq!(button.text, "＋ Новый диалог");
    assert!(menus.lock().unwrap()[0].1.contains("пока нет"));
    for _ in 0..2 {
        gateway
            .handle_event(
                Event::Message(picker_action(&button.data, "new")),
                None,
                None,
            )
            .await
            .unwrap();
    }
    assert_eq!(store.list().await.unwrap().len(), 1);
    assert!(
        store
            .load(
                &store
                    .active_id(&SessionId::new("telegram:test:42"))
                    .await
                    .unwrap()
            )
            .await
            .unwrap()
            .unwrap()
            .messages()
            .is_empty()
    );
    assert_eq!(sent.lock().unwrap().len(), 1);
    assert_eq!(callbacks.lock().unwrap().len(), 2);
    assert!(
        menus.lock().unwrap().last().unwrap().2.is_empty(),
        "new dialog must dismiss the keyboard"
    );
    assert!(
        menus
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .1
            .contains("Новый диалог создан")
    );
}
