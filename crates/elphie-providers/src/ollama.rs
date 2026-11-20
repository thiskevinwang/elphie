use std::{collections::HashSet, time::Duration};

use crate::{text_history, text_message, text_reply, validate_calls};
use async_trait::async_trait;
use elphie_core::{
    Completion, FinishReason, Message, ModelInput, ModelOption, ModelTurn, Provider, ProviderError,
    ProviderResult, ToolCall, ToolDefinition, ToolRequest,
};
use futures_util::{StreamExt, TryStreamExt, stream};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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

    async fn installed_models(
        &self,
        require_tools: bool,
    ) -> Result<Vec<ModelOption>, ProviderError> {
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
            .map(|id| self.chat_model(id, require_tools))
            .buffered(4)
            .try_filter_map(|item| std::future::ready(Ok(item)))
            .try_collect()
            .await
    }

    async fn chat_model(
        &self,
        id: String,
        require_tools: bool,
    ) -> Result<Option<ModelOption>, ProviderError> {
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
        if details.capabilities.is_some_and(|caps| {
            !caps.iter().any(|c| c == "completion")
                || (require_tools && !caps.iter().any(|c| c == "tools"))
        }) {
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

#[derive(Deserialize)]
struct ChatResponse {
    message: Reply,
    done: bool,
    done_reason: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct Reply {
    role: String,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<OllamaCall>,
}

#[derive(Deserialize, Serialize)]
struct OllamaCall {
    id: Option<String>,
    function: Function,
}

#[derive(Deserialize, Serialize)]
struct Function {
    #[serde(skip_serializing_if = "Option::is_none")]
    index: Option<usize>,
    name: String,
    arguments: Value,
}

impl ChatResponse {
    fn turn(mut self, tools_enabled: bool) -> Result<ModelTurn, ProviderError> {
        let reason = match self.done_reason.as_deref() {
            Some("length") => FinishReason::MaxTokens,
            Some("stop") | None => FinishReason::EndTurn,
            Some(_) => return Err(ProviderError::Failed("Ollama stopped unexpectedly".into())),
        };
        if !self.done || self.message.role != "assistant" {
            return Err(ProviderError::Failed(
                "Ollama returned an incomplete assistant message".into(),
            ));
        }
        if !self.message.tool_calls.is_empty() {
            if !tools_enabled || reason != FinishReason::EndTurn {
                return Err(ProviderError::Failed(
                    "Unexpected or incomplete Ollama tool call.".into(),
                ));
            }
            let calls: Vec<_> = self
                .message
                .tool_calls
                .iter_mut()
                .map(|call| {
                    let id = call
                        .id
                        .get_or_insert_with(|| uuid::Uuid::new_v4().to_string());
                    ToolCall {
                        id: id.clone(),
                        name: call.function.name.clone(),
                        arguments: call.function.arguments.clone(),
                    }
                })
                .collect();
            validate_calls(&calls)?;
            return Ok(ModelTurn::Tools(ToolRequest {
                calls,
                continuation: json!(self.message),
            }));
        }
        if reason == FinishReason::EndTurn && self.message.content.is_empty() {
            return Err(ProviderError::Failed(
                "Ollama returned an empty assistant message".into(),
            ));
        }
        Ok(ModelTurn::Reply(Completion {
            text: self.message.content,
            reason,
        }))
    }
}

#[async_trait]
impl Provider for Ollama {
    fn default_model(&self) -> &str {
        &self.default_model
    }

    async fn models(&self) -> Result<Vec<ModelOption>, ProviderError> {
        self.installed_models(false).await
    }

    async fn models_for_tools(&self) -> Result<Vec<ModelOption>, ProviderError> {
        self.installed_models(true).await
    }

    async fn complete(&self, model: &str, messages: &[Message]) -> ProviderResult {
        text_reply(self.turn(model, &text_history(messages), &[]).await?)
    }

    async fn turn(
        &self,
        model: &str,
        history: &[ModelInput],
        tools: &[ToolDefinition],
    ) -> Result<ModelTurn, ProviderError> {
        let mut messages = Vec::new();
        for item in history {
            match item {
                ModelInput::Message(message) => messages.push(text_message(message)),
                ModelInput::Tools { request, outputs } => {
                    messages.push(request.continuation.clone());
                    for (call, output) in request.results(outputs)? {
                        messages.push(json!({"role":"tool", "tool_name":call.name, "tool_call_id":call.id, "content":output}));
                    }
                }
            }
        }
        let mut body = json!({"model": model, "messages": messages, "stream": false});
        if !tools.is_empty() {
            let definitions: Vec<_> = tools.iter().map(|tool| json!({
                "type": "function", "function": {
                    "name": tool.name, "description": tool.description, "parameters": tool.parameters
                }
            })).collect();
            body["tools"] = Value::Array(definitions);
        }
        let response = self
            .http
            .post(self.endpoint("/api/chat"))
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                ProviderError::Failed(format!("Cannot reach Ollama at {}: {error}", self.host))
            })?;
        let status = response.status();
        if !status.is_success() {
            if status == StatusCode::BAD_REQUEST && !tools.is_empty() {
                #[derive(Deserialize)]
                struct OllamaError {
                    error: String,
                }
                if response
                    .json::<OllamaError>()
                    .await
                    .is_ok_and(|body| body.error.ends_with("does not support tools"))
                {
                    return Err(ProviderError::Failed(
                        "This Ollama model cannot use tools. Choose a model with /model or restart without --tools.".into(),
                    ));
                }
            }
            return Err(ProviderError::Failed(format!(
                "Ollama returned HTTP {} for model {}",
                status, model
            )));
        }
        let reply: ChatResponse = response
            .json()
            .await
            .map_err(|error| ProviderError::Failed(format!("Invalid Ollama response: {error}")))?;
        reply.turn(!tools.is_empty())
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
                assert!(body.get("tools").is_none());
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
                            {"name": "text:1"},
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
                        "text:1" => json!({"capabilities":["completion"]}),
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
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "chat:1");
        assert_eq!(models[1].id, "text:1");
        assert_eq!(models[2].name, "legacy:latest");
        assert_eq!(models[2].id, provider.default_model());
        let tool_models = provider.models_for_tools().await.unwrap();
        assert_eq!(tool_models, [models[0].clone(), models[2].clone()]);
    }

    #[tokio::test]
    async fn points_to_model_selection_when_an_explicit_model_rejects_tools() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/api/chat",
            post(async || {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error":"internal model path does not support tools"})),
                )
            }),
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let provider = Ollama::new(&url, "text-only".into()).unwrap();
        let error = provider
            .turn(provider.default_model(), &[], &[list_tool()])
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("/model"));
        assert!(error.contains("without --tools"));
        assert!(!error.contains("internal model path"));
    }

    fn list_tool() -> ToolDefinition {
        ToolDefinition {
            name: "list_directory".into(),
            description: "List directory entries.".into(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
        }
    }

    #[tokio::test]
    async fn round_trip_preserves_thinking_and_correlates_same_name_calls() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route("/api/chat", post(async |Json(body): Json<Value>| {
            assert_eq!(body["tools"][0], json!({
                "type":"function", "function":{"name":"list_directory","description":"List directory entries.","parameters":list_tool().parameters}
            }));
            let messages = body["messages"].as_array().unwrap();
            let message = if messages.len() == 1 {
                json!({
                    "role":"assistant","content":"Checking.","thinking":"fixture",
                    "tool_calls":[
                        {"function":{"index":0,"name":"list_directory","arguments":{"path":"."}}},
                        {"id":"call-two","function":{"index":1,"name":"list_directory","arguments":{"path":"src"}}}
                    ]
                })
            } else {
                assert_eq!(messages.len(), 4);
                let reply = &messages[1];
                assert_eq!(reply["thinking"], "fixture");
                assert_eq!(reply["content"], "Checking.");
                let generated = reply["tool_calls"][0]["id"].as_str().unwrap();
                assert!(!generated.is_empty());
                assert_ne!(generated, "call-two");
                assert_eq!(messages[2], json!({"role":"tool","tool_name":"list_directory","tool_call_id":generated,"content":"Cargo.toml"}));
                assert_eq!(messages[3], json!({"role":"tool","tool_name":"list_directory","tool_call_id":"call-two","content":"main.rs"}));
                json!({"role":"assistant","content":"Found both files."})
            };
            Json(json!({"done":true,"done_reason":"stop","message":message}))
        }));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let provider = Ollama::new(&url, "fixture".into()).unwrap();
        let tools = [list_tool()];
        let mut history = vec![ModelInput::Message(Message {
            role: elphie_core::Role::User,
            text: "List the files".into(),
        })];
        let ModelTurn::Tools(request) = provider.turn("fixture", &history, &tools).await.unwrap()
        else {
            panic!("expected calls")
        };
        assert_eq!(request.calls[0].arguments, json!({"path":"."}));
        assert_eq!(request.calls[1].id, "call-two");
        history.push(ModelInput::Tools {
            request,
            outputs: vec!["Cargo.toml".into(), "main.rs".into()],
        });
        assert_eq!(
            provider.turn("fixture", &history, &tools).await.unwrap(),
            ModelTurn::Reply(Completion {
                text: "Found both files.".into(),
                reason: FinishReason::EndTurn
            })
        );
    }

    #[test]
    fn rejects_incomplete_malformed_and_unsolicited_calls() {
        let valid = json!({"id":"call","function":{"name":"list_directory","arguments":{}}});
        let invalid = json!({"function":{"name":"list_directory","arguments":"/tmp"}});
        for (calls, enabled, done, reason) in [
            (vec![valid.clone()], true, false, "stop"),
            (vec![valid.clone()], false, true, "stop"),
            (vec![valid.clone()], true, true, "length"),
            (vec![valid.clone(), valid], true, true, "stop"),
            (vec![invalid], true, true, "stop"),
        ] {
            let reply: ChatResponse = serde_json::from_value(json!({
                "message":{"role":"assistant","content":"Partial","tool_calls":calls},
                "done":done, "done_reason":reason
            }))
            .unwrap();
            assert!(reply.turn(enabled).is_err());
        }
    }
}
