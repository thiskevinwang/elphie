#![doc = include_str!("../README.md")]

use anyhow::{Result, ensure};
pub use rmcp::transport::auth::AuthorizationManager;
use rmcp::transport::auth::{
    AuthorizationMetadataSource, AuthorizationRequest, AuthorizationSession,
};
use url::Host;
pub use url::Url;

pub enum ClientIdentity {
    Dynamic,
    MetadataDocument(Url),
    Preregistered { issuer: String, client_id: String },
}

pub struct PendingAuthorization {
    session: AuthorizationSession,
    redirect: Url,
    resource: Url,
}

/// A completed authorization and the exact MCP endpoint the host selected.
pub struct AuthorizedResource {
    resource: Url,
    manager: AuthorizationManager,
}

impl AuthorizedResource {
    pub fn into_parts(self) -> (Url, AuthorizationManager) {
        (self.resource, self.manager)
    }
}

impl PendingAuthorization {
    pub async fn start(resource: Url, redirect: Url, identity: ClientIdentity) -> Result<Self> {
        ensure!(
            is_valid_endpoint(&resource, true),
            "MCP resource requires HTTPS or loopback HTTP"
        );
        ensure!(
            redirect.scheme() == "http"
                && is_loopback(&redirect)
                && has_plain_authority(&redirect)
                && redirect.query().is_none(),
            "use a loopback HTTP callback with no query or fragment"
        );
        let mut request = AuthorizationRequest::new(redirect.as_str())
            .with_client_name("Elphie")
            .with_application_type("native")
            .with_initial_scope_selection();
        let expected_issuer = match identity {
            ClientIdentity::Dynamic => None,
            ClientIdentity::MetadataDocument(url) => {
                ensure!(
                    url.scheme() == "https" && has_plain_authority(&url) && url.path().len() > 1,
                    "client metadata requires an HTTPS URL with a document path"
                );
                request = request.with_client_metadata_url(url);
                None
            }
            ClientIdentity::Preregistered { issuer, client_id } => {
                ensure!(!client_id.trim().is_empty(), "client ID is empty");
                request = request.with_preregistered_client(client_id);
                Some(issuer)
            }
        };

        let allow_loopback = is_loopback(&resource);
        let mut manager = AuthorizationManager::new(resource.clone()).await?;
        let resolution = manager.resolve_metadata().await?;
        ensure!(
            matches!(
                resolution.source,
                AuthorizationMetadataSource::ProtectedResourceMetadata
            ),
            "MCP server must publish protected resource metadata"
        );
        let metadata = resolution.metadata;
        for endpoint in [
            Some(metadata.authorization_endpoint.as_str()),
            Some(metadata.token_endpoint.as_str()),
            metadata.registration_endpoint.as_deref(),
            metadata.issuer.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            ensure!(
                is_valid_endpoint(&Url::parse(endpoint)?, allow_loopback),
                "unsafe OAuth endpoint"
            );
        }
        if let Some(issuer) = expected_issuer {
            ensure!(
                metadata.issuer.as_deref() == Some(issuer.as_str()),
                "registered client issuer mismatch"
            );
        }
        ensure!(
            metadata
                .code_challenge_methods_supported
                .as_ref()
                .is_some_and(|m| m.iter().any(|s| s == "S256")),
            "authorization server must advertise S256 PKCE"
        );
        manager.set_metadata(metadata);
        let session = AuthorizationSession::new(manager, request)
            .await
            .map_err(|(_, error)| error)?;
        Ok(Self {
            session,
            redirect,
            resource,
        })
    }

    pub fn authorization_url(&self) -> &str {
        self.session.get_authorization_url()
    }

    pub async fn finish(self, callback: Url) -> Result<AuthorizationManager> {
        Ok(self.finish_resource(callback).await?.manager)
    }

    pub async fn finish_resource(self, callback: Url) -> Result<AuthorizedResource> {
        let mut destination = callback.clone();
        destination.set_query(None);
        ensure!(
            destination == self.redirect,
            "unexpected callback destination"
        );
        let mut seen = std::collections::HashSet::new();
        for (key, value) in callback.query_pairs() {
            if matches!(key.as_ref(), "code" | "state" | "iss" | "error") {
                ensure!(
                    !value.is_empty() && seen.insert(key.clone()),
                    "invalid callback parameter"
                );
            }
        }
        ensure!(!seen.contains("error"), "authorization declined or failed");
        self.session.handle_callback_url(callback.as_str()).await?;
        Ok(AuthorizedResource {
            resource: self.resource,
            manager: self.session.auth_manager,
        })
    }
}

fn is_valid_endpoint(url: &Url, allow_loopback: bool) -> bool {
    has_plain_authority(url)
        && match url.scheme() {
            "https" => allow_loopback || !is_loopback(url),
            "http" => allow_loopback && is_loopback(url),
            _ => false,
        }
}

fn has_plain_authority(url: &Url) -> bool {
    url.has_host()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    }
}
