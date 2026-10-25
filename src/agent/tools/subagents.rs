use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ReplayPolicy, Tool, ToolOutput, ToolRegistry};
use crate::agent::subagents::{CURRENT_PARENT, Subagents};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SpawnArgs {
    task: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SendArgs {
    agent_id: String,
    message: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AgentArgs {
    agent_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    agent_id: String,
    timeout_ms: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListArgs {}

#[derive(Clone, Copy)]
enum Action {
    Spawn,
    Send,
    List,
    Wait,
    Stop,
}

struct SubagentTool {
    action: Action,
    manager: Arc<Subagents>,
}

pub(crate) fn register(mut tools: ToolRegistry, manager: Arc<Subagents>) -> ToolRegistry {
    for action in [
        Action::Spawn,
        Action::Send,
        Action::List,
        Action::Wait,
        Action::Stop,
    ] {
        tools = tools.register(Arc::new(SubagentTool {
            action,
            manager: manager.clone(),
        }));
    }
    tools
}

#[async_trait::async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> String {
        match self.action {
            Action::Spawn => "spawn_agent",
            Action::Send => "send_message",
            Action::List => "list_agents",
            Action::Wait => "wait_agent",
            Action::Stop => "stop_agent",
        }
        .into()
    }

    fn description(&self) -> String {
        match self.action {
            Action::Spawn => "Start a child agent asynchronously with a self-contained task (1–16384 bytes, include needed context). Returns agent id immediately. The child inherits your instructions, context snapshots, tools and sandbox, with its own history; your conversation history is not copied. It can edit shared files, so assign independent work. No nested delegation, direct messaging to users, wakeups or webhook management. At most 3 active children per application worker and 16 child sessions per parent session; 16 model attempts/five minutes per turn. Wait for results before finishing: active children stop when your run ends. After an uncertain/interrupted spawn, inspect list_agents before creating a replacement.",
            Action::Send => "Send a task or correction (1–16384 bytes) to a child of THIS session. Input is durably queued and consumed at a safe turn boundary. A completed/stopped child starts a new turn with its existing history. Inspect external state before repeating any interrupted action. An uncertain send must not be blindly repeated.",
            Action::List => "List THIS session's child agents, status and bounded results/errors. Other sessions' children are inaccessible. Interrupted children are not automatically replayed after a process restart; inspect their results and external state before sending a follow-up.",
            Action::Wait => "Wait for one of THIS session's child agents to finish and return its status/result. timeout_ms defaults to 10000; allowed range 1–60000. On timeout returns running; you can continue your own work or wait again. Waiting does not stop the child.",
            Action::Stop => "Stop a child of THIS session and its queued inputs, waiting up to 60 seconds for cancellation to finish. If still running, inspect/wait again. Completed children retain their result. Tool effects already performed are not undone; an interrupted tool's outcome may be unknown. The session history is retained for an explicit follow-up.",
        }.into()
    }

    fn parameters(&self) -> Value {
        match self.action {
            Action::Spawn => schemars::schema_for!(SpawnArgs).to_value(),
            Action::Send => schemars::schema_for!(SendArgs).to_value(),
            Action::List => {
                json!({"type":"object","properties":{},"required":[],"additionalProperties":false})
            }
            Action::Stop => schemars::schema_for!(AgentArgs).to_value(),
            Action::Wait => {
                let mut schema = schemars::schema_for!(WaitArgs);
                schema.insert("required".into(), json!(["agent_id", "timeout_ms"]));
                schema.to_value()
            }
        }
    }

    fn replay_policy(&self) -> ReplayPolicy {
        match self.action {
            Action::List | Action::Wait => ReplayPolicy::Safe,
            _ => ReplayPolicy::Never,
        }
    }

    async fn execute(&self, arguments: Value) -> anyhow::Result<ToolOutput> {
        let parent = CURRENT_PARENT
            .try_with(Clone::clone)
            .ok()
            .flatten()
            .ok_or_else(|| anyhow::anyhow!("Subagent tools require an active parent session"))?;
        let output = match self.action {
            Action::Spawn => {
                let args: SpawnArgs = serde_json::from_value(arguments)?;
                self.manager.spawn(&parent, args.task).await?.view()
            }
            Action::Send => {
                let args: SendArgs = serde_json::from_value(arguments)?;
                self.manager
                    .send(&parent, &args.agent_id, args.message)
                    .await?
                    .view()
            }
            Action::List => {
                let _: ListArgs = serde_json::from_value(arguments)?;
                json!({"agents": self.manager.list(&parent.session).await?.iter().map(|child|child.view()).collect::<Vec<_>>()})
            }
            Action::Wait => {
                let args: WaitArgs = serde_json::from_value(arguments)?;
                self.manager
                    .wait(
                        &parent.session,
                        &args.agent_id,
                        args.timeout_ms.unwrap_or(10_000),
                    )
                    .await?
                    .view()
            }
            Action::Stop => {
                let args: AgentArgs = serde_json::from_value(arguments)?;
                self.manager
                    .stop(&parent.session, &args.agent_id)
                    .await?
                    .view()
            }
        };
        Ok(output.into())
    }
}
