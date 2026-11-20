use std::sync::{Arc, Mutex};

use axum::{Json, Router, extract::State, routing::post};
use elphie_mcp::{McpClient, ServerConfig, TransportConfig};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use url::Url;

type Calls = Arc<Mutex<Vec<Value>>>;

async fn handle(State(calls): State<Calls>, Json(message): Json<Value>) -> Json<Value> {
    let result = match message["method"].as_str().unwrap() {
        "server/discover" => json!({
            "resultType":"complete","supportedVersions":["2026-07-28"],
            "capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private"
        }),
        "tools/list" => {
            let tool = |name| json!({"name":name,"description":"A fixture tool","inputSchema":{"type":"object","properties":{"value":{"type":"string"}}}});
            if message["params"]["cursor"].is_null() {
                json!({"resultType":"complete","tools":[tool("unlisted")],"nextCursor":"second"})
            } else {
                assert_eq!(message["params"]["cursor"], "second");
                json!({"resultType":"complete","tools":[tool("lookup"), tool("fail")]})
            }
        }
        "tools/call" => {
            let params = &message["params"];
            calls.lock().unwrap().push(params.clone());
            match params["name"].as_str().unwrap() {
                "lookup" => {
                    json!({"resultType":"complete","content":[{"type":"text","text":"found"}],"structuredContent":{"value":params["arguments"]["value"]}})
                }
                "fail" => {
                    json!({"resultType":"complete","isError":true,"content":[{"type":"text","text":"PRIVATE_FIXTURE"}]})
                }
                unexpected => panic!("Unexpected tool {unexpected}"),
            }
        }
        unexpected => panic!("Unexpected method {unexpected}"),
    };
    Json(json!({"jsonrpc":"2.0","id":message["id"],"result":result}))
}

#[tokio::test]
async fn discovers_pages_and_routes_only_selected_tools() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/mcp", listener.local_addr().unwrap())).unwrap();
    let calls = Calls::default();
    let app = Router::new()
        .route("/mcp", post(handle))
        .with_state(calls.clone());
    tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
    let mut server = ServerConfig {
        name: "notes".into(),
        tools: vec!["lookup".into(), "fail".into()],
        transport: TransportConfig::Http {
            url,
            bearer_token_env: None,
        },
    };
    let mcp = McpClient::connect(std::slice::from_ref(&server))
        .await
        .unwrap();
    let tools: Vec<_> = mcp.definitions().iter().map(|d| d.name.as_str()).collect();
    assert_eq!(tools, ["mcp_notes__lookup", "mcp_notes__fail"]);
    assert_eq!(mcp.title("mcp_notes__lookup"), Some("notes / lookup"));
    let result = mcp
        .execute(tools[0], &json!({"value":"purple"}))
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&result).unwrap(),
        json!({"text":["found"],"structuredContent":{"value":"purple"}})
    );
    let failure = mcp.execute(tools[1], &json!({})).await.unwrap_err();
    assert_eq!(failure, "MCP server reported a tool error.");
    assert!(
        mcp.execute("mcp_notes__unlisted", &json!({}))
            .await
            .is_err()
    );
    assert!(mcp.execute(tools[0], &Value::Null).await.is_err());
    assert_eq!(calls.lock().unwrap().len(), 2);
    server.tools = vec!["absent".into()];
    let failure = McpClient::connect(&[server]).await.err().unwrap();
    assert_eq!(
        failure.to_string(),
        "MCP server notes: Configured tool absent was not found"
    );
}
