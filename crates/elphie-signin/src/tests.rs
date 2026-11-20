use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use super::*;
use axum::{
    Form, Json, Router,
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{EncodingKey, Header, encode};
use rsa::{RsaPrivateKey, pkcs1::EncodeRsaPrivateKey, traits::PublicKeyParts};
use serde_json::json;
use sha2::{Digest, Sha256};
use url::Url;

type Fields = HashMap<String, String>;

#[tokio::test]
async fn browser_callback_signed_identity_and_serialized_refresh() {
    let key = RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).unwrap();
    let jwks = json!({"keys": [ {"kty":"RSA", "alg":"RS256", "kid":"test", "use":"sig",
        "n":URL_SAFE_NO_PAD.encode(key.n().to_bytes_be()), "e":URL_SAFE_NO_PAD.encode(key.e().to_bytes_be())} ]});
    let signer = EncodingKey::from_rsa_der(key.to_pkcs1_der().unwrap().as_bytes());
    let received = Arc::new(Mutex::new(Fields::new()));
    let count = Arc::new(AtomicUsize::new(0));
    let capture = received.clone();
    let total = count.clone();
    let code_requests = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/keys", get(move || async move { Json(jwks) }))
        .route("/token", post(move |Form(form): Form<Fields>| {
            let capture = capture.clone();
            let total = total.clone();
            let signer = signer.clone();
            let code_requests = code_requests.clone();
            async move {
                assert_eq!(form["client_id"], "issued");
                assert_eq!(form["resource"], RESOURCE);
                if form["grant_type"] == "refresh_token" {
                    assert_eq!(form["refresh_token"], "first-refresh");
                    total.fetch_add(1, Ordering::SeqCst);
                    return (reqwest::StatusCode::OK, Json(json!({"access_token":"new","refresh_token":"rotated","token_type":"Bearer","expires_in":3600})));
                }
                assert_eq!(form["code"], "from-browser");
                if code_requests.fetch_add(1, Ordering::SeqCst) == 0 {
                    return (reqwest::StatusCode::BAD_REQUEST, Json(json!({"error":"invalid_grant"})));
                }
                let fields = capture.lock().unwrap();
                assert_eq!(form["redirect_uri"], fields["redirect_uri"]);
                assert_eq!(URL_SAFE_NO_PAD.encode(Sha256::digest(form["code_verifier"].as_bytes())), fields["code_challenge"]);
                let claims = json!({"sub":"sub1", "nonce":fields["nonce"], "iat":now().unwrap(), "exp":now().unwrap()+600, "iss":ISSUER, "aud":"issued"});
                let mut header = Header::new(Algorithm::RS256);
                header.kid = Some("test".into());
                let token = encode(&header, &claims, &signer).unwrap();
                (reqwest::StatusCode::OK, Json(json!({"access_token":"first", "refresh_token":"first-refresh", "id_token": token, "token_type":"Bearer","expires_in":30, "scope":SCOPE})))
            }
        }));
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("elphie");
    let store = Store::lock(&path).await.unwrap();
    let host = store.data.host_id.clone();
    store.save().unwrap();
    drop(store);
    let endpoint = format!("{base}/token");
    authorize(
        &path,
        &host,
        None,
        &endpoint,
        &format!("{base}/keys"),
        |url| {
            let fields: Fields = url.query_pairs().into_owned().collect();
            let previous = received.lock().unwrap();
            if previous.is_empty() {
                assert_eq!(fields["client_id"], DYNAMIC_CLIENT);
                assert_eq!(fields["agent_name_hint"], "Elphie");
            } else {
                assert_eq!(fields["client_id"], "issued");
                assert!(!fields.contains_key("agent_name_hint"));
                for changed in ["state", "nonce", "code_challenge"] {
                    assert_ne!(previous[changed], fields[changed]);
                }
            }
            drop(previous);
            assert_eq!(fields["ext_agent_host_id"], host);
            let uri = Url::parse(&fields["redirect_uri"])?;
            let state = fields["state"].clone();
            *received.lock().unwrap() = fields;
            tokio::spawn(async move {
                let wrong = Client::new()
                    .get(uri.clone())
                    .query(&[("state", "wrong"), ("code", "wrong")])
                    .send()
                    .await
                    .unwrap();
                assert_eq!(wrong.status(), 400);
                Client::new()
                    .get(uri)
                    .query(&[
                        ("state", state.as_str()),
                        ("code", "from-browser"),
                        ("client_id", "issued"),
                    ])
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap();
            });
            Ok(())
        },
    )
    .await
    .unwrap();
    let (a, b) = tokio::join!(token_for(&path, &endpoint), token_for(&path, &endpoint));
    assert_eq!(a.unwrap(), "new");
    assert_eq!(b.unwrap(), "new");
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let store = Store::lock(&path).await.unwrap();
    assert_eq!(store.data.host_id, host);
    assert_eq!(store.data.active.as_deref(), Some("issued"));
    let account = &store.data.accounts["issued"];
    assert_eq!(account.subject, "sub1");
    assert_eq!(account.tokens.as_ref().unwrap().refresh, "rotated");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path.join("chatgpt.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let keys: JwkSet = client()
        .unwrap()
        .get(format!("{base}/keys"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let expected_nonce = received.lock().unwrap()["nonce"].clone();
    for (nonce, client_id, scope) in [
        ("wrong", "issued", SCOPE),
        (expected_nonce.as_str(), "other", SCOPE),
        (expected_nonce.as_str(), "issued", "openid"),
    ] {
        let reply: Reply = serde_json::from_value(json!({"access_token":"a","refresh_token":"r","id_token":account.tokens.as_ref().unwrap().id,"expires_in":3600,"token_type":"Bearer","scope":scope})).unwrap();
        let plan = registration(reply, &keys, client_id, nonce)
            .and_then(|a| a.tokens.unwrap().check_plan());
        assert!(plan.is_err());
    }
    server.abort();
}

#[tokio::test]
async fn logout_revokes_only_the_active_account_and_keeps_registration_for_sign_in() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config");
    let mut store = Store::lock(&path).await.unwrap();
    let account = Account {
        subject: "subject".into(),
        email: None,
        tokens: Some(Tokens {
            access: "access".into(),
            refresh: "refresh".into(),
            id: "id-token".into(),
            scope: SCOPE.into(),
            expires_at: 99999,
        }),
    };
    store.data.accounts.insert("active".into(), account.clone());
    store.data.accounts.insert("other".into(), account);
    store.data.active = Some("active".into());
    let host = store.data.host_id.clone();
    store.save().unwrap();
    drop(store);
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let metadata = json!({"issuer":issuer, "revocation_endpoint":format!("{issuer}/revoke")});
    let app = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(move || async move { Json(metadata) }),
        )
        .route(
            "/revoke",
            post(|Form(form): Form<Fields>| async move {
                assert_eq!(form["client_id"], "active");
                assert_eq!(form["token_type_hint"], "refresh_token");
                assert_eq!(form["token"], "refresh");
            }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    sign_out(&path, &issuer).await.unwrap();
    sign_out(&path, &issuer).await.unwrap();
    let store = Store::lock(&path).await.unwrap();
    assert_eq!(store.data.host_id, host);
    assert!(store.data.active.is_none());
    let active = &store.data.accounts["active"];
    assert!(active.tokens.is_none());
    assert!(store.data.accounts["other"].tokens.is_some());
    let url = Attempt::new(12345)
        .authorize_url(&host, Some("active"), Some(active))
        .unwrap();
    let fields: Fields = url.query_pairs().into_owned().collect();
    assert_eq!(fields["client_id"], "active");
    assert!(!fields.contains_key("id_token_hint"));
    server.abort();
}

#[tokio::test]
async fn refresh_retains_credentials_on_temporary_failure_and_clears_reused_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config");
    let mut store = Store::lock(&path).await.unwrap();
    store.data.active = Some("issued".into());
    store.data.accounts.insert(
        "issued".into(),
        Account {
            subject: "user".into(),
            email: None,
            tokens: Some(Tokens {
                access: "a".into(),
                refresh: "r".into(),
                id: "hint".into(),
                expires_at: 1,
                scope: SCOPE.into(),
            }),
        },
    );
    store.save().unwrap();
    drop(store);
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route(
            "/temporary",
            post(|| async { reqwest::StatusCode::SERVICE_UNAVAILABLE }),
        )
        .route(
            "/expired",
            post(|| async {
                (
                    reqwest::StatusCode::UNAUTHORIZED,
                    Json(json!({"error":{"code":"refresh_token_reused"}})),
                )
            }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let temporary = token_for(&path, &format!("{base}/temporary"))
        .await
        .unwrap_err();
    assert!(!temporary.is::<AuthenticationRequired>());
    assert!(
        Store::lock(&path).await.unwrap().data.accounts["issued"]
            .tokens
            .is_some()
    );
    let expired = token_for(&path, &format!("{base}/expired"))
        .await
        .unwrap_err();
    assert!(expired.is::<AuthenticationRequired>());
    let store = Store::lock(&path).await.unwrap();
    assert_eq!(store.data.accounts["issued"].subject, "user");
    assert!(store.data.accounts["issued"].tokens.is_none());
    server.abort();
}
