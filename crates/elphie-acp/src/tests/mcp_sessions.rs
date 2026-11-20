use super::*;
use axum::{Json, Router, routing::post};
use elphie_core::{ModelInput, ModelTurn, ToolDefinition};
use std::sync::atomic::AtomicUsize;
use tokio::net::TcpListener;

struct CatalogProvider;

#[async_trait]
impl Provider for CatalogProvider {
    fn default_model(&self) -> &str {
        "fixture"
    }
    async fn models(&self) -> Result<Vec<ModelOption>, ProviderError> {
        Ok(vec![])
    }
    async fn complete(&self, _: &str, _: &[Message]) -> ProviderResult {
        panic!("MCP sessions need a tool catalogue")
    }
    async fn turn(
        &self,
        _: &str,
        _: &[ModelInput],
        tools: &[ToolDefinition],
    ) -> Result<ModelTurn, ProviderError> {
        Ok(ModelTurn::Reply(Completion {
            text: tools
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>()
                .join(" "),
            reason: FinishReason::EndTurn,
        }))
    }
}

#[derive(Default)]
struct ServerState {
    discoveries: AtomicUsize,
    fail_list: AtomicBool,
}

async fn fixture() -> (ClientIo, String, Arc<ServerState>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let connections = Arc::new(ServerState::default());
    let counter = connections.clone();
    let app = Router::new().route("/mcp", post(move |Json(request): Json<Value>| {
        let counter = counter.clone();
        async move {
            let result = match request["method"].as_str().unwrap() {
                "server/discover" => {
                    counter.discoveries.fetch_add(1, Ordering::Relaxed);
                    json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private"})
                }
                "tools/list" if counter.fail_list.load(Ordering::Relaxed) => return Json(json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32603,"message":"private-fixture-details"}})),
                "tools/list" => json!({"resultType":"complete","tools":[{"name":"lookup","inputSchema":{"type":"object","properties":{}}}]}),
                m => panic!("Unexpected request {m}")
            };
            Json(json!({"jsonrpc":"2.0","id":request["id"],"result":result}))
        }
    }));
    tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
    let path = tempfile::NamedTempFile::new().unwrap();
    let servers =
        ["one", "two"].map(|name| json!({"type":"http","name":name,"url":url,"tools":["lookup"]}));
    std::fs::write(
        path.path(),
        serde_json::to_vec(&json!({"servers":servers})).unwrap(),
    )
    .unwrap();
    let config = McpConfig::load(path.path()).unwrap();
    let mut client = ClientIo::with_mcp(Arc::new(CatalogProvider), None, config);
    client
        .send(
            Some(1),
            "initialize",
            json!({"protocolVersion":2,"info":{"name":"fixture","version":"1"}}),
        )
        .await;
    assert_eq!(
        client.read().await["result"]["capabilities"]["session"],
        json!({"mcp":{"http":{}}})
    );
    (client, url, connections)
}

