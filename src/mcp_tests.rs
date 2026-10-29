use super::*;
use crate::agent::{
    UserMessage,
    session::{Session, SessionId},
};
use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use serde_json::{Value, json};

async fn mcp_server(
    sse: bool,
    tool_names: [&str; 2],
) -> (
    String,
    tokio::sync::mpsc::UnboundedReceiver<(HeaderMap, Value)>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let tool_names = tool_names.map(str::to_owned);
    let router = Router::new().route("/mcp", post(move |headers: HeaderMap, Json(body): Json<Value>| {
        let tx = tx.clone();
        let tool_names = tool_names.clone();
        async move {
            tx.send((headers.clone(), body.clone())).unwrap();
            let method = body["method"].as_str().unwrap();
            if method != "initialize" {
                assert_eq!(headers["mcp-session-id"], "fixture-session");
                assert_eq!(headers["mcp-protocol-version"], "2025-11-25");
            }
            if !body.as_object().unwrap().contains_key("id") {
                return StatusCode::ACCEPTED.into_response();
            }
            let result = match method {
                "initialize" => json!({"protocolVersion":body["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}),
                "tools/list" if body["params"]["cursor"] == "page2" => json!({"tools":[{"name":tool_names[1],"description":"Resolve an issue","inputSchema":{"type":"object","properties":{}}}]}),
                "tools/list" => json!({"tools":[{"name":tool_names[0],"description":"List issues","inputSchema":{"type":"object","properties":{"project":{"type":"integer"}}}}],"nextCursor":"page2"}),
                "tools/call" if body["params"]["arguments"]["hang"] == true => {
                    tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                    json!({"content":[]})
                },
                "tools/call" if body["params"]["arguments"]["expired"] == true => return StatusCode::NOT_FOUND.into_response(),
                "tools/call" if body["params"]["name"] == "resolve_issue" => json!({"content":[{"type":"text","text":"Permission denied"}],"isError":true}),
                "tools/call" => json!({"content":[{"type":"text","text":"One issue"}],"structuredContent":{"issues":[{"id":42}]},"isError":false}),
                _ => panic!("unexpected MCP method: {method}"),
            };
            let response = json!({"jsonrpc":"2.0","id":body["id"],"result":result});
            let mut response = if sse {
                ([("content-type", "text/event-stream")], format!("event: message\ndata: {response}\n\n")).into_response()
            } else {
                Json(response).into_response()
            };
            if method == "initialize" {
                response.headers_mut().insert("mcp-session-id", "fixture-session".parse().unwrap());
            }
            response
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, rx, task)
}

#[tokio::test]
async fn mcp_http_owner_can_call_tools_but_isolated_and_proactive_agents_cannot() {
    let (url, mut mcp_requests, mcp_task) =
        mcp_server(false, ["list_issues", "resolve_issue"]).await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let router = Router::new().route("/responses", post(move |Json(body): Json<Value>| {
        let tx = tx.clone();
        async move {
            tx.send(body.clone()).unwrap();
            let results: Vec<_> = body["input"].as_array().unwrap().iter().filter(|item| item["type"] == "function_call_output").collect();
            let failed = results.first().is_some_and(|item| serde_json::from_str::<Value>(item["output"].as_str().unwrap()).unwrap()["error"].is_string());
            let output = if results.len() >= 2 || failed {
                json!({"type":"message","content":[{"type":"output_text","text":"done"}]})
            } else if results.len() == 1 {
                json!({"type":"function_call","name":"mcp__rustrak__list_issues","call_id":"mcp-test","arguments":"{\"project\":7}"})
            } else {
                json!({"type":"function_call","name":"mcp_search","call_id":"mcp-search","arguments":"{\"query\":\"rustrak list issues\",\"limit\":1}"})
            };
            let response = json!({"output":[output]});
            if body["stream"] == true {
                ([("content-type", "text/event-stream")], format!("data: {}\n\ndata: [DONE]\n\n", json!({"type":"response.completed","response":response}))).into_response()
            } else {
                Json(response).into_response()
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let llm_task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let config: Config = toml::from_str(&format!(
        r#"
[gateway]
[provider.primary]
type = "openai"
model = "test"
api_key = "test"
api_base = "{base}"
[sessions]
auto_switch = false
[companion]
learning_enabled = false
[mcp.rustrak]
type = "http"
url = "{url}"
[mcp.rustrak.headers]
Authorization = "Bearer PRIVATE_MCP_TOKEN"
X-Tenant = "test-tenant"
"#
    ))
    .unwrap();
    for isolated in [false, true] {
        let data = tempfile::tempdir().unwrap();
        let app = build_app_at(&config, data.path(), isolated).await.unwrap();
        app.agent_runner.run_once("list issues").await.unwrap();
        let request_count = if isolated { 2 } else { 3 };
        for call in 0..request_count {
            let request = rx.recv().await.unwrap();
            assert!(!request.to_string().contains("PRIVATE_MCP_TOKEN"));
            let tools = request["tools"].as_array().unwrap();
            let mcp_tool = tools
                .iter()
                .find(|tool| tool["name"] == "mcp__rustrak__list_issues");
            assert_eq!(mcp_tool.is_some(), !isolated && call > 0);
            assert_eq!(
                tools.iter().any(|tool| tool["name"] == "mcp_search"),
                !isolated
            );
            assert_eq!(
                tools.iter().find(|tool| tool["name"] == "memory").unwrap()["strict"],
                true
            );
            if let Some(tool) = mcp_tool {
                assert_eq!(tool["strict"], false);
                assert_eq!(
                    tool["parameters"]["properties"]["project"]["type"],
                    "integer"
                );
                assert!(tool["parameters"].get("required").is_none());
                assert!(
                    tools
                        .iter()
                        .all(|tool| tool["name"] != "mcp__rustrak__resolve_issue")
                );
            }
            if call == request_count - 1 {
                let output = request["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|item| item["type"] == "function_call_output")
                    .last()
                    .unwrap();
                let output: Value =
                    serde_json::from_str(output["output"].as_str().unwrap()).unwrap();
                if isolated {
                    assert!(output["error"].is_string());
                } else {
                    assert_eq!(output["structuredContent"]["issues"][0]["id"], 42);
                    assert_eq!(output["content"][0]["text"], "One issue");
                }
            }
        }
        let mut session = Session::new(SessionId::new("companion:proactive:mcp"));
        app.proactive_agent
            .run(
                &mut session,
                UserMessage {
                    content: "probe".into(),
                    images: vec![],
                    files: vec![],
                },
                None,
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
                    .all(|tool| tool["name"] != "mcp_search"
                        && !tool["name"].as_str().unwrap().starts_with("mcp__"))
            );
        }
    }
    let mut methods = Vec::new();
    while let Ok((headers, request)) = mcp_requests.try_recv() {
        assert_eq!(headers["authorization"], "Bearer PRIVATE_MCP_TOKEN");
        assert_eq!(headers["x-tenant"], "test-tenant");
        if request["method"] == "tools/call" {
            assert_eq!(request["params"]["name"], "list_issues");
            assert_eq!(request["params"]["arguments"], json!({"project":7}));
        }
        methods.push(request["method"].as_str().unwrap().to_owned());
    }
    assert_eq!(
        methods
            .iter()
            .filter(|method| *method == "initialize")
            .count(),
        1
    );
    assert_eq!(
        methods
            .iter()
            .filter(|method| *method == "tools/call")
            .count(),
        1
    );
    mcp_task.abort();
    llm_task.abort();
}

#[tokio::test]
async fn mcp_http_sse_preserves_tool_errors_and_never_replays() {
    let (url, mut requests, task) = mcp_server(true, ["list_issues", "resolve_issue"]).await;
    let servers = std::collections::HashMap::from([(
        "rustrak".into(),
        crate::config::McpServerConfig::Http {
            url,
            headers: Default::default(),
        },
    )]);
    let tools = crate::agent::tools::mcp::connect(&servers).await.unwrap();
    let tool = tools
        .iter()
        .find(|tool| tool.name() == "mcp__rustrak__resolve_issue")
        .unwrap();
    assert_eq!(
        tool.replay_policy(),
        crate::agent::tools::ReplayPolicy::Never
    );
    assert!(tool.execute(Value::Null).await.is_err());
    let output = tool.execute(json!({})).await.unwrap();
    assert_eq!(output.output["isError"], true);
    assert_eq!(output.output["content"][0]["text"], "Permission denied");
    let mut calls = 0;
    while let Ok((_, request)) = requests.try_recv() {
        calls += usize::from(request["method"] == "tools/call");
    }
    assert_eq!(calls, 1);
    task.abort();
}

#[tokio::test]
async fn mcp_http_rejects_unsafe_config_and_duplicate_or_invalid_tool_names() {
    for (url, headers) in [
        ("file:///tmp/mcp", std::collections::HashMap::new()),
        ("https://user:password@example.com/mcp", Default::default()),
        ("https://example.com/mcp#fragment", Default::default()),
        (
            "https://example.com/mcp",
            std::collections::HashMap::from([(
                "Authorization".into(),
                "PRIVATE_TOKEN\ninvalid".into(),
            )]),
        ),
        (
            "https://example.com/mcp",
            std::collections::HashMap::from([("invalid\nname".into(), "PRIVATE_TOKEN".into())]),
        ),
    ] {
        let config = crate::config::McpServerConfig::Http {
            url: url.into(),
            headers,
        };
        assert!(!format!("{config:?}").contains("PRIVATE_TOKEN"));
        let servers = std::collections::HashMap::from([("rustrak".into(), config)]);
        let error = crate::agent::tools::mcp::connect(&servers)
            .await
            .err()
            .unwrap();
        let error = format!("{error:#}");
        assert!(error.contains("MCP server rustrak"), "{error}");
        assert!(!error.contains("PRIVATE_TOKEN"), "{error}");
    }
    for names in [
        ["list_issues", "list_issues"],
        ["invalid.name", "valid"],
        [
            "tool_with_a_name_that_exceeds_the_provider_limit_when_namespaced",
            "valid",
        ],
    ] {
        let (url, _requests, task) = mcp_server(false, names).await;
        let servers = std::collections::HashMap::from([(
            "rustrak".into(),
            crate::config::McpServerConfig::Http {
                url,
                headers: Default::default(),
            },
        )]);
        let error = crate::agent::tools::mcp::connect(&servers)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("Duplicate MCP tool name") || error.contains("MCP tool name"),
            "{error}"
        );
        task.abort();
    }
}

#[tokio::test]
async fn mcp_http_call_timeout_does_not_repeat_the_request() {
    let (url, mut requests, task) = mcp_server(false, ["list_issues", "resolve_issue"]).await;
    let servers = std::collections::HashMap::from([(
        "rustrak".into(),
        crate::config::McpServerConfig::Http {
            url,
            headers: Default::default(),
        },
    )]);
    let tools = crate::agent::tools::mcp::connect(&servers).await.unwrap();
    let tool = tools
        .into_iter()
        .find(|tool| tool.name() == "mcp__rustrak__list_issues")
        .unwrap();
    let call = tokio::spawn(async move { tool.execute(json!({"hang":true})).await });
    while requests.recv().await.unwrap().1["method"] != "tools/call" {}
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(61)).await;
    let error = call.await.unwrap().unwrap_err();
    assert!(format!("{error:#}").contains("timed out after 60 seconds"));
    while let Ok((_, request)) = requests.try_recv() {
        assert_ne!(request["method"], "tools/call");
    }
    task.abort();
}

#[tokio::test]
async fn mcp_http_expired_session_does_not_reinitialize_or_repeat_a_call() {
    let (url, mut requests, task) = mcp_server(false, ["list_issues", "resolve_issue"]).await;
    let servers = std::collections::HashMap::from([(
        "rustrak".into(),
        crate::config::McpServerConfig::Http {
            url,
            headers: Default::default(),
        },
    )]);
    let tools = crate::agent::tools::mcp::connect(&servers).await.unwrap();
    assert!(tools[0].execute(json!({"expired":true})).await.is_err());
    let mut methods = Vec::new();
    while let Ok((_, request)) = requests.try_recv() {
        methods.push(request["method"].as_str().unwrap().to_owned());
    }
    assert_eq!(
        methods
            .iter()
            .filter(|method| *method == "initialize")
            .count(),
        1
    );
    assert_eq!(
        methods
            .iter()
            .filter(|method| *method == "tools/call")
            .count(),
        1
    );
    task.abort();
}

#[tokio::test]
async fn mcp_http_startup_errors_hide_headers_and_redirects_are_not_followed() {
    for status in [StatusCode::UNAUTHORIZED, StatusCode::TEMPORARY_REDIRECT] {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let redirected = tx.clone();
        let router = Router::new()
            .route(
                "/mcp",
                post(move |headers: HeaderMap| {
                    let tx = tx.clone();
                    async move {
                        tx.send(headers).unwrap();
                        (status, [("location", "/must-not-receive-credentials")])
                    }
                }),
            )
            .route(
                "/must-not-receive-credentials",
                post(move |headers: HeaderMap| {
                    let redirected = redirected.clone();
                    async move {
                        redirected.send(headers).unwrap();
                        StatusCode::OK
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let servers = std::collections::HashMap::from([(
            "rustrak".into(),
            crate::config::McpServerConfig::Http {
                url,
                headers: std::collections::HashMap::from([(
                    "Authorization".into(),
                    "Bearer PRIVATE_TOKEN".into(),
                )]),
            },
        )]);
        let error = crate::agent::tools::mcp::connect(&servers)
            .await
            .err()
            .unwrap();
        let error = format!("{error:#}");
        assert!(error.contains("MCP server rustrak"), "{error}");
        assert!(!error.contains("PRIVATE_TOKEN"), "{error}");
        assert_eq!(
            rx.recv().await.unwrap()["authorization"],
            "Bearer PRIVATE_TOKEN"
        );
        assert!(rx.try_recv().is_err());
        task.abort();
    }
}

#[tokio::test]
async fn mcp_http_startup_has_a_deadline() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let router = Router::new().route(
        "/mcp",
        post(move || {
            let tx = tx.clone();
            async move {
                tx.send(()).unwrap();
                std::future::pending::<()>().await;
                StatusCode::OK
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let servers = std::collections::HashMap::from([(
        "rustrak".into(),
        crate::config::McpServerConfig::Http {
            url,
            headers: Default::default(),
        },
    )]);
    let connection = tokio::spawn(async move { crate::agent::tools::mcp::connect(&servers).await });
    rx.recv().await.unwrap();
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    let error = connection.await.unwrap().err().unwrap().to_string();
    assert!(error.contains("timed out after 30 seconds"), "{error}");
    task.abort();
}
