use crate::agent::{
    reminders::{Action, ReminderRequest, ReminderStore, WAKEUP_CONTEXT},
    tools::{Tool, ToolOutput},
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WakeupRequest {
    action: Action,
    id: Option<String>,
    instruction: Option<String>,
    due_at: Option<String>,
}

pub struct WakeupTool {
    store: ReminderStore,
}

impl WakeupTool {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            store: ReminderStore::wakeups(directory),
        }
    }
}

#[async_trait::async_trait]
impl Tool for WakeupTool {
    fn name(&self) -> String {
        "wakeups".into()
    }

    fn description(&self) -> String {
        "Schedule your own one-shot agent wakeups in this Telegram chat: create, list, reschedule, cancel. You may create them on your own initiative for a concrete useful follow-up. create requires a self-contained instruction (1–6000 characters, include necessary context) and a future due_at (RFC 3339 with explicit offset); interpret local time in the clock timezone. reschedule requires id and due_at; cancel requires id; list takes no fields. Use null for unused fields. Source chat and session are bound by the application. Only pending tasks can be cancelled/rescheduled. A wakeup runs with your ordinary tools in a separate internal session, for at most 16 model calls/five minutes. It may finish silently or send a useful message; quiet hours defer message delivery. You may schedule another one-shot wakeup during execution. Scheduling grants no additional action permissions. User-facing Apple Reminders belong to the remindctl skill when available. Tasks survive restarts but require the gateway to run. Unsafe interrupted execution or uncertain delivery does not automatically retry; consult list before creating a replacement. Confirm scheduling only after successful persistence. No recurring schedules.".into()
    }

    fn parameters(&self) -> serde_json::Value {
        let mut schema = schemars::schema_for!(WakeupRequest);
        schema.insert(
            "required".into(),
            serde_json::json!(["action", "id", "instruction", "due_at"]),
        );
        schema.to_value()
    }

    fn available(&self) -> bool {
        WAKEUP_CONTEXT
            .try_with(|context| context.is_some())
            .unwrap_or(false)
    }

    async fn execute(&self, properties: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let context = WAKEUP_CONTEXT
            .try_with(Clone::clone)
            .ok()
            .flatten()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Wakeups require a Telegram conversation or an internal wakeup turn"
                )
            })?;
        let args: WakeupRequest = serde_json::from_value(properties)?;
        let request = ReminderRequest {
            action: args.action,
            id: args.id,
            text: args.instruction,
            due_at: args.due_at,
        };
        let key = crate::agent::run_state::CURRENT_TOOL
            .try_with(Clone::clone)
            .ok()
            .flatten()
            .map(|(run, call)| serde_json::to_string(&(run, call)).unwrap());
        let store = self.store.clone();
        crate::agent::run_state::spawn_owned_blocking(move || {
            store.execute_wakeup_idempotent(
                &context.recipient,
                &context.session,
                request,
                chrono::Utc::now(),
                key,
            )
        })
        .await?
        .map(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{
        reminders::{Recipient, WakeupContext},
        tools::ToolRegistry,
    };
    use std::sync::Arc;

    #[tokio::test]
    async fn wakeup_creation_replay_keeps_one_task_and_rejects_changed_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WakeupTool::new(dir.path());
        let context = WakeupContext {
            recipient: Recipient {
                interface: "telegram:test".into(),
                chat: "42".into(),
            },
            session: "source".into(),
        };
        let args = serde_json::json!({"action":"create","instruction":"check","due_at":(chrono::Utc::now()+chrono::Duration::minutes(5)).to_rfc3339()});
        WAKEUP_CONTEXT
            .scope(
                Some(context),
                crate::agent::run_state::CURRENT_TOOL.scope(
                    Some(("run".into(), "call".into())),
                    async {
                        let first = tool.execute(args.clone()).await.unwrap();
                        let second = tool.execute(args.clone()).await.unwrap();
                        assert_eq!(first.output, second.output);
                        let mut changed = args;
                        changed["instruction"] = "different".into();
                        assert!(tool.execute(changed).await.is_err());
                        assert_eq!(
                            tool.execute(serde_json::json!({"action":"list"}))
                                .await
                                .unwrap()
                                .output["wakeups"]
                                .as_array()
                                .unwrap()
                                .len(),
                            1
                        );
                    },
                ),
            )
            .await;
    }

    #[tokio::test]
    async fn wakeups_bind_each_concurrent_chat_and_source_session() {
        let directory = tempfile::tempdir().unwrap();
        let registry =
            Arc::new(ToolRegistry::default().register(Arc::new(WakeupTool::new(directory.path()))));
        assert!(registry.list().is_empty());
        assert!(
            registry
                .execute("wakeups", serde_json::json!({"action":"list"}))
                .await
                .is_err()
        );
        let mut tasks = Vec::new();
        for chat in ["42", "7"] {
            let registry = registry.clone();
            tasks.push(tokio::spawn(WAKEUP_CONTEXT.scope(Some(WakeupContext {
                recipient: Recipient { interface: "telegram:test".into(), chat: chat.into() },
                session: format!("source-{chat}"),
            }), async move {
                assert_eq!(registry.list().len(), 1);
                let due = (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339();
                registry.execute("wakeups", serde_json::json!({"action":"create","instruction":"Check build", "due_at":due})).await.unwrap();
                let result = registry.execute("wakeups", serde_json::json!({"action":"list"})).await.unwrap().output;
                let tasks = result["wakeups"].as_array().unwrap();
                assert_eq!(tasks.len(), 1);
                assert_eq!(tasks[0]["recipient"]["chat"], chat);
                assert_eq!(tasks[0]["source_session"], format!("source-{chat}"));
                assert!(registry.execute("wakeups", serde_json::json!({"action":"create","instruction":"Check", "due_at":due, "chat":"999"})).await.is_err());
            })));
        }
        for task in tasks {
            task.await.unwrap();
        }
    }
}