#[tokio::test]
async fn unconfigured_requests_fail_before_mcp_discovery() {
    let (mut client, url, connections) = fixture().await;
    let valid = json!({"type":"http","name":"one","url":url});
    for descriptors in [
        json!([valid, valid]),
        json!([{"type":"http","name":"one","url":format!("{url}/other")}]),
        json!([{"type":"http","name":"three","url":url}]),
        json!([{"type":"http","name":"one","url":url,"headers":[{"name":"Authorization","value":"private-fixture-header"}]}]),
        json!([{"type":"stdio","name":"one","command":"/unused"}]),
    ] {
        client
            .send(
                Some(2),
                "session/new",
                json!({"cwd":"/tmp","mcpServers":descriptors}),
            )
            .await;
        let failure = client.read().await;
        assert_eq!(failure["error"]["code"], -32602);
        assert!(!failure.to_string().contains("private-fixture-header"));
    }
    client
        .send(
            Some(3),
            "session/resume",
            json!({"sessionId":"missing","cwd":"/tmp","mcpServers":[valid]}),
        )
        .await;
    assert_eq!(client.read().await["error"]["data"]["sessionId"], "missing");
    assert_eq!(connections.discoveries.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn mcp_grants_and_reconnections_are_session_scoped() {
    let (mut client, url, connections) = fixture().await;
    let one = json!({"type":"http","name":"one","url":url});
    let two = json!({"type":"http","name":"two","url":url});
    client
        .send(
            Some(2),
            "session/new",
            json!({"cwd":"/tmp","mcpServers":[one]}),
        )
        .await;
    let new = client.read().await;
    assert_eq!(
        new["result"]["_meta"][MCP_SERVERS_META_KEY],
        json!(["one"]),
        "{new}"
    );
    let first = new["result"]["sessionId"].clone();
    client
        .send(Some(3), "session/new", json!({"cwd":"/tmp"}))
        .await;
    let new = client.read().await;
    assert_eq!(
        new["result"]["_meta"][MCP_SERVERS_META_KEY],
        json!(["one", "two"])
    );
    let second = new["result"]["sessionId"].clone();
    assert_eq!(
        client.completion(&first, "p", "list").await,
        "mcp_one__lookup"
    );
    assert_eq!(
        client.completion(&second, "p", "list").await,
        "mcp_one__lookup mcp_two__lookup"
    );
    let before = connections.discoveries.load(Ordering::Relaxed);
    for (descriptor, names) in [(one, json!(["one"])), (two.clone(), json!(["two"]))] {
        client
            .send(
                Some(4),
                "session/resume",
                json!({"sessionId":first,"cwd":"/tmp","mcpServers":[descriptor]}),
            )
            .await;
        client.update("session_info_update").await;
        client.update("state_update").await;
        assert_eq!(
            client.read().await["result"]["_meta"][MCP_SERVERS_META_KEY],
            names
        );
    }
    assert_eq!(connections.discoveries.load(Ordering::Relaxed), before + 1);
    assert_eq!(
        client.completion(&first, "p2", "list again").await,
        "mcp_two__lookup"
    );
    assert_eq!(
        client.completion(&second, "p2", "list again").await,
        "mcp_one__lookup mcp_two__lookup"
    );
    client
        .send(Some(5), "session/close", json!({"sessionId":first}))
        .await;
    assert!(client.read().await["result"].is_object());
    client
        .send(
            Some(6),
            "session/resume",
            json!({"sessionId":first,"cwd":"/tmp","mcpServers":[two]}),
        )
        .await;
    client.update("session_info_update").await;
    client.update("state_update").await;
    assert!(client.read().await["result"].is_object());
    assert_eq!(connections.discoveries.load(Ordering::Relaxed), before + 2);
}

#[tokio::test]
async fn failed_setup_preserves_existing_sessions_and_keeps_server_errors_private() {
    let (mut client, url, state) = fixture().await;
    let one = json!({"type":"http","name":"one","url":url});
    let two = json!({"type":"http","name":"two","url":url});
    client
        .send(
            Some(2),
            "session/new",
            json!({"cwd":"/tmp","mcpServers":[one]}),
        )
        .await;
    let session = client.read().await["result"]["sessionId"].clone();
    state.fail_list.store(true, Ordering::Relaxed);
    for (method, params) in [
        (
            "session/resume",
            json!({"sessionId":session,"cwd":"/tmp","mcpServers":[two]}),
        ),
        ("session/new", json!({"cwd":"/tmp","mcpServers":[two]})),
    ] {
        client.send(Some(3), method, params).await;
        let reply = client.read().await;
        assert_eq!(reply["error"]["code"], -32603);
        assert!(!reply.to_string().contains("private-fixture-details"));
    }
    client.send(Some(4), "session/list", json!({})).await;
    assert_eq!(
        client.read().await["result"]["sessions"],
        json!([{"sessionId":session,"cwd":"/tmp"}])
    );
    assert_eq!(
        client.completion(&session, "still-live", "list").await,
        "mcp_one__lookup"
    );
}

#[test]
fn stdio_descriptors_can_only_select_the_operator_command() {
    let executable = std::env::current_exe().unwrap();
    let grant = json!({"name":"fixture","type":"stdio","tools":["lookup"],"command":executable,
        "args":["--service"],"env":["MCP_FIXTURE_KEY"]});
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        file.path(),
        serde_json::to_vec(&json!({"servers":[grant]})).unwrap(),
    )
    .unwrap();
    let host = mcp::HostMcp::new(Arc::new(McpConfig::load(file.path()).unwrap()));
    assert_eq!(
        serde_json::to_value(host.capabilities()).unwrap(),
        json!({"mcp":{"stdio":{}}})
    );
    let descriptor =
        json!({"type":"stdio","name":"fixture","command":executable,"args":["--service"]});
    let parse = |value| serde_json::from_value::<schema::McpServer>(value).unwrap();
    assert_eq!(host.select(&[parse(descriptor.clone())]).unwrap().len(), 1);
    for (key, value) in [
        ("command", json!(executable.with_extension("different"))),
        ("args", json!(["--other"])),
        (
            "env",
            json!([{"name":"MCP_FIXTURE_KEY","value":"private-fixture-value"}]),
        ),
    ] {
        let mut override_attempt = descriptor.clone();
        override_attempt[key] = value;
        assert!(host.select(&[parse(override_attempt)]).is_err());
    }
}
