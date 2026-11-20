use std::collections::HashSet;

use agent_client_protocol::{
    Client, Error, Result,
    v2::{ConnectionTo, schema::*},
};
use elphie_core::ModelOption;

pub(super) fn commands(supports_form: bool) -> Vec<AvailableCommand> {
    if supports_form {
        vec![AvailableCommand::Command(Command::new(
            "model",
            "Select a model for this session",
        ))]
    } else {
        vec![]
    }
}

pub(super) fn request(
    session: SessionId,
    current: &str,
    models: &[ModelOption],
) -> Result<CreateElicitationRequest> {
    if models.is_empty() {
        return Err(Error::new(
            ErrorCode::InternalError,
            "The provider has no available models.",
        ));
    }
    let mut ids = HashSet::new();
    let mut options = Vec::with_capacity(models.len());
    for model in models {
        if model.id.trim().is_empty() || !ids.insert(model.id.as_str()) {
            return Err(Error::new(
                ErrorCode::InternalError,
                "The provider returned an invalid model ID.",
            ));
        }
        options.push(EnumOption::new(&model.id, &model.name));
    }
    let field = StringPropertySchema::new()
        .title("Model")
        .one_of(options)
        .default_value(ids.contains(current).then(|| current.to_owned()));
    let form = ElicitationFormMode::new(
        ElicitationSessionScope::new(session),
        ElicitationSchema::new().property("model", field, true),
    );
    Ok(CreateElicitationRequest::new(form, "Select a model"))
}

pub(super) enum Selection {
    Selected(ModelOption),
    Declined,
    Cancelled,
}

pub(super) async fn select(
    client: ConnectionTo<Client>,
    request: CreateElicitationRequest,
    models: Vec<ModelOption>,
) -> Result<Selection> {
    let response = client.send_request(request).block_task().await?;
    match response.action {
        ElicitationAction::Accept(accepted) => {
            let value = accepted
                .content
                .as_ref()
                .and_then(|fields| fields.get("model"));
            if let Some(ElicitationContentValue::String(id)) = value
                && let Some(model) = models.into_iter().find(|model| model.id == *id)
            {
                return Ok(Selection::Selected(model));
            }
            Err(Error::new(
                ErrorCode::InvalidParams,
                "Select a model from the available list.",
            ))
        }
        ElicitationAction::Decline => Ok(Selection::Declined),
        _ => Ok(Selection::Cancelled),
    }
}
