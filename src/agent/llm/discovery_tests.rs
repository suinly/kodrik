use super::*;
use crate::agent::{
    ContextBuilder, LlmContext,
    llm::{LlmProvider, OpenAiLlm},
    message::{ToolCall, ToolResult, truncate_tool_outputs},
    session::{Session, SessionId},
    tools::{ToolOutput, ToolRegistry},
};
use std::sync::Arc;

struct Records;
#[async_trait::async_trait]
impl Tool for Records {
    fn name(&self) -> String {
        "datasets".into()
    }
    fn description(&self) -> String {
        "Read records".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"id":{"type":"integer"}},"required":["id"],"additionalProperties":false})
    }
    async fn execute(&self, arguments: Value) -> anyhow::Result<ToolOutput> {
        Ok(arguments.into())
    }
}
fn registry() -> ToolRegistry {
    ToolRegistry::default()
        .register(Arc::new(Records))
        .with_deferred_builtin_schemas()
}
async fn discover(registry: &ToolRegistry) -> Vec<Message> {
    let arguments = json!({"query":"datasets","limit":1});
    vec![
        Message::ToolCall(ToolCall {
            call_id: "search".into(),
            name: SEARCH.into(),
            arguments: arguments.clone(),
        }),
        Message::ToolResult(ToolResult {
            call_id: "search".into(),
            content: registry.execute(SEARCH, arguments).await.unwrap(),
        }),
    ]
}

