use std::{io::Read, sync::Arc};

use anyhow::{Context, ensure};

use crate::agent::{
    datasets::{DatasetRequest, DatasetStore, REQUEST_LIMIT},
    sandbox::Sandbox,
    tools::{Tool, ToolOutput},
};

pub struct DatasetsTool {
    store: DatasetStore,
    sandbox: Option<Arc<Sandbox>>,
}

impl DatasetsTool {
    pub(crate) fn new(store: DatasetStore, sandbox: Option<Arc<Sandbox>>) -> Self {
        Self { store, sandbox }
    }
}

#[async_trait::async_trait]
impl Tool for DatasetsTool {
    fn name(&self) -> String {
        "datasets".into()
    }

    fn description(&self) -> String {
        "Manage durable typed records in this conversation space (nutrition diaries, trackers). Prefer this tool over shell/patch commands or direct SQLite access for structured record tasks. list/describe discover immutable schemas; create requires dataset, title, fields and optional required date_field. Field types: string, integer, number, boolean, date (YYYY-MM-DD). upsert requires UUID request_id and 1–100 complete records: stable id, expected_revision (null for insert; current revision for replacement), values as [{field,value}]. Provide either records inline or records_file (set records null): a UTF-8 JSON array in exactly the same record format, maximum 256 KiB. Generate file batches programmatically from source data instead of transcribing large JSON through the model; preserve IDs and values, then verify imported counts and totals. records_file uses the current process filesystem for the owner, or only the chat virtual filesystem in isolated chats (relative paths start at /workspace). Files are read-only; no arbitrary source format conversion. Query before editing. A batch commits atomically; retry uncertain writes with the SAME request_id and payload (keep file contents unchanged). query supports typed equality filters [{field,value}], date_from inclusive/date_to exclusive, limit 1–200 (default 50), next_cursor continuation. Pages stop at 64 KiB; changed datasets invalidate cursors. summarize computes count and numeric metrics (sum/average/count), optionally group_by day/month, maximum 366 groups. Missing optional numbers are excluded from averages. Use null for unused parameters. No SQL, deletions or schema edits. Tool results are historical snapshots; query again for current values.".into()
    }

    fn parameters(&self) -> serde_json::Value {
        let mut schema = schemars::schema_for!(DatasetRequest).to_value();
        require_properties(&mut schema);
        schema
    }

    async fn execute(&self, properties: serde_json::Value) -> anyhow::Result<ToolOutput> {
        ensure!(
            serde_json::to_vec(&properties)?.len() <= REQUEST_LIMIT,
            "Dataset request exceeds 256 KiB"
        );
        let mut request: DatasetRequest = serde_json::from_value(properties)?;
        request.validate_parameters()?;
        if let Some(path) = request.records_file.take() {
            ensure!(
                request.records.is_none(),
                "Provide records or records_file, not both"
            );
            ensure!(!path.trim().is_empty(), "records_file must not be empty");
            let bytes = if let Some(sandbox) = &self.sandbox {
                sandbox.read_file(&path).await?
            } else {
                crate::agent::run_state::spawn_owned_blocking(move || -> anyhow::Result<Vec<u8>> {
                    let metadata = std::fs::metadata(&path)?;
                    ensure!(metadata.is_file(), "records_file must be a regular file");
                    ensure!(
                        metadata.len() <= REQUEST_LIMIT as u64,
                        "Records file exceeds 256 KiB"
                    );
                    let mut bytes = Vec::new();
                    std::fs::File::open(path)?
                        .take(REQUEST_LIMIT as u64 + 1)
                        .read_to_end(&mut bytes)?;
                    Ok(bytes)
                })
                .await??
            };
            ensure!(bytes.len() <= REQUEST_LIMIT, "Records file exceeds 256 KiB");
            request.records = Some(
                serde_json::from_slice(&bytes)
                    .context("records_file must contain a JSON array of records")?,
            );
        }
        let store = self.store.clone();
        crate::agent::run_state::spawn_owned_blocking(move || store.execute(request))
            .await?
            .map(Into::into)
    }
}

