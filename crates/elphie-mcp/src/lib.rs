mod config;
mod connection;

pub use config::{McpConfig, ServerConfig, TransportConfig};

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use anyhow::{Result, anyhow, bail, ensure};
use elphie_core::ToolDefinition;
use elphie_oauth::AuthorizedResource;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, PaginatedRequestParams,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::time::timeout;

use connection::Connection;

const MAX_OUTPUT: usize = 32 * 1024;
const MAX_CATALOGUE_BYTES: usize = 256 * 1024;
const MAX_PAGES: usize = 32;
const SETUP_TIMEOUT: Duration = Duration::from_secs(25);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

struct Route {
    server: usize,
    name: String,
    title: String,
}

#[derive(Default)]
pub struct McpClient {
    connections: Vec<Connection>,
    routes: HashMap<String, Route>,
    definitions: Vec<ToolDefinition>,
}

impl McpClient {
    pub async fn connect(servers: &[ServerConfig]) -> Result<Self> {
        config::validate_servers(servers)?;
        let mut client = Self::default();
        for server in servers {
            client.add(server, None).await?;
        }
        Ok(client)
    }

    /// Connect to the HTTP resource for which the host completed OAuth.
    pub async fn connect_authorized(
        server: &ServerConfig,
        grant: AuthorizedResource,
    ) -> Result<Self> {
        config::validate_servers(std::slice::from_ref(server))?;
        let mut client = Self::default();
        client.add(server, Some(grant)).await?;
        Ok(client)
    }

    async fn add(
        &mut self,
        server: &ServerConfig,
        grant: Option<AuthorizedResource>,
    ) -> Result<()> {
        timeout(SETUP_TIMEOUT, async {
            let connection = Connection::open(&server.transport, grant).await?;
            ensure!(
                connection
                    .service
                    .peer_info()
                    .is_some_and(|s| s.capabilities.tools.is_some()),
                "Server does not advertise tools"
            );
            let mut remaining: HashSet<_> = server.tools.iter().map(String::as_str).collect();
            let mut seen_cursors = HashSet::new();
            let mut page = None;
            let mut catalogue_bytes = 0;
            for _ in 0..MAX_PAGES {
                let result = connection
                    .service
                    .list_tools(page.take())
                    .await
                    .map_err(|_| anyhow!("Tool discovery failed"))?;
                for tool in result.tools {
                    let name = tool.name.as_ref();
                    if !server.tools.iter().any(|allowed| allowed == name) {
                        continue;
                    }
                    ensure!(remaining.remove(name), "Duplicate MCP tool {name}");
                    let alias = model_name(&server.name, name);
                    let title = format!("{} / {name}", server.name);
                    let parameters = Value::Object((*tool.input_schema).clone());
                    ensure!(
                        parameters["type"] == "object",
                        "Invalid input schema for tool {name}"
                    );
                    let description = tool
                        .description
                        .map_or_else(|| title.clone(), |d| d.into_owned());
                    catalogue_bytes += description.len() + serde_json::to_vec(&parameters)?.len();
                    ensure!(
                        catalogue_bytes <= MAX_CATALOGUE_BYTES,
                        "Tool catalogue exceeds its size limit"
                    );
                    ensure!(
                        !self.routes.contains_key(&alias),
                        "Duplicate model-facing tool name"
                    );
                    self.routes.insert(
                        alias.clone(),
                        Route {
                            server: self.connections.len(),
                            name: name.into(),
                            title,
                        },
                    );
                    self.definitions.push(ToolDefinition {
                        name: alias,
                        description,
                        parameters,
                    });
                }
                let Some(cursor) = result.next_cursor else {
                    if let Some(name) = server
                        .tools
                        .iter()
                        .find(|name| remaining.contains(name.as_str()))
                    {
                        bail!("Configured tool {name} was not found");
                    }
                    self.connections.push(connection);
                    return Ok(());
                };
                ensure!(
                    seen_cursors.insert(cursor.clone()),
                    "Repeated tool discovery cursor"
                );
                page = Some(PaginatedRequestParams::default().with_cursor(Some(cursor)));
            }
            bail!("Tool discovery exceeded the page limit")
        })
        .await
        .map_err(|_| anyhow!("MCP server {} setup timed out", server.name))?
        .map_err(|cause| anyhow!("MCP server {}: {cause}", server.name))
    }

