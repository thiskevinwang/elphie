use agent_client_protocol::schema::{MaybeUndefined, ProtocolVersion};
use agent_client_protocol::v2::{ConnectionTo, Responder, schema};
use agent_client_protocol::{AcpAgent, Agent, Client, Error};
use std::collections::BTreeMap;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

use crate::view_model::{Action, Edit, Event, ModelChoice, Phase};
use elphie_core::{ModelOption, Role};

pub async fn run(
    agent: AcpAgent,
    mut actions: UnboundedReceiver<Action>,
    events: UnboundedSender<Event>,
) -> anyhow::Result<()> {
    let updates = events.clone();
    let forms = events.clone();
    Client
        .v2()
        .name("Elphie terminal")
        .on_receive_notification(
            async move |notification: schema::UpdateSessionNotification, _: ConnectionTo<Agent>| {
                if let schema::SessionUpdate::AgentMessage(message) = &notification.update
                    && let MaybeUndefined::Value(meta) = &message.meta
                    && let Some(id) = meta
                        .get(elphie_acp::MODEL_META_KEY)
                        .and_then(|v| v.as_str())
                {
                    let _ = updates.send(Event::ModelChanged(id.to_owned()));
                }
                if let Some(event) = project(notification.update) {
                    let _ = updates.send(event);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: schema::CreateElicitationRequest,
                        responder: Responder<schema::CreateElicitationResponse>,
                        connection: ConnectionTo<Agent>| {
                connection.spawn(choose_model(request, responder, forms.clone()))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, async move |connection| {
            let init = connection
                .send_request(
                    schema::InitializeRequest::new(
                        ProtocolVersion::V2,
                        elphie_acp::implementation(
                            "elphie-terminal",
                            "Elphie Terminal",
                            env!("CARGO_PKG_VERSION"),
                        ),
                    )
                    .capabilities(
                        schema::ClientCapabilities::new().elicitation(
                            schema::ElicitationCapabilities::new()
                                .form(schema::ElicitationFormCapabilities::new()),
                        ),
                    ),
                )
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

async fn choose_model(
    request: schema::CreateElicitationRequest,
    responder: Responder<schema::CreateElicitationResponse>,
    events: UnboundedSender<Event>,
) -> agent_client_protocol::Result<()> {
    use schema::{CreateElicitationResponse, ElicitationAcceptAction, ElicitationAction};
    let schema::ElicitationMode::Form(form) = request.mode else {
        return responder.respond_with_error(Error::invalid_params());
    };
    let (current, models) = match model_options(form.requested_schema) {
        Ok(options) => options,
        Err(error) => return responder.respond_with_error(error),
    };
    let request_id = responder.id().clone();
    let (reply, selection) = oneshot::channel();
    let _ = events.send(Event::ChooseModel {
        request_id: request_id.clone(),
        message: request.message,
        current,
        models,
        reply,
    });
    let cancellation = responder.cancellation();
    let selected = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            let _ = events.send(Event::DismissModel(request_id));
            ModelChoice::Cancel
        }
        result = selection => result.unwrap_or(ModelChoice::Cancel),
    };
    let action = match selected {
        ModelChoice::Select(id) => ElicitationAction::Accept(
            ElicitationAcceptAction::new().content(BTreeMap::from([("model".into(), id.into())])),
        ),
        ModelChoice::Decline => ElicitationAction::Decline,
        ModelChoice::Cancel => ElicitationAction::Cancel,
    };
    responder.respond(CreateElicitationResponse::new(action))
}

fn model_options(
    mut form: schema::ElicitationSchema,
) -> agent_client_protocol::Result<(Option<String>, Vec<ModelOption>)> {
    let unsupported =
        || Error::invalid_params().data("The terminal supports a single model selection.");
    if form.properties.len() != 1 || form.required.iter().flatten().any(|name| name != "model") {
        return Err(unsupported());
    }
    let Some(schema::ElicitationPropertySchema::String(field)) = form.properties.remove("model")
    else {
        return Err(unsupported());
    };
    let models = match (field.one_of, field.enum_values) {
        (Some(choices), None) if !choices.is_empty() => choices
            .into_iter()
            .map(|choice| ModelOption {
                id: choice.value,
                name: choice.title,
            })
            .collect(),
        (None, Some(choices)) if !choices.is_empty() => choices
            .into_iter()
            .map(|id| ModelOption {
                name: id.clone(),
                id,
            })
            .collect(),
        _ => return Err(unsupported()),
    };
    Ok((field.default, models))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_form_accepts_plain_enum_and_rejects_conflicting_or_unsupported_fields() {
        let plain = schema::StringPropertySchema::new().enum_values(vec!["large".to_owned()]);
        let form = schema::ElicitationSchema::new().property("model", plain.clone(), true);
        assert_eq!(model_options(form.clone()).unwrap().1[0].name, "large");
        let conflicting = plain.one_of(vec![schema::EnumOption::new("large", "Large")]);
        let bad_enum = schema::ElicitationSchema::new().property("model", conflicting, true);
        let mut missing_field = form;
        missing_field.required = Some(vec!["other".into()]);
        for schema in [bad_enum, missing_field] {
            assert_eq!(
                model_options(schema).unwrap_err().code,
                Error::invalid_params().code
            );
        }
    }
}
