use std::{env, error::Error, process::Stdio, time::Duration};

use anyhow::{Context, Result, anyhow, ensure};
use elphie_oauth::AuthorizedResource;
use rmcp::{
    ClientLifecycleMode, ClientServiceExt, RoleClient,
    model::{ClientConfig, Icon, Implementation, ProtocolVersion},
    service::RunningService,
    transport::{
        IntoTransport, StreamableHttpClientTransport, async_rw::AsyncRwTransport, auth::AuthClient,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use tokio::process::{Child, Command};

use crate::TransportConfig;

const MAX_WIRE_BYTES: usize = 1024 * 1024;
type Service = RunningService<RoleClient, ClientConfig>;

pub(super) struct Connection {
    pub service: Service,
    // Retain the process for the session. kill_on_drop also covers failed startup.
    _child: Option<Child>,
}

impl Connection {
    pub async fn open(
        transport: &TransportConfig,
        grant: Option<AuthorizedResource>,
    ) -> Result<Self> {
        match transport {
            TransportConfig::Http {
                url,
                bearer_token_env,
            } => {
                let http = reqwest::Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .retry(reqwest::retry::never())
                    .connect_timeout(Duration::from_secs(10))
                    .build()
                    .map_err(|_| anyhow!("Cannot create MCP HTTP client"))?;
                let mut config = StreamableHttpClientTransportConfig::with_uri(url.as_str())
                    .max_sse_event_size(MAX_WIRE_BYTES)
                    .max_json_response_size(MAX_WIRE_BYTES)
                    .max_error_response_size(0);
                let service = if let Some(grant) = grant {
                    let (resource, manager) = grant.into_parts();
                    ensure!(
                        resource == *url && bearer_token_env.is_none(),
                        "OAuth grant does not match the MCP server configuration"
                    );
                    handshake(StreamableHttpClientTransport::with_client(
                        AuthClient::new(http, manager),
                        config,
                    ))
                    .await?
                } else {
                    if let Some(key) = bearer_token_env {
                        let token =
                            env::var(key)
                                .ok()
                                .filter(|s| !s.is_empty())
                                .ok_or_else(|| {
                                    anyhow!("MCP bearer token environment variable is unavailable")
                                })?;
                        config = config.auth_header(token);
                    }
                    handshake(StreamableHttpClientTransport::with_client(http, config)).await?
                };
                Ok(Self {
                    service,
                    _child: None,
                })
            }
            TransportConfig::Stdio {
                command,
                args,
                env: names,
            } => {
                ensure!(grant.is_none(), "MCP OAuth requires HTTP");
                let mut process = Command::new(command);
                process
                    .current_dir(command.parent().context("Invalid MCP executable path")?)
                    .env_clear()
                    .args(args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .kill_on_drop(true);
                for name in names {
                    let value = env::var_os(name)
                        .context("MCP child environment variable is unavailable")?;
                    process.env(name, value);
                }
                let mut child = process
                    .spawn()
                    .map_err(|_| anyhow!("Cannot start configured MCP executable"))?;
                let stdout = child.stdout.take().context("MCP stdout unavailable")?;
                let stdin = child.stdin.take().context("MCP stdin unavailable")?;
                let service = handshake(
                    AsyncRwTransport::new(stdout, stdin).with_max_line_length(MAX_WIRE_BYTES),
                )
                .await?;
                Ok(Self {
                    service,
                    _child: Some(child),
                })
            }
        }
    }
}

async fn handshake<T, E, A>(transport: T) -> Result<Service>
where
    T: IntoTransport<RoleClient, E, A>,
    E: Error + Send + Sync + 'static,
{
    let mut icon = Icon::new(elphie_core::icon_data_uri());
    icon.mime_type = Some("image/webp".into());
    icon.sizes = Some(vec!["192x192".into()]);
    let info = Implementation::new("elphie", env!("CARGO_PKG_VERSION"))
        .with_title("Elphie")
        .with_icons(vec![icon]);
    ClientConfig::new(Default::default(), info)
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Auto {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
                legacy_version: Some(ProtocolVersion::V_2025_11_25),
            },
        )
        .await
        .map_err(|_| anyhow!("MCP handshake failed"))
}
