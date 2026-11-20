use super::*;
use axum::{Json, Router, http::HeaderMap, routing::post};
use serde_json::Value;
use tokio::net::TcpListener;

const TEST_TOKEN: &str = "fixture-access-token";

#[tokio::test]
async fn sends_roles_and_model_then_returns_only_the_final_assistant_message() {
    let app = Router::new().route(
        "/",
        post(|headers: HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(headers["authorization"], "Bearer fixture-access-token");
            assert_eq!(body["model"], "fixture");
            assert_eq!(body["store"], false);
            assert_eq!(body["stream"], true);
            assert_eq!(body["input"][0]["content"], "Remember Elphie");
            assert_eq!(body["input"][1]["role"], "assistant");
            assert_eq!(body["input"][1]["content"], "OK");
            assert_eq!(body["input"][2]["content"], "Which name?");
            assert!(body.get("tools").is_none());
            let commentary = json!({"type": "message", "role": "assistant", "phase": "commentary",
            "content": [{"type": "output_text", "text": "Checking."}]});
            let reply = json!({"type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": "Elphie."}]});
            [
                json!({"type": "response.output_item.done", "item": commentary}),
                json!({"type": "response.output_item.done", "item": reply}),
                json!({"type": "response.completed", "response": {"output": []}}),
            ]
            .iter()
            .map(|event| format!("data: {event}\r\n\r\n"))
            .collect::<String>()
        }),
    );
    let messages = [
        Message {
            role: Role::User,
            text: "Remember Elphie".into(),
        },
        Message {
            role: Role::Assistant,
            text: "OK".into(),
        },
        Message {
            role: Role::User,
            text: "Which name?".into(),
        },
    ];
    let url = serve(app).await;
    let result = ChatGpt::new("fixture")
        .unwrap()
        .request(&messages, TEST_TOKEN, &url)
        .await;
    assert_eq!(
        result.unwrap(),
        Completion {
            text: "Elphie.".into(),
            reason: FinishReason::EndTurn
        }
    );
}

#[tokio::test]
async fn truncated_reply_does_not_complete_the_turn() {
    let app = Router::new().route("/", post(|| async {
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Part\"}]}}\n\n"
    }));
    let url = serve(app).await;
    let err = ChatGpt::new("fixture")
        .unwrap()
        .request(&[], TEST_TOKEN, &url)
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Failed(msg) if msg.contains("disconnected")));
}

#[tokio::test]
async fn rejected_credentials_show_sign_in_instructions() {
    let app = Router::new().route(
        "/",
        post(|| async { (StatusCode::UNAUTHORIZED, "private error detail") }),
    );
    let url = serve(app).await;
    let err = ChatGpt::new("fixture")
        .unwrap()
        .request(&[], TEST_TOKEN, &url)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ProviderError::AuthenticationRequired(msg) if msg.contains("elphie login") && !msg.contains("private"))
    );
}

#[tokio::test]
async fn subscription_limit_in_the_stream_identifies_the_recipient_and_next_action() {
    let app = Router::new().route("/", post(|| async {
        ([("x-request-id", "req_fixture")], r#"data: {"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded"}}}

"#)
    }));
    let url = serve(app).await;
    let error = ChatGpt::new("fixture")
        .unwrap()
        .request(&[], TEST_TOKEN, &url)
        .await
        .unwrap_err();
    let ProviderError::Failed(error) = error else {
        panic!("expected a provider failure")
    };
    assert!(error.contains("https://chatgpt.com/settings/usage"));
    assert!(error.contains("subscription_sharing_usage_limit_exceeded"));
    assert!(error.contains("req_fixture"));
}

async fn serve(app: Router) -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/")
}

#[tokio::test]
async fn preserves_refusal_and_token_limited_output() {
    for (event, reason, part) in [
        (
            "response.completed",
            FinishReason::Refusal,
            json!({"type":"refusal", "refusal":"Cannot answer"}),
        ),
        (
            "response.incomplete",
            FinishReason::MaxTokens,
            json!({"type":"output_text", "text":"Partial answer"}),
        ),
    ] {
        let expected = part["refusal"]
            .as_str()
            .or_else(|| part["text"].as_str())
            .unwrap()
            .to_owned();
        let response = json!({"type":event,"response":{"output":[{"type":"message","role":"assistant","content":[part]}],"incomplete_details":{"reason":"max_output_tokens"}}});
        let url =
            serve(Router::new().route("/", post(async move || format!("data: {response}\n\n"))))
                .await;
        let reply = ChatGpt::new("fixture")
            .unwrap()
            .request(&[], TEST_TOKEN, &url)
            .await
            .unwrap();
        assert_eq!(
            reply,
            Completion {
                text: expected,
                reason
            }
        );
    }
}
