use axum::{Json, Router, response::IntoResponse, routing::post};
use serde_json::{Value, json};
use std::{fs, time::Duration};

#[tokio::test]
async fn cli_loads_agents_only_from_its_launch_directory() {
    let runtime = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let nested = project.path().join("nested");
    fs::create_dir(&nested).unwrap();
    fs::write(project.path().join("AGENTS.md"), "PROJECT_RULE_SENTINEL").unwrap();
    let (sender, mut received) = tokio::sync::mpsc::unbounded_channel();
    let router = Router::new().route("/responses", post(move |Json(body): Json<Value>| {
        let sender = sender.clone();
        async move {
            sender.send(body["instructions"].as_str().unwrap().to_owned()).unwrap();
            let response = json!({"output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]}]});
            if body["stream"] == true {
                ([("content-type", "text/event-stream")], format!("data: {}\n\ndata: [DONE]\n\n", json!({"type":"response.completed", "response":response}))).into_response()
            } else { Json(response).into_response() }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let data = runtime.path().join(".kodrik");
    fs::create_dir(&data).unwrap();
    fs::write(data.join("AGENTS.md"), "PERSONAL_RULE_SENTINEL").unwrap();
    fs::write(data.join("config.toml"), format!(
        "[gateway]\n[companion]\nlearning_enabled=false\nproactive_enabled=false\n[provider.primary]\ntype='openai'\nmodel='test'\napi_key='test'\napi_base='{endpoint}'\n"
    )).unwrap();

    for (directory, expected) in [(project.path(), true), (nested.as_path(), false)] {
        let output = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::process::Command::new(env!("CARGO_BIN_EXE_kodrik"))
                .args(["ask", "hello"])
                .current_dir(directory)
                .env("HOME", runtime.path())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let instructions = received.try_recv().unwrap();
        assert_eq!(instructions.contains("PROJECT_RULE_SENTINEL"), expected);
        assert!(!instructions.contains("PERSONAL_RULE_SENTINEL"));
        assert!(received.try_recv().is_err());
    }
    server.abort();
}
