pub use crate::agent::message::ToolOutput;
use std::{collections::HashMap, sync::Arc};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReplayPolicy {
    Never,
    Safe,
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> String;
    fn description(&self) -> String;
    fn parameters(&self) -> serde_json::Value;
    fn strict_schema(&self) -> bool {
        true
    }
    fn replay_policy(&self) -> ReplayPolicy {
        ReplayPolicy::Never
    }
    fn available(&self) -> bool {
        true
    }
    async fn execute(&self, properties: serde_json::Value) -> anyhow::Result<ToolOutput>;
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub(crate) fn without(&self, names: &[&str]) -> Self {
        Self {
            tools: self
                .tools
                .iter()
                .filter(|(name, _)| !names.contains(&name.as_str()))
                .map(|(name, tool)| (name.clone(), tool.clone()))
                .collect(),
        }
    }

    pub fn list(&self) -> Vec<&dyn Tool> {
        self.tools
            .values()
            .filter(|tool| tool.available())
            .map(|tool| tool.as_ref())
            .collect()
    }

    pub fn register(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.insert(tool.name(), tool);
        self
    }

    pub(crate) fn replay_policy(&self, name: &str) -> ReplayPolicy {
        self.tools
            .get(name)
            .filter(|tool| tool.available())
            .map_or(ReplayPolicy::Never, |tool| tool.replay_policy())
    }

    pub async fn execute(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<ToolOutput> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("Unknown tool: {name}"))?;

        anyhow::ensure!(
            tool.available(),
            "Tool {name} is unavailable in this conversation"
        );

        tool.execute(arguments).await
    }
}
