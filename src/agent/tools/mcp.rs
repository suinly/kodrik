use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, ensure};
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::{
    RoleClient, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, ClientCapabilities, ClientConfig, Implementation,
        ProtocolVersion,
    },
    service::RunningService,
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::Value;

use super::{Tool, ToolOutput};
use crate::config::McpServerConfig;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

struct McpTool {
    name: String,
    definition: rmcp::model::Tool,
    client: Arc<RunningService<RoleClient, ClientConfig>>,
}

pub(crate) async fn connect(
    servers: &HashMap<String, McpServerConfig>,
) -> anyhow::Result<Vec<Arc<dyn Tool>>> {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    let mut names = HashSet::new();
    for (server, config) in servers {
        let connected = tokio::time::timeout(CONNECT_TIMEOUT, async {
            let McpServerConfig::Http { url, headers } = config;
            let url = reqwest::Url::parse(url).context("Invalid MCP URL")?;
            ensure!(
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.fragment().is_none(),
                "MCP URL must use HTTP(S) without embedded credentials or fragment"
            );
            let mut parsed_headers = HashMap::new();
            for (name, value) in headers {
                let name =
                    HeaderName::from_bytes(name.as_bytes()).context("Invalid MCP header name")?;
                let mut value = HeaderValue::from_str(value).context("Invalid MCP header value")?;
                value.set_sensitive(true);
                ensure!(
                    parsed_headers.insert(name, value).is_none(),
                    "Duplicate MCP header name"
                );
            }
            let http = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .build()?;
            let transport = StreamableHttpClientTransport::with_client(
                http,
                StreamableHttpClientTransportConfig::with_uri(url.as_str())
                    .custom_headers(parsed_headers)
                    // shortcut: expired sessions need a restart; reconnect before a later call if needed.
                    .reinit_on_expired_session(false),
            );
            let info = ClientConfig::new(
                ClientCapabilities::default(),
                Implementation::new("kodrik", env!("CARGO_PKG_VERSION")),
            )
            .with_protocol_version(ProtocolVersion::V_2025_11_25);
            let client = Arc::new(
                info.serve(transport)
                    .await
                    .context("MCP initialization failed")?,
            );
            let definitions = client
                .list_all_tools()
                .await
                .context("MCP tool discovery failed")?;
            Ok::<_, anyhow::Error>((client, definitions))
        })
        .await
        .with_context(|| {
            format!("MCP server {server}: connection/discovery timed out after 30 seconds")
        })?
        .with_context(|| format!("MCP server {server}"))?;
        let (client, definitions) = connected;
        for definition in definitions {
            let name = format!("mcp__{server}__{}", definition.name);
            ensure!(
                name.len() <= 64
                    && name
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-'),
                "MCP tool name {name:?} must fit 64 ASCII letters, digits, underscores or hyphens"
            );
            ensure!(
                names.insert(name.clone()),
                "Duplicate MCP tool name: {name}"
            );
            tools.push(Arc::new(McpTool {
                name,
                definition,
                client: client.clone(),
            }));
        }
    }
    Ok(tools)
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn description(&self) -> String {
        self.definition
            .description
            .as_deref()
            .unwrap_or("")
            .to_owned()
    }

    fn parameters(&self) -> Value {
        Value::Object((*self.definition.input_schema).clone())
    }

    fn strict_schema(&self) -> bool {
        false
    }

    async fn execute(&self, properties: Value) -> anyhow::Result<ToolOutput> {
        let Value::Object(arguments) = properties else {
            anyhow::bail!("MCP tool arguments must be an object");
        };
        let result = tokio::time::timeout(
            CALL_TIMEOUT,
            self.client.call_tool_once(
                CallToolRequestParams::new(self.definition.name.clone()).with_arguments(arguments),
            ),
        )
        .await
        .with_context(|| {
            format!(
                "MCP tool {} timed out after 60 seconds; execution outcome is unknown",
                self.name
            )
        })?
        .with_context(|| format!("MCP tool {} failed", self.name))?;
        match result {
            CallToolResponse::Complete(result) => Ok(serde_json::to_value(result)?.into()),
            _ => anyhow::bail!(
                "MCP tool {} requires an unsupported interactive or task response",
                self.name
            ),
        }
    }
}