    pub fn definitions(&self) -> &[ToolDefinition] {
        &self.definitions
    }

    pub fn title(&self, model_name: &str) -> Option<&str> {
        self.routes
            .get(model_name)
            .map(|route| route.title.as_str())
    }

    pub async fn execute(&self, name: &str, arguments: &Value) -> Result<String, String> {
        let route = self.routes.get(name).ok_or("Unknown MCP tool.")?;
        let args = arguments
            .as_object()
            .ok_or("MCP tool arguments must be an object.")?;
        if arguments.to_string().len() > MAX_OUTPUT {
            return Err("MCP tool arguments exceed 32 KiB.".into());
        }
        let request = CallToolRequestParams::new(route.name.clone()).with_arguments(args.clone());
        let response = timeout(
            CALL_TIMEOUT,
            self.connections[route.server]
                .service
                .call_tool_once(request),
        )
        .await
        .map_err(|_| "MCP tool timed out.")?
        .map_err(|_| "MCP tool request failed.")?;
        match response {
            CallToolResponse::Complete(result) => output(result),
            _ => Err("MCP tool requires an unsupported follow-up.".into()),
        }
    }
}

fn model_name(server: &str, tool: &str) -> String {
    let name = format!("mcp_{server}__{tool}");
    if name.len() <= 64 && !server.contains("__") && !server.ends_with('_') && !tool.contains('.') {
        return name;
    }
    // Hashed names have no double underscore, so they cannot collide with readable names.
    let mut digest = Sha256::new();
    digest.update(server.as_bytes());
    digest.update([0]);
    digest.update(tool.as_bytes());
    format!("mcp_{:.60}", format!("{:x}", digest.finalize()))
}

fn output(result: CallToolResult) -> Result<String, String> {
    if result.is_error == Some(true) {
        return Err("MCP server reported a tool error.".into());
    }
    let text: Vec<_> = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
        .collect();
    let value = match (text.is_empty(), result.structured_content) {
        (true, Some(value)) => value.to_string(),
        (false, Some(value)) => json!({"text":text,"structuredContent":value}).to_string(),
        (true, None) if !result.content.is_empty() => {
            return Err("MCP tool returned unsupported content.".into());
        }
        (_, None) => text.join("\n"),
    };
    if value.len() > MAX_OUTPUT {
        return Err("MCP tool result exceeds 32 KiB.".into());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::ContentBlock;

    #[test]
    fn provider_names_are_stable_and_unambiguous() {
        let names = [
            model_name("a_", "b"),
            model_name("a", "_b"),
            model_name("a", "tool.long"),
            model_name("a", "tool_long"),
            model_name("a", &"x".repeat(128)),
        ];
        assert!(names.iter().all(|s| {
            s.len() <= 64
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        }));
        assert_eq!(names.iter().collect::<HashSet<_>>().len(), names.len());
        assert_eq!(names[2], model_name("a", "tool.long"));
    }

    #[test]
    fn output_checks_format_and_byte_count_before_delivery() {
        let text = |size| CallToolResult::success(vec![ContentBlock::text("a".repeat(size))]);
        assert_eq!(output(text(MAX_OUTPUT)).unwrap().len(), MAX_OUTPUT);
        assert!(output(text(MAX_OUTPUT + 1)).is_err());
        let image: CallToolResult = serde_json::from_value(json!({
            "content":[{"type":"image","mimeType":"image/png","data":"AA=="}]
        }))
        .unwrap();
        assert_eq!(
            output(image).unwrap_err(),
            "MCP tool returned unsupported content."
        );
        let structured: CallToolResult =
            serde_json::from_value(json!({"structuredContent":null})).unwrap();
        assert_eq!(output(structured).unwrap(), "null");
    }
}
