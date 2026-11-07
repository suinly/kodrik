use crate::agent::{AgentEvent, LlmContext, message::Message, tools::Tool};
use tokio::sync::mpsc;

/// Counts reported for one completed provider call; cache reads are a subset of input.
#[derive(Debug, Clone, Copy, Default)]
pub struct GenerationUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

#[async_trait::async_trait]
pub trait LlmProvider: Send + Sync {
    fn identity(&self) -> String {
        std::any::type_name::<Self>().into()
    }
    /// One generation attempt. Durable runtime owns retries and charges each attempt.
    async fn generate_attempt(
        &self,
        context: &LlmContext<'_>,
        tools: &[&dyn Tool],
        progress: Option<&mpsc::Sender<AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)> {
        self.generate(context, tools, progress).await
    }

    async fn generate_attempt_with_usage(
        &self,
        context: &LlmContext<'_>,
        tools: &[&dyn Tool],
        progress: Option<&mpsc::Sender<AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, GenerationUsage)> {
        let (messages, input_tokens) = self.generate_attempt(context, tools, progress).await?;
        Ok((
            messages,
            GenerationUsage {
                input_tokens,
                ..Default::default()
            },
        ))
    }

    async fn generate_with_usage(
        &self,
        context: &LlmContext<'_>,
        tools: &[&dyn Tool],
        progress: Option<&mpsc::Sender<AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, GenerationUsage)> {
        let (messages, input_tokens) = self.generate(context, tools, progress).await?;
        Ok((
            messages,
            GenerationUsage {
                input_tokens,
                ..Default::default()
            },
        ))
    }

    async fn generate(
        &self,
        context: &LlmContext<'_>,
        tools: &[&dyn Tool],
        progress: Option<&mpsc::Sender<AgentEvent>>,
    ) -> anyhow::Result<(Vec<Message>, Option<u64>)>;
}

#[derive(Debug)]
pub struct GenerationRetry {
    pub limit: u32,
    pub retry_after: Option<std::time::Duration>,
    pub error: anyhow::Error,
}
impl std::fmt::Display for GenerationRetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}
impl std::error::Error for GenerationRetry {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

/// A structured provider rejection, never inferred from model text or HTTP status alone.
#[derive(Debug)]
pub struct ContextOverflow;
impl std::fmt::Display for ContextOverflow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Context overflow: reduce the input or start a new session")
    }
}
impl std::error::Error for ContextOverflow {}
