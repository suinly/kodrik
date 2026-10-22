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
