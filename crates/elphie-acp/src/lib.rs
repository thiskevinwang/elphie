mod session;

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
};

use agent_client_protocol::{
    Agent, ConnectTo, Error, Stdio, on_receive_notification, on_receive_request,
    schema::ProtocolVersion, v2::schema,
};
use elphie_core::Provider;
use tokio::sync::mpsc;
use uuid::Uuid;

use session::{Command, Session};

pub async fn serve(provider: Arc<dyn Provider>) -> anyhow::Result<()> {
    Ok(connect(provider, Stdio::new()).await?)
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
) -> agent_client_protocol::Result<()> {
    let sessions = Sessions::default();
    Agent
        .v2()
        .name("Elphie")
        .on_receive_request(
            async |_: schema::InitializeRequest, response, _| {
                response.respond(
                    schema::InitializeResponse::new(
                        ProtocolVersion::V2,
                        schema::Implementation::new("elphie", env!("CARGO_PKG_VERSION"))
                            .title("Elphie"),
                    )
                    .capabilities(
                        schema::AgentCapabilities::new()
                            .session(schema::SessionCapabilities::new()),
                    ),
                )
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async |request: schema::NewSessionRequest, response, connection| {
                if let Err(error) =
                    validate_setup(&request.cwd, &request.mcp_servers, &request.system_prompt)
                {
                    return response.respond_with_error(error);
                }
                let id = schema::SessionId::new(Uuid::new_v4().to_string());
                let (tx, rx) = mpsc::unbounded_channel();
                let session = Session::new(
                    id.clone(),
                    request.cwd.clone(),
                    provider.clone(),
                    connection.clone(),
                );
                connection.spawn(session.run(rx))?;
                sessions.0.lock().unwrap().insert(
                    id.clone(),
                    SessionHandle {
                        cwd: request.cwd,
                        commands: tx,
                    },
                );
                response.respond(schema::NewSessionResponse::new(id))
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
                if let Err(error) =
                    validate_setup(&request.cwd, &request.mcp_servers, &request.system_prompt)
                {
                    return response.respond_with_error(error);
                }
                sessions.send(
                    &request.session_id,
                    Command::Resume(request.cwd, request.replay_from, response),
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
    mcp: &[schema::McpServer],
    system_prompt: &Option<schema::SystemPrompt>,
) -> agent_client_protocol::Result<()> {
    if !AsRef::<Path>::as_ref(cwd).is_absolute() || !mcp.is_empty() || system_prompt.is_some() {
        return Err(Error::invalid_params());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
