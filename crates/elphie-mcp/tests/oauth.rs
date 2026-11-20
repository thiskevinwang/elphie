use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::Form,
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::{any, get, post},
};
use elphie_mcp::{McpClient, ServerConfig, TransportConfig};
use elphie_oauth::{AuthorizedResource, ClientIdentity, PendingAuthorization, Url};
use serde_json::{Value, json};
use tokio::net::TcpListener;

const DOCUMENT: &str = "https://example.com/elphie.json";
const REDIRECT: &str = "http://127.0.0.1:8765/callback";

async fn authorize(resource: &Url) -> AuthorizedResource {
    let pending = PendingAuthorization::start(
        resource.clone(),
        REDIRECT.parse().unwrap(),
        ClientIdentity::MetadataDocument(DOCUMENT.parse().unwrap()),
    )
    .await
    .unwrap();
    let authorization = Url::parse(pending.authorization_url()).unwrap();
    let query: HashMap<_, _> = authorization.query_pairs().collect();
    assert_eq!(query["client_id"], DOCUMENT);
    let mut callback = Url::parse(REDIRECT).unwrap();
    callback
        .query_pairs_mut()
        .append_pair("code", "fixture-code")
        .append_pair("state", &query["state"]);
    pending.finish_resource(callback).await.unwrap()
}

#[tokio::test]
async fn uses_the_cimd_grant_only_at_its_authorized_mcp_endpoint() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let resource = Url::parse(&format!("{origin}/mcp")).unwrap();
    let challenge =
        format!(r#"Bearer resource_metadata="{origin}/.well-known/oauth-protected-resource""#);
    let authorization = json!({
        "issuer":origin, "authorization_endpoint":format!("{origin}/authorize"),
        "token_endpoint":format!("{origin}/token"), "code_challenge_methods_supported":["S256"],
        "client_id_metadata_document_supported":true
    });
    let protected = json!({"resource":resource, "authorization_servers":[origin]});
    let methods = Arc::new(Mutex::new(Vec::new()));
    let method_log = Arc::clone(&methods);
    let other_requests = Arc::new(AtomicUsize::new(0));
    let other_log = Arc::clone(&other_requests);
    let app = Router::new()
        .route("/.well-known/oauth-protected-resource", get(async move || Json(protected)))
        .route("/.well-known/oauth-authorization-server", get(async move || Json(authorization)))
        .route("/token", post(async |Form(form): Form<HashMap<String, String>>| {
            assert_eq!(form["client_id"], DOCUMENT);
            Json(json!({"access_token":"fixture-token", "token_type":"bearer","expires_in":3600}))
        }))
        .route("/other", any(async move || {
            other_log.fetch_add(1, Ordering::Relaxed);
            StatusCode::FORBIDDEN
        }))
        .route("/mcp", any(async move |headers: HeaderMap, body: Bytes| {
            if headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) != Some("Bearer fixture-token") {
                return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, challenge.clone())]).into_response();
            }
            let message: Value = serde_json::from_slice(&body).unwrap();
            let method = message["method"].as_str().unwrap();
            method_log.lock().unwrap().push(method.to_owned());
            let result = match method {
                "server/discover" => json!({"resultType":"complete", "supportedVersions":["2026-07-28"], "capabilities":{"tools":{}}, "ttlMs":0, "cacheScope":"private"}),
                "tools/list" => json!({"resultType":"complete", "tools":[{"name":"lookup", "inputSchema":{"type":"object"}}]}),
                "tools/call" => {
                    assert_eq!(message["params"]["name"], "lookup");
                    json!({"resultType":"complete", "content":[{"type":"text","text":"Authorized response"}]})
                }
                _ => panic!("Unexpected MCP request {method}"),
            };
            Json(json!({"jsonrpc":"2.0","id":message["id"],"result":result})).into_response()
        }));
    tokio::spawn(async { axum::serve(listener, app).await.unwrap() });

    let server = |url| ServerConfig {
        name: "fixture".into(),
        tools: vec!["lookup".into()],
        transport: TransportConfig::Http {
            url,
            bearer_token_env: None,
        },
    };
    let client =
        McpClient::connect_authorized(&server(resource.clone()), authorize(&resource).await)
            .await
            .unwrap();
    let result = client
        .execute(&client.definitions()[0].name, &json!({}))
        .await
        .unwrap();
    assert_eq!(result, "Authorized response");
    assert_eq!(
        *methods.lock().unwrap(),
        ["server/discover", "tools/list", "tools/call"]
    );

    let other = server(resource.join("/other").unwrap());
    let grant = authorize(&resource).await;
    assert!(McpClient::connect_authorized(&other, grant).await.is_err());
    assert_eq!(other_requests.load(Ordering::Relaxed), 0);
}
