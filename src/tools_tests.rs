use super::*;
use crate::{
    agent::{
        session::SessionId,
        tools::telegram_reply::{REPLY_CONTEXT, ReplyContext},
    },
    gateway::{IncomingMessage, InterfaceId},
};
use axum::{Json, Router, response::IntoResponse, routing::post};
use serde_json::{Value, json};

#[tokio::test]
async fn wakeups_run_without_telegram_and_stop_with_the_application() {
    use crate::agent::{
        message::Message,
        reminders::{Recipient, ReminderStore},
        session::SessionStore,
    };
    use chrono::{Timelike, Utc};
    let router = Router::new().route("/responses", post(|Json(body): Json<Value>| async move {
        assert!(body["tools"].as_array().unwrap().iter().any(|t| t["name"] == "wakeups"));
        let response = json!({"output":[{"type":"message","content":[{"type":"output_text","text":"{\"message\":\"Scheduled result\"}"}]}]});
        if body["stream"] == true {
            ([("content-type", "text/event-stream")], format!("data: {}\n\ndata: [DONE]\n\n", json!({"type":"response.completed","response":response}))).into_response()
        } else { Json(response).into_response() }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let data = tempfile::tempdir().unwrap();
    let mut config: Config = toml::from_str(&format!("[gateway]\n[sessions]\nauto_switch=false\n[companion]\nlearning_enabled=false\n[provider.primary]\ntype='openai'\nmodel='test'\napi_key='test'\napi_base='{base}'")).unwrap();
    let now = Utc::now();
    config.companion.quiet_start = (now.hour() + 1) % 24;
    config.companion.quiet_end = (now.hour() + 2) % 24;
    let app = build_app_at(&config, data.path(), false).await.unwrap();
    let chat = uuid::Uuid::new_v4().to_string();
    let source = app
        .agent_runner
        .new_session(&SessionId::new(&chat))
        .await
        .unwrap();
    let journal = ReminderStore::wakeups(data.path());
    journal.execute_wakeup(&Recipient { interface: "web".into(), chat }, source.id().as_str(), serde_json::from_value(json!({"action":"create","text":"Check","due_at":(now-chrono::Duration::seconds(1)).to_rfc3339()})).unwrap(), now-chrono::Duration::seconds(2)).unwrap();
    let sessions = app.agent_runner.sqlite_store().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let saved = sessions.load(source.id()).await.unwrap().unwrap();
            if matches!(saved.messages().last(), Some(Message::Assistant(answer)) if answer.content == "Scheduled result") { break; }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }).await.unwrap();
    app.agent_runner.suspend();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if journal.scheduler_lock().unwrap().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    server.abort();
}

#[tokio::test]
async fn owner_tools_are_identical_across_foreground_interfaces() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let router = Router::new().route("/responses", post(move |Json(body): Json<Value>| {
        let tx = tx.clone();
        async move {
            let mut names: Vec<_> = body["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_owned()).collect();
            names.sort();
            if !names.is_empty() { tx.send(names).unwrap(); }
            let response = json!({"output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]}]});
            if body["stream"] == true {
                ([("content-type", "text/event-stream")], format!("data: {}\n\ndata: [DONE]\n\n", json!({"type":"response.completed","response":response}))).into_response()
            } else { Json(response).into_response() }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let data = tempfile::tempdir().unwrap();
    let config: Config = toml::from_str(&format!("[owner]\ninterface='main'\nuser_id=42\n[gateway.main]\ntype='telegram'\ntoken='test'\n[web]\nenabled=true\nbase_url='http://localhost/'\n[research]\nsearxng_url='http://localhost:1/'\n[sessions]\nauto_switch=false\n[companion]\nlearning_enabled=false\n[provider.primary]\ntype='openai'\nmodel='test'\napi_key='test'\napi_base='{base}'")).unwrap();
    let app = build_app_at(&config, data.path(), false).await.unwrap();
    let interface = Arc::new(crate::interfaces::TelegramInterface::new(
        "main",
        "test",
        data.path(),
    ));
    REPLY_CONTEXT
        .scope(
            ReplyContext::new(interface, "42".into(), None),
            app.agent_runner.run(
                SessionId::new("telegram:main:42"),
                IncomingMessage::new(InterfaceId("telegram:main".into()), "42", "hello"),
                None,
            ),
        )
        .await
        .unwrap();
    let expected = rx.recv().await.unwrap();
    for name in [
        "wakeups",
        "telegram_reply",
        "telegram_results",
        "webhooks",
        "messages",
        "show_ui",
        "publish_page",
        "web_search",
        "web_read",
    ] {
        assert!(expected.iter().any(|n| n == name), "{name}: {expected:?}");
    }
    app.agent_runner.run_once("hello").await.unwrap();
    assert_eq!(rx.recv().await.unwrap(), expected, "one-shot CLI");
    for interface in ["cli", "web", "webhook"] {
        let chat = if interface == "cli" {
            "main".into()
        } else {
            uuid::Uuid::new_v4().to_string()
        };
        let conversation = if interface == "cli" {
            "cli:main".into()
        } else {
            chat.clone()
        };
        let runner = if interface == "web" {
            Arc::new(
                app.agent_runner
                    .for_web(Arc::new(app.agent_runner.sqlite_store().unwrap())),
            )
        } else {
            app.agent_runner.clone()
        };
        runner
            .run(
                SessionId::new(conversation),
                IncomingMessage::new(InterfaceId(interface.into()), &chat, "hello"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(rx.recv().await.unwrap(), expected, "{interface}");
    }
    app.agent_runner.suspend();
    server.abort();
}
