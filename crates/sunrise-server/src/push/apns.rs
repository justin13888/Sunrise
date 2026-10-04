//! APNs over HTTP/2, authenticated with a provider token signed by a `.p8`
//! key.
//!
//! Every push is the same request: `POST /3/device/<token>` with
//! [`APNS_PAYLOAD`], `apns-push-type: background` and `apns-priority: 5` —
//! the `silent` tier of `docs/06-server/push-notifications.md`. The provider
//! token is an ES256 JWT (`kid` = key id, `iss` = team id, `iat`), reused for
//! forty minutes and re-signed early when APNs calls it expired.

use super::{PushError, PushIntent, PushPlatform, PushProvider};
use crate::config::ApnsConfig;
use crate::state::Clock;
use async_trait::async_trait;
use base64::Engine as _;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

/// The whole APNs body: wake the app in the background, say nothing.
pub const APNS_PAYLOAD: &str = r#"{"aps":{"content-available":1}}"#;

/// How long one provider token is reused. APNs refuses a token older than an
/// hour and throttles one refreshed more often than every twenty minutes.
const APNS_JWT_TTL_SECS: u64 = 40 * 60;

/// Largest APNs error body read. Real ones are a few dozen bytes of JSON.
const APNS_MAX_ERROR_BODY: usize = 4 * 1024;

type HttpsClient = hyper_util::client::legacy::Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    String,
>;

/// APNs over HTTP/2 with token-based (`.p8`) authentication.
pub struct ApnsProvider {
    client: HttpsClient,
    endpoint: String,
    topic: String,
    key_id: String,
    team_id: String,
    key: jsonwebtoken::EncodingKey,
    clock: Arc<dyn Clock>,
    /// The provider token in use and its `iat`, in seconds.
    jwt: Mutex<Option<(String, u64)>>,
}

impl std::fmt::Debug for ApnsProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApnsProvider")
            .field("endpoint", &self.endpoint)
            .field("topic", &self.topic)
            .field("key_id", &self.key_id)
            .field("team_id", &self.team_id)
            .finish_non_exhaustive()
    }
}

/// Why a push provider could not be built at startup.
#[derive(Debug, Error)]
pub enum PushSetupError {
    /// The key file could not be read.
    #[error("[push.apns] key_path {path}: cannot read: {cause}")]
    KeyUnreadable {
        /// The configured path.
        path: PathBuf,
        /// The I/O failure.
        cause: String,
    },
    /// The key file is readable by someone other than its owner.
    #[error(
        "[push.apns] key_path {path} has mode {mode:o}; it signs for the whole app, so it must \
         not be readable by group or others (chmod 600)"
    )]
    KeyPermissions {
        /// The configured path.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
    },
    /// The key file is not a P-256 PKCS#8 key.
    #[error("[push.apns] key_path {path} is not an APNs .p8 key: {cause}")]
    BadKey {
        /// The configured path.
        path: PathBuf,
        /// What was wrong with it.
        cause: String,
    },
}

/// Read a `.p8` key: refuse it unless only its owner can read it, then turn
/// its PEM into the PKCS#8 DER the signer takes.
fn read_p8(path: &Path) -> Result<Vec<u8>, PushSetupError> {
    let unreadable = |e: std::io::Error| PushSetupError::KeyUnreadable {
        path: path.to_owned(),
        cause: e.to_string(),
    };
    let meta = std::fs::metadata(path).map_err(unreadable)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(PushSetupError::KeyPermissions {
                path: path.to_owned(),
                mode,
            });
        }
    }
    #[cfg(not(unix))]
    let _ = meta;
    let pem = std::fs::read_to_string(path).map_err(unreadable)?;
    let bad = |cause: &str| PushSetupError::BadKey {
        path: path.to_owned(),
        cause: cause.to_owned(),
    };
    let body = pem
        .split("-----BEGIN PRIVATE KEY-----")
        .nth(1)
        .and_then(|rest| rest.split("-----END PRIVATE KEY-----").next())
        .ok_or_else(|| bad("no PRIVATE KEY block"))?;
    let b64: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|_| bad("the PRIVATE KEY block is not base64"))
}

/// The provider-token claims APNs reads.
#[derive(Serialize)]
struct ApnsClaims<'a> {
    iss: &'a str,
    iat: u64,
}

/// The JSON body APNs sends with every non-200.
#[derive(Deserialize)]
struct ApnsReason {
    reason: String,
}

