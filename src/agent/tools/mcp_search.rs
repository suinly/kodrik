use anyhow::ensure;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{ReplayPolicy, Tool, ToolOutput};

pub(super) const NAME: &str = "mcp_search";
pub(super) const MAX_RESULTS: usize = 16;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Request {
    query: String,
    limit: Option<usize>,
}

#[derive(Serialize)]
struct Entry {
    name: String,
    description: String,
}

pub(super) struct McpSearchTool {
    catalog: Vec<Entry>,
}

impl McpSearchTool {
    pub(super) fn new(catalog: Vec<(String, String)>) -> Self {
        Self {
            catalog: catalog
                .into_iter()
                .map(|(name, description)| Entry { name, description })
                .collect(),
        }
    }
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

#[async_trait::async_trait]
impl Tool for McpSearchTool {
    fn name(&self) -> String {
        NAME.into()
    }

    fn description(&self) -> String {
        "Find and load MCP tools for connected external services. Search before calling an MCP tool that is not currently advertised. query: concise keywords from the server/tool name or description (English names/keywords usually work best); use an exact mcp__server__tool name to find that function. limit: 1–16, null defaults to 8. Matches are ranked and their schemas become callable on the NEXT model request; call their exact names then. At most 32 recently discovered MCP tools remain advertised; search again to reload an older tool. All configured tools remain searchable. This tool only searches local metadata; it does not execute remote actions. A failed or empty search does not grant new capabilities.".into()
    }

    fn parameters(&self) -> Value {
        let mut schema = schemars::schema_for!(Request);
        schema.insert("required".into(), json!(["query", "limit"]));
        schema.to_value()
    }

    fn replay_policy(&self) -> ReplayPolicy {
        ReplayPolicy::Safe
    }

    async fn execute(&self, properties: Value) -> anyhow::Result<ToolOutput> {
        let request: Request = serde_json::from_value(properties)?;
        ensure!(
            !request.query.trim().is_empty() && request.query.chars().count() <= 256,
            "query must contain 1–256 characters"
        );
        let limit = request.limit.unwrap_or(8);
        ensure!(
            (1..=MAX_RESULTS).contains(&limit),
            "limit must be between 1 and 16"
        );
        let query = request.query.trim().to_lowercase();
        let mut terms = words(&query);
        terms.sort();
        terms.dedup();
        ensure!(!terms.is_empty(), "query must contain letters or digits");
        let mut matches = Vec::new();
        for entry in &self.catalog {
            let name = words(&entry.name);
            let description = words(&entry.description);
            let mut hits = 0;
            let mut name_hits = 0;
            for term in &terms {
                if name.contains(term) {
                    hits += 1;
                    name_hits += 1;
                } else if description.contains(term) {
                    hits += 1;
                }
            }
            if hits > 0 {
                matches.push((entry, (entry.name.to_lowercase() == query, hits, name_hits)));
            }
        }
        matches.sort_by(|(a, score_a), (b, score_b)| {
            score_b.cmp(score_a).then_with(|| a.name.cmp(&b.name))
        });
        let total = matches.len();
        let selected: Vec<_> = matches
            .into_iter()
            .take(limit)
            .map(|(entry, _)| entry)
            .collect();
        let loaded: Vec<_> = selected.iter().map(|entry| entry.name.as_str()).collect();
        let summaries: Vec<_> = selected.iter().map(|entry| json!({"name":entry.name,"description":entry.description.chars().take(400).collect::<String>()})).collect();
        Ok(json!({"tools":summaries,"loaded_tools":loaded,"total_matches":total,"has_more":total>limit}).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mcp_search_ranks_names_and_descriptions_and_matches_case_insensitively() {
        let tool = McpSearchTool::new(vec![
            (
                "mcp__rustrak__list_issues".into(),
                "Browse project issues".into(),
            ),
            ("mcp__devflow__search_issues".into(), "Search issues".into()),
            (
                "mcp__rustrak__search_issues".into(),
                "Найти задачи проекта".into(),
            ),
        ]);
        for query in [
            "RUSTRaK search issues",
            "mcp__rustrak__search_issues",
            "НАЙТИ ЗАДАЧИ",
        ] {
            let result = tool
                .execute(json!({"query":query,"limit":1}))
                .await
                .unwrap()
                .output;
            assert_eq!(
                result["loaded_tools"],
                json!(["mcp__rustrak__search_issues"])
            );
        }
        let result = tool
            .execute(json!({"query":"issues","limit":null}))
            .await
            .unwrap()
            .output;
        assert_eq!(result["tools"][0]["name"], "mcp__devflow__search_issues");
        assert_eq!(result["tools"][1]["name"], "mcp__rustrak__list_issues");
    }

    #[tokio::test]
    async fn mcp_search_bounds_results_and_rejects_invalid_requests() {
        let tool = McpSearchTool::new(
            (0..20)
                .map(|i| (format!("mcp__service__tool{i:02}"), "Новости ".repeat(100)))
                .collect(),
        );
        for request in [
            json!({"query":"","limit":1}),
            json!({"query":"   "}),
            json!({"query":"---"}),
            json!({"query":"x".repeat(257)}),
            json!({"query":"tool","limit":0}),
            json!({"query":"tool","limit":17}),
            json!({"query":"tool","unknown":true}),
        ] {
            assert!(tool.execute(request).await.is_err());
        }
        for (limit, count) in [(None, 8), (Some(16), 16)] {
            let result = tool
                .execute(json!({"query":"НОВОСТИ","limit":limit}))
                .await
                .unwrap()
                .output;
            assert_eq!(result["tools"].as_array().unwrap().len(), count);
            assert_eq!(result["total_matches"], 20);
            assert_eq!(result["has_more"], true);
            assert!(
                result["tools"][0]["description"]
                    .as_str()
                    .unwrap()
                    .chars()
                    .count()
                    <= 400
            );
        }
        let result = tool
            .execute(json!({"query":"nonexistentkeyword","limit":2}))
            .await
            .unwrap()
            .output;
        assert_eq!(result["tools"], json!([]));
        assert_eq!(result["loaded_tools"], json!([]));
    }
}
