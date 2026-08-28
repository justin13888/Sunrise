//! Client-side OIDC login for Sunrise (issue #7).
//!
//! OIDC is the **only** auth surface (`docs/00-product/non-goals.md`): no
//! passwords, no Sunrise-issued tokens, no refresh-token rotation of our own.
//! This crate is the relying-party half that was missing entirely — the server
//! could verify a bearer, and nothing in the tree could obtain one.
//!
//! # Shape
//!
//! ```text
//!   discover()      GET {issuer}/.well-known/openid-configuration
//!        │          … checked: https, and the document must claim the
//!        │            issuer that was configured
//!        ▼
//!   begin_login()   bind 127.0.0.1:0, mint PKCE + state,
//!        │          build the authorize URL
//!        ▼
//!   open_in_browser()          user authenticates at the issuer
//!        ▼
//!   wait_for_redirect()        state compared BEFORE the code is read
//!        ▼
//!   exchange()      POST {token_endpoint} with the PKCE verifier
//!        ▼
//!   Credentials     access token + refresh token + a renew-at deadline
//! ```
//!
//! Renewal is [`OidcClient::refresh`], driven from
//! [`Credentials::renew_at_ms`] — 75% of the token's lifetime, per
//! `docs/06-server/auth.md`. The relay's `0x12 RefreshToken` frame then carries
//! the new access token into a *live* session, so a renewal costs no reconnect.
//!
//! # What is deliberately not here
//!
//! - **ID-token signature validation.** The access token is what the relay
//!   verifies, and it verifies it properly against the issuer's JWKS
//!   (`sunrise-server::auth::oidc`). The ID token is received directly from the
//!   token endpoint over TLS in this flow, which OIDC Core §3.1.3.7 explicitly
//!   allows to stand without re-validating the signature. Pulling in an
//!   RSA/JWT stack client-side to re-check a token nobody downstream reads
//!   would be cost without a defence.
//! - **Credential storage.** Where a token is persisted is a platform question
//!   — Keychain on macOS, a file mode 0600 for the CLI — and belongs to the
//!   client, not here. [`Credentials`] is `Serialize` so a client can persist
//!   it, and redacts under `Debug` so it does not leak on the way past.
//! - **Device registration.** [`login::DEVICE_ID_PARAM`] asks the issuer to
//!   mint the device claim; `POST /api/v1/devices` and the
//!   `X-Sunrise-Device-Sig` header are the relay's REST surface and live with
//!   the client that owns the device key.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions
)]

pub mod credentials;
pub mod discovery;
pub mod error;
pub mod http;
pub mod login;

pub use credentials::{Credentials, RENEW_AT_FRACTION};
pub use discovery::{discover, discovery_url, ProviderMetadata};
pub use error::LoginError;
pub use http::{HttpClient, HttpsClient};
pub use login::{
    open_in_browser, LoginSession, OidcClient, RedirectCapture, DEFAULT_REDIRECT_TIMEOUT,
    DEVICE_ID_CLAIM, DEVICE_ID_PARAM,
};
