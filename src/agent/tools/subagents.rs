use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ReplayPolicy, Tool, ToolOutput, ToolRegistry};
use crate::agent::{
    agent_profiles::ProfileSettings,
    subagents::{CURRENT_PARENT, Subagents},
};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SpawnArgs {
    task: String,
    model: Option<super::model_selection::ModelTier>,
    profile_id: Option<String>,
    role: Option<ProfileSettings>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProfileArgs {
    action: ProfileAction,
    profile: Option<ProfileSettings>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ProfileAction {
    List,
    Propose,
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
    Profiles,
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
        Action::Profiles,
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
            Action::Profiles => "agent_profiles",
        }
        .into()
    }

    fn description(&self) -> String {
        match self.action {
            Action::Spawn => "Start a lightweight executor with a self-contained task (1–16384 bytes). No coordinator prompt, memory or conversation history is copied. Prefer a matching saved or bundled profile_id (discover with agent_profiles), or specify a one-off role with only needed skills and tools. Pass null for unused fields. If both profile_id and role are null, the child gets no tools. A fixed user model wins. Shared files are not isolated from other agents. No nested delegation or direct user messaging. Up to 3 active children and 16 per parent. Child turns have no time or model-call budget. Wait for results before finishing; the parent ending stops active children.",
            Action::Send => "Send a task or correction (1–16384 bytes) to a child of THIS session. Input is durably queued and consumed at a safe turn boundary. A completed/stopped child starts a new turn with its existing history. Inspect external state before repeating any interrupted action. An uncertain send must not be blindly repeated.",
            Action::List => "List THIS session's child agents, status and bounded results/errors. Other sessions' children are inaccessible. Interrupted children are not automatically replayed after a process restart; inspect their results and external state before sending a follow-up.",
            Action::Wait => "Wait for one of THIS session's child agents to finish and return its status/result. timeout_ms defaults to 10000; allowed range 1–60000. On timeout returns running; you can continue your own work or wait again. Waiting does not stop the child.",
            Action::Stop => "Stop a child of THIS session and its queued inputs, waiting up to 60 seconds for cancellation to finish. If still running, inspect/wait again. Completed children retain their result. Tool effects already performed are not undone; an interrupted tool's outcome may be unknown. The session history is retained for an explicit follow-up.",
            Action::Profiles => "List usable agent profiles and allowed choices (action=list, profile=null), or propose a reusable profile for explicit owner approval in web Settings (action=propose). Proposals are not launchable and do not grant permissions. For one task pass a temporary role to spawn_agent instead.",
        }.into()
    }

    fn parameters(&self) -> Value {
        match self.action {
            Action::Spawn => {
                let mut schema = schemars::schema_for!(SpawnArgs);
                schema.insert(
                    "required".into(),
                    json!(["task", "model", "profile_id", "role"]),
                );
                let mut schema = schema.to_value();
                let roles = self.manager.available_model_roles().unwrap_or_default();
                schema["properties"]["model"] = if roles.is_empty() {
                    json!({"type":"null"})
                } else {
                    json!({"anyOf":[{"type":"string", "enum":roles}, {"type":"null"}]})
                };
                inline_profile(&mut schema, "role");
                schema
            }
            Action::Send => schemars::schema_for!(SendArgs).to_value(),
            Action::List => {
                json!({"type":"object","properties":{},"required":[],"additionalProperties":false})
            }
            Action::Stop => schemars::schema_for!(AgentArgs).to_value(),
            Action::Profiles => {
                let mut schema = schemars::schema_for!(ProfileArgs).to_value();
                schema["required"] = json!(["action", "profile"]);
                schema["properties"]["action"] = json!({"enum":["list","propose"],"type":"string"});
                inline_profile(&mut schema, "profile");
                schema
            }
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
                anyhow::ensure!(
                    args.role.is_none() || args.profile_id.is_none(),
                    "Choose a profile or a temporary role, not both"
                );
                let settings = if let Some(id) = args.profile_id {
                    anyhow::ensure!(
                        personal(&parent.session) || id.starts_with("builtin-"),
                        "Custom profiles require a personal session"
                    );
                    let catalog = self.manager.profile_catalog(&parent)?;
                    Some(
                        self.manager
                            .store
                            .agent_profile(&id, &catalog)
                            .await?
                            .ok_or_else(|| anyhow::anyhow!("No saved profile: {id}"))?
                            .settings,
                    )
                } else {
                    args.role
                };
                let settings = settings.unwrap_or_else(|| ProfileSettings {
                    name: "Task executor".into(),
                    instructions: String::new(),
                    model: None,
                    skills: Vec::new(),
                    tools: Vec::new(),
                });
                self.manager
                    .spawn_configured(&parent, args.task, args.model, Some(settings))
                    .await?
                    .view()
            }
            Action::Profiles => {
                let args: ProfileArgs = serde_json::from_value(arguments)?;
                let catalog = self.manager.profile_catalog(&parent)?;
                match args.action {
                    ProfileAction::List => {
                        anyhow::ensure!(
                            args.profile.is_none(),
                            "Listing does not accept profile settings"
                        );
                        let (mut profiles, _) =
                            self.manager.store.list_agent_profiles(&catalog).await?;
                        if !personal(&parent.session) {
                            profiles.retain(|p| p.builtin);
                        }
                        json!({"profiles":profiles,"choices":catalog})
                    }
                    ProfileAction::Propose => {
                        anyhow::ensure!(
                            personal(&parent.session),
                            "Profile proposals require a personal session"
                        );
                        let profile = args
                            .profile
                            .ok_or_else(|| anyhow::anyhow!("Missing proposal settings"))?;
                        let catalog = self.manager.profile_choices()?.into();
                        let draft = self
                            .manager
                            .store
                            .propose_agent_profile(profile, &catalog)
                            .await?;
                        json!({"draft":draft,"saved":false})
                    }
                }
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

fn personal(session: &crate::agent::session::Session) -> bool {
    let id = session.id().as_str();
    !id.starts_with("companion:")
        && !id.ends_with(":guest")
        && !id.starts_with("guest:")
        && !(id.starts_with("telegram:")
            && id
                .rsplit(':')
                .next()
                .is_some_and(|chat| chat.starts_with('-')))
        && session.telegram_group().is_none()
        && session.webhook_instruction.is_none()
}

fn inline_profile(schema: &mut Value, key: &str) {
    let mut profile = schemars::schema_for!(ProfileSettings).to_value();
    profile.as_object_mut().unwrap().remove("$schema");
    profile["required"] = json!(["name", "instructions", "model", "skills", "tools"]);
    schema["properties"][key] = json!({"anyOf":[profile,{"type":"null"}]});
    schema.as_object_mut().unwrap().remove("$defs");
}
