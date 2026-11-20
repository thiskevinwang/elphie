# MCP sign-in

A terminal or desktop host opens the authorization URL in the user's browser and receives the redirect on a loopback HTTP listener. Open the listener before starting sign-in. Pass the absolute callback URL to `finish`. The returned rmcp manager holds the credentials in memory and handles token refresh.

```rust,no_run
use elphie_oauth::{AuthorizationManager, ClientIdentity, PendingAuthorization, Url};

async fn sign_in(
    resource: Url,
    redirect: Url,
    metadata_url: Url,
    open_browser: impl FnOnce(&str) -> anyhow::Result<()>,
    receive_callback: impl Future<Output = anyhow::Result<Url>>,
) -> anyhow::Result<AuthorizationManager> {
    let pending = PendingAuthorization::start(
        resource, redirect, ClientIdentity::MetadataDocument(metadata_url),
    ).await?;
    open_browser(pending.authorization_url())?;
    pending.finish(receive_callback.await?).await
}
```

For a Client ID Metadata Document (CIMD), host your JSON at an HTTPS URL. Set `client_id` to that exact URL and include the loopback callback in `redirect_uris`. Example document:

```json
{
  "client_id": "https://example.com/elphie.json",
  "client_name": "Elphie",
  "logo_uri": "https://example.com/elphie.webp",
  "redirect_uris": ["http://127.0.0.1:8765/callback"],
  "token_endpoint_auth_method": "none"
}
```

To show Elphie’s icon during authorization, host the [Elphie image](../elphie-acp/assets/elphie.webp) with your document and set `logo_uri` to its HTTPS URL.

The resource must publish MCP protected resource metadata. Its authorization server must advertise S256 PKCE. If it supports CIMD, rmcp sends the document URL as the client ID. Otherwise rmcp attempts dynamic registration. Use `ClientIdentity::Dynamic` to start without a document. For an existing public client, pass `ClientIdentity::Preregistered { issuer, client_id }`.

This flow authorizes access to the selected MCP resource. ChatGPT sign-in belongs to the model provider. [MCP client registration](https://modelcontextprotocol.io/specification/draft/basic/authorization/client-registration)
