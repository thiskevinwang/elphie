use std::{collections::HashSet, time::Duration};

use async_trait::async_trait;
use elphie_core::{
    Completion, FinishReason, Message, ModelOption, Provider, ProviderError, ProviderResult, Role,
};
use futures_util::{StreamExt, TryStreamExt, stream};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub struct Ollama {
    http: Client,
    host: Url,
    default_model: String,
}

impl Ollama {
    pub fn new(host: &str, model: String) -> Result<Self, String> {
        let endpoint = Url::parse(host).map_err(|_| "Invalid Ollama host URL")?;
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
        let http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            http,
            host: endpoint,
            default_model: model,
        })
    }

    fn endpoint(&self, path: &str) -> Url {
        let mut url = self.host.clone();
        url.set_path(path);
        url
    }

    async fn chat_model(&self, id: String) -> Result<Option<ModelOption>, ProviderError> {
        #[derive(Deserialize)]
        struct Details {
            capabilities: Option<Vec<String>>,
        }
        let request = self
            .http
            .post(self.endpoint("/api/show"))
            .json(&json!({ "model": &id }));
        let fetch_error =
            |e| ProviderError::Failed(format!("Cannot inspect Ollama model {id}: {e}"));
        let response = request
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(fetch_error)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let details: Details = response
            .error_for_status()
            .map_err(fetch_error)?
            .json()
            .await
            .map_err(fetch_error)?;
        if details
            .capabilities
            .is_some_and(|caps| !caps.iter().any(|c| c == "completion"))
        {
            return Ok(None);
        }
        // Ollama lists untagged model names with their implicit :latest tag.
        let selected_id = match id.strip_suffix(":latest") {
            Some(alias) if alias == self.default_model => alias.to_owned(),
            _ => id.clone(),
        };
        Ok(Some(ModelOption {
            name: id,
            id: selected_id,
        }))
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
    fn default_model(&self) -> &str {
        &self.default_model
    }

    async fn models(&self) -> Result<Vec<ModelOption>, ProviderError> {
        #[derive(Deserialize)]
        struct Tags {
            models: Vec<Tag>,
        }
        #[derive(Deserialize)]
        struct Tag {
            name: String,
        }
        let fetch_error = |e| ProviderError::Failed(format!("Cannot list Ollama models: {e}"));
        let tags: Tags = self
            .http
            .get(self.endpoint("/api/tags"))
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(fetch_error)?
            .error_for_status()
            .map_err(fetch_error)?
            .json()
            .await
            .map_err(fetch_error)?;
        let mut seen = HashSet::new();
        let names = tags
            .models
            .into_iter()
            .map(|tag| tag.name)
            .filter(|name| !name.trim().is_empty() && seen.insert(name.clone()));
        stream::iter(names)
            .map(|id| self.chat_model(id))
            .buffered(4)
            .try_filter_map(|item| std::future::ready(Ok(item)))
            .try_collect()
            .await
    }

    async fn complete(&self, model: &str, messages: &[Message]) -> ProviderResult {
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
            model,
            messages,
            stream: false,
        };
        let response = self
            .http
            .post(self.endpoint("/api/chat"))
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                ProviderError::Failed(format!("Cannot reach Ollama at {}: {error}", self.host))
            })?;
        if !response.status().is_success() {
            return Err(ProviderError::Failed(format!(
                "Ollama returned HTTP {} for model {}",
                response.status(),
                model
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
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use serde_json::json;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn keeps_ollama_text_when_the_model_reaches_its_token_limit() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/api/chat",
            post(async |Json(body): Json<serde_json::Value>| {
                assert_eq!(body["model"], "selected");
                Json(json!({
                    "done": true,
                    "done_reason": "length",
                    "message": {"role": "assistant", "content": "Partial"}
                }))
            }),
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let reply = Ollama::new(&url, "fixture".into())
            .unwrap()
            .complete("selected", &[])
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

    #[tokio::test]
    async fn lists_installed_completion_models_and_legacy_tags() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route(
                "/api/tags",
                get(async || {
                    Json(json!({
                        "models": [
                            {"name": "chat:1"},
                            {"name": "embed:1"},
                            {"name": "legacy:latest"},
                            {"name": "gone"}
                        ]
                    }))
                }),
            )
            .route(
                "/api/show",
                post(async |Json(body): Json<serde_json::Value>| {
                    let caps = match body["model"].as_str().unwrap() {
                        "chat:1" => json!({"capabilities":["completion", "tools"]}),
                        "embed:1" => json!({"capabilities":["embedding"]}),
                        "legacy:latest" => json!({}),
                        "gone" => return (StatusCode::NOT_FOUND, Json(json!({}))),
                        _ => panic!("Unexpected tag"),
                    };
                    (StatusCode::OK, Json(caps))
                }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let provider = Ollama::new(&url, "legacy".into()).unwrap();
        let models = provider.models().await.unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "chat:1");
        assert_eq!(models[1].name, "legacy:latest");
        assert_eq!(models[1].id, provider.default_model());
    }
}
