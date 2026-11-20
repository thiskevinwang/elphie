use std::{collections::HashSet, time::Duration};

use crate::{text_history, text_message, text_reply, validate_calls};
use async_trait::async_trait;
use elphie_core::{
    Completion, FinishReason, Message, ModelInput, ModelOption, ModelTurn, Provider, ProviderError,
    ProviderResult, ToolCall, ToolDefinition, ToolRequest,
};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::{Client, Response as HttpResponse, StatusCode, redirect::Policy};
use serde::Deserialize;
use serde_json::{Value, json};

const ENDPOINT: &str = "https://api.openai.com/v1/responses";
const MODELS_ENDPOINT: &str = "https://api.openai.com/v1/models";
const LOGIN: &str = "Sign in again with elphie login.";
const TOOL_NAMESPACE: &str = "workspace";

pub struct ChatGpt {
    client: Client,
    default_model: String,
}

impl ChatGpt {
    pub fn new(model: impl Into<String>) -> Result<Self, String> {
        let model = model.into();
        if model.trim().is_empty() {
            return Err("Select a ChatGPT model.".into());
        }
        let client = Client::builder()
            .user_agent(concat!("Elphie/", env!("CARGO_PKG_VERSION")))
            .redirect(Policy::none())
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            client,
            default_model: model,
        })
    }

    async fn catalog(&self, token: &str, url: &str) -> Result<Vec<ModelOption>, ProviderError> {
        #[derive(Deserialize)]
        struct Catalog {
            models: Vec<Entry>,
        }
        #[derive(Deserialize)]
        struct Entry {
            visibility: Option<String>,
            slug: Option<String>,
            display_name: Option<String>,
        }
        let response = self
            .client
            .get(url)
            .bearer_auth(token)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| ProviderError::Failed(format!("Cannot list ChatGPT models: {e}")))?;
        let (response, _) = checked_response(response).await?;
        let catalog: Catalog = response
            .json()
            .await
            .map_err(|e| ProviderError::Failed(format!("Invalid ChatGPT model list: {e}")))?;
        let mut seen = HashSet::new();
        Ok(catalog
            .models
            .into_iter()
            .filter_map(|entry| {
                let id = entry.slug?;
                if entry.visibility.as_deref() != Some("list")
                    || id.trim().is_empty()
                    || !seen.insert(id.clone())
                {
                    return None;
                }
                let name = entry
                    .display_name
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| id.clone());
                Some(ModelOption { id, name })
            })
            .collect())
    }

    async fn request(
        &self,
        model: &str,
        messages: &[Message],
        token: &str,
        url: &str,
    ) -> ProviderResult {
        text_reply(
            self.request_turn(model, &text_history(messages), &[], token, url)
                .await?,
        )
    }

    async fn request_turn(
        &self,
        model: &str,
        history: &[ModelInput],
        tools: &[ToolDefinition],
        token: &str,
        url: &str,
    ) -> Result<ModelTurn, ProviderError> {
        let mut input = Vec::new();
        for item in history {
            match item {
                ModelInput::Message(message) => input.push(text_message(message)),
                ModelInput::Tools { request, outputs } => {
                    let items = request.continuation.as_array().ok_or_else(|| {
                        ProviderError::Failed("Missing ChatGPT tool call history.".into())
                    })?;
                    input.extend(items.iter().cloned());
                    for (call, output) in request.results(outputs)? {
                        input.push(json!({
                            "type": "function_call_output", "call_id": call.id, "output": output
                        }));
                    }
                }
            }
        }
        let mut body = json!({"model": model, "store": false, "stream": true, "input": input});
        if !tools.is_empty() {
            let functions: Vec<_> = tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function", "name": tool.name,
                        "description": tool.description, "parameters": tool.parameters,
                    })
                })
                .collect();
            body["tools"] = json!([{
                "type": "namespace", "name": TOOL_NAMESPACE,
                "description": "Use the tools available in this session.", "tools": functions
            }]);
        }
        let response = self
            .client
            .post(url)
            .bearer_auth(token)
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Failed(format!("ChatGPT request failed. {e}")))?;
        let (response, request_id) = checked_response(response).await?;
        let mut events = response.bytes_stream().eventsource();
        let mut items = Vec::new();
        while let Some(event) = events.next().await {
            let data = event
                .map_err(|_| ProviderError::Failed("ChatGPT response stream failed.".into()))?
                .data;
            if data == "[DONE]" {
                break;
            }
            let event: ReplyEvent = serde_json::from_str(&data)
                .map_err(|_| ProviderError::Failed("Invalid ChatGPT response event.".into()))?;
            match event {
                ReplyEvent::ItemDone { item, output_index } => {
                    items.push((output_index.unwrap_or(items.len()), item));
                }
                ReplyEvent::Completed { response } => {
                    return response.turn(items, FinishReason::EndTurn, !tools.is_empty());
                }
                ReplyEvent::Incomplete { response } => {
                    if response
                        .incomplete_details
                        .as_ref()
                        .is_some_and(|detail| detail.reason == "max_output_tokens")
                    {
                        return response.turn(items, FinishReason::MaxTokens, !tools.is_empty());
                    }
                    return Err(ProviderError::Failed(
                        "ChatGPT returned an incomplete reply.".into(),
                    ));
                }
                ReplyEvent::Failed { response } => {
                    return Err(ProviderError::Failed(failure_message(
                        None,
                        &response,
                        &request_id,
                    )));
                }
                ReplyEvent::Error { code, param } => {
                    return Err(ProviderError::Failed(failure_message(
                        None,
                        &Failure {
                            error: Some(ErrorDetail { code, param }),
                        },
                        &request_id,
                    )));
                }
                ReplyEvent::Other => {}
            }
        }
        Err(ProviderError::Failed(
            "ChatGPT disconnected before completing the reply.".into(),
        ))
    }
}

