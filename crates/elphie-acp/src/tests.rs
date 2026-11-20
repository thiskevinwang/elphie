use super::*;
use agent_client_protocol::ByteStreams;
use async_trait::async_trait;
use elphie_core::{Completion, FinishReason, Message, ProviderError, ProviderResult};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf},
    time::{Duration, timeout},
};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

struct TestProvider;

#[async_trait]
impl Provider for TestProvider {
    async fn complete(&self, messages: &[Message]) -> ProviderResult {
        match messages.last().unwrap().text.as_str() {
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
        let (client, server) = tokio::io::duplex(16384);
        let (reader, writer) = tokio::io::split(server);
        tokio::spawn(connect(
            Arc::new(TestProvider),
            ByteStreams::new(writer.compat_write(), reader.compat()),
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
        self.send(
            Some(1),
            "initialize",
            json!({"protocolVersion":2,"capabilities":{},"info":{"name":"test","version":"1"}}),
        )
        .await;
        let init = self.read().await;
        assert_eq!(init["result"]["protocolVersion"], 2);
        assert_eq!(init["result"]["capabilities"]["session"], json!({}));
        self.send(Some(2), "session/new", json!({"cwd":"/tmp"}))
            .await;
        let reply = self.read().await;
        assert!(reply["result"]["sessionId"].is_string());
        reply["result"]["sessionId"].clone()
    }

    async fn prompt(&mut self, request: u64, session: &Value, key: &str, text: &str) {
        self.send(Some(request), "session/prompt", json!({"sessionId":session,"idempotencyKey":key,"prompt":[{"type":"text","text":text}]})).await;
    }

    async fn update(&mut self, expected: &str) -> Value {
        let frame = self.read().await;
        assert_eq!(frame["method"], "session/update", "{frame}");
        let update = &frame["params"]["update"];
        assert_eq!(update["sessionUpdate"], expected);
        update.clone()
    }
}

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
