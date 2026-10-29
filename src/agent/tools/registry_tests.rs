use super::*;
use crate::agent::{
    message::{ToolCall, ToolResult},
    tools::mcp_search::McpSearchTool,
};
use serde_json::json;

struct Function {
    name: String,
    deferred: bool,
}
#[async_trait::async_trait]
impl Tool for Function {
    fn name(&self) -> String {
        self.name.clone()
    }
    fn description(&self) -> String {
        "Read an issue".into()
    }
    fn parameters(&self) -> serde_json::Value {
        json!({"type":"object","properties":{"id":{"type":"integer"}}})
    }
    fn deferred(&self) -> bool {
        self.deferred
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolOutput> {
        Ok(json!({"name":self.name}).into())
    }
}

fn registry(native: usize, groups: usize) -> ToolRegistry {
    let mut registry = ToolRegistry::default().register(Arc::new(Function {
        name: "show_ui".into(),
        deferred: false,
    }));
    for i in 1..native {
        registry = registry.register(Arc::new(Function {
            name: format!("native_{i:03}"),
            deferred: false,
        }));
    }
    let mut catalog = Vec::new();
    for group in 0..groups {
        for i in 0..16 {
            let name = format!("mcp__rustrak__group{group}_tool{i:02}");
            catalog.push((name.clone(), "Read an issue".into()));
            registry = registry.register(Arc::new(Function {
                name,
                deferred: true,
            }));
        }
    }
    registry.register(Arc::new(McpSearchTool::new(catalog)))
}

async fn discover(registry: &ToolRegistry, messages: &mut Vec<Message>, query: &str, limit: usize) {
    let arguments = json!({"query":query,"limit":limit});
    let result = registry
        .execute("mcp_search", arguments.clone())
        .await
        .unwrap();
    let id = format!("search-{}", messages.len());
    messages.push(Message::ToolCall(ToolCall {
        call_id: id.clone(),
        name: "mcp_search".into(),
        arguments,
    }));
    messages.push(Message::ToolResult(ToolResult {
        call_id: id,
        content: result,
    }));
}

fn names(registry: &ToolRegistry, messages: &[Message]) -> Vec<String> {
    registry
        .for_model(messages)
        .unwrap()
        .iter()
        .map(|t| t.name())
        .collect()
}

#[tokio::test]
async fn mcp_discovery_selection_is_per_history_bounded_and_reloadable() {
    let registry = registry(1, 3);
    assert_eq!(names(&registry, &[]), ["mcp_search", "show_ui"]);
    let mut history = Vec::new();
    for group in 0..3 {
        discover(&registry, &mut history, &format!("group{group}"), 16).await;
    }
    let loaded = names(&registry, &history);
    assert_eq!(loaded.len(), 34);
    assert!(!loaded.iter().any(|n| n.contains("group0")));
    assert!(loaded.contains(&"mcp__rustrak__group2_tool15".into()));
    assert_eq!(names(&registry, &[]), ["mcp_search", "show_ui"]);
    discover(&registry, &mut history, "mcp__rustrak__group0_tool00", 1).await;
    let loaded = names(&registry, &history);
    assert_eq!(loaded.len(), 34);
    assert!(loaded.contains(&"mcp__rustrak__group0_tool00".into()));
    let restored: Vec<Message> =
        serde_json::from_slice(&serde_json::to_vec(&history).unwrap()).unwrap();
    assert_eq!(names(&registry, &restored), loaded);
    assert!(loaded.windows(2).all(|pair| pair[0] < pair[1]));
}

#[tokio::test]
async fn mcp_discovery_preserves_native_tools_when_request_budget_is_tight() {
    let registry = registry(100, 3);
    let mut history = Vec::new();
    for group in 0..3 {
        discover(&registry, &mut history, &format!("group{group}"), 16).await;
    }
    let loaded = names(&registry, &history);
    assert_eq!(loaded.len(), 128);
    assert_eq!(loaded.iter().filter(|n| n.starts_with("mcp__")).count(), 27);
    assert!(loaded.contains(&"show_ui".into()));
    for i in 1..100 {
        assert!(loaded.contains(&format!("native_{i:03}")));
    }
}

#[test]
fn mcp_discovery_does_not_advertise_an_oversized_native_catalog() {
    assert!(registry(128, 1).for_model(&[]).is_err());
    assert!(registry(127, 1).for_model(&[]).is_err());
}

#[tokio::test]
async fn mcp_discovery_ignores_unpaired_foreign_failed_and_unregistered_results() {
    let registry = registry(1, 1);
    let activate = json!({"loaded_tools":["mcp__rustrak__group0_tool00"]});
    let result = |id: &str, output: serde_json::Value| {
        Message::ToolResult(ToolResult {
            call_id: id.into(),
            content: output.into(),
        })
    };
    let call = |id: &str, name: &str| {
        Message::ToolCall(ToolCall {
            call_id: id.into(),
            name: name.into(),
            arguments: json!({}),
        })
    };
    for history in [
        vec![result("orphan", activate.clone())],
        vec![
            call("foreign", "mcp__rustrak__group0_tool01"),
            result("foreign", activate.clone()),
        ],
        vec![
            call("search", "mcp_search"),
            result("wrong-id", activate.clone()),
        ],
        vec![
            call("search", "mcp_search"),
            result(
                "search",
                json!({"error":"failed","loaded_tools":["mcp__rustrak__group0_tool00"]}),
            ),
        ],
        vec![
            call("search", "mcp_search"),
            result(
                "search",
                json!({"loaded_tools":["mcp__absent__tool","show_ui"]}),
            ),
        ],
        vec![
            call("same", "mcp_search"),
            call("same", "other"),
            result("same", activate),
        ],
    ] {
        assert_eq!(names(&registry, &history), ["mcp_search", "show_ui"]);
    }
    let output = registry
        .execute("mcp__rustrak__group0_tool00", json!({}))
        .await
        .unwrap();
    assert_eq!(output.output["name"], "mcp__rustrak__group0_tool00");
    assert_eq!(
        registry.replay_policy("mcp__rustrak__group0_tool00"),
        ReplayPolicy::Never
    );
}

#[tokio::test]
async fn mcp_discovery_recovery_loads_the_saved_selection_before_resuming_the_model() {
    use crate::agent::{
        Agent, AgentEvent, AgentRunner, AssistantMessage, ContextBuilder, LlmContext, UserMessage,
        llm::LlmProvider,
        recovery::RecoveryTarget,
        run_state::{Checkpoint, RunPhase},
        session::{Session, SessionId, SqliteSessionStore},
    };
    struct Model;
    #[async_trait::async_trait]
    impl LlmProvider for Model {
        async fn generate(
            &self,
            context: &LlmContext<'_>,
            tools: &[&dyn Tool],
            _: Option<&tokio::sync::mpsc::Sender<AgentEvent>>,
        ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
            assert!(
                tools
                    .iter()
                    .any(|t| t.name() == "mcp__rustrak__group0_tool00")
            );
            let message = if let Some(Message::ToolResult(result)) = context.messages.last()
                && result.call_id == "resume-mcp"
            {
                assert_eq!(result.content.output["name"], "mcp__rustrak__group0_tool00");
                Message::Assistant(AssistantMessage {
                    content: "recovered".into(),
                })
            } else {
                Message::ToolCall(ToolCall {
                    call_id: "resume-mcp".into(),
                    name: "mcp__rustrak__group0_tool00".into(),
                    arguments: json!({"id":42}),
                })
            };
            Ok((vec![message], None))
        }
    }
    let registry = registry(1, 1);
    let mut history = vec![Message::User(UserMessage {
        content: "Read the issue".into(),
        files: vec![],
        images: vec![],
    })];
    discover(&registry, &mut history, "mcp__rustrak__group0_tool00", 1).await;
    let mut session = Session::new(SessionId::new("cli:main"));
    session.initialize_system_prompt("test");
    for message in history {
        session.push(message);
    }
    let data = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteSessionStore::open_async(data.path().join("sessions"))
            .await
            .unwrap(),
    );
    let mut checkpoint = Checkpoint::new(&store, session.id(), &session);
    checkpoint.record.isolated = Some(false);
    checkpoint
        .save(&session, RunPhase::NeedModel, None)
        .await
        .unwrap();
    let runner = AgentRunner::new(
        Arc::new(Agent::new(
            Arc::new(Model),
            registry,
            ContextBuilder::new("test"),
        )),
        store.clone(),
    );
    let report = runner
        .recover(RecoveryTarget::Conversation(session.id().clone()))
        .await
        .unwrap();
    assert!(report.blocked.is_empty(), "{:?}", report.blocked);
    assert_eq!(report.completed.len(), 1);
    assert_eq!(report.completed[0].1.content, "recovered");
    let execution = store
        .tool_execution(&checkpoint.record.id, "resume-mcp")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(execution.replay, ReplayPolicy::Never);
    assert_eq!(
        execution.result.unwrap().output["name"],
        "mcp__rustrak__group0_tool00"
    );
}