#[async_trait]
impl Provider for ChatGpt {
    fn default_model(&self) -> &str {
        &self.default_model
    }

    async fn models(&self) -> Result<Vec<ModelOption>, ProviderError> {
        self.catalog(&access_token().await?, MODELS_ENDPOINT).await
    }

    async fn complete(&self, model: &str, messages: &[Message]) -> ProviderResult {
        self.request(model, messages, &access_token().await?, ENDPOINT)
            .await
    }

    async fn turn(
        &self,
        model: &str,
        history: &[ModelInput],
        tools: &[ToolDefinition],
    ) -> Result<ModelTurn, ProviderError> {
        self.request_turn(model, history, tools, &access_token().await?, ENDPOINT)
            .await
    }
}

async fn access_token() -> Result<String, ProviderError> {
    elphie_signin::access_token().await.map_err(|error| {
        if error.is::<elphie_signin::AuthenticationRequired>() {
            ProviderError::AuthenticationRequired(error.to_string())
        } else {
            ProviderError::Failed(error.to_string())
        }
    })
}

async fn checked_response(response: HttpResponse) -> Result<(HttpResponse, String), ProviderError> {
    let status = response.status();
    let request_id = response
        .headers()
        .get("x-request-id")
        .and_then(|id| id.to_str().ok())
        .unwrap_or("")
        .to_owned();
    if status.is_success() {
        return Ok((response, request_id));
    }
    let body = response.json::<Failure>().await.unwrap_or_default();
    let message = failure_message(Some(status), &body, &request_id);
    Err(if status == StatusCode::UNAUTHORIZED {
        ProviderError::AuthenticationRequired(message)
    } else {
        ProviderError::Failed(message)
    })
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ReplyEvent {
    #[serde(rename = "response.output_item.done")]
    ItemDone {
        item: Value,
        output_index: Option<usize>,
    },
    #[serde(rename = "response.completed")]
    Completed { response: Response },
    #[serde(rename = "response.failed")]
    Failed { response: Failure },
    #[serde(rename = "response.incomplete")]
    Incomplete { response: Response },
    #[serde(rename = "error")]
    Error {
        code: Option<String>,
        param: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Default, Deserialize)]
struct Failure {
    error: Option<ErrorDetail>,
}

#[derive(Deserialize)]
struct ErrorDetail {
    code: Option<String>,
    param: Option<String>,
}

fn failure_message(status: Option<StatusCode>, failure: &Failure, request_id: &str) -> String {
    let error = failure.error.as_ref();
    let code = error.and_then(|e| e.code.as_deref());
    let reason = match (code, status) {
        (Some("subscription_sharing_usage_limit_exceeded"), _) => {
            "ChatGPT usage limit reached. Check https://chatgpt.com/settings/usage."
        }
        (Some("subscription_sharing_user_not_eligible"), _) => {
            "ChatGPT plan usage is unavailable for the selected account or workspace."
        }
        (Some("subscription_sharing_unsupported_capability"), _) => {
            "The ChatGPT plan does not support this request. Check the model selection."
        }
        (
            Some(
                "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable",
            ),
            _,
        )
        | (_, Some(StatusCode::SERVICE_UNAVAILABLE)) => {
            "ChatGPT is temporarily unavailable. Try again later."
        }
        (_, Some(StatusCode::UNAUTHORIZED)) => LOGIN,
        (_, Some(StatusCode::FORBIDDEN)) => "ChatGPT denied access to this model or account.",
        (_, Some(StatusCode::TOO_MANY_REQUESTS)) => {
            "ChatGPT request limit reached. Try again later."
        }
        _ => "ChatGPT could not complete the request.",
    };
    let mut details = Vec::new();
    if let Some(status) = status {
        details.push(format!("HTTP {}", status.as_u16()));
    }
    for diagnostic in [
        code,
        error.and_then(|e| e.param.as_deref()),
        Some(request_id),
    ]
    .into_iter()
    .flatten()
    {
        if !diagnostic.is_empty()
            && diagnostic.len() <= 128
            && diagnostic
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.[]".contains(&c))
        {
            details.push(diagnostic.to_owned());
        }
    }
    if details.is_empty() {
        reason.into()
    } else {
        format!("{reason} ({})", details.join(", "))
    }
}

#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    output: Vec<Value>,
    incomplete_details: Option<IncompleteDetails>,
}

