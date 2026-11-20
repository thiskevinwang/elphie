use super::*;
use agent_client_protocol::ByteStreams;
use async_trait::async_trait;
use elphie_core::{Completion, FinishReason, Message, ModelOption, ProviderError, ProviderResult};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf},
    time::{Duration, timeout},
};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

type Catalog = fn() -> Result<Vec<ModelOption>, ProviderError>;

struct TestProvider {
    catalog: Catalog,
}

#[async_trait]
impl Provider for TestProvider {
    fn default_model(&self) -> &str {
        "first"
    }

    async fn models(&self) -> Result<Vec<ModelOption>, ProviderError> {
        (self.catalog)()
    }

    async fn complete(&self, model: &str, messages: &[Message]) -> ProviderResult {
        match messages.last().unwrap().text.as_str() {
            "model?" => Ok(Completion {
                text: model.into(),
                reason: FinishReason::EndTurn,
            }),
            "/model" => panic!("The server must handle the model command"),
            "wait" => std::future::pending().await,
            "fail" => Err(ProviderError::Failed("Model unavailable".into())),
            "auth" => Err(ProviderError::AuthenticationRequired(
                "Sign in first".into(),
            )),
            "limit" => Ok(Completion {
                text: "Partial".into(),
                reason: FinishReason::MaxTokens,
            }),
            "refuse" => Ok(Completion {
                text: "Cannot answer".into(),
                reason: FinishReason::Refusal,
            }),
            _ => Ok(Completion {
                text: messages
                    .iter()
                    .map(|m| m.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" / "),
                reason: FinishReason::EndTurn,
            }),
        }
    }
}

struct ClientIo {
    output: Lines<BufReader<ReadHalf<DuplexStream>>>,
    input: WriteHalf<DuplexStream>,
}

impl ClientIo {
    fn start() -> Self {
        Self::with_catalog(|| {
            Ok(vec![
                ModelOption {
                    id: "first".into(),
                    name: "First".into(),
                },
                ModelOption {
                    id: "second".into(),
                    name: "Second".into(),
                },
            ])
        })
    }

    fn with_catalog(catalog: Catalog) -> Self {
        Self::with_provider(Arc::new(TestProvider { catalog }), None)
    }

    fn with_provider(provider: Arc<dyn Provider>, workspace: Option<Arc<Workspace>>) -> Self {
        let (client, server) = tokio::io::duplex(16384);
        let (reader, writer) = tokio::io::split(server);
        tokio::spawn(connect(
            provider,
            ByteStreams::new(writer.compat_write(), reader.compat()),
            workspace,
        ));
        let (output, input) = tokio::io::split(client);
        Self {
            output: BufReader::new(output).lines(),
            input,
        }
    }

    async fn send(&mut self, id: Option<u64>, method: &str, params: Value) {
        let mut frame = json!({"jsonrpc":"2.0", "method":method, "params":params});
        if let Some(id) = id {
            frame["id"] = id.into();
        }
        self.input
            .write_all(format!("{frame}\n").as_bytes())
            .await
            .unwrap();
    }

