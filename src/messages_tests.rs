use super::*;
use crate::agent::{message::Message, session::SessionStore};
use axum::{Json, Router, response::IntoResponse, routing::post};
use serde_json::{Value, json};

#[tokio::test]
async fn messages_owner_can_send_to_web_but_isolated_agents_cannot() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let router = Router::new().route("/responses", post(move |Json(body): Json<Value>| {
        let tx = tx.clone();
        async move {
            let result = body["input"].as_array().unwrap().iter().find(|v| v["type"] == "function_call_output");
            let output = if let Some(result) = result {
                tx.send((body["tools"].clone(), serde_json::from_str::<Value>(result["output"].as_str().unwrap()).unwrap())).unwrap();
                json!({"type":"message","content":[{"type":"output_text","text":"done"}]})
            } else {
                json!({"type":"function_call","call_id":"send-web","name":"messages","arguments":json!({"action":"send","channel":"web","text":"Checkout is broken","conversation_id":null}).to_string()})
            };
            let response = json!({"output":[output]});
            if body["stream"] == true {
                ([("content-type","text/event-stream")], format!("data: {}\n\ndata: [DONE]\n\n",json!({"type":"response.completed","response":response}))).into_response()
            } else { Json(response).into_response() }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async { axum::serve(listener, router).await.unwrap() });
    for isolated in [false, true] {
        let data = tempfile::tempdir().unwrap();
        let config: Config = toml::from_str(&format!(
            r#"
[gateway]
[web]
enabled = true
base_url = "http://localhost/kodrik/"
[companion]
learning_enabled = false
[sessions]
auto_switch = false
[provider.primary]
type = "openai"
model = "test"
api_key = "test"
api_base = "{base}"
"#
        ))
        .unwrap();
        let app = build_app_at(&config, data.path(), isolated).await.unwrap();
        let webhook_conversation = if isolated {
            app.agent_runner
                .run_once("Send me a web notification")
                .await
                .unwrap();
            None
        } else {
            use axum::{
                body::{Body, to_bytes},
                http::Request,
            };
            use tower::ServiceExt;
            let auth = crate::web::auth::AuthStore::new(data.path());
            auth.set_password("test-password").unwrap();
            let token = auth
                .login("test-password", chrono::Utc::now().timestamp())
                .unwrap()
                .unwrap();
            let chat = crate::web::chat::Chat::new(&app.agent_runner, data.path())
                .await
                .unwrap();
            let router = crate::web::server::router_with_chat(
                crate::web::Settings::from_config(&config.web)
                    .unwrap()
                    .unwrap(),
                data.path(),
                Some(chat),
            );
            let response = router.clone().oneshot(Request::builder().method("POST").uri("/kodrik/api/webhooks")
                .header("cookie",format!("kodrik_web_session={token}")).header("origin","http://localhost").header("content-type","application/json")
                .body(Body::from(json!({"name":"Site monitor","instruction":"Send the incident to me in the web chat using messages."}).to_string())).unwrap()).await.unwrap();
            assert_eq!(response.status(), 201);
            let hook: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                    .unwrap();
            let response = router
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(format!(
                            "/kodrik/api/webhooks/{}",
                            hook["id"].as_str().unwrap()
                        ))
                        .header(
                            "authorization",
                            format!("Bearer {}", hook["token"].as_str().unwrap()),
                        )
                        .body(Body::from("Checkout failed"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), 202);
            let accepted: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                    .unwrap();
            Some(accepted["conversation_id"].as_str().unwrap().to_owned())
        };
        let (tools, result) = rx.recv().await.unwrap();
        if let Some(id) = webhook_conversation {
            let store = app.agent_runner.sqlite_store().unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while store.conversation_view(&id).await.unwrap().busy {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let source = store
                .load(&crate::agent::session::SessionId::new(&id))
                .await
                .unwrap()
                .unwrap();
            assert!(
                source
                    .webhook_instruction
                    .as_ref()
                    .unwrap()
                    .contains("Site monitor")
            );
            assert!(
                matches!(&source.messages()[0],Message::User(message) if message.content == "Checkout failed")
            );
        }
        let available = tools
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "messages");
        assert_eq!(available, !isolated);
        assert_eq!(
            tools
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "webhooks"),
            !isolated
        );
        let sessions = app
            .agent_runner
            .sqlite_store()
            .unwrap()
            .list()
            .await
            .unwrap();
        if isolated {
            assert!(result["error"].is_string(), "{result}");
            assert_eq!(sessions.len(), 1);
        } else {
            assert_eq!(result["status"], "sent", "{result}");
            let id = result["conversation_id"].as_str().unwrap();
            assert_eq!(
                result["url"],
                format!("http://localhost/kodrik/app/chat/{id}")
            );
            let delivered = sessions.iter().find(|s| s.id().as_str() == id).unwrap();
            assert!(
                matches!(&delivered.messages()[0], Message::Assistant(message) if message.content == "Checkout is broken")
            );
        }
        app.agent_runner.suspend();
    }
    server.abort();
}
