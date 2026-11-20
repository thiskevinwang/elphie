use std::{collections::HashSet, future::Future, path::Path, pin::Pin, sync::Arc};

use agent_client_protocol::{Error, Result, v2::schema::*};
use elphie_mcp::{McpClient, McpConfig, ServerConfig, TransportConfig};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const MCP_SERVERS_META_KEY: &str = "io.github.thiskevinwang.elphie/mcp-servers";

#[derive(Clone)]
pub(super) struct HostMcp {
    config: Arc<McpConfig>,
    slots: Arc<Semaphore>,
}

pub(super) struct SessionMcp {
    pub client: McpClient,
    names: Vec<String>,
    _slots: OwnedSemaphorePermit,
}

impl SessionMcp {
    pub(super) fn meta(&self) -> Meta {
        [(MCP_SERVERS_META_KEY.into(), serde_json::json!(self.names))]
            .into_iter()
            .collect()
    }

    pub(super) fn names(&self) -> &[String] {
        &self.names
    }
}

pub(super) type OpenMcp = Pin<Box<dyn Future<Output = Result<Option<Arc<SessionMcp>>>> + Send>>;

impl HostMcp {
    pub(super) fn new(config: Arc<McpConfig>) -> Self {
        Self {
            config,
            slots: Arc::new(Semaphore::new(64)),
        }
    }

    pub(super) fn capabilities(&self) -> SessionCapabilities {
        let session = SessionCapabilities::new();
        if self.config.servers().is_empty() {
            return session;
        }
        let mut mcp = McpCapabilities::new();
        for server in self.config.servers() {
            match server.transport {
                TransportConfig::Http { .. } => mcp.http = Some(McpHttpCapabilities::new()),
                TransportConfig::Stdio { .. } => mcp.stdio = Some(McpStdioCapabilities::new()),
            }
        }
        session.mcp(mcp)
    }

    pub(super) fn select(&self, requested: &[McpServer]) -> Result<Vec<ServerConfig>> {
        let approved = self.config.servers();
        if requested.is_empty() {
            return Ok(approved.to_vec());
        }
        let mut selected = HashSet::new();
        for descriptor in requested {
            let Some(server) =
                approved
                    .iter()
                    .find(|server| match (&server.transport, descriptor) {
                        (TransportConfig::Http { url, .. }, McpServer::Http(input)) => {
                            input.name == server.name
                                && input.headers.is_empty()
                                && url::Url::parse(&input.url).as_ref() == Ok(url)
                        }
                        (TransportConfig::Stdio { command, args, .. }, McpServer::Stdio(input)) => {
                            input.name == server.name
                                && AsRef::<Path>::as_ref(&input.command) == command
                                && &input.args == args
                                && input.env.is_empty()
                        }
                        _ => false,
                    })
            else {
                return Err(
                    Error::invalid_params().data("MCP server does not match a configured grant.")
                );
            };
            if !selected.insert(&server.name) {
                return Err(Error::invalid_params().data("Duplicate MCP server name."));
            }
        }
        Ok(approved
            .iter()
            .filter(|s| selected.contains(&s.name))
            .cloned()
            .collect())
    }

    pub(super) fn open(&self, servers: Vec<ServerConfig>) -> OpenMcp {
        let slots = self.slots.clone();
        Box::pin(async move {
            if servers.is_empty() {
                return Ok(None);
            }
            let permits = slots
                .try_acquire_many_owned(servers.len() as u32)
                .map_err(|_| {
                    Error::new(ErrorCode::InternalError, "MCP connection limit reached.")
                })?;
            let client = McpClient::connect(&servers)
                .await
                .map_err(|error| Error::new(ErrorCode::InternalError, error.to_string()))?;
            Ok(Some(Arc::new(SessionMcp {
                client,
                names: servers.into_iter().map(|s| s.name).collect(),
                _slots: permits,
            })))
        })
    }
}
