mod browser;
mod store;

use std::{
    io::{self, Write},
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use browser::Attempt;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use reqwest::{Client, redirect::Policy};
use serde::Deserialize;
use store::{Account, Store, Tokens, config_dir};
use tokio::net::TcpListener;

const ISSUER: &str = "https://auth.openai.com";
const AUTHORIZE: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN: &str = "https://auth.openai.com/api/accounts/oauth/token";
const JWKS: &str = "https://auth.openai.com/.well-known/jwks.json";
const RESOURCE: &str = "https://api.openai.com/v1";
const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";

#[derive(Debug)]
pub struct AuthenticationRequired(&'static str);

impl std::fmt::Display for AuthenticationRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for AuthenticationRequired {}

fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn client() -> Result<Client> {
    Ok(Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?)
}

pub async fn login() -> Result<()> {
    let dir = config_dir()?;
    let store = Store::lock(&dir).await?;
    store.save()?;
    let host = store.data.host_id.clone();
    let accounts = store.data.accounts.clone();
    let active = store.data.active.clone();
    drop(store);
    let selected = tokio::task::spawn_blocking(move || {
        if accounts.is_empty() {
            return Ok(None);
        }
        eprintln!("Choose a ChatGPT registration:");
        let mut choices = Vec::new();
        for (id, account) in accounts {
            let label: String = account
                .email
                .as_deref()
                .unwrap_or("ChatGPT")
                .chars()
                .filter(|c| !c.is_control())
                .collect();
            let current = if active.as_ref() == Some(&id) {
                " (active)"
            } else {
                ""
            };
            let registration: String = id.chars().filter(|c| !c.is_control()).collect();
            eprintln!("  {}. {label} [{registration}]{current}", choices.len() + 1);
            choices.push((id, account));
        }
        eprint!("Select a number, or enter 0 for another account: ");
        io::stderr().flush()?;
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        let index: usize = line.trim().parse().context("Enter a registration number")?;
        ensure!(index <= choices.len(), "Invalid registration number");
        Ok::<_, anyhow::Error>(index.checked_sub(1).map(|i| choices.swap_remove(i)))
    })
    .await??;
    authorize(&dir, &host, selected.as_ref(), TOKEN, JWKS, |url| {
        eprintln!("Continue with ChatGPT in your browser.");
        open::that_detached(url.as_str())
            .map_err(|_| anyhow::anyhow!("Cannot open the browser for ChatGPT sign-in"))
    })
    .await?;
    eprintln!("ChatGPT sign-in complete.");
    Ok(())
}

async fn authorize(
    dir: &Path,
    host: &str,
    selected: Option<&(String, Account)>,
    token_endpoint: &str,
    keys_endpoint: &str,
    open: impl Fn(&url::Url) -> Result<()>,
) -> Result<()> {
    let client = client()?;
    let mut client_id = selected.map(|(id, _)| id.clone());
    let mut retried = false;
    let (id, reply, nonce) = loop {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let attempt = Attempt::new(listener.local_addr()?.port());
        let url = attempt.authorize_url(host, client_id.as_deref(), selected.map(|(_, a)| a))?;
        open(&url)?;
        let callback = attempt.callback(listener).await?;
        let (id, code) = callback.grant(client_id.as_deref())?;
        match exchange(
            &client,
            token_endpoint,
            &[
                ("grant_type", "authorization_code"),
                ("client_id", &id),
                ("code", &code),
                ("redirect_uri", &attempt.redirect),
                ("code_verifier", &attempt.verifier),
                ("resource", RESOURCE),
            ],
        )
        .await
        {
            Ok(reply) => break (id, reply, attempt.nonce),
            Err(error) if !retried && error.is::<AuthenticationRequired>() => {
                client_id = Some(id);
                retried = true;
            }
            Err(error) => return Err(error),
        }
    };
    let keys: JwkSet = client
        .get(keys_endpoint)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let account = registration(reply, &keys, &id, &nonce)?;
    let mut store = Store::lock(dir).await?;
    if let Some(previous) = store.data.accounts.get(&id) {
        ensure!(
            account.subject == previous.subject,
            "ChatGPT account changed during sign-in"
        );
    }
    let plan = account
        .tokens
        .as_ref()
        .context("Missing sign-in credentials")?
        .check_plan();
    store.data.accounts.insert(id.clone(), account);
    store.data.active = Some(id);
    store.save()?;
    plan
}

#[derive(Deserialize)]
struct Reply {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    id_token: Option<String>,
    token_type: String,
    expires_in: u64,
    scope: Option<String>,
}

impl Reply {
    fn tokens(self, id: String, scope: String) -> Result<Tokens> {
        ensure!(
            self.token_type.eq_ignore_ascii_case("Bearer")
                && !self.access_token.is_empty()
                && (!scope.split_ascii_whitespace().any(|s| s == PLAN_SCOPE)
                    || !self.refresh_token.is_empty())
                && self.expires_in > 0,
            "Invalid ChatGPT token response"
        );
        Ok(Tokens {
            access: self.access_token,
            refresh: self.refresh_token,
            id,
            scope,
            expires_at: now()?
                .checked_add(self.expires_in)
                .context("Invalid token expiration")?,
        })
    }
}

impl Tokens {
    fn check_plan(&self) -> Result<()> {
        if self.scope.split_ascii_whitespace().any(|s| s == PLAN_SCOPE) {
            return Ok(());
        }
        Err(AuthenticationRequired("ChatGPT plan access was not granted. Run elphie login.").into())
    }
}

async fn exchange(client: &Client, endpoint: &str, form: &[(&str, &str)]) -> Result<Reply> {
    let response = client.post(endpoint).form(form).send().await?;
    if !response.status().is_success() {
        let status = response.status();
        if status.is_client_error()
            && let Ok(rejection) = response.json::<serde_json::Value>().await
        {
            let error = &rejection["error"];
            let code = error.as_str().or_else(|| error["code"].as_str());
            if matches!(
                code,
                Some(
                    "invalid_grant"
                        | "invalid_refresh_token"
                        | "token_expired"
                        | "refresh_token_expired"
                        | "refresh_token_invalidated"
                        | "refresh_token_reused"
                )
            ) {
                return Err(AuthenticationRequired(
                    "ChatGPT authorization expired. Run elphie login.",
                )
                .into());
            }
        }
        bail!("ChatGPT token exchange returned HTTP {status}");
    }
    response
        .json()
        .await
        .context("Invalid ChatGPT token response")
}

#[derive(Deserialize)]
struct Identity {
    sub: String,
    nonce: String,
    iat: u64,
    email: Option<String>,
    azp: Option<String>,
}

fn registration(reply: Reply, keys: &JwkSet, id: &str, nonce: &str) -> Result<Account> {
    let jwt = reply
        .id_token
        .as_deref()
        .context("Missing ChatGPT ID token")?;
    let header = decode_header(jwt)?;
    ensure!(
        header.alg == Algorithm::RS256,
        "Unsupported ChatGPT identity signature"
    );
    let key = keys
        .find(&header.kid.context("Missing ChatGPT signing key ID")?)
        .context("Unknown ChatGPT signing key")?;
    let mut checks = Validation::new(Algorithm::RS256);
    checks.leeway = 5;
    checks.set_issuer(&[ISSUER]);
    checks.set_audience(&[id]);
    checks.set_required_spec_claims(&["exp", "aud", "iss", "sub"]);
    checks.validate_nbf = true;
    let identity = decode::<Identity>(jwt, &DecodingKey::from_jwk(key)?, &checks)?.claims;
    ensure!(
        identity.nonce == nonce
            && !identity.sub.trim().is_empty()
            && identity.iat <= now()? + 5
            && identity.azp.as_deref().is_none_or(|azp| azp == id),
        "Invalid ChatGPT identity"
    );
    let jwt = jwt.to_owned();
    let scope = reply.scope.clone().context("Missing ChatGPT OAuth grant")?;
    Ok(Account {
        subject: identity.sub,
        email: identity.email,
        tokens: Some(reply.tokens(jwt, scope)?),
    })
}

pub async fn access_token() -> Result<String> {
    let dir = config_dir()?;
    // Finish rotating credentials even when the caller cancels the model request.
    tokio::spawn(async move { token_for(&dir, TOKEN).await }).await?
}

async fn token_for(dir: &Path, token_endpoint: &str) -> Result<String> {
    let mut store = Store::lock(dir).await?;
    let id = store.data.active.as_ref().ok_or(AuthenticationRequired(
        "Continue with ChatGPT using elphie login",
    ))?;
    let account = store
        .data
        .accounts
        .get_mut(id)
        .ok_or(AuthenticationRequired(
            "Missing ChatGPT registration. Run elphie login.",
        ))?;
    let tokens = account.tokens.as_mut().ok_or(AuthenticationRequired(
        "Continue with ChatGPT using elphie login",
    ))?;
    tokens.check_plan()?;
    if tokens.expires_at > now()? + 60 {
        return Ok(tokens.access.clone());
    }
    let reply = match exchange(
        &client()?,
        token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", id),
            ("refresh_token", &tokens.refresh),
            ("resource", RESOURCE),
        ],
    )
    .await
    {
        Err(error) if error.is::<AuthenticationRequired>() => {
            account.tokens = None;
            store.save()?;
            return Err(error);
        }
        other => other?,
    };
    let scope = reply.scope.clone().unwrap_or_else(|| tokens.scope.clone());
    *tokens = reply.tokens(tokens.id.clone(), scope)?;
    let token = tokens.check_plan().map(|()| tokens.access.clone());
    store.save()?;
    token
}

pub async fn logout() -> Result<()> {
    sign_out(&config_dir()?, ISSUER).await
}

async fn sign_out(dir: &Path, issuer: &str) -> Result<()> {
    let mut store = Store::lock(dir).await?;
    let Some(id) = store.data.active.take() else {
        return Ok(());
    };
    let account = store
        .data
        .accounts
        .get_mut(&id)
        .context("Missing active ChatGPT registration")?;
    let Some(tokens) = account.tokens.take() else {
        return store.save();
    };
    let confirmed = tokens.refresh.is_empty() || revoke(issuer, &id, &tokens.refresh).await.is_ok();
    store.save()?;
    ensure!(
        confirmed,
        "Signed out locally. Remote revocation was not confirmed. Disconnect Elphie in ChatGPT Settings."
    );
    Ok(())
}

async fn revoke(issuer: &str, id: &str, refresh: &str) -> Result<()> {
    #[derive(Deserialize)]
    struct Configuration {
        issuer: String,
        revocation_endpoint: String,
    }
    let http = client()?;
    let config: Configuration = http
        .get(format!("{issuer}/.well-known/openid-configuration"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let url = url::Url::parse(&config.revocation_endpoint)?;
    ensure!(
        config.issuer == issuer && url.origin() == url::Url::parse(issuer)?.origin(),
        "Unexpected ChatGPT revocation issuer"
    );
    for retry in 0..3 {
        let response = http
            .post(url.clone())
            .form(&[
                ("token", refresh),
                ("token_type_hint", "refresh_token"),
                ("client_id", id),
            ])
            .send()
            .await;
        match response {
            Ok(response) if response.status() == reqwest::StatusCode::OK => return Ok(()),
            Ok(response) if !response.status().is_server_error() => break,
            _ if retry < 2 => tokio::time::sleep(Duration::from_millis(250 << retry)).await,
            _ => break,
        }
    }
    bail!("ChatGPT revocation failed")
}

#[cfg(test)]
mod tests;
