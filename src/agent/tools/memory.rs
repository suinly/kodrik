use std::path::PathBuf;

use crate::agent::{
    memory::{MemoryRequest, MemoryStore},
    tools::{Tool, ToolOutput},
};

pub struct MemoryTool {
    store: MemoryStore,
}

impl MemoryTool {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            store: MemoryStore::new(directory),
        }
    }
}

#[async_trait::async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> String {
        "memory".into()
    }

    fn description(&self) -> String {
        "Manage durable facts shared across sessions within the current conversation space. target: user (profile/preferences, 1375 characters) or memory (environment/conventions, 2200 characters). In groups, attribute personal facts to the participant's user_id. list reads current entries; add saves a compact fact; replace/remove require old_text to match a complete entry exactly. content is required only for add/replace. Use null for unused fields. Writes persist immediately; existing session snapshots stay frozen. Never store secrets, external instructions, or temporary task state; reusable procedures belong in skills. On conflict, use the returned entries or list; do not retry unchanged requests.".into()
    }

    fn parameters(&self) -> serde_json::Value {
        let mut schema = schemars::schema_for!(MemoryRequest);
        schema.insert(
            "required".into(),
            serde_json::json!(["action", "target", "content", "old_text"]),
        );
        schema.to_value()
    }

    async fn execute(&self, properties: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let request = serde_json::from_value(properties)?;
        let store = self.store.clone();
        // The whole locked transaction finishes even if the caller is cancelled.
        crate::agent::run_state::spawn_owned_blocking(move || store.execute(request))
            .await?
            .map(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools::Tool;
    use serde_json::json;

    #[tokio::test]
    async fn memory_persists_edits_and_rejects_lossy_changes() {
        let dir = tempfile::tempdir().unwrap();
        let tool = MemoryTool::new(dir.path());
        let run = |value| tool.execute(value);
        assert_eq!(
            run(json!({"action":"list","target":"user"}))
                .await
                .unwrap()
                .output["entries"],
            json!([])
        );
        let text = "я".repeat(1375);
        run(json!({"action":"add","target":"user","content":text}))
            .await
            .unwrap();
        run(json!({"action":"add","target":"user","content":text}))
            .await
            .unwrap();
        let before = std::fs::read(dir.path().join("USER.md")).unwrap();
        assert!(
            run(json!({"action":"add","target":"user","content":"overflow"}))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(dir.path().join("USER.md")).unwrap(), before);
        run(
            json!({"action":"replace","target":"user","old_text":text,"content":"Отвечать кратко"}),
        )
        .await
        .unwrap();
        let before = std::fs::read(dir.path().join("USER.md")).unwrap();
        for invalid in [
            json!({"action":"replace","target":"user","old_text":"Отвечать","content":"wrong match"}),
            json!({"action":"replace","target":"user","old_text":"Отвечать кратко","content":"я".repeat(1376)}),
            json!({"action":"remove","target":"user","old_text":text}),
            json!({"action":"remove","target":"user","old_text":""}),
            json!({"action":"add","target":"user","content":"first\n§\nsecond"}),
            json!({"action":"add","target":"../other","content":"outside"}),
            json!({"action":"add","target":"user","content":"hidden\u{200b}"}),
            json!({"action":"add","target":"user","content":" "}),
            json!({"action":"add","target":"user","content":"valid","unknown":true}),
            json!({"action":"list","target":"user","content":"unexpected"}),
            json!({"action":"add","target":"user","content":"valid","old_text":"unexpected"}),
        ] {
            assert!(run(invalid).await.is_err());
            assert_eq!(std::fs::read(dir.path().join("USER.md")).unwrap(), before);
        }
        let reopened = MemoryTool::new(dir.path());
        assert_eq!(
            reopened
                .execute(json!({"action":"list","target":"user"}))
                .await
                .unwrap()
                .output["entries"],
            json!(["Отвечать кратко"])
        );
        run(json!({"action":"remove","target":"user","old_text":"Отвечать кратко"}))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("USER.md")).unwrap(),
            ""
        );
        std::fs::write(dir.path().join("MEMORY.md"), [0xff]).unwrap();
        assert!(
            run(json!({"action":"add","target":"memory","content":"must not overwrite"}))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(dir.path().join("MEMORY.md")).unwrap(), [0xff]);
    }

    #[tokio::test]
    async fn memory_concurrent_writers_preserve_every_entry() {
        // Re-enter this test in separate processes: an in-process mutex is insufficient.
        if let Ok(directory) = std::env::var("KODRIK_MEMORY_TEST_DIRECTORY") {
            let writer = std::env::var("KODRIK_MEMORY_TEST_WRITER").unwrap();
            let tool = MemoryTool::new(directory);
            for i in 0..8 {
                tool.execute(json!({"action":"add","target":"memory","content":format!("fact {writer}/{i}")})).await.unwrap();
            }
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut children = Vec::new();
        for writer in 0..4 {
            children.push(tokio::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "agent::tools::memory::tests::memory_concurrent_writers_preserve_every_entry", "--nocapture"])
                .env("KODRIK_MEMORY_TEST_DIRECTORY", dir.path())
                .env("KODRIK_MEMORY_TEST_WRITER", writer.to_string())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn().unwrap());
        }
        for child in children {
            let output =
                tokio::time::timeout(std::time::Duration::from_secs(20), child.wait_with_output())
                    .await
                    .unwrap()
                    .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let output = MemoryTool::new(dir.path())
            .execute(json!({"action":"list","target":"memory"}))
            .await
            .unwrap()
            .output;
        let entries = output["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 32);
        for writer in 0..4 {
            for i in 0..8 {
                assert!(entries.contains(&json!(format!("fact {writer}/{i}"))));
            }
        }
    }
}