#[tokio::test]
async fn compatible_call_restores_identity_and_rejects_unloaded_or_invalid_targets() {
    let registry = registry();
    let history = discover(&registry).await;
    let tools = registry.for_model(&history).unwrap();
    let call = |name: &str, arguments: &str| {
        vec![Message::ToolCall(ToolCall {
            call_id: "execute".into(),
            name: CALL.into(),
            arguments: json!({"name":name,"arguments":arguments}),
        })]
    };
    let mut valid = call("datasets", r#"{"id":42}"#);
    normalize_calls(&mut valid, &registry.for_model(&[]).unwrap());
    assert!(matches!(&valid[0],Message::ToolCall(call) if call.name == CALL));
    normalize_calls(&mut valid, &tools);
    let Message::ToolCall(restored) = &valid[0] else {
        panic!("tool call lost")
    };
    assert_eq!(restored.name, "datasets");
    assert_eq!(restored.call_id, "execute");
    assert_eq!(restored.arguments, json!({"id":42}));
    for (name, args) in [
        ("exec_command", "{}"),
        (SEARCH, "{}"),
        ("missing", "{}"),
        ("datasets", "[]"),
        ("datasets", "bad"),
    ] {
        let mut invalid = call(name, args);
        normalize_calls(&mut invalid, &tools);
        assert!(
            matches!(&invalid[0],Message::ToolCall(call) if call.name == CALL),
            "accepted {name}: {args}"
        );
        assert!(
            registry
                .execute(CALL, json!({"name":name,"arguments":args}))
                .await
                .is_err()
        );
    }
    let mut revoked = call("datasets", "{}");
    normalize_calls(
        &mut revoked,
        &registry.without(&["datasets"]).for_model(&history).unwrap(),
    );
    assert!(matches!(&revoked[0],Message::ToolCall(call) if call.name == CALL));
}

#[tokio::test]
async fn discovery_schema_snapshot_survives_context_truncation() {
    let registry = registry();
    let mut history = discover(&registry).await;
    let Message::ToolResult(result) = &mut history[1] else {
        unreachable!()
    };
    result.content.output["tool_definitions"][0]["description"] = json!("schema ".repeat(6000));
    let before = serde_json::to_value(&history).unwrap();
    let projected = truncate_tool_outputs(history.into());
    assert!(
        serde_json::to_value(projected).unwrap() == before,
        "discovery schema snapshot was truncated"
    );
}

#[tokio::test]
async fn native_discovery_restores_schemas_from_legacy_search_results() {
    let registry = registry();
    let mut history = discover(&registry).await;
    let Message::ToolResult(result) = &mut history[1] else {
        unreachable!()
    };
    result
        .content
        .output
        .as_object_mut()
        .unwrap()
        .remove("tool_definitions");
    let output = result.content.output.to_string();
    let tools = registry.for_model(&history).unwrap();
    let mut request = json!({"tools":[{"type":"function","name":SEARCH},{"type":"function","name":"datasets"}],"input":[
        {"type":"function_call","name":SEARCH,"call_id":"search","arguments":r#"{"query":"datasets","limit":1}"#},
        {"type":"function_call_output","call_id":"search","output":output}
    ]});
    project(&mut request, &tools, ToolDiscovery::Native).unwrap();
    assert_eq!(
        request["input"][1]["tools"][0]["tools"][0]["name"],
        "datasets"
    );
    assert_eq!(
        request["input"][1]["tools"][0]["tools"][0]["parameters"]["properties"]["id"]["type"],
        "integer"
    );
}

#[tokio::test]
async fn legacy_native_schema_prefix_does_not_change_when_selection_evicts_a_tool() {
    struct Other(String);
    #[async_trait::async_trait]
    impl Tool for Other {
        fn name(&self) -> String {
            self.0.clone()
        }
        fn description(&self) -> String {
            "Other tool".into()
        }
        fn parameters(&self) -> Value {
            json!({"type":"object","properties":{}})
        }
        fn deferred(&self) -> bool {
            true
        }
        async fn execute(&self, _: Value) -> anyhow::Result<ToolOutput> {
            Ok(json!({}).into())
        }
    }
    let mut registry = registry();
    for index in 0..33 {
        registry = registry.register(Arc::new(Other(format!("other_{index}"))));
    }
    let registry = registry.with_mcp_search();
    let mut history = discover(&registry).await;
    let Message::ToolResult(result) = &mut history[1] else {
        unreachable!()
    };
    result
        .content
        .output
        .as_object_mut()
        .unwrap()
        .remove("tool_definitions");
    let output = result.content.output.to_string();
    let initial = json!({"tools":[{"type":"function","name":SEARCH}],"input":[
        {"type":"function_call","name":SEARCH,"call_id":"search","arguments":"{}"},
        {"type":"function_call_output","call_id":"search","output":output}
    ]});
    let mut first = initial.clone();
    project(
        &mut first,
        &registry.for_model(&history).unwrap(),
        ToolDiscovery::Native,
    )
    .unwrap();
    for index in 0..33 {
        history.push(Message::ToolCall(ToolCall {
            call_id: format!("other-search-{index}"),
            name: SEARCH.into(),
            arguments: json!({}),
        }));
        history.push(Message::ToolResult(ToolResult {
            call_id: format!("other-search-{index}"),
            content: json!({"loaded_tools":[format!("other_{index}")]}).into(),
        }));
    }
    let tools = registry.for_model(&history).unwrap();
    assert!(!tools.iter().any(|tool| tool.name() == "datasets"));
    let mut after = initial;
    project(&mut after, &tools, ToolDiscovery::Native).unwrap();
    assert_eq!(first, after);
}

#[tokio::test]
async fn native_and_compatible_discovery_keep_wire_prefix_through_reloaded_history() {
    use axum::{Json, Router, response::IntoResponse, routing::post};
    use tokio::sync::mpsc;
    for mode in [ToolDiscovery::Compatible, ToolDiscovery::Native] {
        for streaming in [false, true] {
            let (capture, mut requests) = mpsc::unbounded_channel();
            let app = Router::new().route("/responses", post(move |Json(body): Json<Value>| {
                let capture = capture.clone();
                async move {
                    let step = body["input"].as_array().unwrap().iter().filter(|item| matches!(item["type"].as_str(), Some("tool_search_output" | "function_call_output"))).count();
                    let output = if step == 0 {
                        if mode == ToolDiscovery::Native { json!({"type":"tool_search_call","execution":"client","call_id":"search","status":"completed","arguments":{"query":"datasets","limit":1}}) }
                        else { json!({"type":"function_call","call_id":"search","name":SEARCH,"arguments":r#"{"query":"datasets","limit":1}"#}) }
                    } else if step == 1 {
                        if mode == ToolDiscovery::Native { json!({"type":"function_call","call_id":"execute","name":"datasets","namespace":NAMESPACE,"arguments":r#"{"id":42}"#}) }
                        else { json!({"type":"function_call","call_id":"execute","name":CALL,"arguments":json!({"name":"datasets","arguments":r#"{"id":42}"#}).to_string()}) }
                    } else { json!({"type":"message","content":[{"type":"output_text","text":"done"}]}) };
                    let response = json!({"output":[output],"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":80},"output_tokens":5,"total_tokens":105}});
                    let stream = body["stream"] == true;
                    capture.send(body).unwrap();
                    if stream { ([("content-type","text/event-stream")], format!("data: {}\n\ndata: [DONE]\n\n",json!({"type":"response.completed","response":response}))).into_response() }
                    else { Json(response).into_response() }
                }
            }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let llm = OpenAiLlm::new(
                "test",
                Some("test"),
                Some(format!("http://{}", listener.local_addr().unwrap())),
            )
            .with_tool_discovery(mode)
            .without_retries();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let registry = registry();
            let builder = ContextBuilder::new("base");
            let mut session = Session::new(SessionId::new("discovery"));
            let mut recorded = Vec::new();
            for step in 0..3 {
                let (progress, mut events) = mpsc::channel(32);
                let (messages, usage) = llm
                    .generate_with_usage(
                        &builder.build(&session),
                        &registry.for_model(session.messages()).unwrap(),
                        streaming.then_some(&progress),
                    )
                    .await
                    .unwrap();
                assert_eq!(usage.cached_tokens, Some(80));
                assert_eq!(messages.len(), 1);
                let message = messages.into_iter().next().unwrap();
                match &message {
                    Message::ToolCall(call) => {
                        assert_eq!(call.name, if step == 0 { SEARCH } else { "datasets" });
                        let result = registry
                            .execute(&call.name, call.arguments.clone())
                            .await
                            .unwrap();
                        session.push(message.clone());
                        session.push(Message::ToolResult(ToolResult {
                            call_id: call.call_id.clone(),
                            content: result,
                        }));
                    }
                    Message::Assistant(answer) => assert_eq!(answer.content, "done"),
                    _ => panic!("unexpected model output"),
                }
                while events.try_recv().is_ok() {}
                session = serde_json::from_value(serde_json::to_value(&session).unwrap()).unwrap();
                recorded.push(requests.recv().await.unwrap());
            }
            for request in &recorded {
                serde_json::from_value::<async_openai::types::responses::CreateResponse>(
                    request.clone(),
                )
                .unwrap();
            }
            for next in &recorded[1..] {
                assert_eq!(recorded[0]["tools"], next["tools"]);
                assert_eq!(recorded[0]["instructions"], next["instructions"]);
            }
            let prefix = recorded[1]["input"].as_array().unwrap();
            assert_eq!(
                prefix,
                &recorded[2]["input"].as_array().unwrap()[..prefix.len()]
            );
            if mode == ToolDiscovery::Native {
                assert_eq!(recorded[0]["tools"][0]["type"], "tool_search");
                assert_eq!(recorded[1]["input"][1]["tools"][0]["name"], NAMESPACE);
                assert_eq!(
                    recorded[1]["input"][1]["tools"][0]["tools"][0]["defer_loading"],
                    true
                );
                assert_eq!(recorded[2]["input"][2]["namespace"], NAMESPACE);
            } else {
                assert_eq!(recorded[2]["input"][2]["name"], CALL);
            }
            server.abort();
        }
    }
}

#[tokio::test]
async fn positioned_developer_context_emits_multiple_notes_and_tail() {
    use axum::{Json, Router, routing::post};
    use tokio::sync::mpsc;
    let (capture, mut requests) = mpsc::unbounded_channel();
    let app = Router::new().route(
        "/responses",
        post(move |Json(body): Json<Value>| {
            let capture = capture.clone();
            async move {
                capture.send(body).unwrap();
                Json(json!({"output":[]}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let llm = OpenAiLlm::new(
        "test",
        Some("test"),
        Some(format!("http://{}", listener.local_addr().unwrap())),
    )
    .without_retries();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let messages: Vec<Message> =
        serde_json::from_value(json!([{"User":{"content":"hello"}}])).unwrap();
    let context = LlmContext {
        instructions: "base".into(),
        messages: messages.into(),
        turn_clocks: vec![
            (0, "receipt".into()),
            (0, "clock".into()),
            (1, "tail".into()),
        ],
    };
    llm.generate(&context, &[], None).await.unwrap();
    let request = requests.recv().await.unwrap();
    let input = request["input"].as_array().unwrap();
    assert_eq!(input.len(), 4);
    assert_eq!(
        input
            .iter()
            .map(|item| item["content"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["receipt", "clock", "hello", "tail"]
    );
    server.abort();
}

#[tokio::test]
async fn invalid_compatible_call_returns_a_tool_error_and_the_agent_repairs_it() {
    use crate::agent::{Agent, UserMessage};
    use axum::{Json, Router, routing::post};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    let app = Router::new().route("/responses", post(move |Json(body): Json<Value>| {
        let step = observed.fetch_add(1, Ordering::SeqCst);
        async move {
            let output = match step {
                0 => json!({"type":"function_call","name":SEARCH,"call_id":"search","arguments":r#"{"query":"datasets","limit":1}"#}),
                1 => json!({"type":"function_call","name":CALL,"call_id":"bad","arguments":json!({"name":"missing","arguments":"[]"}).to_string()}),
                2 => {
                    let result = body["input"].as_array().unwrap().iter().find(|item|item["call_id"] == "bad" && item["type"] == "function_call_output").unwrap();
                    let error: Value = serde_json::from_str(result["output"].as_str().unwrap()).unwrap();
                    assert!(error["error"].as_str().unwrap().contains("mcp_search"));
                    json!({"type":"function_call","name":CALL,"call_id":"good","arguments":json!({"name":"datasets","arguments":r#"{"id":42}"#}).to_string()})
                }
                3 => json!({"type":"message","content":[{"type":"output_text","text":"repaired"}]}),
                _ => panic!("unexpected extra model call"),
            };
            Json(json!({"output":[output]}))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let llm = OpenAiLlm::new(
        "test",
        Some("test"),
        Some(format!("http://{}", listener.local_addr().unwrap())),
    )
    .without_retries();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let agent = Agent::new(Arc::new(llm), registry(), ContextBuilder::new("base"));
    let mut session = Session::new(SessionId::new("repair"));
    let result = agent
        .run(
            &mut session,
            UserMessage {
                content: "Find records".into(),
                browser_client_id: None,
                browser_context: None,
                images: vec![],
                files: vec![],
            },
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(result.content, "repaired");
    assert_eq!(count.load(Ordering::SeqCst), 4);
    assert!(session.messages().iter().any(|message|matches!(message, Message::ToolCall(call) if call.call_id == "good" && call.name == "datasets")));
    server.abort();
}
