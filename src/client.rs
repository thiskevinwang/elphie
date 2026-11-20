use agent_client_protocol::schema::{MaybeUndefined, ProtocolVersion};
use agent_client_protocol::v2::{ConnectionTo, schema};
use agent_client_protocol::{AcpAgent, Agent, Client, Error};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::view_model::{Action, Edit, Event, Phase};
use elphie_core::Role;

pub async fn run(
    agent: AcpAgent,
    mut actions: UnboundedReceiver<Action>,
    events: UnboundedSender<Event>,
) -> anyhow::Result<()> {
    let updates = events.clone();
    Client
        .v2()
        .name("Elphie terminal")
        .on_receive_notification(
            async move |notification: schema::UpdateSessionNotification, _: ConnectionTo<Agent>| {
                if let Some(event) = project(notification.update) {
                    let _ = updates.send(event);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(agent, async move |connection| {
            let init = connection
                .send_request(schema::InitializeRequest::new(
                    ProtocolVersion::V2,
                    elphie_acp::implementation(
                        "elphie-terminal",
                        "Elphie Terminal",
                        env!("CARGO_PKG_VERSION"),
                    ),
                ))
                .block_task()
                .await?;
            if init.protocol_version != ProtocolVersion::V2 || init.capabilities.session.is_none() {
                return Err(Error::invalid_params().data("Elphie requires ACP v2 sessions"));
            }
            let session = connection
                .build_session_cwd()?
                .start_session()
                .block_task()
                .await?
                .session;
            let _ = events.send(Event::Phase(Phase::Ready));
            while let Some(action) = actions.recv().await {
                match action {
                    Action::Send(prompt) => {
                        if let Err(error) = session.send_prompt(prompt.as_str()).block_task().await
                        {
                            let _ = events.send(Event::Failed(error.to_string()));
                        }
                    }
                    Action::Cancel => {
                        connection.send_notification(schema::CancelSessionNotification::new(
                            session.session_id().clone(),
                        ))?;
                    }
                }
            }
            session.close().block_task().await?;
            Ok(())
        })
        .await?;
    Ok(())
}

fn text(block: schema::ContentBlock) -> String {
    match block {
        schema::ContentBlock::Text(text) => text.text,
        schema::ContentBlock::ResourceLink(link) => format!("{} ({})", link.name, link.uri),
        _ => "[Unsupported content]".into(),
    }
}

fn snapshot(
    role: Role,
    id: schema::MessageId,
    content: MaybeUndefined<Vec<schema::ContentBlock>>,
) -> Event {
    let edit = match content {
        MaybeUndefined::Undefined => Edit::Append(String::new()),
        MaybeUndefined::Null => Edit::Replace(String::new()),
        MaybeUndefined::Value(content) => {
            Edit::Replace(content.into_iter().map(text).collect::<Vec<_>>().join("\n"))
        }
    };
    Event::Message(role, id.to_string(), edit)
}

fn project(update: schema::SessionUpdate) -> Option<Event> {
    use schema::{SessionUpdate as U, StateUpdate as S, StopReason};
    Some(match update {
        U::UserMessage(m) => snapshot(Role::User, m.message_id, m.content),
        U::AgentMessage(m) => snapshot(Role::Assistant, m.message_id, m.content),
        U::UserMessageChunk(m) => Event::Message(
            Role::User,
            m.message_id.to_string(),
            Edit::Append(text(m.content)),
        ),
        U::AgentMessageChunk(m) => Event::Message(
            Role::Assistant,
            m.message_id.to_string(),
            Edit::Append(text(m.content)),
        ),
        U::StateUpdate(S::Idle(state)) => match state.stop_reason {
            Some(StopReason::Error(reason)) => Event::Failed(
                reason
                    .error
                    .map(|e| e.message.to_string())
                    .unwrap_or_else(|| "The provider could not complete the turn".into()),
            ),
            Some(StopReason::Cancelled) => Event::Phase(Phase::Cancelled),
            Some(StopReason::Refusal) => Event::Failed("The model declined the prompt".into()),
            Some(StopReason::MaxTokens | StopReason::MaxTurnRequests) => {
                Event::Failed("The model reached its turn limit".into())
            }
            _ => Event::Phase(Phase::Ready),
        },
        U::StateUpdate(_) => Event::Phase(Phase::Working),
        _ => return None,
    })
}
