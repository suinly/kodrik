use super::{Tool, ToolOutput};
use serde_json::{Value, json};
use std::sync::LazyLock;

pub(crate) struct UiComponentsTool;

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Args {
    component: Option<String>,
}

static CATALOG: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("ui_catalog.json")).expect("generated Nuxt UI catalog")
});

#[async_trait::async_trait]
impl Tool for UiComponentsTool {
    fn name(&self) -> String {
        "ui_components".into()
    }
    fn description(&self) -> String {
        "Discover the Nuxt UI components actually available in show_ui Markdown/MDC. Set component null for the short catalog, or its tag/name (u-chart/UChart) for package props, slots, types and adapter rules. Read a component's API before using it. The catalog excludes Layout, AI Chat, Editor, app navigation/providers, marketing/publishing, AuthForm and FileUpload. No filesystem/package inspection is needed.".into()
    }
    fn parameters(&self) -> Value {
        let mut schema = schemars::schema_for!(Args).to_value();
        super::datasets::require_properties(&mut schema);
        schema
    }
    async fn execute(&self, properties: Value) -> anyhow::Result<ToolOutput> {
        let args: Args = serde_json::from_value(properties)?;
        let catalog = &*CATALOG;
        let components = catalog["components"]
            .as_object()
            .expect("catalog components");
        let rules = "Use ::u-* blocks, literal props or :prop='JSON', and #slot-name sections. Only own-property paths under sources.<id> (flat row arrays), views.<id> (jq-computed JSON), or slot scope are readable; no expressions/functions, event handlers, HTML or component remapping. JSON dates/times for date/time components use ISO strings. Controls keep local state; forms use declared document actions through submit-action and field names. Chat-only action forms are disabled until published privately. UChart has tooltips; its range {min,max} adapter draws a translucent bounds corridor and per-row midpoint line, with optional reference categories; period filtering needs an explicit data view, not an invented chart prop. Existing metric/data-table/line-chart/button/columns aliases remain supported. data-table accepts :columns='[{\"accessorKey\":\"date\",\"header\":\"Date\",\"dateFormat\":\"short\"}]'; UTable column definitions accept the same dateFormat extension. Reuse :date-value{:value=\"slot.row.original.date\" date-format=\"long\"} in any text/slot (or sources/views values). dateFormat is a dateStyle string or Intl JSON options with optional locale/timeZone; defaults ru-RU/UTC. Display formatting preserves raw sort/filter values.";
        let output = if let Some(name) = args.component {
            anyhow::ensure!(name.len() <= 100, "Invalid component name");
            let (tag, component) = components
                .iter()
                .find(|(tag, value)| **tag == name || value["name"] == name)
                .ok_or_else(|| {
                    anyhow::anyhow!("Component is unavailable; list ui_components first")
                })?;
            json!({"version":catalog["version"],"tag":tag,"component":component,"adapter":rules})
        } else {
            json!({"version":catalog["version"],"components":components.keys().collect::<Vec<_>>(),"adapter":rules})
        };
        Ok(output.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn discovery_lists_real_components_and_returns_their_api() {
        let list = UiComponentsTool
            .execute(json!({"component":null}))
            .await
            .unwrap()
            .output;
        let names = list["components"].as_array().unwrap();
        assert!(names.contains(&json!("u-chart")));
        assert!(!names.contains(&json!("u-editor")));
        let chart = UiComponentsTool
            .execute(json!({"component":"UChart"}))
            .await
            .unwrap()
            .output;
        assert_eq!(chart["tag"], "u-chart");
        assert!(
            chart["component"]["props"]
                .as_array()
                .unwrap()
                .contains(&json!("range"))
        );
        assert!(
            chart["component"]["api"]
                .as_str()
                .unwrap()
                .contains("MDC range adapter")
        );
        assert!(
            chart["component"]["api"]
                .as_str()
                .unwrap()
                .contains("tooltip")
        );
        assert!(
            UiComponentsTool
                .execute(json!({"component":"u-editor"}))
                .await
                .is_err()
        );
        assert!(
            UiComponentsTool
                .execute(json!({"component":null,"extra":1}))
                .await
                .is_err()
        );
    }
}
