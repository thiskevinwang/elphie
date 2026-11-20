use super::*;
use elphie_core::{ModelInput, ModelTurn, ToolCall, ToolDefinition, ToolRequest};
use tempfile::TempDir;

struct ToolProvider;

fn call(id: &str, name: &str, path: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: json!({ "path": path }),
    }
}

#[async_trait]
impl Provider for ToolProvider {
    fn default_model(&self) -> &str {
        "tools"
    }

    async fn models(&self) -> Result<Vec<ModelOption>, ProviderError> {
        Ok(vec![])
    }

    async fn complete(&self, _: &str, _: &[Message]) -> ProviderResult {
        panic!("A tool session uses the structured provider interface")
    }

    async fn turn(
        &self,
        _: &str,
        history: &[ModelInput],
        tools: &[ToolDefinition],
    ) -> Result<ModelTurn, ProviderError> {
        assert_eq!(tools, elphie_tools::definitions());
        let prompt = history
            .iter()
            .rev()
            .find_map(|item| match item {
                ModelInput::Message(message) if message.role == elphie_core::Role::User => {
                    Some(message.text.as_str())
                }
                _ => None,
            })
            .unwrap();
        let calls = match (prompt, history.last().unwrap()) {
            ("loop", _) => vec![call("same-id", "list_directory", ".")],
            ("read", ModelInput::Message(_)) => vec![
                call("file", "read_file", "sample"),
                call("unknown", "unknown_tool", "."),
            ],
            ("read", ModelInput::Tools { request, outputs }) => {
                assert_eq!(request.calls.len(), 2);
                assert_eq!(outputs[0], "private sample contents");
                assert!(outputs[1].starts_with("Error: "));
                vec![]
            }
            ("follow up", _) => {
                assert!(history.iter().any(|item| matches!(item, ModelInput::Tools { outputs, .. } if outputs[0] == "private sample contents")));
                vec![]
            }
            ("after cancel", _) => {
                assert!(
                    history
                        .iter()
                        .all(|item| matches!(item, ModelInput::Message(_)))
                );
                vec![]
            }
            _ => panic!("Unexpected prompt or history"),
        };
        if calls.is_empty() {
            Ok(ModelTurn::Reply(Completion {
                text: "Checked.".into(),
                reason: FinishReason::EndTurn,
            }))
        } else {
            Ok(ModelTurn::Tools(ToolRequest::new(calls)))
        }
    }
}

fn client() -> (TempDir, ClientIo) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("sample"), "private sample contents").unwrap();
    let workspace = Arc::new(Workspace::open(root.path()).unwrap());
    (
        root,
        ClientIo::with_provider(Arc::new(ToolProvider), Some(workspace)),
    )
}

async fn start(client: &mut ClientIo, session: &Value, prompt: &str) -> Value {
    client.prompt(3, session, prompt, prompt).await;
    assert!(client.read().await["result"]["messageId"].is_string());
    let user = client.update("user_message").await;
    assert_eq!(client.update("state_update").await["state"], "running");
    user
}

#[tokio::test]
async fn tools_report_replay_and_return_bounded_results_to_the_model() {
    let (_root, mut client) = client();
    let session = client.session().await;
    let mut transcript = vec![start(&mut client, &session, "read").await];
    let mut ids = std::collections::HashSet::new();
    for (name, kind, status) in [
        ("read_file", "read", "completed"),
        ("unknown_tool", "other", "failed"),
    ] {
        let started = client.update("tool_call_update").await;
        let finished = client.update("tool_call_update").await;
        assert_eq!(started["status"], "in_progress");
        assert_eq!(started["name"], name);
        assert_eq!(started["kind"], kind);
        if name == "read_file" {
            assert_eq!(started["title"], "Read file sample");
        }
        assert!(ids.insert(started["toolCallId"].as_str().unwrap().to_owned()));
        assert_eq!(finished["toolCallId"], started["toolCallId"]);
        assert_eq!(finished["status"], status);
        assert!(finished["content"][0]["content"]["text"].is_string());
        assert!(finished.get("rawOutput").is_none());
        assert!(!finished.to_string().contains("private sample contents"));
        transcript.extend([started, finished]);
    }
    transcript.push(client.update("agent_message").await);
    assert_eq!(
        client.update("state_update").await["stopReason"],
        "end_turn"
    );
    client
        .send(
            Some(4),
            "session/resume",
            json!({"cwd":"/tmp","sessionId":session,"replayFrom":{"type":"start"}}),
        )
        .await;
    for saved in transcript {
        assert_eq!(
            client
                .update(saved["sessionUpdate"].as_str().unwrap())
                .await,
            saved
        );
    }
    client.update("replay_complete").await;
    client.update("session_info_update").await;
    assert_eq!(client.update("state_update").await["state"], "idle");
    assert!(client.read().await["result"].is_object());
    assert_eq!(
        client.completion(&session, "next", "follow up").await,
        "Checked."
    );
}

#[tokio::test]
async fn repeated_requests_stop_at_the_per_prompt_limit() {
    let (_root, mut client) = client();
    let session = client.session().await;
    start(&mut client, &session, "loop").await;
    let mut ids = std::collections::HashSet::new();
    for _ in 0..tool_call::CALLS_PER_PROMPT {
        let started = client.update("tool_call_update").await;
        assert!(ids.insert(started["toolCallId"].as_str().unwrap().to_owned()));
        assert_eq!(
            client.update("tool_call_update").await["status"],
            "completed"
        );
    }
    let idle = client.update("state_update").await;
    assert_eq!(idle["stopReason"], "error");
    assert!(idle["error"]["message"].as_str().unwrap().contains("limit"));
}

#[test]
fn cancellation_closes_the_active_call_and_discards_the_unstarted_batch() {
    // Keep the first read queued so cancel arrives before the filesystem task runs.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (_root, mut client) = client();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let (ready, waiting) = tokio::sync::oneshot::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            ready.send(()).unwrap();
            let _ = held.recv();
        });
        waiting.await.unwrap();
        let session = client.session().await;
        start(&mut client, &session, "read").await;
        let started = client.update("tool_call_update").await;
        client
            .send(None, "session/cancel", json!({"sessionId":session}))
            .await;
        let cancelled = client.update("tool_call_update").await;
        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(cancelled["toolCallId"], started["toolCallId"]);
        assert_eq!(
            client.update("state_update").await["stopReason"],
            "cancelled"
        );
        drop(release);
        blocker.await.unwrap();
        assert_eq!(
            client.completion(&session, "next", "after cancel").await,
            "Checked."
        );
    });
}
