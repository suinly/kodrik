use super::*;
use futures::StreamExt;

#[tokio::test]
async fn child_sessions_are_listed_searchable_and_read_only() {
    use crate::agent::session::{Session, SessionId};
    let f = Fixture::new().await;
    let store = SqliteSessionStore::open(f.data.path().join("owner-sessions")).unwrap();
    let id = "companion:subagent:listed";
    for session_id in [id, "companion:private", "companion:wakeup:hidden"] {
        let mut session = Session::new(SessionId::new(session_id));
        session.push(Message::User(crate::agent::UserMessage {
            content: "Проверить авторизацию".into(),
            files: vec![],
            images: vec![],
        }));
        store.save(&session).await.unwrap();
    }
    let parent = store
        .load(&SessionId::new("companion:private"))
        .await
        .unwrap()
        .unwrap();
    let child = json!({"id":id,"parent":parent.id().as_str(),"generation":parent.generation(),"submission":"fixture","status":"completed","task":"Проверить авторизацию","result":null,"error":null});
    store
        .with_connection(move |db| {
            db.execute(
                "INSERT INTO subagent_sessions VALUES(?1,?2,?3,?4)",
                rusqlite::params![
                    id,
                    child["parent"].as_str().unwrap(),
                    child["generation"].as_str().unwrap(),
                    serde_json::to_vec(&child)?
                ],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    for path in ["sessions", "sessions?q=авторизацию"] {
        let entries: Vec<Value> = f
            .request(reqwest::Method::GET, path)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["id"], id);
        assert_eq!(entries[0]["subagent"], true);
        assert_eq!(entries[0]["title"], "Проверить авторизацию");
    }
    let history = f.history(id).await;
    assert_eq!(history["subagent"], true);
    assert_eq!(
        history["messages"][0]["parts"][0]["text"],
        "Проверить авторизацию"
    );
    assert_eq!(
        f.request(reqwest::Method::GET, &format!("sessions/{id}/run"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        f.request(reqwest::Method::POST, "send")
            .json(&json!({"id":id,"text":"do not start a web worker"}))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}

#[tokio::test]
async fn child_tasks_stream_live_status_and_stop_only_within_their_parent() {
    let fixture = Fixture::new().await;
    let parent = fixture.create().await;
    let other = fixture.create().await;
    fixture
        .request(reqwest::Method::POST, "send")
        .json(&json!({"id":parent,"text":"subagents"}))
        .send()
        .await
        .unwrap();
    let response = fixture
        .request(reqwest::Method::GET, &format!("sessions/{parent}/events"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut stream = response.bytes_stream();
    let snapshot = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut buffer = String::new();
        'snapshots: loop {
            buffer.push_str(std::str::from_utf8(&stream.next().await.unwrap().unwrap()).unwrap());
            while let Some(end) = buffer.find("\n\n") {
                let frame = buffer.drain(..end + 2).collect::<String>();
                let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) else {
                    continue;
                };
                let snapshot: Value = serde_json::from_str(data).unwrap();
                assert!(
                    snapshot["subagents"].is_array(),
                    "child tasks must be included in live snapshots"
                );
                let waiting = snapshot["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| {
                        message["parts"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|part| part["toolName"] == "wait_agent")
                    });
                if waiting
                    && snapshot["subagents"].as_array().is_some_and(|children| {
                        children.len() == 2
                            && children.iter().any(|child| child["status"] == "completed")
                    })
                {
                    break 'snapshots snapshot;
                }
            }
        }
    })
    .await
    .unwrap();
    let children = snapshot["subagents"].as_array().unwrap();
    let running = children
        .iter()
        .find(|child| child["status"] == "running")
        .unwrap();
    let done = children
        .iter()
        .find(|child| child["status"] == "completed")
        .unwrap();
    assert_eq!(running["task"], "Проверить авторизацию");
    assert_eq!(done["task"], "Проверить тесты");
    assert_eq!(done["result"], "Проверка тестов завершена");
    let tools: Vec<_> = snapshot["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["parts"].as_array().unwrap())
        .filter(|p| p["type"] == "dynamic-tool")
        .collect();
    assert_eq!(
        tools.len(),
        3,
        "child status changes must stream while the parent is still waiting"
    );
    assert!(tools.iter().all(|tool| tool["state"] != "output-error"));
    let child = running["id"].as_str().unwrap();
    let path = format!("sessions/{parent}/subagents/{child}/stop");
    assert_eq!(
        fixture
            .request(
                reqwest::Method::POST,
                &format!("sessions/{other}/subagents/{child}/stop")
            )
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        fixture
            .client
            .post(format!("{}api/chat/{path}", fixture.base))
            .header("Cookie", &fixture.cookie)
            .header("Origin", "https://other.invalid")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        fixture
            .client
            .post(format!("{}api/chat/{path}", fixture.base))
            .header("Origin", &fixture.origin)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let foreign: Value = fixture
        .request(reqwest::Method::GET, &format!("sessions/{other}/run"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(foreign["subagents"], json!([]));
    assert_eq!(
        fixture
            .request(reqwest::Method::GET, &format!("sessions/{child}/run"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let stopped: Value = fixture
        .request(reqwest::Method::POST, &path)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stopped["status"], "stopped");
    fixture.idle(&parent).await;
    let latest: Value = fixture
        .request(reqwest::Method::GET, &format!("sessions/{parent}/run"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(latest["revision"].as_i64().unwrap() > snapshot["revision"].as_i64().unwrap());
    assert!(
        latest["subagents"]
            .as_array()
            .unwrap()
            .iter()
            .all(|child| child["status"] != "running")
    );
}
