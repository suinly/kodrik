use crate::{
    agent::{
        datasets::DatasetStore,
        tools::{Tool, ToolOutput},
    },
    web::{
        Settings,
        documents::{Document, PublishedDocument},
        pages::{PageStore, Visibility},
    },
};
use std::path::PathBuf;

pub(crate) struct ShowUiTool {
    datasets: DatasetStore,
    publication: Option<(PathBuf, Settings)>,
}

impl ShowUiTool {
    pub(crate) fn new(datasets: DatasetStore, publication: Option<(PathBuf, Settings)>) -> Self {
        Self {
            datasets,
            publication,
        }
    }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Args {
    title: String,
    body: String,
    sources: Vec<crate::web::documents::UiSource>,
    #[serde(default)]
    views: Vec<crate::web::views::UiView>,
    #[serde(default)]
    actions: Vec<crate::web::documents::UiAction>,
    slug: Option<String>,
    visibility: Option<Visibility>,
}

#[async_trait::async_trait]
impl Tool for ShowUiTool {
    fn name(&self) -> String {
        "show_ui".into()
    }
    fn description(&self) -> String {
        format!(
            "Render Nuxt UI in chat or publish a persistent MDC page. Use this for new pages/reports/charts and their follow-ups, including interactivity; retain the native slug. Discover available components and their exact props/slots with ui_components before using them. body accepts Markdown, ::u-* components, literal props or :prop=\"JSON\", #named-slot sections, own-property references sources.<id>, views.<id> and slot.<field> (also in text bindings). No executable JS, expressions, handlers, arbitrary HTML or component remapping. Controls keep local state. UChart supports line/area/bar/donut and tooltips; range {{min,max}} adds a translucent bounds corridor and per-row midpoint line; period controls require explicit views/data, not invented props. Existing aliases: card(title/description), metric(label,value,unit or source+field), data-table(source,title,columns JSON accessorKey/header/dateFormat), date-value(value,dateFormat), line-chart(source,x,y,label,unit), callout(title,color), button(label,prompt), columns(count optional 1–4, default 3; count 4 gives four desktop columns, two on tablet and one on phone). For date display, dateFormat is short/medium/long/full or Intl JSON options (locale/timeZone/dateStyle/timeStyle/year/month/day/weekday/hour/minute/second/hour12), default ru-RU/UTC. Use date-value for bound values in text/slots; table column dateFormat changes display while keeping raw sort values. Calendar dates preserve their day; timestamps need an explicit offset. Close blocks with ::; nested blocks use :::. Prefer standard Nuxt UI defaults and Catppuccin Blue. For forms, inspect datasets schema, declare actions (max 8): id, dataset, operation insert/update, fields (allowed dataset field names), record_id (null for insert, fixed existing id for update). Bind ::u-form{{submit-action=\"action_id\"}}, ::u-form-field{{name=\"field\" label=\"...\"}} with input controls and ::u-button{{type=\"submit\" label=\"Save\"}}. Private published forms validate and save through gateway without an agent call; update forms initialize current values and protect revisions. Chat-only and public forms cannot save; publish privately to enable them. Submit success reloads related data. Sources (max 8): unique id and exactly one of data_json (flat scalar rows, max 200 rows/64 KiB) or query (datasets read: mode query/summarize, dataset, optional dates/filters/metrics/group_by/limit, or last_days 1–366 plus IANA timezone instead of date_from/date_to). Relative periods include today and resolve on every private page read; timezone defaults to UTC, so use the user timezone explicitly. Views (max 8): id, source (one declared source id), jq (1–8192 bytes). Input is the source complete flat row array; use sort_by(.date), map, last, arithmetic or aggregates and return exactly one bounded JSON value. sources.<id> remains flat rows; views.<id> is the computed JSON result, e.g. :data=\"views.week\", :model-value=\"views.latest.kcal\", :max=\"views.latest.target\". No shell, host files or network in jq. Views reject paginated/incomplete inputs: narrow the source query or summarize first. Empty latest may be null and displays no data. View errors prevent publication; chart/progress binding shapes are checked before display. Unused fields null. Body max 32 KiB, snapshot max 512 KiB. Chat values remain frozen. {} New pages default private; null visibility retains access. Explicit authorization is needed for public publication; public pages are snapshots and never accept form writes. Chat prompt buttons prepare a message for user review; standalone prompt buttons are inactive. If a feature is unsupported, explain it and offer an MDC alternative; no HTML fallback or host-app edits. publish_page maintains existing HTML only.",
            if self.publication.is_some() {
                "Set slug to publish/update the stable URL; null slug renders in chat."
            } else {
                "Publishing unavailable here: slug and visibility must be null."
            }
        )
    }
    fn parameters(&self) -> serde_json::Value {
        let mut schema = schemars::schema_for!(Args).to_value();
        super::datasets::require_properties(&mut schema);
        schema
    }
    async fn execute(&self, properties: serde_json::Value) -> anyhow::Result<ToolOutput> {
        anyhow::ensure!(
            serde_json::to_vec(&properties)?.len() <= 512 * 1024,
            "UI request exceeds 512 KiB"
        );
        let args: Args = serde_json::from_value(properties)?;
        anyhow::ensure!(
            args.slug.is_some() || args.visibility.is_none(),
            "visibility requires slug"
        );
        if let Some(slug) = &args.slug {
            crate::web::pages::validate_slug(slug)?;
            anyhow::ensure!(
                self.publication.is_some(),
                "Page publication is unavailable in this conversation"
            );
        }
        let store = self.datasets.clone();
        let publication = self.publication.clone();
        crate::agent::run_state::spawn_owned_blocking(move || {
            let document = Document {
                title: args.title,
                body: args.body,
                sources: args.sources,
                views: args.views,
                actions: args.actions,
            };
            let snapshot = document.snapshot(&store)?;
            let mut result = serde_json::json!({"kind":"ui_document","document":snapshot});
            if let Some(slug) = args.slug {
                let (data, settings) =
                    publication.ok_or_else(|| anyhow::anyhow!("Page publication unavailable"))?;
                let published = PublishedDocument {
                    document,
                    snapshot: result["document"].clone(),
                };
                let manifest =
                    PageStore::new(&data).publish_document(&slug, &published, args.visibility)?;
                result["url"] = serde_json::json!(settings.url(&format!("pages/{slug}/"))?);
                result["visibility"] = serde_json::json!(manifest.visibility);
                result["release"] = serde_json::json!(manifest.release);
            }
            Ok(result.into())
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::{datasets::DatasetStore, tools::Tool};
    use serde_json::json;

    #[tokio::test]
    async fn view_errors_prevent_replacing_a_published_release() {
        let dir = tempfile::tempdir().unwrap();
        let datasets = DatasetStore::open(dir.path().join("datasets")).unwrap();
        let settings = crate::web::Settings::from_config(&crate::config::WebConfig {
            trusted_proxies: vec![],
            enabled: true,
            listen: "127.0.0.1:0".into(),
            base_url: Some("http://127.0.0.1/".into()),
        })
        .unwrap()
        .unwrap();
        let tool = super::ShowUiTool::new(datasets, Some((dir.path().to_owned(), settings)));
        let mut request = json!({"title":"Views","body":"::u-progress{:model-value=\"views.latest.kcal\" :max=\"views.latest.target\"}\n::","sources":[{"id":"days","data_json":"[{\"date\":\"2026-10-21\",\"kcal\":1900,\"target\":2200}]","query":null}],"views":[{"id":"latest","source":"days","jq":"sort_by(.date) | last"}],"slug":"views","visibility":null});
        let result = tool.execute(request.clone()).await.unwrap().output;
        assert_eq!(result["document"]["views"]["latest"]["kcal"], 1900);
        request["views"][0]["jq"] = json!("empty");
        assert!(tool.execute(request).await.is_err());
        let manifest = crate::web::pages::PageStore::new(dir.path())
            .manifest("views")
            .unwrap()
            .unwrap();
        assert_eq!(
            manifest.release.to_string(),
            result["release"].as_str().unwrap()
        );
    }
    #[tokio::test]
    async fn ui_tool_returns_a_frozen_document_and_publishes_native_pages() {
        let dir = tempfile::tempdir().unwrap();
        let datasets = DatasetStore::open(dir.path().join("datasets")).unwrap();
        let settings = crate::web::Settings::from_config(&crate::config::WebConfig {
            trusted_proxies: vec![],
            enabled: true,
            listen: "127.0.0.1:0".into(),
            base_url: Some("http://127.0.0.1/kodrik/".into()),
        })
        .unwrap()
        .unwrap();
        let tool = super::ShowUiTool::new(datasets, Some((dir.path().to_owned(), settings)));
        let result=tool.execute(json!({"title":"Nutrition","body":"# Nutrition\n\n::metric{label=\"Calories\" value=\"350\"}\n::",
            "sources":[{"id":"meals","data_json":"[{\"meal\":\"Breakfast\",\"kcal\":350}]","query":null}],
            "slug":"nutrition","visibility":null})).await.unwrap().output;
        assert_eq!(result["kind"], "ui_document");
        assert_eq!(result["document"]["sources"][0]["data"][0]["kcal"], 350);
        assert_eq!(result["url"], "http://127.0.0.1/kodrik/pages/nutrition/");
        let pages = crate::web::pages::PageStore::new(dir.path());
        let manifest = pages.manifest("nutrition").unwrap().unwrap();
        assert!(manifest.document);
        assert_eq!(manifest.visibility, crate::web::pages::Visibility::Private);
        assert!(
            pages
                .read("nutrition", &manifest, "document.json")
                .unwrap()
                .is_some()
        );
        assert!(tool.execute(json!({"title":"Bad","body":"ok","sources":[{"id":"x","data_json":"{}","query":null}],"slug":null,"visibility":null})).await.is_err());
        assert!(
            tool.execute(
                json!({"title":"Bad","body":"ok","sources":[],"slug":"../escape","visibility":null})
            )
            .await
            .is_err()
        );
    }
}
