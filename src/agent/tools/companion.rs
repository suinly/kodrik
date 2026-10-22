use crate::agent::{
    companion::{CompanionRequest, CompanionStore},
    tools::{Tool, ToolOutput},
};
use std::path::PathBuf;

pub struct CompanionTool {
    store: CompanionStore,
}
impl CompanionTool {
    pub fn new(data: impl Into<PathBuf>) -> Self {
        Self {
            store: CompanionStore::new(data),
        }
    }
}
#[async_trait::async_trait]
impl Tool for CompanionTool {
    fn name(&self) -> String {
        "companion".into()
    }
    fn description(&self) -> String {
        "Manage durable interests and inspect or undo learned lessons. list shows ids and source evidence. add_interest requires topic, question, why; findings starts empty and status defaults active. update_interest changes supplied fields by id; preserve existing findings when adding results and include source URLs. remove_interest and forget_lesson require id. Forget preserves a generated skill if the owner edited it and reports this explicitly. Use null for unused fields. Bounds: 10 interests / 4000 characters; 20 lessons / 6000 characters. Never store secrets. Interests and findings confer no permission for external actions. Keep actual conversational curiosities, not invented interests. To correct a lesson, forget it and allow future evidence-based learning.".into()
    }
    fn parameters(&self) -> serde_json::Value {
        let mut schema = schemars::schema_for!(CompanionRequest);
        schema.insert(
            "required".into(),
            serde_json::json!([
                "action", "id", "topic", "question", "why", "findings", "status"
            ]),
        );
        schema.to_value()
    }
    async fn execute(&self, properties: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let request = serde_json::from_value(properties)?;
        let store = self.store.clone();
        crate::agent::run_state::spawn_owned_blocking(move || store.execute(request))
            .await?
            .map(Into::into)
    }
}