pub(super) fn require_properties(schema: &mut serde_json::Value) {
    match schema {
        serde_json::Value::Object(object) => {
            if let Some(properties) = object
                .get("properties")
                .and_then(serde_json::Value::as_object)
            {
                let names: Vec<_> = properties.keys().cloned().collect();
                object.insert("required".into(), serde_json::json!(names));
            }
            for value in object.values_mut() {
                require_properties(value);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                require_properties(item);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tools::ReplayPolicy;
    use serde_json::{Value, json};

    fn record(id: &str, value: Value) -> Value {
        json!({"id":id,"expected_revision":null,"values":[{"field":"value","value":value}]})
    }

    async fn create(tool: &DatasetsTool) {
        tool.execute(json!({"action":"create","dataset":"items","title":"Items",
            "fields":[{"name":"value","type":"string","required":true}]}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn file_batches_preserve_values_and_reuse_existing_receipts() {
        let dir = tempfile::tempdir().unwrap();
        let tool = DatasetsTool::new(
            DatasetStore::open(dir.path().join("datasets")).unwrap(),
            None,
        );
        create(&tool).await;
        let records = json!([record("one", json!("Ингредиенты: ½ яблока, 20–30 г"))]);
        let bytes = serde_json::to_vec_pretty(&records).unwrap();
        let path = dir.path().join("records.json");
        std::fs::write(&path, &bytes).unwrap();
        let request = json!({"action":"upsert","dataset":"items","request_id":uuid::Uuid::new_v4().to_string(),"records_file":path});
        let first = tool.execute(request.clone()).await.unwrap().output;
        assert_eq!(tool.execute(request.clone()).await.unwrap().output, first);
        let mut inline = request.clone();
        inline.as_object_mut().unwrap().remove("records_file");
        inline["records"] = records.clone();
        assert_eq!(tool.execute(inline).await.unwrap().output, first);
        let rows = tool
            .execute(json!({"action":"query","dataset":"items"}))
            .await
            .unwrap()
            .output;
        assert_eq!(
            rows["records"][0]["values"]["value"],
            records[0]["values"][0]["value"]
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::write(
            &path,
            serde_json::to_vec(&json!([record("one", json!("changed"))])).unwrap(),
        )
        .unwrap();
        assert!(
            tool.execute(request)
                .await
                .unwrap_err()
                .to_string()
                .contains("different payload")
        );
    }

    #[tokio::test]
    async fn invalid_file_batches_never_partially_write() {
        let dir = tempfile::tempdir().unwrap();
        let tool = DatasetsTool::new(
            DatasetStore::open(dir.path().join("datasets")).unwrap(),
            None,
        );
        create(&tool).await;
        let path = dir.path().join("records.json");
        for bytes in [
            b"not json".to_vec(),
            b"{}".to_vec(),
            b"[]".to_vec(),
            vec![b' '; 256 * 1024 + 1],
            serde_json::to_vec(&json!([
                record("valid", json!("keep")),
                record("invalid", json!(42))
            ]))
            .unwrap(),
            serde_json::to_vec(
                &(0..101)
                    .map(|index| record(&format!("r{index}"), json!("value")))
                    .collect::<Vec<_>>(),
            )
            .unwrap(),
        ] {
            std::fs::write(&path, bytes).unwrap();
            assert!(tool.execute(json!({"action":"upsert","dataset":"items","request_id":uuid::Uuid::new_v4().to_string(),"records_file":path})).await.is_err());
        }
        std::fs::write(
            &path,
            serde_json::to_vec(&json!([record("valid", json!("keep"))])).unwrap(),
        )
        .unwrap();
        for request in [
            json!({"action":"upsert","dataset":"items","request_id":uuid::Uuid::new_v4().to_string(),"records_file":path,"records":[record("valid",json!("keep"))]}),
            json!({"action":"query","dataset":"items","records_file":path}),
            json!({"action":"upsert","dataset":"items","request_id":uuid::Uuid::new_v4().to_string(),"records_file":dir.path()}),
            json!({"action":"upsert","dataset":"items","request_id":uuid::Uuid::new_v4().to_string(),"records_file":dir.path().join("missing")}),
        ] {
            assert!(tool.execute(request).await.is_err());
        }
        assert_eq!(
            tool.execute(json!({"action":"query","dataset":"items"}))
                .await
                .unwrap()
                .output["records"],
            json!([])
        );
    }

    #[tokio::test]
    async fn isolated_file_batches_read_only_their_persisted_virtual_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Arc::new(Sandbox::new(dir.path().join("sandbox")));
        let store = DatasetStore::open(dir.path().join("datasets")).unwrap();
        let tool = DatasetsTool::new(store.clone(), Some(sandbox.clone()));
        create(&tool).await;
        let records =
            serde_json::to_string(&json!([record("virtual", json!("from sandbox"))])).unwrap();
        let host_path = dir.path().join("records.json");
        std::fs::write(&host_path, &records).unwrap();
        let request = json!({"action":"upsert","dataset":"items","request_id":uuid::Uuid::new_v4().to_string(),"records_file":"records.json"});
        for path in [host_path.to_str().unwrap(), "../records.json", "/workspace"] {
            let mut invalid = request.clone();
            invalid["records_file"] = json!(path);
            assert!(tool.execute(invalid).await.is_err());
        }
        let output = sandbox
            .tool("exec_command")
            .execute(json!({"cmd":format!("cat > records.json <<'JSON'\n{records}\nJSON")}))
            .await
            .unwrap();
        assert_eq!(output.output["exit_code"], 0);
        let tool = DatasetsTool::new(
            store,
            Some(Arc::new(Sandbox::new(dir.path().join("sandbox")))),
        );
        let receipt = tool.execute(request.clone()).await.unwrap().output;
        let mut absolute = request.clone();
        absolute["records_file"] = json!("/workspace/records.json");
        assert_eq!(tool.execute(absolute).await.unwrap().output, receipt);
        let output = sandbox
            .tool("apply_patch")
            .execute(json!({"workdir":"/workspace","patch":format!("*** Begin Patch\n*** Add File: large.json\n+{}\n*** End Patch"," ".repeat(REQUEST_LIMIT+1))}))
            .await
            .unwrap();
        assert_eq!(
            output.output["changed_files"][0]["path"],
            "/workspace/large.json"
        );
        let mut large = request;
        large["records_file"] = json!("large.json");
        assert!(
            tool.execute(large)
                .await
                .unwrap_err()
                .to_string()
                .contains("exceeds 256 KiB")
        );
    }

    #[tokio::test]
    async fn tool_executes_in_its_store_and_generates_closed_strict_schema() {
        let dir = tempfile::tempdir().unwrap();
        let tool = DatasetsTool::new(DatasetStore::open(dir.path().to_owned()).unwrap(), None);
        assert_eq!(tool.replay_policy(), ReplayPolicy::Never);
        assert_eq!(
            tool.execute(json!({"action":"list"})).await.unwrap().output["datasets"],
            json!([])
        );
        assert!(
            tool.execute(json!({"action":"list","dataset":"not_allowed"}))
                .await
                .is_err()
        );
        assert!(
            tool.execute(json!({"action":"list","unknown":true}))
                .await
                .is_err()
        );
        fn check(schema: &Value) {
            match schema {
                Value::Object(object) => {
                    if object.get("type") == Some(&json!("object")) {
                        assert_eq!(object["additionalProperties"], false);
                        let properties = object["properties"].as_object().unwrap();
                        assert_eq!(
                            object["required"].as_array().unwrap().len(),
                            properties.len()
                        );
                    }
                    for value in object.values() {
                        check(value);
                    }
                }
                Value::Array(items) => {
                    for item in items {
                        check(item);
                    }
                }
                _ => {}
            }
        }
        check(&tool.parameters());
    }
}
