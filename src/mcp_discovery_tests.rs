use super::*;
use crate::{
    agent::session::{SessionId, SessionStore},
    gateway::{IncomingMessage, InterfaceId},
};
use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::post};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn mcp_discovery_bounds_large_catalog_and_restores_selection_after_compaction_and_restart() {
    let calls = Arc::new(AtomicUsize::new(0));
    let received_calls = calls.clone();
    let mcp = Router::new().route("/mcp", post(move |Json(body): Json<Value>| {
        let calls = received_calls.clone();
        async move {
            if body.get("id").is_none() { return StatusCode::ACCEPTED.into_response(); }
            let result = match body["method"].as_str().unwrap() {
                "initialize" => json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}),
                "tools/list" => {
                    let mut tools: Vec<Value> = (0..140).map(|i| json!({"name":format!("noise_{i:03}"),"description":"Unrelated action","inputSchema":{"type":"object","properties":{}}})).collect();
                    tools.push(json!({"name":"search_issues","description":"Find issues in a project","inputSchema":{"type":"object","properties":{"project":{"type":"integer"}},"required":["project"]}}));
                    tools.push(json!({"name":"archive_issue","description":"Archive an issue","inputSchema":{"type":"object","properties":{"id":{"type":"integer"}}}}));
                    json!({"tools":tools})
                }
                "tools/call" => {
                    assert_eq!(body["params"]["name"], "search_issues");
                    assert_eq!(body["params"]["arguments"], json!({"project":7}));
                    calls.fetch_add(1, Ordering::SeqCst);
                    json!({"content":[{"type":"text","text":"One issue"}],"structuredContent":{"issues":[{"id":42}]},"isError":false})
                }
                method => panic!("Unexpected MCP method: {method}"),
            };
            Json(json!({"jsonrpc":"2.0","id":body["id"],"result":result})).into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mcp_url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let mcp_task = tokio::spawn(async move { axum::serve(listener, mcp).await.unwrap() });
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let llm = Router::new().route("/responses", post(move |Json(body): Json<Value>| {
        let tx = tx.clone();
        async move {
            tx.send(body.clone()).unwrap();
            let items = body["input"].as_array().unwrap();
            let start = items.iter().rposition(|item| item["role"] == "user").unwrap();
            let again = items[start]["content"].to_string().contains("again");
            let steps = items[start+1..].iter().filter(|item| item["type"] == "function_call_output").count();
            let output = if steps == 0 && !again {
                json!({"type":"function_call","name":"mcp_search","call_id":"discover","arguments":"{\"query\":\"rustrak search_issues\",\"limit\":1}"})
            } else if steps == usize::from(!again) {
                json!({"type":"function_call","namespace":"mcp__rustrak","name":"search_issues","call_id":if again { "read-again" } else { "read" },"arguments":"{\"project\":7}"})
            } else {
                json!({"type":"message","content":[{"type":"output_text","text":"done"}]})
            };
            let response = json!({"output":[output]});
            if body["stream"] == true {
                ([("content-type","text/event-stream")], format!("data: {}\n\ndata: [DONE]\n\n", json!({"type":"response.completed","response":response}))).into_response()
            } else { Json(response).into_response() }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let llm_task = tokio::spawn(async move { axum::serve(listener, llm).await.unwrap() });
    let config: Config = toml::from_str(&format!("[gateway]\n[web]\nenabled=true\nbase_url='http://localhost/'\n[research]\nsearxng_url='http://localhost:1/'\n[sessions]\nauto_switch=false\ncompact_threshold_tokens=0\n[companion]\nlearning_enabled=false\n[provider.primary]\ntype='openai'\nmodel='test'\napi_key='test'\napi_base='{base}'\n[mcp.rustrak]\ntype='http'\nurl='{mcp_url}'\n[mcp.rustrak.headers]\nAuthorization='Bearer PRIVATE_MCP_TOKEN'")).unwrap();
    let data = tempfile::tempdir().unwrap();
    let app = build_app_at(&config, data.path(), false).await.unwrap();
    let id = SessionId::new(uuid::Uuid::new_v4().to_string());
    app.agent_runner
        .run(
            id.clone(),
            IncomingMessage::new(InterfaceId("web".into()), id.as_str(), "Find issues"),
            None,
        )
        .await
        .unwrap();
    for step in 0..3 {
        let request = rx.recv().await.unwrap();
        assert!(!request.to_string().contains("PRIVATE_MCP_TOKEN"));
        let tools = request["tools"].as_array().unwrap();
        assert!(tools.len() <= 128, "model received {} tools", tools.len());
        for native in [
            "show_ui",
            "ui_components",
            "wakeups",
            "web_search",
            "web_read",
            "mcp_search",
        ] {
            assert!(
                tools.iter().any(|tool| tool["name"] == native),
                "{native} missing"
            );
        }
        let loaded: Vec<_> = tools
            .iter()
            .filter(|tool| tool["name"].as_str().unwrap().starts_with("mcp__"))
            .collect();
        if step == 0 {
            assert!(loaded.is_empty());
        } else {
            assert_eq!(loaded.len(), 1);
            assert_eq!(loaded[0]["name"], "mcp__rustrak__search_issues");
            assert_eq!(loaded[0]["strict"], false);
            assert_eq!(loaded[0]["parameters"]["required"], json!(["project"]));
        }
        if step == 2 {
            let output = request["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["type"] == "function_call_output")
                .last()
                .unwrap();
            let output: Value = serde_json::from_str(output["output"].as_str().unwrap()).unwrap();
            assert_eq!(output["structuredContent"]["issues"][0]["id"], 42);
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let store = app.agent_runner.sqlite_store().unwrap();
    let mut session = store.load(&id).await.unwrap().unwrap();
    session.set_compaction(
        session.messages().len(),
        "Previously found the project issues".into(),
    );
    store.save(&session).await.unwrap();
    app.agent_runner.suspend();
    drop(app);
    let app = build_app_at(&config, data.path(), false).await.unwrap();
    app.agent_runner
        .run(
            id.clone(),
            IncomingMessage::new(InterfaceId("web".into()), id.as_str(), "again"),
            None,
        )
        .await
        .unwrap();
    for _ in 0..2 {
        let request = rx.recv().await.unwrap();
        assert!(
            request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "mcp__rustrak__search_issues")
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    app.agent_runner.suspend();
    mcp_task.abort();
    llm_task.abort();
}
