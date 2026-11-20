use async_trait::async_trait;

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
}
