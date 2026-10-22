mod openai;
mod provider;

tokio::task_local! {
    // Providers charge each network attempt, including retries, in a bounded run.
    pub(crate) static REQUEST_BUDGET: std::cell::Cell<usize>;
}

pub use openai::OpenAiLlm;
pub use provider::LlmProvider;

pub use provider::{ContextOverflow, GenerationRetry};
