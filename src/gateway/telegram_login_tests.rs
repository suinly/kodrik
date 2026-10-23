#[tokio::test]
async fn telegram_login_bot_approval_is_private_explicit_and_never_model_work() {
    use crate::{
        config::Config,
        web::auth::{AuthStore, Identity, LoginPoll},
    };
    let (dir, gateway, store, _, menus, callbacks) =
        setup_with_model(vec![], Arc::new(NoModel)).await;
    let config: Config = toml::from_str("[owner]\ninterface='test'\nuser_id=42\n[gateway.test]\ntype='telegram'\ntoken='test'\n[companion]\nlearning_enabled=false\nproactive_enabled=false\n[web]\nenabled=true\nbase_url='https://kodrik.example/'\n[provider.primary]\ntype='openai'\nmodel='test'\napi_key='test'").unwrap();
    let app = crate::bootstrap::build_app_at(&config, &dir.path().join("owner"), false)
        .await
        .unwrap();
    let gateway = gateway
        .with_telegram_spaces(&config, dir.path(), app)
        .await
        .unwrap();
    let auth = AuthStore::new(dir.path());
    auth.register_telegram_bot("test", "TestBot").unwrap();
    let now = chrono::Utc::now().timestamp();
    for approve in [true, false] {
        let login = auth
            .begin_telegram_login(None, "/chat/", now)
            .unwrap()
            .unwrap();
        gateway
            .handle_event(
                Event::Message(command(&format!("/start login_{}", login.id))),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            auth.poll_telegram_login(&login.cookie, now).unwrap(),
            LoginPoll::Pending
        );
        let menu = menus.lock().unwrap().last().unwrap().clone();
        assert!(
            menu.1.contains("https://kodrik.example/")
                && menu.1.contains(&login.id[..6].to_ascii_uppercase())
        );
        let choice = menu
            .2
            .iter()
            .flatten()
            .find(|button| {
                button.text
                    == if approve {
                        "Войти"
                    } else {
                        "Отказать"
                    }
            })
            .unwrap()
            .data
            .clone();
        let mut foreign = picker_action(&choice, "foreign");
        foreign.source.conversation_id = "7".into();
        gateway
            .handle_event(Event::Message(foreign), None, None)
            .await
            .unwrap();
        assert_eq!(
            auth.poll_telegram_login(&login.cookie, now).unwrap(),
            LoginPoll::Pending
        );
        gateway
            .handle_event(
                Event::Message(picker_action(&choice, "decision")),
                None,
                None,
            )
            .await
            .unwrap();
        assert!(menus.lock().unwrap().last().unwrap().2.is_empty());
        match auth.poll_telegram_login(&login.cookie, now).unwrap() {
            LoginPoll::Approved { token, .. } if approve => {
                assert_eq!(
                    auth.identity(&token, now).unwrap(),
                    Some(Identity::Telegram(42))
                );
                assert!(auth.authenticated(&token, now).unwrap());
            }
            LoginPoll::Denied if !approve => {}
            other => panic!("wrong decision {other:?}"),
        }
    }
    for variant in 0..3 {
        let login = auth
            .begin_telegram_login(None, "/chat/", now)
            .unwrap()
            .unwrap();
        let mut message = command(&format!("/start login_{}", login.id));
        if variant == 0 {
            message.is_group = true;
        }
        if variant == 1 {
            message.guest_query_id = Some("guest".into());
        }
        if variant == 2 {
            message.images.push(crate::agent::Image {
                data_url: "data:image/png;base64,test".into(),
                width: 1,
                height: 1,
            });
        }
        gateway
            .handle_event(Event::Message(message), None, None)
            .await
            .unwrap();
        assert!(
            !auth
                .decide_telegram_login(&login.id, "test", 42, true, now)
                .unwrap()
        );
    }
    gateway
        .handle_event(Event::Message(command("/start login_invalid")), None, None)
        .await
        .unwrap();
    assert!(store.list().await.unwrap().is_empty());
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
    assert_eq!(callbacks.lock().unwrap().len(), 4);
}
