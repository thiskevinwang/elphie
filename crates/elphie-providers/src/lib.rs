mod chatgpt;
mod ollama;

pub use chatgpt::ChatGpt;
pub use ollama::Ollama;

use elphie_core::{Message, ModelInput, ModelTurn, ProviderError, ProviderResult, Role, ToolCall};
use serde_json::{Value, json};
use std::collections::HashSet;

fn text_history(messages: &[Message]) -> Vec<ModelInput> {
    messages.iter().cloned().map(ModelInput::Message).collect()
}

fn text_reply(turn: ModelTurn) -> ProviderResult {
    match turn {
        ModelTurn::Reply(completion) => Ok(completion),
        ModelTurn::Tools(_) => Err(ProviderError::Failed("Tools are disabled.".into())),
    }
}

fn text_message(message: &Message) -> Value {
    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    json!({ "role": role, "content": message.text })
}

fn validate_calls(calls: &[ToolCall]) -> Result<(), ProviderError> {
    let mut ids = HashSet::new();
    for call in calls {
        if call.id.trim().is_empty()
            || call.name.trim().is_empty()
            || !call.arguments.is_object()
            || !ids.insert(&call.id)
        {
            return Err(ProviderError::Failed(
                "Invalid function call from the model.".into(),
            ));
        }
    }
    Ok(())
}
