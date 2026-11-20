use async_trait::async_trait;
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message {
    pub role: Role,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FinishReason {
    EndTurn,
    MaxTokens,
    Refusal,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Completion {
    pub text: String,
    pub reason: FinishReason,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRequest {
    pub calls: Vec<ToolCall>,
    /// Provider-specific response data for the next inference request.
    pub continuation: Value,
}

impl ToolRequest {
    pub fn new(calls: Vec<ToolCall>) -> Self {
        Self {
            calls,
            continuation: Value::Null,
        }
    }

    pub fn results<'a>(
        &'a self,
        outputs: &'a [String],
    ) -> Result<impl Iterator<Item = (&'a ToolCall, &'a String)>, ProviderError> {
        if self.calls.len() != outputs.len() {
            return Err(ProviderError::Failed(
                "Each tool call needs one result.".into(),
            ));
        }
        Ok(self.calls.iter().zip(outputs))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelInput {
    Message(Message),
    Tools {
        request: ToolRequest,
        /// One result for each call, in the same order.
        outputs: Vec<String>,
    },
}

#[derive(Debug, Eq, PartialEq)]
pub enum ModelTurn {
    Reply(Completion),
    Tools(ToolRequest),
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("{0}")]
    AuthenticationRequired(String),
    #[error("{0}")]
    Failed(String),
}

pub type ProviderResult = Result<Completion, ProviderError>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelOption {
    pub id: String,
    pub name: String,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn default_model(&self) -> &str;
    async fn models(&self) -> Result<Vec<ModelOption>, ProviderError>;
    async fn complete(&self, model: &str, messages: &[Message]) -> ProviderResult;
    async fn turn(
        &self,
        _model: &str,
        _history: &[ModelInput],
        _tools: &[ToolDefinition],
    ) -> Result<ModelTurn, ProviderError> {
        Err(ProviderError::Failed(
            "This provider does not support tools.".into(),
        ))
    }
}