#[derive(Deserialize)]
struct IncompleteDetails {
    reason: String,
}

impl Response {
    fn turn(
        self,
        mut streamed: Vec<(usize, Value)>,
        mut reason: FinishReason,
        tools_enabled: bool,
    ) -> Result<ModelTurn, ProviderError> {
        let items = if self.output.is_empty() {
            streamed.sort_unstable_by_key(|(index, _)| *index);
            if streamed
                .iter()
                .enumerate()
                .any(|(expected, (index, _))| expected != *index)
            {
                return Err(ProviderError::Failed(
                    "Incomplete ChatGPT output items.".into(),
                ));
            }
            streamed.into_iter().map(|(_, item)| item).collect()
        } else {
            self.output
        };
        let mut messages = Vec::new();
        let mut calls = Vec::new();
        let mut replayable = true;
        for item in &items {
            let output = OutputItem::deserialize(item)
                .map_err(|_| ProviderError::Failed("Invalid ChatGPT response item.".into()))?;
            match output {
                OutputItem::FunctionCall {
                    namespace,
                    name,
                    call_id,
                    arguments,
                    status,
                } => {
                    if !tools_enabled
                        || namespace != TOOL_NAMESPACE
                        || status.as_deref().is_some_and(|s| s != "completed")
                    {
                        return Err(ProviderError::Failed(
                            "Unexpected or incomplete ChatGPT tool call.".into(),
                        ));
                    }
                    calls.push(ToolCall {
                        id: call_id,
                        name,
                        arguments: serde_json::from_str(&arguments).map_err(|_| {
                            ProviderError::Failed("Invalid ChatGPT tool arguments.".into())
                        })?,
                    });
                }
                OutputItem::Message {
                    role,
                    phase,
                    content,
                } if role == "assistant" => {
                    let mut message = String::new();
                    for part in content {
                        match part {
                            Part::OutputText { text } => message.push_str(&text),
                            Part::Refusal { refusal } => {
                                message.push_str(&refusal);
                                reason = FinishReason::Refusal;
                            }
                            Part::Other => {}
                        }
                    }
                    if phase.as_deref() != Some("commentary") && !message.is_empty() {
                        messages.push(message);
                    }
                }
                OutputItem::Reasoning => {}
                _ => replayable = false,
            }
        }
        if !calls.is_empty() {
            if reason != FinishReason::EndTurn || !replayable {
                return Err(ProviderError::Failed(
                    "ChatGPT returned an incomplete or unsupported tool turn.".into(),
                ));
            }
            validate_calls(&calls)?;
            return Ok(ModelTurn::Tools(ToolRequest {
                calls,
                continuation: Value::Array(items),
            }));
        }
        if reason == FinishReason::EndTurn && messages.is_empty() {
            return Err(ProviderError::Failed(
                "ChatGPT completed without a text reply.".into(),
            ));
        }
        Ok(ModelTurn::Reply(Completion {
            text: messages.join("\n"),
            reason,
        }))
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OutputItem {
    Message {
        role: String,
        phase: Option<String>,
        content: Vec<Part>,
    },
    FunctionCall {
        namespace: String,
        call_id: String,
        name: String,
        arguments: String,
        status: Option<String>,
    },
    Reasoning,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Part {
    OutputText {
        text: String,
    },
    Refusal {
        refusal: String,
    },
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests;
