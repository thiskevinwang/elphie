use std::time::Duration;

use async_trait::async_trait;
use elphie_core::{
    Completion, FinishReason, Message, Provider, ProviderError, ProviderResult, Role,
};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};

pub struct Ollama {
    http: Client,
    endpoint: Url,
    model: String,
}

impl Ollama {
    pub fn new(host: &str, model: String) -> Result<Self, String> {
        let mut endpoint = Url::parse(host).map_err(|_| "Invalid Ollama host URL")?;
        if endpoint.scheme() != "http"
            || !endpoint.has_host()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.path() != "/"
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err("Use an HTTP origin for the local Ollama server".into());
        }
        if model.trim().is_empty() {
            return Err("Choose an installed Ollama model with --model".into());
        }
        endpoint.set_path("/api/chat");
        let http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            http,
            endpoint,
            model,
        })
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    stream: bool,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    message: Reply,
    done: bool,
    done_reason: Option<String>,
}

#[derive(Deserialize)]
struct Reply {
    role: String,
    content: String,
}

#[async_trait]
impl Provider for Ollama {
    async fn complete(&self, messages: &[Message]) -> ProviderResult {
        let messages = messages
            .iter()
            .map(|message| ChatMessage {
                role: match message.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                },
                content: &message.text,
            })
            .collect();
        let body = ChatRequest {
            model: &self.model,
            messages,
            stream: false,
        };
        let response = self
            .http
            .post(self.endpoint.clone())
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                ProviderError::Failed(format!("Cannot reach Ollama at {}: {error}", self.endpoint))
            })?;
        if !response.status().is_success() {
            return Err(ProviderError::Failed(format!(
                "Ollama returned HTTP {} for model {}",
                response.status(),
                self.model
            )));
        }
        let reply: ChatResponse = response
            .json()
            .await
            .map_err(|error| ProviderError::Failed(format!("Invalid Ollama response: {error}")))?;
        let reason = match reply.done_reason.as_deref() {
            Some("length") => FinishReason::MaxTokens,
            Some("stop") | None => FinishReason::EndTurn,
            Some(_) => return Err(ProviderError::Failed("Ollama stopped unexpectedly".into())),
        };
        if !reply.done
            || reply.message.role != "assistant"
            || (reason == FinishReason::EndTurn && reply.message.content.is_empty())
        {
            return Err(ProviderError::Failed(
                "Ollama returned an incomplete assistant message".into(),
            ));
        }
        Ok(Completion {
            text: reply.message.content,
            reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::post};
    use serde_json::json;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn keeps_ollama_text_when_the_model_reaches_its_token_limit() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route("/api/chat", post(async || Json(json!({
            "done": true, "done_reason": "length", "message": {"role": "assistant", "content": "Partial"}
        }))));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let reply = Ollama::new(&url, "fixture".into())
            .unwrap()
            .complete(&[])
            .await
            .unwrap();
        assert_eq!(
            reply,
            Completion {
                text: "Partial".into(),
                reason: FinishReason::MaxTokens
            }
        );
    }
}
