//! OIDC discovery: `{issuer}/.well-known/openid-configuration`.
//!
//! Only the three fields the authorization-code flow needs are read. The rest
//! of the document is ignored rather than modelled, so an issuer that adds a
//! field does not break login — the same forward-compatibility posture the op
//! envelope takes (`docs/10-cross-cutting/protocol-versioning.md`).

use std::sync::Arc;

use serde::Deserialize;

use crate::error::LoginError;
use crate::http::HttpClient;

/// The endpoints a login needs, as the issuer advertises them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderMetadata {
    /// The issuer's own canonical identifier, as it states it.
    pub issuer: String,
    /// Where the browser is sent.
    pub authorization_endpoint: String,
    /// Where the authorization code is exchanged.
    pub token_endpoint: String,
}

#[derive(Deserialize)]
struct RawMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
}

/// Strip a trailing slash so `https://idp.example` and `https://idp.example/`
/// compare equal. The server's verifier normalizes the same way, and the two
/// must agree or a token minted for a configured issuer would fail `iss`
/// checking on cosmetics.
fn normalize(issuer: &str) -> &str {
    issuer.trim_end_matches('/')
}

/// The discovery URL for an issuer.
#[must_use]
pub fn discovery_url(issuer: &str) -> String {
    format!("{}/.well-known/openid-configuration", normalize(issuer))
}

/// Fetch and validate the issuer's metadata.
///
/// # Errors
/// [`LoginError::Provider`] if the issuer is not `https://`, if the document
/// names a different issuer than the one configured, or if an endpoint it
/// advertises is not `https://`. [`LoginError::Transport`] or
/// [`LoginError::Malformed`] for a failed or unparseable fetch.
pub async fn discover(
    http: &Arc<dyn HttpClient>,
    issuer: &str,
) -> Result<ProviderMetadata, LoginError> {
    if !issuer.starts_with("https://") {
        return Err(LoginError::Provider(format!(
            "OIDC issuer must be https://, got {issuer}"
        )));
    }
    let url = discovery_url(issuer);
    let req = http::Request::builder()
        .method(http::Method::GET)
        .uri(&url)
        .header(http::header::ACCEPT, "application/json")
        .body(Vec::new())
        .map_err(|e| LoginError::Provider(format!("build discovery request: {e}")))?;

    let res = http.call(req).await?;
    if !res.status().is_success() {
        return Err(LoginError::Transport(format!(
            "discovery at {url} returned HTTP {}",
            res.status().as_u16()
        )));
    }
    let raw: RawMetadata = serde_json::from_slice(res.body())
        .map_err(|e| LoginError::Malformed(format!("discovery document: {e}")))?;

    // The issuer-mix-up defence, and the reason this is not just a struct
    // fetch. A document served from one issuer's well-known path that claims
    // to *be* another issuer is how a malicious or misconfigured provider
    // redirects a login at a token endpoint the operator never chose.
    if normalize(&raw.issuer) != normalize(issuer) {
        return Err(LoginError::Provider(format!(
            "discovery document at {url} claims issuer {:?}, which is not the configured issuer",
            raw.issuer
        )));
    }
    for (name, endpoint) in [
        ("authorization_endpoint", &raw.authorization_endpoint),
        ("token_endpoint", &raw.token_endpoint),
    ] {
        if !endpoint.starts_with("https://") {
            return Err(LoginError::Provider(format!(
                "issuer advertises a non-HTTPS {name}: {endpoint}"
            )));
        }
    }

    Ok(ProviderMetadata {
        issuer: raw.issuer,
        authorization_endpoint: raw.authorization_endpoint,
        token_endpoint: raw.token_endpoint,
    })
}
