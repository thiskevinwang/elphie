mod mcp;
mod metadata;
mod model;
mod session;
mod tool_call;

pub use mcp::MCP_SERVERS_META_KEY;
pub use metadata::implementation;

use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use agent_client_protocol::{
    Agent, ConnectTo, Error, Stdio, on_receive_notification, on_receive_request,
    schema::ProtocolVersion, v2::schema,
};
use elphie_core::Provider;
use elphie_mcp::McpConfig;
use elphie_tools::Workspace;
use tokio::sync::mpsc;
use uuid::Uuid;

use session::{Command, Session};

/// The selected model ID in a successful, replayable ACP agent message.
pub const MODEL_META_KEY: &str = "io.github.thiskevinwang.elphie/model";

pub async fn serve(provider: Arc<dyn Provider>) -> anyhow::Result<()> {
    serve_with_tools(provider, None).await
}

/// All sessions share the workspace granted by the host. ACP requests cannot change it.
pub async fn serve_with_tools(
    provider: Arc<dyn Provider>,
    workspace: Option<Arc<Workspace>>,
) -> anyhow::Result<()> {
    serve_with_mcp(provider, workspace, Arc::new(McpConfig::default())).await
}

/// MCP tools use only the server and tool grants supplied by the host.
pub async fn serve_with_mcp(
    provider: Arc<dyn Provider>,
    workspace: Option<Arc<Workspace>>,
    config: Arc<McpConfig>,
) -> anyhow::Result<()> {
    Ok(connect(provider, Stdio::new(), workspace, config).await?)
}

#[derive(Default)]
struct Sessions(Mutex<HashMap<schema::SessionId, SessionHandle>>);

struct SessionHandle {
    cwd: schema::AbsolutePath,
    commands: mpsc::UnboundedSender<Command>,
}

impl Sessions {
    fn send(&self, id: &schema::SessionId, command: Command) -> agent_client_protocol::Result<()> {
        match self.0.lock().unwrap().get(id) {
            Some(handle) => handle
                .commands
                .send(command)
                .map_err(|_| Error::internal_error()),
            None => command.unknown_session(id),
        }
    }
}

async fn connect(
    provider: Arc<dyn Provider>,
    transport: impl ConnectTo<Agent>,
    workspace: Option<Arc<Workspace>>,
    config: Arc<McpConfig>,
) -> agent_client_protocol::Result<()> {
    let sessions = Arc::new(Sessions::default());
    let mcp = mcp::HostMcp::new(config);
    let supports_form = AtomicBool::new(false);
    Agent
        .v2()
        .name("Elphie")
        .on_receive_request(
            async |request: schema::InitializeRequest, response, _| {
                supports_form.store(
                    request
                        .capabilities
                        .elicitation
                        .is_some_and(|e| e.form.is_some()),
                    Ordering::Relaxed,
                );
                response.respond(
                    schema::InitializeResponse::new(
                        ProtocolVersion::V2,
                        implementation("elphie", "Elphie", env!("CARGO_PKG_VERSION")),
                    )
                    .capabilities(schema::AgentCapabilities::new().session(mcp.capabilities())),
                )
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async |request: schema::NewSessionRequest, response, connection| {
                if let Err(error) = validate_setup(&request.cwd, &request.system_prompt) {
                    return response.respond_with_error(error);
                }
                let servers = match mcp.select(&request.mcp_servers) {
                    Ok(servers) => servers,
                    Err(error) => return response.respond_with_error(error),
                };
                let form = supports_form.load(Ordering::Relaxed);
                let setup = mcp.open(servers);
                let provider = provider.clone();
                let workspace = workspace.clone();
                let sessions = sessions.clone();
                let client = connection.clone();
                connection.spawn(async move {
                    let cancellation = response.cancellation();
                    let mcp = tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => return response.respond_with_error(Error::request_cancelled()),
                        result = setup => match result {
                            Ok(mcp) => mcp,
                            Err(error) => return response.respond_with_error(error),
                        },
                    };
                    let id = schema::SessionId::new(Uuid::new_v4().to_string());
                    let (tx, rx) = mpsc::unbounded_channel();
                    let meta = mcp.as_ref().map(|s| s.meta());
                    let session = Session::new(
                        id.clone(), request.cwd.clone(), provider, client.clone(), form,
                        workspace, mcp,
                    );
                    client.spawn(session.run(rx))?;
                    let mut sessions = sessions.0.lock().unwrap();
                    response.respond(
                        schema::NewSessionResponse::new(id.clone())
                            .available_commands(model::commands(form))
                            .meta(meta),
                    )?;
                    sessions.insert(id, SessionHandle { cwd: request.cwd, commands: tx });
                    Ok(())
                })
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async |request: schema::ListSessionsRequest, response, _| {
                if request.cursor.is_some()
                    || request
                        .cwd
                        .as_ref()
                        .is_some_and(|cwd| !AsRef::<Path>::as_ref(cwd).is_absolute())
                {
                    return response.respond_with_error(Error::invalid_params());
                }
                let listed = sessions
                    .0
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(_, handle)| {
                        request
                            .cwd
                            .as_ref()
                            .is_none_or(|filter| filter == &handle.cwd)
                    })
                    .map(|(id, handle)| schema::SessionInfo::new(id.clone(), handle.cwd.clone()))
                    .collect::<Vec<_>>();
                response.respond(schema::ListSessionsResponse::new(listed))
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async |request: schema::ResumeSessionRequest, response, _| {
                if let Err(error) = validate_setup(&request.cwd, &request.system_prompt) {
                    return response.respond_with_error(error);
                }
                let servers = match mcp.select(&request.mcp_servers) {
                    Ok(servers) => servers,
                    Err(error) => return response.respond_with_error(error),
                };
                let names = servers.iter().map(|server| server.name.clone()).collect();
                sessions.send(
                    &request.session_id,
                    Command::Resume(
                        request.cwd, request.replay_from, names, mcp.open(servers), response,
                    ),
                )
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async |request: schema::PromptRequest, response, _| {
                sessions.send(
                    &request.session_id,
                    Command::Prompt(request.prompt, request.idempotency_key, response),
                )
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async |request: schema::CloseSessionRequest, response, _| {
                sessions.send(&request.session_id, Command::Close(response))
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            async |request: schema::CancelSessionNotification, _| {
                sessions.send(&request.session_id, Command::Cancel)
            },
            on_receive_notification!(),
        )
        .connect_to(transport)
        .await
}

fn validate_setup(
    cwd: &schema::AbsolutePath,
    system_prompt: &Option<schema::SystemPrompt>,
) -> agent_client_protocol::Result<()> {
    if !AsRef::<Path>::as_ref(cwd).is_absolute() || system_prompt.is_some() {
        return Err(Error::invalid_params());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
