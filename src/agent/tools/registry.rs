use crate::agent::message::Message;
pub use crate::agent::message::ToolOutput;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;

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
    fn deferred(&self) -> bool {
        false
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
        let mut tools: Vec<&dyn Tool> = self
            .tools
            .values()
            .filter(|tool| tool.available() && !tool.deferred())
            .map(|tool| tool.as_ref())
            .collect();
        tools.sort_by_cached_key(|tool| tool.name());
        tools
    }

    pub(crate) fn for_model(&self, messages: &[Message]) -> anyhow::Result<Vec<&dyn Tool>> {
        let mut tools = self.list();
        anyhow::ensure!(
            tools.len() <= 128,
            "More than 128 non-deferred tools; reduce the advertised catalog"
        );
        let capacity = 32.min(128 - tools.len());
        anyhow::ensure!(
            capacity > 0
                || !self
                    .tools
                    .values()
                    .any(|tool| tool.deferred() && tool.available()),
            "Non-deferred tools leave no capacity for MCP discovery; reduce the advertised catalog"
        );
        let mut selected = VecDeque::new();
        let mut searches = HashSet::new();
        for message in messages {
            match message {
                Message::ToolCall(call) => {
                    searches.remove(&call.call_id);
                    if call.name == super::mcp_search::NAME {
                        searches.insert(call.call_id.clone());
                    }
                }
                Message::ToolResult(result)
                    if searches.remove(&result.call_id)
                        && result.content.output.get("error").is_none() =>
                {
                    if let Some(names) = result
                        .content
                        .output
                        .get("loaded_tools")
                        .and_then(|v| v.as_array())
                    {
                        for name in names
                            .iter()
                            .take(super::mcp_search::MAX_RESULTS)
                            .rev()
                            .filter_map(|v| v.as_str())
                        {
                            let Some(tool) = self
                                .tools
                                .get(name)
                                .filter(|tool| tool.deferred() && tool.available())
                            else {
                                continue;
                            };
                            selected.retain(|selected| *selected != name);
                            selected.push_back(tool.name());
                            if selected.len() > capacity {
                                selected.pop_front();
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        for name in selected {
            tools.push(self.tools[&name].as_ref());
        }
        tools.sort_by_cached_key(|tool| tool.name());
        Ok(tools)
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