/// Whether `token` can be an APNs device token: hex, and short enough to be
/// one. It is a path segment of the request, so nothing else may reach it.
fn is_apns_token(token: &str) -> bool {
    !token.is_empty() && token.len() <= 200 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// An APNs `reason`, kept to the characters Apple's vocabulary uses so a
/// hostile or broken peer cannot write arbitrary text into a log.
fn clean_reason(reason: &str) -> String {
    reason
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(64)
        .collect()
}

impl ApnsProvider {
    /// Build the provider `[push.apns]` describes.
    ///
    /// Reads the key file, refuses it when group or others can read it, and
    /// signs one provider token so a key that cannot sign is a startup refusal
    /// rather than the first push failing.
    pub fn from_config(cfg: &ApnsConfig, clock: Arc<dyn Clock>) -> Result<Self, PushSetupError> {
        let der = read_p8(&cfg.key_path)?;
        let provider = Self::build(
            &der,
            cfg,
            cfg.environment.endpoint().to_owned(),
            false,
            clock,
        );
        provider.mint().map_err(|e| PushSetupError::BadKey {
            path: cfg.key_path.clone(),
            cause: e.to_string(),
        })?;
        Ok(provider)
    }

    /// The provider against `endpoint`, which may be `http://` when
    /// `plaintext` is set. Tests point it at a local HTTP/2 server.
    fn build(
        der: &[u8],
        cfg: &ApnsConfig,
        endpoint: String,
        plaintext: bool,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let builder = hyper_rustls::HttpsConnectorBuilder::new().with_webpki_roots();
        let https = if plaintext {
            builder.https_or_http().enable_http2().build()
        } else {
            builder.https_only().enable_http2().build()
        };
        let client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .http2_only(true)
                .build(https);
        Self {
            client,
            endpoint,
            topic: cfg.topic.clone(),
            key_id: cfg.key_id.clone(),
            team_id: cfg.team_id.clone(),
            key: jsonwebtoken::EncodingKey::from_ec_der(der),
            clock,
            jwt: Mutex::new(None),
        }
    }

    /// Sign a fresh provider token.
    fn mint(&self) -> Result<(String, u64), PushError> {
        let iat = self.clock.now_ms() / 1000;
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
        header.kid = Some(self.key_id.clone());
        let claims = ApnsClaims {
            iss: &self.team_id,
            iat,
        };
        let jwt = jsonwebtoken::encode(&header, &claims, &self.key)
            .map_err(|e| PushError::Rejected(format!("cannot sign the provider token: {e}")))?;
        Ok((jwt, iat))
    }

    /// The provider token to send, re-signed once it is old enough.
    fn bearer(&self) -> Result<String, PushError> {
        let now = self.clock.now_ms() / 1000;
        let mut cached = self.jwt.lock();
        if let Some((jwt, iat)) = cached.as_ref() {
            if now >= *iat && now - iat < APNS_JWT_TTL_SECS {
                return Ok(jwt.clone());
            }
        }
        let fresh = self.mint()?;
        let jwt = fresh.0.clone();
        *cached = Some(fresh);
        Ok(jwt)
    }

    /// What a non-200 means for the dispatcher.
    fn classify(&self, status: u16, reason: &str) -> PushError {
        let reason = clean_reason(reason);
        match (status, reason.as_str()) {
            (410, _) | (400, "BadDeviceToken") => PushError::Unregistered(reason),
            // The token aged out or was revoked under us. Drop it, and let the
            // retry sign a new one.
            (403, "ExpiredProviderToken" | "InvalidProviderToken") => {
                *self.jwt.lock() = None;
                PushError::Unavailable(reason)
            }
            (429, _) => PushError::Throttled(reason),
            (500..=599, _) => PushError::Unavailable(format!("{status} {reason}")),
            _ => PushError::Rejected(format!("{status} {reason}")),
        }
    }
}

#[async_trait]
impl PushProvider for ApnsProvider {
    fn platform(&self) -> PushPlatform {
        PushPlatform::Apns
    }

    async fn send(&self, intent: &PushIntent) -> Result<(), PushError> {
        use http_body_util::BodyExt as _;

        let token = &intent.registration.token;
        // A token that cannot be one is as undeliverable as one APNs refused,
        // and it would otherwise be spliced into the request path.
        if !is_apns_token(token) {
            return Err(PushError::Unregistered("NotHex".to_owned()));
        }
        let request = hyper::Request::builder()
            .method(hyper::Method::POST)
            .uri(format!("{}/3/device/{token}", self.endpoint))
            .header(
                hyper::header::AUTHORIZATION,
                format!("bearer {}", self.bearer()?),
            )
            .header("apns-topic", &self.topic)
            .header("apns-push-type", "background")
            .header("apns-priority", "5")
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(APNS_PAYLOAD.to_owned())
            .map_err(|_| PushError::Rejected("the request could not be built".to_owned()))?;

        // `hyper_util`'s error names the failure kind, never the URI, so it
        // cannot carry the token out.
        let response = self
            .client
            .request(request)
            .await
            .map_err(|e| PushError::Unavailable(e.to_string()))?;
        let status = response.status().as_u16();
        if status == 200 {
            return Ok(());
        }
        let body = http_body_util::Limited::new(response.into_body(), APNS_MAX_ERROR_BODY)
            .collect()
            .await
            .map(http_body_util::Collected::to_bytes)
            .unwrap_or_default();
        let reason = serde_json::from_slice::<ApnsReason>(&body)
            .map(|r| r.reason)
            .unwrap_or_default();
        Err(self.classify(status, &reason))
    }
}
