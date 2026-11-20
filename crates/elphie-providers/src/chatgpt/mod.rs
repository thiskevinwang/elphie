use std::time::Duration;

use async_trait::async_trait;
use elphie_core::{
    Completion, FinishReason, Message, Provider, ProviderError, ProviderResult, Role,
};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::{Client, StatusCode, redirect::Policy};
use serde::Deserialize;
use serde_json::json;

const ENDPOINT: &str = "https://api.openai.com/v1/responses";
const LOGIN: &str = "Sign in again with elphie login.";

pub struct ChatGpt {
    client: Client,
    model: String,
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
        Ok(Self { client, model })
    }

    async fn request(&self, messages: &[Message], token: &str, url: &str) -> ProviderResult {
        let input: Vec<_> = messages
            .iter()
            .map(|message| {
                let role = match message.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };
                json!({ "role": role, "content": message.text })
            })
            .collect();
        let response = self
            .client
            .post(url)
            .bearer_auth(token)
            .header("accept", "text/event-stream")
            .json(&json!({
                "model": self.model, "store": false, "stream": true,
                "input": input
            }))
            .send()
            .await
            .map_err(|e| ProviderError::Failed(format!("ChatGPT request failed. {e}")))?;
        let status = response.status();
        let request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|id| id.to_str().ok())
            .unwrap_or("")
            .to_owned();
        if !status.is_success() {
            let body = response.json::<Failure>().await.unwrap_or_default();
            let message = failure_message(Some(status), &body, &request_id);
            return Err(if status == StatusCode::UNAUTHORIZED {
                ProviderError::AuthenticationRequired(message)
            } else {
                ProviderError::Failed(message)
            });
        }
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
                ReplyEvent::ItemDone { item } => items.push(item),
                ReplyEvent::Completed { response } => {
                    return response.completion(items, FinishReason::EndTurn);
                }
                ReplyEvent::Incomplete { response } => {
                    if response
                        .incomplete_details
                        .as_ref()
                        .is_some_and(|detail| detail.reason == "max_output_tokens")
                    {
                        return response.completion(items, FinishReason::MaxTokens);
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
    async fn complete(&self, messages: &[Message]) -> ProviderResult {
        let token = elphie_signin::access_token().await.map_err(|error| {
            if error.is::<elphie_signin::AuthenticationRequired>() {
                ProviderError::AuthenticationRequired(error.to_string())
            } else {
                ProviderError::Failed(error.to_string())
            }
        })?;
        self.request(messages, &token, ENDPOINT).await
    }
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ReplyEvent {
    #[serde(rename = "response.output_item.done")]
    ItemDone { item: OutputItem },
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
        _ => "ChatGPT could not complete the reply.",
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
    output: Vec<OutputItem>,
    incomplete_details: Option<IncompleteDetails>,
}

#[derive(Deserialize)]
struct IncompleteDetails {
    reason: String,
}

impl Response {
    fn completion(self, streamed: Vec<OutputItem>, mut reason: FinishReason) -> ProviderResult {
        let items = if self.output.is_empty() {
            streamed
        } else {
            self.output
        };
        let mut messages = Vec::new();
        for item in items {
            let OutputItem::Message {
                role,
                phase,
                content,
            } = item
            else {
                continue;
            };
            if role != "assistant" || phase.as_deref() == Some("commentary") {
                continue;
            }
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
            if !message.is_empty() {
                messages.push(message);
            }
        }
        if reason == FinishReason::EndTurn && messages.is_empty() {
            return Err(ProviderError::Failed(
                "ChatGPT completed without a text reply.".into(),
            ));
        }
        Ok(Completion {
            text: messages.join("\n"),
            reason,
        })
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
