use crate::agent::tools::{Tool, ToolOutput};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    pub step: String,
    pub status: StepStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskPlan {
    pub explanation: Option<String>,
    pub plan: Vec<PlanStep>,
}

impl TaskPlan {
    pub fn is_completed(&self) -> bool {
        !self.plan.is_empty()
            && self
                .plan
                .iter()
                .all(|step| step.status == StepStatus::Completed)
    }

    pub(crate) fn from_history(messages: &[crate::agent::message::Message]) -> Option<Self> {
        use crate::agent::message::Message;
        let mut plan: Option<Self> = None;
        let mut calls = std::collections::HashMap::new();
        for message in messages {
            match message {
                Message::User(_) if plan.as_ref().is_some_and(Self::is_completed) => plan = None,
                Message::ToolCall(call) if call.name == "update_plan" => {
                    calls.insert(&call.call_id, &call.arguments);
                }
                Message::ToolResult(result) => {
                    if let Some(arguments) = calls.remove(&result.call_id)
                        && result.content.output.get("error").is_none()
                        && result.content.output.get("interrupted").is_none()
                        && let Ok(next) = Self::parse(arguments.clone())
                    {
                        plan = (!next.plan.is_empty()).then_some(next);
                    }
                }
                _ => {}
            }
        }
        plan
    }

    pub fn parse(value: serde_json::Value) -> anyhow::Result<Self> {
        let plan: Self = serde_json::from_value(value)?;
        anyhow::ensure!(
            plan.plan.iter().all(|item| !item.step.trim().is_empty()),
            "Plan steps must not be blank"
        );
        anyhow::ensure!(
            plan.plan
                .iter()
                .filter(|item| item.status == StepStatus::InProgress)
                .count()
                <= 1,
            "At most one step can be in_progress"
        );
        Ok(plan)
    }

    pub fn render(&self) -> String {
        let completed = self
            .plan
            .iter()
            .filter(|item| item.status == StepStatus::Completed)
            .count();
        let mut text = format!("Plan · {completed}/{}", self.plan.len());
        for item in &self.plan {
            let marker = match item.status {
                StepStatus::Pending => "○",
                StepStatus::InProgress => "→",
                StepStatus::Completed => "✓",
            };
            text.push_str(&format!("\n{marker} {}", item.step));
        }
        if let Some(explanation) = &self.explanation {
            text.push_str(&format!("\n\n{explanation}"));
        }
        text
    }
}

pub struct UpdatePlanTool;

#[async_trait::async_trait]
impl Tool for UpdatePlanTool {
    fn name(&self) -> String {
        "update_plan".into()
    }

    fn description(&self) -> String {
        "Replace the current task plan with the full list of steps and statuses (pending, in_progress, completed). At most one step may be in_progress. Provide an optional explanation when changing approach. An empty list clears the plan. This tracks work; it does not execute steps.".into()
    }

    fn parameters(&self) -> serde_json::Value {
        let mut schema = schema_for!(TaskPlan);
        schema.insert(
            "required".into(),
            serde_json::json!(["explanation", "plan"]),
        );
        schema.to_value()
    }

    async fn execute(&self, properties: serde_json::Value) -> anyhow::Result<ToolOutput> {
        TaskPlan::parse(properties)?;
        Ok(serde_json::json!("Plan updated").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn history_plan_uses_successful_results_in_order_and_respects_user_boundaries() {
        use crate::agent::{
            UserMessage,
            message::{Message, ToolCall, ToolResult},
        };
        let mut messages = vec![];
        for (id, status, output, expected) in [
            ("pending", "pending", json!("Plan updated"), Some(false)),
            (
                "failed",
                "completed",
                json!({"error":"failed"}),
                Some(false),
            ),
            (
                "interrupted",
                "completed",
                json!({"interrupted":true}),
                Some(false),
            ),
            ("done", "completed", json!("Plan updated"), Some(true)),
        ] {
            messages.push(Message::ToolCall(ToolCall {
                call_id: id.into(),
                name: "update_plan".into(),
                arguments: json!({"plan":[{"step":"Inspect","status":status}]}),
            }));
            assert_eq!(
                TaskPlan::from_history(&messages).map(|p| p.is_completed()),
                if id == "pending" { None } else { Some(false) }
            );
            messages.push(Message::ToolResult(ToolResult {
                call_id: id.into(),
                content: output.into(),
            }));
            assert_eq!(
                TaskPlan::from_history(&messages).map(|p| p.is_completed()),
                expected
            );
        }
        messages.push(Message::User(UserMessage {
            content: "Next".into(),
            files: vec![],
            images: vec![],
        }));
        assert!(TaskPlan::from_history(&messages).is_none());
        messages.push(Message::ToolCall(ToolCall {
            call_id: "next".into(),
            name: "update_plan".into(),
            arguments: json!({"plan":[{"step":"New work","status":"in_progress"}]}),
        }));
        messages.push(Message::ToolResult(ToolResult {
            call_id: "next".into(),
            content: json!("Plan updated").into(),
        }));
        assert_eq!(
            TaskPlan::from_history(&messages).unwrap().plan[0].step,
            "New work"
        );
        messages.push(Message::ToolCall(ToolCall {
            call_id: "clear".into(),
            name: "update_plan".into(),
            arguments: json!({"plan":[]}),
        }));
        messages.push(Message::ToolResult(ToolResult {
            call_id: "clear".into(),
            content: json!("Plan updated").into(),
        }));
        assert!(TaskPlan::from_history(&messages).is_none());
    }

    #[tokio::test]
    async fn validates_plan() {
        let valid = json!({"plan": [
            {"step": "Inspect", "status": "completed"},
            {"step": "Implement", "status": "in_progress"},
            {"step": "Test", "status": "pending"}
        ]});
        assert_eq!(
            UpdatePlanTool.execute(valid.clone()).await.unwrap().output,
            "Plan updated"
        );
        let mut plan = TaskPlan::parse(valid).unwrap();
        let expected = "Plan · 1/3\n✓ Inspect\n→ Implement\n○ Test";
        assert_eq!(plan.render(), expected);
        plan.explanation = Some("Implementation started".into());
        assert_eq!(
            plan.render(),
            format!("{expected}\n\nImplementation started")
        );
        for invalid in [
            json!({"plan": [{"step": " ", "status": "pending"}]}),
            json!({"plan": [{"step": "A", "status": "blocked"}]}),
            json!({"plan": [{"step": "A", "status": "in_progress"}, {"step": "B", "status": "in_progress"}]}),
            json!({"plan": [], "unknown": true}),
        ] {
            assert!(UpdatePlanTool.execute(invalid).await.is_err());
        }
        assert!(UpdatePlanTool.execute(json!({"plan": []})).await.is_ok());
    }
}
