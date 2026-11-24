use super::{ReplayPolicy, Tool, ToolOutput};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ModelTier {
    Fast,
    Smart,
    Deep,
    Vision,
}

impl ModelTier {
    pub(crate) const ALL: [Self; 4] = [Self::Fast, Self::Smart, Self::Deep, Self::Vision];

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tier| tier.name() == name)
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Smart => "smart",
            Self::Deep => "deep",
            Self::Vision => "vision",
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selection {
    pub model: ModelTier,
}

pub(crate) struct SelectModel {
    pub roles: Vec<String>,
    pub default: &'static str,
}

#[async_trait::async_trait]
impl Tool for SelectModel {
    fn name(&self) -> String {
        "select_model".into()
    }
    fn description(&self) -> String {
        format!(
            "In Auto mode, switch to a configured role for subsequent calls in this turn. Auto starts each turn on {}. Use fast for narrow, easily checked work; smart for everyday work; deep for hard or high-cost decisions, or after smart when stronger reasoning can help (not for missing data, access, or permission); vision when screen or image contents determine the next action. This preserves the current context and cannot override a user's fixed selection.",
            self.default,
        )
    }
    fn parameters(&self) -> serde_json::Value {
        let mut schema = schemars::schema_for!(Selection).to_value();
        schema["properties"]["model"] = serde_json::json!({"type":"string", "enum":self.roles});
        schema.as_object_mut().unwrap().remove("$defs");
        schema
    }
    fn replay_policy(&self) -> ReplayPolicy {
        ReplayPolicy::Safe
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolOutput> {
        anyhow::bail!("Model selection requires an active agent turn")
    }
}
