use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use axum::{Router, extract::Query, http::StatusCode, routing::get};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{RngCore, rngs::OsRng};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{net::TcpListener, sync::oneshot};
use url::Url;

use crate::{AUTHORIZE, DYNAMIC_CLIENT, RESOURCE, SCOPE, store::Account};

pub(crate) struct Attempt {
    pub verifier: String,
    pub nonce: String,
    state: String,
    pub redirect: String,
}

fn secret() -> String {
    let mut bytes = [0; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

impl Attempt {
    pub fn new(port: u16) -> Self {
        Self {
            verifier: secret(),
            nonce: secret(),
            state: secret(),
            redirect: format!("http://127.0.0.1:{port}/auth/callback"),
        }
    }

    pub fn authorize_url(
        &self,
        host: &str,
        id: Option<&str>,
        account: Option<&Account>,
    ) -> Result<Url> {
        let mut url = Url::parse(AUTHORIZE)?;
        let mut query = url.query_pairs_mut();
        query.extend_pairs([
            ("response_type", "code"),
            ("redirect_uri", &self.redirect),
            ("scope", SCOPE),
            ("state", &self.state),
            ("nonce", &self.nonce),
            ("resource", RESOURCE),
            ("ext_agent_host_id", host),
            ("code_challenge_method", "S256"),
            (
                "code_challenge",
                &URL_SAFE_NO_PAD.encode(Sha256::digest(self.verifier.as_bytes())),
            ),
        ]);
        match id {
            Some(id) => {
                query.append_pair("client_id", id);
                if let Some(tokens) = account.and_then(|a| a.tokens.as_ref()) {
                    if tokens.check_plan().is_err() {
                        query.append_pair("prompt", "consent");
                    }
                    query.append_pair("id_token_hint", &tokens.id);
                }
            }
            None => {
                query
                    .append_pair("client_id", DYNAMIC_CLIENT)
                    .append_pair("agent_name_hint", "Elphie");
            }
        }
        drop(query);
        Ok(url)
    }

    pub async fn callback(&self, listener: TcpListener) -> Result<Callback> {
        let expected = self.state.clone();
        let (tx, rx) = oneshot::channel();
        let result = Arc::new(Mutex::new(None));
        let callback = result.clone();
        let stop = Arc::new(Mutex::new(Some(tx)));
        let app = Router::new().route(
            "/auth/callback",
            get(move |Query(value): Query<Callback>| {
                let trusted = value.state == expected;
                let callback = callback.clone();
                let stop = stop.clone();
                async move {
                    if !trusted {
                        return (StatusCode::BAD_REQUEST, "Invalid sign-in state.");
                    }
                    if let Some(tx) = stop.lock().unwrap().take() {
                        *callback.lock().unwrap() = Some(value);
                        let _ = tx.send(());
                    }
                    (StatusCode::OK, "Return to Elphie to finish sign-in.")
                }
            }),
        );
        tokio::time::timeout(Duration::from_secs(300), async {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
        })
        .await
        .context("ChatGPT sign-in timed out")??;
        result
            .lock()
            .unwrap()
            .take()
            .context("No sign-in callback received")
    }
}

#[derive(Deserialize)]
pub(crate) struct Callback {
    pub state: String,
    code: Option<String>,
    error: Option<String>,
    client_id: Option<String>,
}

impl Callback {
    pub fn grant(self, returning: Option<&str>) -> Result<(String, String)> {
        if self.error.is_some() {
            bail!("ChatGPT authorization was declined or failed");
        }
        let id = match (returning, self.client_id) {
            (Some(id), Some(returned)) => {
                ensure!(
                    id == returned,
                    "ChatGPT registration changed during sign-in"
                );
                returned
            }
            (Some(id), None) => id.to_owned(),
            (None, id) => id.context("ChatGPT did not return an issued client ID")?,
        };
        ensure!(
            !id.is_empty() && id != DYNAMIC_CLIENT,
            "Invalid ChatGPT registration"
        );
        let code = self
            .code
            .filter(|c| !c.is_empty())
            .context("Missing ChatGPT authorization code")?;
        Ok((id, code))
    }
}
