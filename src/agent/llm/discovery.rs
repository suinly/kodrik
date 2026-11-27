use std::collections::HashSet;

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::{message::Message, tools::Tool};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolDiscovery {
    #[default]
    Compatible,
    Native,
}

const SEARCH: &str = "mcp_search";
const CALL: &str = "mcp_call";
const NAMESPACE: &str = "kodrik";

#[cfg(test)]
#[path = "discovery_tests.rs"]
mod tests;

fn deferred_names(tools: &[&dyn Tool]) -> HashSet<String> {
    tools
        .iter()
        .flat_map(|tool| tool.discovery_names())
        .collect()
}

/// Only the wire representation changes; runtime history retains the audited tool identity.
pub(super) fn project(
    request: &mut Value,
    tools: &[&dyn Tool],
    mode: ToolDiscovery,
) -> anyhow::Result<()> {
    let deferred = deferred_names(tools);
    let Some(search) = tools.iter().find(|tool| tool.name() == SEARCH) else {
        return Ok(());
    };
    let native = mode == ToolDiscovery::Native;
    let api_tools = request["tools"]
        .as_array_mut()
        .context("Missing tools array")?;
    api_tools.retain(|tool| !deferred.contains(tool["name"].as_str().unwrap_or_default()));
    let search_definition = api_tools
        .iter_mut()
        .find(|tool| tool["name"] == SEARCH)
        .context("Missing search definition")?;
    if native {
        *search_definition = json!({"type":"tool_search", "execution":"client", "description":search.description(), "parameters":search.parameters()});
    } else {
        search_definition["description"] = json!(format!(
            "{} Use mcp_call with the exact returned name and arguments encoded as a JSON object string. Definitions are returned in tool_definitions; they are not added to the tools array.",
            search.description()
        ));
        api_tools.push(json!({"type":"function", "name":CALL,
            "description":"Call a deferred tool returned by mcp_search. Only recently loaded and currently available tools can be called. All normal permissions and approvals apply.",
            "parameters":{"type":"object","properties":{"name":{"type":"string"},"arguments":{"type":"string","description":"Tool arguments encoded as a JSON object string."}},"required":["name","arguments"],"additionalProperties":false}, "strict":true}));
    }
    ensure!(
        api_tools.len() <= 128,
        "More than 128 tools including discovery; reduce the advertised catalog"
    );

    let mut searches = HashSet::new();
    let input = request["input"]
        .as_array_mut()
        .context("Missing input array")?;
    let mut projected = Vec::with_capacity(input.len());
    for mut item in input.drain(..) {
        match item["type"].as_str() {
            Some("function_call") if native && item["name"] == SEARCH => {
                let id = item["call_id"]
                    .as_str()
                    .context("Missing search call ID")?
                    .to_owned();
                searches.insert(id);
                let arguments: Value = serde_json::from_str(
                    item["arguments"]
                        .as_str()
                        .context("Missing search arguments")?,
                )?;
                item = json!({"type":"tool_search_call","execution":"client","call_id":item["call_id"],"status":"completed","arguments":arguments});
            }
            Some("function_call")
                if deferred.contains(item["name"].as_str().unwrap_or_default()) =>
            {
                if native {
                    item["namespace"] = json!(NAMESPACE);
                } else {
                    let arguments = json!({"name":item["name"],"arguments":item["arguments"]});
                    item["name"] = json!(CALL);
                    item["arguments"] = json!(arguments.to_string());
                }
            }
            Some("function_call_output")
                if native
                    && item["call_id"]
                        .as_str()
                        .is_some_and(|id| searches.remove(id)) =>
            {
                let output: Value = serde_json::from_str(
                    item["output"].as_str().context("Missing search result")?,
                )?;
                let mut definitions = output["tool_definitions"]
                    .as_array()
                    .cloned()
                    .unwrap_or_else(|| {
                        // Legacy sessions saved discovery names without schema snapshots.
                        output["loaded_tools"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|name| search.discovery_definition(name.as_str()?))
                            .collect()
                    });
                for definition in &mut definitions {
                    definition["defer_loading"] = json!(true);
                }
                let loaded = if definitions.is_empty() {
                    vec![]
                } else {
                    vec![
                        json!({"type":"namespace","name":NAMESPACE,"description":"Discovered Kodrik tools. Execute using their exact names; normal permissions apply.","tools":definitions}),
                    ]
                };
                projected.push(json!({"type":"tool_search_output","execution":"client","call_id":item["call_id"],"status":"completed","tools":loaded}));
                if let Some(error) = output.get("error") {
                    projected.push(json!({"role":"developer","content":format!("Tool search failed; no tools were loaded. Untrusted error data: {error}")}));
                }
                continue;
            }
            _ => {}
        }
        projected.push(item);
    }
    *input = projected;
    Ok(())
}

pub(super) fn normalize_calls(messages: &mut [Message], tools: &[&dyn Tool]) {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Call {
        name: String,
        arguments: String,
    }
    let deferred = deferred_names(tools);
    for message in messages {
        if let Message::ToolCall(call) = message
            && call.name == CALL
        {
            // Invalid wrappers stay intact for the runtime's model-visible tool-error path.
            let Ok(wrapped) = serde_json::from_value::<Call>(call.arguments.clone()) else {
                continue;
            };
            if !deferred.contains(&wrapped.name)
                || !tools
                    .iter()
                    .any(|tool| tool.name() == wrapped.name && tool.available())
            {
                continue;
            }
            let Ok(arguments) = serde_json::from_str::<Value>(&wrapped.arguments) else {
                continue;
            };
            if !arguments.is_object() {
                continue;
            }
            call.name = wrapped.name;
            call.arguments = arguments;
        }
    }
}