    async fn read(&mut self) -> Value {
        let line = timeout(Duration::from_secs(5), self.output.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    async fn session(&mut self) -> Value {
        self.session_with_form(false).await
    }

    async fn session_with_form(&mut self, form: bool) -> Value {
        let capabilities = if form {
            json!({"elicitation":{"form":{}}})
        } else {
            json!({})
        };
        self.send(
            Some(1),
            "initialize",
            json!({"protocolVersion":2,"capabilities":capabilities,"info":{"name":"test","version":"1"}}),
        )
        .await;
        let init = self.read().await;
        assert_eq!(init["result"]["protocolVersion"], 2);
        assert_eq!(init["result"]["capabilities"]["session"], json!({}));
        self.send(Some(2), "session/new", json!({"cwd":"/tmp"}))
            .await;
        let reply = self.read().await;
        if form {
            assert_eq!(reply["result"]["availableCommands"][0]["name"], "model");
        } else {
            assert!(reply["result"].get("availableCommands").is_none());
        }
        assert!(reply["result"]["sessionId"].is_string());
        reply["result"]["sessionId"].clone()
    }

    async fn prompt(&mut self, request: u64, session: &Value, key: &str, text: &str) {
        self.send(Some(request), "session/prompt", json!({"sessionId":session,"idempotencyKey":key,"prompt":[{"type":"text","text":text}]})).await;
    }

    async fn picker(&mut self, session: &Value, key: &str) -> (Value, Value) {
        self.prompt(5, session, key, "/model").await;
        let message_id = self.read().await["result"]["messageId"].clone();
        assert!(message_id.is_string());
        assert_eq!(
            self.update("user_message").await["content"][0]["text"],
            "/model"
        );
        assert_eq!(self.update("state_update").await["state"], "running");
        assert_eq!(
            self.update("state_update").await["state"],
            "requires_action"
        );
        let picker = self.read().await;
        assert_eq!(picker["method"], "elicitation/create");
        assert_eq!(picker["params"]["sessionId"], *session);
        assert_eq!(picker["params"]["mode"], "form");
        (message_id, picker)
    }

    async fn answer(&mut self, picker: &Value, result: Value) {
        let frame = json!({"jsonrpc":"2.0","id":picker["id"],"result":result});
        self.input
            .write_all(format!("{frame}\n").as_bytes())
            .await
            .unwrap();
    }

    async fn completion(&mut self, session: &Value, key: &str, prompt: &str) -> String {
        self.prompt(10, session, key, prompt).await;
        assert!(self.read().await["result"]["messageId"].is_string());
        self.update("user_message").await;
        assert_eq!(self.update("state_update").await["state"], "running");
        let text = self.update("agent_message").await["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(self.update("state_update").await["stopReason"], "end_turn");
        text
    }

    async fn update(&mut self, expected: &str) -> Value {
        let frame = self.read().await;
        assert_eq!(frame["method"], "session/update", "{frame}");
        let update = &frame["params"]["update"];
        assert_eq!(update["sessionUpdate"], expected);
        update.clone()
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod tools;

#[tokio::test]
async fn version_negotiation_and_baseline_sessions() {
    let mut client = ClientIo::start();
    client
        .send(Some(0), "session/new", json!({"cwd":"/tmp"}))
        .await;
    assert_eq!(client.read().await["error"]["code"], -32600);
    client
        .send(Some(0), "initialize", json!({"protocolVersion":1}))
        .await;
    assert_eq!(
        client.read().await["error"]["data"],
        json!({"requested":1,"supported":[2]})
    );
    let session = client.session().await;

    client
        .send(Some(3), "session/list", json!({"cwd":"/tmp"}))
        .await;
    assert_eq!(
        client.read().await["result"]["sessions"],
        json!([{"sessionId":session,"cwd":"/tmp"}])
    );
    client
        .send(Some(4), "session/new", json!({"cwd":"relative"}))
        .await;
    assert_eq!(client.read().await["error"]["code"], -32602);
    client.send(Some(5), "session/prompt", json!({"sessionId":session,"prompt":[{"type":"audio","mimeType":"audio/mpeg","data":"AA=="}]})).await;
    assert_eq!(client.read().await["error"]["code"], -32602);
    let block = json!({"type":"resource_link","name":"notes","uri":"urn:notes"});
    client
        .send(
            Some(6),
            "session/prompt",
            json!({"sessionId":session,"prompt":[block],"idempotencyKey":"one"}),
        )
        .await;
    let first_id = client.read().await["result"]["messageId"].clone();
    let user = client.update("user_message").await;
    assert_eq!(user["content"], json!([block]));
    assert_eq!(user["messageId"], first_id);
    assert_eq!(client.update("state_update").await["state"], "running");
    let agent = client.update("agent_message").await;
    assert_eq!(agent["content"][0]["text"], "notes (urn:notes)");
    assert_eq!(
        client.update("state_update").await["stopReason"],
        "end_turn"
    );

    client
        .send(Some(7), "session/close", json!({"sessionId":session}))
        .await;
    assert_eq!(client.read().await["result"], json!({}));
    client.prompt(8, &session, "one", "duplicate").await;
    assert_eq!(client.read().await["error"]["data"]["sessionId"], session);
    client
        .send(
            Some(9),
            "session/resume",
            json!({"cwd":"/tmp","sessionId":session,"replayFrom":{"type":"start"}}),
        )
        .await;
    assert_eq!(client.update("user_message").await, user);
    assert_eq!(client.update("agent_message").await, agent);
    client.update("replay_complete").await;
    assert!(client.update("session_info_update").await["title"].is_null());
    assert_eq!(client.update("state_update").await["state"], "idle");
    assert_eq!(client.read().await["result"], json!({}));
    client.prompt(10, &session, "one", "duplicate").await;
    assert_eq!(client.read().await["result"]["messageId"], first_id);
    client.prompt(11, &session, "two", "again").await;
    assert!(client.read().await["result"]["messageId"].is_string());
    client.update("user_message").await;
    client.update("state_update").await;
    assert_eq!(
        client.update("agent_message").await["content"][0]["text"],
        "notes (urn:notes) / notes (urn:notes) / again"
    );
    assert_eq!(
        client.update("state_update").await["stopReason"],
        "end_turn"
    );
}

#[tokio::test]
async fn cancel_drops_foreground_and_all_queued_retries() {
    let mut client = ClientIo::start();
    let session = client.session().await;
    client.prompt(3, &session, "active", "wait").await;
    assert!(client.read().await["result"]["messageId"].is_string());
    client.update("user_message").await;
    client.update("state_update").await;

    client.prompt(4, &session, "held", "fail").await;
    client.prompt(5, &session, "held", "fail").await;
    client
        .send(None, "session/cancel", json!({"sessionId":session}))
        .await;
    for id in [4, 5] {
        let frame = client.read().await;
        assert_eq!(frame["id"], id);
        assert_eq!(frame["error"]["code"], -32800);
    }
    assert_eq!(
        client.update("state_update").await["stopReason"],
        "cancelled"
    );
    client.prompt(6, &session, "held", "fail").await;
    assert!(client.read().await["result"]["messageId"].is_string());
    client.update("user_message").await;
    client.update("state_update").await;
    let idle = client.update("state_update").await;
    assert_eq!(idle["stopReason"], "error");
    assert_eq!(idle["error"]["code"], -32603);
    assert_eq!(idle["error"]["message"], "Model unavailable");
}

#[tokio::test]
async fn reports_authentication_and_model_stop_reasons() {
    let mut client = ClientIo::start();
    let session = client.session().await;
    for (prompt, reason, output) in [
        ("auth", "error", None),
        ("limit", "max_tokens", Some("Partial")),
        ("refuse", "refusal", Some("Cannot answer")),
    ] {
        client.prompt(3, &session, prompt, prompt).await;
        assert!(client.read().await["result"]["messageId"].is_string());
        client.update("user_message").await;
        client.update("state_update").await;
        if let Some(text) = output {
            assert_eq!(
                client.update("agent_message").await["content"][0]["text"],
                text
            );
        }
        let idle = client.update("state_update").await;
        assert_eq!(idle["state"], "idle");
        assert_eq!(idle["stopReason"], reason);
        if prompt == "auth" {
            assert_eq!(idle["error"]["code"], -32000);
        }
    }
}

#[tokio::test]
async fn selected_model_is_session_scoped_and_replayable() {
    let mut client = ClientIo::start();
    let session = client.session_with_form(true).await;
    let (message_id, picker) = client.picker(&session, "pick").await;
    let schema = &picker["params"]["requestedSchema"];
    assert_eq!(schema["required"], json!(["model"]));
    let field = &schema["properties"]["model"];
    assert_eq!(field["type"], "string");
    assert_eq!(field["default"], "first");
    assert_eq!(
        field["oneOf"],
        json!([{"const":"first","title":"First"},{"const":"second","title":"Second"}])
    );

    client.prompt(6, &session, "pick", "/model").await;
    assert_eq!(client.read().await["result"]["messageId"], message_id);
    client
        .answer(
            &picker,
            json!({"action":"accept","content":{"model":"second"}}),
        )
        .await;
    assert_eq!(client.update("state_update").await["state"], "running");
    let confirmation = client.update("agent_message").await;
    assert_eq!(confirmation["content"][0]["text"], "Model set to Second.");
    assert_eq!(confirmation["_meta"][MODEL_META_KEY], "second");
    assert_eq!(
        client.update("state_update").await["stopReason"],
        "end_turn"
    );
    client
        .send(Some(7), "session/close", json!({"sessionId":session}))
        .await;
    assert_eq!(client.read().await["result"], json!({}));
    client
        .send(
            Some(8),
            "session/resume",
            json!({"cwd":"/tmp","sessionId":session,"replayFrom":{"type":"start"}}),
        )
        .await;
    assert_eq!(client.update("user_message").await["messageId"], message_id);
    assert_eq!(client.update("agent_message").await, confirmation);
    client.update("replay_complete").await;
    client.update("session_info_update").await;
    assert_eq!(client.update("state_update").await["state"], "idle");
    assert_eq!(
        client.read().await["result"]["availableCommands"][0]["name"],
        "model"
    );

    // The provider echoes its full context. Both command messages must be absent.
    assert_eq!(client.completion(&session, "chat", "hello").await, "hello");
    assert_eq!(
        client.completion(&session, "model-check", "model?").await,
        "second"
    );
    let (_, picker) = client.picker(&session, "check-default").await;
    assert_eq!(
        picker["params"]["requestedSchema"]["properties"]["model"]["default"],
        "second"
    );
    client.answer(&picker, json!({"action":"decline"})).await;
    assert_eq!(
        client.update("state_update").await["stopReason"],
        "end_turn"
    );
    client
        .send(Some(9), "session/new", json!({"cwd":"/tmp"}))
        .await;
    let other = client.read().await["result"]["sessionId"].clone();
    assert_eq!(
        client.completion(&other, "model-check", "model?").await,
        "first"
    );
}

#[tokio::test]
async fn invalid_and_cancelled_selections_keep_the_previous_model() {
    let mut client = ClientIo::start();
    let session = client.session_with_form(true).await;
    for result in [
        json!({"action":"accept","content":{"model":"missing"}}),
        json!({"action":"accept"}),
    ] {
        let (_, picker) = client.picker(&session, &result.to_string()).await;
        client.answer(&picker, result).await;
        let state = client.update("state_update").await;
        assert_eq!(state["state"], "idle");
        assert_eq!(state["error"]["code"], -32602);
    }
    let (_, picker) = client.picker(&session, "cancel-form").await;
    client.answer(&picker, json!({"action":"cancel"})).await;
    assert_eq!(
        client.update("state_update").await["stopReason"],
        "cancelled"
    );

    let (_, picker) = client.picker(&session, "cancel-session").await;
    client
        .send(None, "session/cancel", json!({"sessionId":session}))
        .await;
    let cancel = client.read().await;
    assert_eq!(cancel["method"], "$/cancel_request");
    assert_eq!(cancel["params"]["requestId"], picker["id"]);
    assert_eq!(
        client.update("state_update").await["stopReason"],
        "cancelled"
    );
    client
        .answer(
            &picker,
            json!({"action":"accept","content":{"model":"second"}}),
        )
        .await;
    assert_eq!(
        client.completion(&session, "model-check", "model?").await,
        "first"
    );
}

#[tokio::test]
async fn model_command_explains_missing_form_support() {
    let mut client = ClientIo::start();
    let session = client.session().await;
    assert!(
        client
            .completion(&session, "pick", "/model")
            .await
            .contains("form support")
    );
    assert_eq!(client.completion(&session, "chat", "hello").await, "hello");
    assert!(
        client
            .completion(&session, "space", "/model \n")
            .await
            .contains("form support")
    );
    assert!(
        client
            .completion(&session, "literal", "/model comparison")
            .await
            .ends_with("/model comparison")
    );
}

#[tokio::test]
async fn catalog_failures_end_the_command_and_allow_the_next_prompt() {
    let empty: Catalog = || Ok(vec![]);
    let offline: Catalog = || {
        Err(ProviderError::AuthenticationRequired(
            "Sign in first".into(),
        ))
    };
    for (catalog, code) in [(empty, -32603), (offline, -32000)] {
        let mut client = ClientIo::with_catalog(catalog);
        let session = client.session_with_form(true).await;
        client.prompt(5, &session, "pick", "/model").await;
        assert!(client.read().await["result"]["messageId"].is_string());
        client.update("user_message").await;
        assert_eq!(client.update("state_update").await["state"], "running");
        let idle = client.update("state_update").await;
        assert_eq!(idle["state"], "idle");
        assert_eq!(idle["error"]["code"], code);
        assert_eq!(client.completion(&session, "chat", "hello").await, "hello");
    }
}

#[tokio::test]
async fn queued_prompt_uses_the_model_chosen_from_a_changed_catalog() {
    let mut client = ClientIo::with_catalog(|| {
        Ok(vec![ModelOption {
            id: "new".into(),
            name: "New".into(),
        }])
    });
    let session = client.session_with_form(true).await;
    let (_, picker) = client.picker(&session, "pick").await;
    assert!(
        picker["params"]["requestedSchema"]["properties"]["model"]
            .get("default")
            .is_none()
    );
    client.prompt(5, &session, "queued", "model?").await;
    client
        .answer(
            &picker,
            json!({"action":"accept","content":{"model":"new"}}),
        )
        .await;
    assert_eq!(client.update("state_update").await["state"], "running");
    client.update("agent_message").await;
    assert_eq!(client.update("state_update").await["state"], "idle");
    assert!(client.read().await["result"]["messageId"].is_string());
    client.update("user_message").await;
    assert_eq!(client.update("state_update").await["state"], "running");
    assert_eq!(
        client.update("agent_message").await["content"][0]["text"],
        "new"
    );
    assert_eq!(client.update("state_update").await["state"], "idle");
}
