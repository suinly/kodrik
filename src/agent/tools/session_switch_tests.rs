use super::*;
use crate::agent::{
    LlmContext,
    message::ToolCall,
    session::{SessionStore, SqliteSessionStore},
    tools::{Tool, session_switch::SwitchSessionTool},
};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering::SeqCst},
};

struct HandoffModel {
    create_new: bool,
    seen: Mutex<Vec<Vec<Message>>>,
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl LlmProvider for HandoffModel {
    async fn generate(
        &self,
        context: &LlmContext<'_>,
        _: &[&dyn Tool],
        _: Option<&mpsc::Sender<AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        self.seen.lock().unwrap().push(context.messages.to_vec());
        let messages = if self.calls.fetch_add(1, SeqCst) == 0 {
            vec![
                Message::ToolCall(ToolCall {
                    call_id: "move".into(),
                    name: "switch_session".into(),
                    arguments: if self.create_new {
                        serde_json::json!({"create_new":true,"context":"New unrelated task; completed effect must not replay"})
                    } else {
                        serde_json::json!({"session_id":"target","context":"Continue task; completed effect must not replay"})
                    },
                }),
                Message::ToolCall(ToolCall {
                    call_id: "effect".into(),
                    name: "effect".into(),
                    arguments: serde_json::json!({}),
                }),
            ]
        } else {
            vec![Message::Assistant(AssistantMessage {
                content: "done".into(),
            })]
        };
        Ok((messages, Some(10)))
    }
}
struct Effect(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl Tool for Effect {
    fn name(&self) -> String {
        "effect".into()
    }
    fn description(&self) -> String {
        "test effect".into()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{},"additionalProperties":false})
    }
    async fn execute(
        &self,
        _: serde_json::Value,
    ) -> anyhow::Result<crate::agent::tools::ToolOutput> {
        self.0.fetch_add(1, SeqCst);
        Ok(serde_json::json!({"effect":"completed"}).into())
    }
}
fn user(content: &str) -> UserMessage {
    UserMessage {
        content: content.into(),
        images: vec![],
        files: vec![],
    }
}

#[tokio::test]
async fn handoff_core_moves_after_whole_batch_and_future_input_uses_target_history() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().join("sessions")).unwrap();
    let mut target = Session::new(SessionId::new("target"));
    target.push(Message::User(user("target question")));
    target.push(Message::Assistant(AssistantMessage {
        content: "target answer".into(),
    }));
    store.save(&target).await.unwrap();
    let mut source = Session::new(SessionId::new("source"));
    source.push(Message::User(user("old unrelated source history")));
    let model = Arc::new(HandoffModel {
        create_new: false,
        seen: Mutex::new(vec![]),
        calls: AtomicUsize::new(0),
    });
    let effects = Arc::new(AtomicUsize::new(0));
    let agent = Agent::new(
        model.clone(),
        ToolRegistry::default()
            .register(Arc::new(SwitchSessionTool))
            .register(Arc::new(Effect(effects.clone()))),
        ContextBuilder::new("rules"),
    );
    assert!(!agent.child_agent().tools.contains("switch_session"));
    let mut input = user("original task");
    input.images.push(crate::agent::Image {
        data_url: "data:image/png;base64,dGVzdA==".into(),
        width: 1,
        height: 1,
    });
    input.files.push(
        crate::agent::FileAttachment::save(
            dir.path(),
            "task.txt",
            Some("text/plain"),
            b"task attachment",
        )
        .unwrap(),
    );
    let mut cp = Checkpoint::new(&store, source.id(), &source);
    let (tx, mut events) = mpsc::channel(32);
    agent
        .run_durable(&mut source, input.clone(), Some(&tx), None, Some(&mut cp))
        .await
        .unwrap();
    let mut switched_title = None;
    while let Ok(event) = events.try_recv() {
        if let AgentEvent::SessionChanged { title, .. } = event {
            switched_title = title;
        }
    }
    assert_eq!(switched_title.as_deref(), Some("target question"));
    assert_eq!(source.id().as_str(), "target");
    assert_eq!(effects.load(SeqCst), 1);
    let seen = model.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(matches!(&seen[1][0],Message::User(u) if u.content=="target question"));
    assert!(matches!(&seen[1][1],Message::Assistant(a) if a.content=="target answer"));
    assert!(seen[1].iter().any(|m| matches!(m,Message::User(u) if u.content=="original task" && u.images.len()==1 && u.files.len()==1)));
    assert!(
        !seen[1]
            .iter()
            .any(|m| matches!(m,Message::User(u) if u.content=="old unrelated source history"))
    );
    drop(seen);
    let preserved = store
        .load(&SessionId::new("source"))
        .await
        .unwrap()
        .unwrap();
    assert!(
        preserved
            .messages()
            .iter()
            .any(|m| matches!(m,Message::ToolResult(r) if r.call_id=="effect"))
    );
    assert_eq!(
        store.active_id(&SessionId::new("source")).await.unwrap(),
        SessionId::new("target")
    );
    let (record, _) = store.load_run(&cp.record.id).await.unwrap().unwrap();
    assert_eq!(record.session_id, "target");
    assert!(matches!(record.phase, RunPhase::Completed));
    assert_eq!(record.handoff_hops, 1);
    let mut next = store
        .load(&store.active_id(&SessionId::new("source")).await.unwrap())
        .await
        .unwrap()
        .unwrap();
    let mut next_cp = Checkpoint::new(&store, &SessionId::new("source"), &next);
    agent
        .run_durable(&mut next, user("follow up"), None, None, Some(&mut next_cp))
        .await
        .unwrap();
    assert_eq!(effects.load(SeqCst), 1);
    assert_eq!(next.id().as_str(), "target");
    assert!(
        model.seen.lock().unwrap()[2]
            .iter()
            .any(|m| matches!(m,Message::User(u) if u.content=="follow up"))
    );
}

