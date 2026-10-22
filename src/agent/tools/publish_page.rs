use crate::{
    agent::tools::{Tool, ToolOutput},
    web::{
        Settings,
        pages::{PageStore, Visibility},
    },
};
use std::path::Path;

pub struct PublishPageTool {
    store: PageStore,
    settings: Settings,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct PublishPageArgs {
    slug: String,
    visibility: Option<Visibility>,
}

impl PublishPageTool {
    pub(crate) fn new(data: &Path, settings: Settings) -> Self {
        Self {
            store: PageStore::new(data),
            settings,
        }
    }
}

#[async_trait::async_trait]
impl Tool for PublishPageTool {
    fn name(&self) -> String {
        "publish_page".into()
    }
    fn description(&self) -> String {
        format!(
            "Maintain existing custom HTML/CSS/JS pages from {}/<slug>/. For new pages, dashboards, reports, charts and native-page updates use show_ui with Markdown/MDC. A request for interactivity or a feature missing from MDC is not a reason to use this tool: explain the limitation and offer a supported MDC alternative. If show_ui is unavailable, report it rather than creating an HTML fallback. Requires index.html; optional assets and data.json. Slug: 1–64 lowercase ASCII letters, digits or hyphens. Limit: 1000 files, 32 MiB; no symlinks. visibility: private (owner login), public (all files accessible without login), or null (preserve existing mode; new pages default private). Set public only on explicit user request. Republish after changing data or presentation. Returns the real permanent URL and release. Pages share one browser origin without iframe; all JavaScript must be trusted. Server runs with kodrik gateway; publication does not start the server.",
            self.store.drafts().display()
        )
    }
    fn parameters(&self) -> serde_json::Value {
        let mut schema = schemars::schema_for!(PublishPageArgs);
        schema.insert("required".into(), serde_json::json!(["slug", "visibility"]));
        schema.to_value()
    }
    async fn execute(&self, properties: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let args: PublishPageArgs = serde_json::from_value(properties)?;
        let store = self.store.clone();
        let settings = self.settings.clone();
        crate::agent::run_state::spawn_owned_blocking(move || {
            let manifest = store.publish(&args.slug, args.visibility)?;
            Ok(serde_json::json!({"slug": args.slug,
                "url": settings.url(&format!("pages/{}/", args.slug))?,
                "visibility": manifest.visibility, "release": manifest.release})
            .into())
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{agent::tools::Tool, config::WebConfig};

    #[tokio::test]
    async fn page_tool_returns_stable_url_and_preserves_visibility() {
        let dir = tempfile::tempdir().unwrap();
        let draft = dir.path().join("pages/drafts/nutrition");
        std::fs::create_dir_all(&draft).unwrap();
        std::fs::write(draft.join("index.html"), "report").unwrap();
        let settings = crate::web::Settings::from_config(&WebConfig {
            trusted_proxies: vec![],
            enabled: true,
            listen: "0.0.0.0:8080".into(),
            base_url: Some("https://pages.example.com/kodrik/".into()),
        })
        .unwrap()
        .unwrap();
        let tool = PublishPageTool::new(dir.path(), settings);
        let first = tool
            .execute(serde_json::json!({"slug":"nutrition","visibility":null}))
            .await
            .unwrap()
            .output;
        assert_eq!(first["visibility"], "private");
        assert_eq!(
            first["url"],
            "https://pages.example.com/kodrik/pages/nutrition/"
        );
        tool.execute(serde_json::json!({"slug":"nutrition","visibility":"public"}))
            .await
            .unwrap();
        let second = tool
            .execute(serde_json::json!({"slug":"nutrition","visibility":null}))
            .await
            .unwrap()
            .output;
        assert_eq!(first["url"], second["url"]);
        assert_eq!(second["visibility"], "public");
        assert_ne!(first["release"], second["release"]);
        assert!(
            tool.execute(serde_json::json!({"slug":"nutrition","visibility":"wrong"}))
                .await
                .is_err()
        );
        assert!(
            tool.execute(serde_json::json!({"slug":"../outside","visibility":null}))
                .await
                .is_err()
        );
        assert!(
            tool.execute(serde_json::json!({"slug":"nutrition","visibility":null,"extra":1}))
                .await
                .is_err()
        );
        assert_eq!(
            tool.parameters()["required"],
            serde_json::json!(["slug", "visibility"])
        );
    }
}
