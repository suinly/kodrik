use super::{ReplayPolicy, Tool, ToolOutput};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ModelTier {
    Fast,
    Smart,
}

impl ModelTier {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Smart => "smart",
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selection {
    pub model: ModelTier,
}

pub(crate) struct SelectModel;

#[async_trait::async_trait]
impl Tool for SelectModel {
    fn name(&self) -> String {
        "select_model".into()
    }
    fn description(&self) -> String {
        "In Auto mode, select fast or smart for subsequent calls in this turn. Start on fast; use smart only when the task requires materially stronger reasoning, and return to fast for routine work. The next user turn starts on fast. Cannot override a user's fixed selection.".into()
    }
    fn parameters(&self) -> serde_json::Value {
        schemars::schema_for!(Selection).to_value()
    }
    fn replay_policy(&self) -> ReplayPolicy {
        ReplayPolicy::Safe
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolOutput> {
        anyhow::bail!("Model selection requires an active agent turn")
    }
}