#[tokio::test]
async fn handoff_core_resumes_saved_pending_result_without_replaying_effects() {
    use crate::agent::{
        message::ToolResult,
        run_state::ToolExecution,
        tools::{ReplayPolicy, session_switch::HandoffRequest},
    };
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().into()).unwrap();
    let mut target = Session::new(SessionId::new("target"));
    target.push(Message::User(user("saved target history")));
    store.save(&target).await.unwrap();
    let mut source = Session::new(SessionId::new("source"));
    source.push(Message::User(user("original task")));
    let call = ToolCall {
        call_id: "move".into(),
        name: "switch_session".into(),
        arguments: serde_json::json!({"session_id":"target","context":"effect already completed"}),
    };
    source.push(Message::ToolCall(call.clone()));
    let output: crate::agent::tools::ToolOutput = serde_json::json!({"handoff":"pending"}).into();
    source.push(Message::ToolResult(ToolResult {
        call_id: call.call_id.clone(),
        content: output.clone(),
    }));
    let mut cp = Checkpoint::new(&store, source.id(), &source);
    cp.record.isolated = Some(false);
    cp.record.pending_handoff = Some(HandoffRequest {
        create_new: false,
        session_id: "target".into(),
        context: "effect already completed".into(),
    });
    cp.save(
        &source,
        RunPhase::NeedModel,
        Some(ToolExecution {
            call,
            replay: ReplayPolicy::Safe,
            result: Some(output),
        }),
    )
    .await
    .unwrap();
    let id = cp.record.id.clone();
    drop(cp);
    let (record, mut source) = store.load_run(&id).await.unwrap().unwrap();
    let ownership = store.claim_execution("source", "source").await.unwrap();
    let mut cp = Checkpoint::resume(&store, record, ownership);
    let model = Arc::new(HandoffModel {
        create_new: false,
        seen: Mutex::new(vec![]),
        calls: AtomicUsize::new(1),
    });
    let effects = Arc::new(AtomicUsize::new(0));
    let agent = Agent::new(
        model.clone(),
        ToolRegistry::default()
            .register(Arc::new(SwitchSessionTool))
            .register(Arc::new(Effect(effects.clone()))),
        ContextBuilder::new("rules"),
    );
    agent.resume_durable(&mut source, &mut cp).await.unwrap();
    assert_eq!(source.id().as_str(), "target");
    assert_eq!(cp.record.handoff_hops, 1);
    assert_eq!(effects.load(SeqCst), 0);
    assert_eq!(model.seen.lock().unwrap().len(), 1);
    assert!(
        model.seen.lock().unwrap()[0]
            .iter()
            .any(|m| matches!(m,Message::User(u) if u.content=="saved target history"))
    );
    assert!(
        store
            .tool_execution(&id, "move")
            .await
            .unwrap()
            .unwrap()
            .result
            .is_some()
    );
}

#[tokio::test]
async fn handoff_core_creates_dialogue_and_announces_creation_without_replaying_effects() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteSessionStore::open(dir.path().into()).unwrap();
    let mut source = Session::new(SessionId::new("source"));
    source.push(Message::User(user("old unrelated history")));
    let model = Arc::new(HandoffModel {
        create_new: true,
        seen: Mutex::new(vec![]),
        calls: AtomicUsize::new(0),
    });
    let effects = Arc::new(AtomicUsize::new(0));
    let agent = Agent::new(
        model.clone(),
        ToolRegistry::default()
            .register(Arc::new(SwitchSessionTool))
            .register(Arc::new(Effect(effects.clone()))),
        ContextBuilder::new("rules"),
    );
    let mut cp = Checkpoint::new(&store, source.id(), &source);
    let (tx, mut events) = mpsc::channel(32);
    agent
        .run_durable(
            &mut source,
            user("new topic"),
            Some(&tx),
            None,
            Some(&mut cp),
        )
        .await
        .unwrap();
    assert_ne!(source.id().as_str(), "source");
    assert_eq!(
        store.active_id(&SessionId::new("source")).await.unwrap(),
        *source.id()
    );
    assert_eq!(effects.load(SeqCst), 1);
    assert!(
        model.seen.lock().unwrap()[1]
            .iter()
            .all(|m| !matches!(m, Message::User(u) if u.content == "old unrelated history"))
    );
    let mut created = false;
    while let Ok(event) = events.try_recv() {
        if let AgentEvent::SessionChanged { title, .. } = event {
            assert!(title.is_none());
            created = true;
        }
    }
    assert!(created);
    assert_eq!(store.list().await.unwrap().len(), 2);
}
