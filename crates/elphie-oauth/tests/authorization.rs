use axum::{
    Json, Router,
    extract::Form,
    http::{StatusCode, header},
    routing::{any, get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use elphie_oauth::{ClientIdentity, PendingAuthorization, Url};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::net::TcpListener;

const DOCUMENT: &str = "https://example.com/elphie.json";
const REDIRECT: &str = "http://127.0.0.1:8765/callback";
type Fields = HashMap<String, String>;

struct Server {
    resource: Url,
    registrations: Arc<Mutex<Vec<Value>>>,
    exchanges: Arc<Mutex<Vec<Fields>>>,
}

async fn server(supports_cimd: bool) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let resource = format!("{origin}/mcp");
    let challenge =
        format!(r#"Bearer resource_metadata="{origin}/.well-known/oauth-protected-resource""#);
    let protected_resource = json!({"resource":resource, "authorization_servers":[origin]});
    let authorization = json!({
        "issuer": origin, "authorization_endpoint": format!("{origin}/authorize"),
        "registration_endpoint": format!("{origin}/register"), "token_endpoint":format!("{origin}/token"),
        "code_challenge_methods_supported":["S256"], "client_id_metadata_document_supported":supports_cimd,
    });
    let registrations = Arc::new(Mutex::new(Vec::new()));
    let exchanges = Arc::new(Mutex::new(Vec::new()));
    let registration_log = Arc::clone(&registrations);
    let exchange_log = Arc::clone(&exchanges);
    let app = Router::new()
        .route("/mcp", any(async move || (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE,challenge)])))
        .route("/.well-known/oauth-protected-resource", get(async move || Json(protected_resource)))
        .route("/.well-known/oauth-authorization-server", get(async move || Json(authorization)))
        .route("/register", post(async move |Json(request): Json<Value>| {
            registration_log.lock().unwrap().push(request);
            Json(json!({"client_id":"registered-elphie", "redirect_uris":[REDIRECT]}))
        }))
        .route("/token", post(async move |Form(request): Form<Fields>| {
            exchange_log.lock().unwrap().push(request);
            Json(json!({"access_token":"example-access-token", "token_type":"bearer", "expires_in":3600}))
        }));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server {
        resource: resource.parse().unwrap(),
        registrations,
        exchanges,
    }
}

#[tokio::test]
async fn exchanges_cimd_code_with_matching_pkce_verifier_and_resource() {
    let server = server(true).await;
    let pending = PendingAuthorization::start(
        server.resource.clone(),
        REDIRECT.parse().unwrap(),
        ClientIdentity::MetadataDocument(DOCUMENT.parse().unwrap()),
    )
    .await
    .unwrap();
    let authorization = Url::parse(pending.authorization_url()).unwrap();
    let query: Fields = authorization.query_pairs().into_owned().collect();
    assert_eq!(query["client_id"], DOCUMENT);
    assert_eq!(query["resource"], server.resource.as_str());
    assert_eq!(query["code_challenge_method"], "S256");
    assert!(server.registrations.lock().unwrap().is_empty());
    let mut callback = Url::parse(REDIRECT).unwrap();
    callback
        .query_pairs_mut()
        .append_pair("code", "example-code")
        .append_pair("state", &query["state"]);
    let (resource, manager) = pending
        .finish_resource(callback)
        .await
        .unwrap()
        .into_parts();
    assert_eq!(resource, server.resource);
    assert_eq!(
        manager.get_access_token().await.unwrap(),
        "example-access-token"
    );
    let exchanges = server.exchanges.lock().unwrap();
    assert_eq!(exchanges.len(), 1);
    assert_eq!(exchanges[0]["resource"], server.resource.as_str());
    assert_eq!(exchanges[0]["redirect_uri"], REDIRECT);
    assert_eq!(
        URL_SAFE_NO_PAD.encode(Sha256::digest(&exchanges[0]["code_verifier"])),
        query["code_challenge"]
    );
}

#[tokio::test]
async fn registers_native_client_when_cimd_is_absent_or_unsupported() {
    for identity in [
        ClientIdentity::Dynamic,
        ClientIdentity::MetadataDocument(DOCUMENT.parse().unwrap()),
    ] {
        let server = server(false).await;
        let pending =
            PendingAuthorization::start(server.resource, REDIRECT.parse().unwrap(), identity)
                .await
                .unwrap();
        let authorization = Url::parse(pending.authorization_url()).unwrap();
        let query: Fields = authorization.query_pairs().into_owned().collect();
        assert_eq!(query["client_id"], "registered-elphie");
        let log = server.registrations.lock().unwrap();
        assert_eq!(log[0]["application_type"], "native");
        assert_eq!(log[0]["redirect_uris"], json!([REDIRECT]));
    }
}

#[tokio::test]
async fn rejects_wrong_state_and_callback_destination_before_token_exchange() {
    let server = server(true).await;
    for wrong_state in [false, true] {
        let pending = PendingAuthorization::start(
            server.resource.clone(),
            REDIRECT.parse().unwrap(),
            ClientIdentity::MetadataDocument(DOCUMENT.parse().unwrap()),
        )
        .await
        .unwrap();
        let authorization = Url::parse(pending.authorization_url()).unwrap();
        let query: Fields = authorization.query_pairs().into_owned().collect();
        let mut callback = Url::parse(REDIRECT).unwrap();
        if !wrong_state {
            callback.set_path("/other");
        }
        callback
            .query_pairs_mut()
            .append_pair("code", "example-code")
            .append_pair(
                "state",
                if wrong_state {
                    "wrong-state"
                } else {
                    &query["state"]
                },
            );
        assert!(pending.finish(callback).await.is_err());
    }
    assert!(server.exchanges.lock().unwrap().is_empty());
}

#[tokio::test]
async fn checks_issuer_before_using_a_registered_client_id() {
    let server = server(true).await;
    let issuer = server.resource.origin().ascii_serialization();
    for expected in [format!("{issuer}/"), issuer.clone()] {
        let result = PendingAuthorization::start(
            server.resource.clone(),
            REDIRECT.parse().unwrap(),
            ClientIdentity::Preregistered {
                issuer: expected.clone(),
                client_id: "public-example".into(),
            },
        )
        .await;
        if expected == issuer {
            let url = Url::parse(result.unwrap().authorization_url()).unwrap();
            assert!(
                url.query_pairs()
                    .any(|(k, v)| k == "client_id" && v == "public-example")
            );
        } else {
            assert!(result.is_err());
        }
    }
    assert!(server.registrations.lock().unwrap().is_empty());
}
