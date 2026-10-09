//! Filing this device's push token with the relay.
//!
//! `POST /api/v1/devices/push-tokens` had no caller in the workspace: the relay
//! could deliver a content-less wake-up (`docs/06-server/push-notifications.md`)
//! to a token nobody ever uploaded. This is the upload, over the generated
//! client like every other route here, so the body shape is the description's
//! rather than a hand-written copy of it.
//!
//! # Signed, not merely authenticated
//!
//! The route takes a `Signed<PushRegistration>`: the relay checks the
//! ADR-0022 `header_sig_v2` binding over the RFC 8785 canonical form of the
//! body it parsed. So the binding here signs the *value* it sends, through
//! [`sunrise_http_sig::sign_with`] — the same encoder the relay's verifier
//! recomputes with — and never the octets `reqwest` happens to serialize.
//!
//! The signature is an operation the caller hands in rather than a key, for
//! the reason `sign_with` takes one: `D_S_priv` lives wrapped inside the vault
//! and never leaves it, so the vault signs on this function's behalf.

use crate::api::{self, Credential, RegisterPushTokenParams, SecretString};

/// The route this module calls, exactly as the relay sees it in the request
/// target the signature covers.
const PUSH_TOKENS_PATH: &str = "/api/v1/devices/push-tokens";

/// One push token, and the device it belongs to.
#[derive(Clone)]
pub struct PushTokenRegistration {
    /// The relay's id for this device: the ULID `POST /api/v1/devices`
    /// returned. It is both the body's `device_id` and the
    /// `X-Sunrise-Device` the request is signed as, and the relay refuses a
    /// body naming a device the caller's account does not own.
    pub relay_device_id: String,
    /// Which provider the token is for.
    pub platform: api::types::PushPlatform,
    /// The provider's token, as the provider spells it — lowercase hex of the
    /// APNs device token for Apple.
    pub token: String,
}

// Hand-written: a push token is an address that wakes this device, and there is
// no reason for one to reach a log line.
impl std::fmt::Debug for PushTokenRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushTokenRegistration")
            .field("relay_device_id", &self.relay_device_id)
            .field("platform", &self.platform)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Why a push token was not filed.
///
/// Two arms because a caller does different things with them: a request that
/// could not be built will not be built next time either, and a refusal or a
/// transport failure is retried on the next token delivery or sync start.
#[derive(Debug, thiserror::Error)]
pub enum PushTokenError {
    /// Nothing was sent: the origin, the device id or the token cannot form a
    /// request.
    #[error("push token request could not be built: {0}")]
    Request(String),
    /// The relay was asked and did not file the token.
    #[error("push token registration failed: {0}")]
    Relay(String),
}

/// File `registration` with the relay at `base_url`, signed as
/// `registration.relay_device_id`.
///
/// `base_url` is the relay origin, as [`crate::bootstrap`] takes it. `now_ms`
/// is the instant the signed `Date` header names; the relay refuses a date
/// outside its skew window, so it comes from the caller's injected clock like
/// every other signed request. `sign` turns the canonical string into the 64
/// raw Ed25519 bytes `D_S_priv` makes of it.
///
/// Idempotent at the relay: a second upload of the same token for the same
/// device replaces the row with itself, and a rotated token replaces the old
/// one. So a caller uploads whenever it is unsure rather than tracking whether
/// the relay already holds it.
///
/// # Errors
/// [`PushTokenError::Request`] for an empty device id or token, or an origin
/// that is not one; [`PushTokenError::Relay`] with what the generated client
/// reported otherwise.
pub async fn register_push_token(
    base_url: &str,
    bearer: &str,
    registration: PushTokenRegistration,
    now_ms: u64,
    sign: impl FnOnce(&[u8]) -> [u8; 64],
) -> Result<(), PushTokenError> {
    let device = registration.relay_device_id.trim();
    if device.is_empty() {
        // An empty `X-Sunrise-Device` names no row, and the relay answers that
        // as a bad bearer so a caller cannot enumerate devices — which would
        // make this undiagnosable from the client.
        return Err(PushTokenError::Request(
            "no relay device id: register this device first".to_owned(),
        ));
    }
    if registration.token.is_empty() {
        return Err(PushTokenError::Request("empty push token".to_owned()));
    }

    let body = api::types::PushRegistration {
        device_id: device.to_owned(),
        platform: registration.platform,
        token: registration.token,
    };
    let date = sunrise_http_sig::date_header(now_ms);
    let signature = sunrise_http_sig::sign_with(sign, "POST", PUSH_TOKENS_PATH, &date, Some(&body))
        .map_err(|e| PushTokenError::Request(e.to_string()))?;

    let client = api::Client::new(base_url)
        .map_err(|e| PushTokenError::Request(e.to_string()))?
        .with_credential(
            "AccountToken",
            Credential::Bearer(SecretString::from(bearer.to_owned())),
        );
    client
        .register_push_token(
            RegisterPushTokenParams::default()
                .x_sunrise_device(device.to_owned())
                .x_sunrise_device_sig(signature)
                .date(date),
            &body,
        )
        .await
        .map_err(|e| PushTokenError::Relay(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{register_push_token, PushTokenError, PushTokenRegistration};
    use crate::api::types::PushPlatform;

    fn registration(device: &str, token: &str) -> PushTokenRegistration {
        PushTokenRegistration {
            relay_device_id: device.to_owned(),
            platform: PushPlatform::Apns,
            token: token.to_owned(),
        }
    }

    /// Refused before anything is signed or sent: an empty id would put an
    /// `X-Sunrise-Device` on the wire naming no row.
    #[tokio::test]
    async fn an_empty_device_id_is_refused_before_the_request() {
        let refused = register_push_token(
            "http://127.0.0.1:1",
            "bearer",
            registration("  ", "abcd"),
            0,
            |_| panic!("nothing is signed for a request that cannot be sent"),
        )
        .await;
        assert!(
            matches!(refused, Err(PushTokenError::Request(_))),
            "{refused:?}"
        );
    }

    /// The relay refuses an empty token too, but it would replace a working
    /// token first if the check were only there.
    #[tokio::test]
    async fn an_empty_token_is_refused_before_the_request() {
        let refused = register_push_token(
            "http://127.0.0.1:1",
            "bearer",
            registration("01HZY3W9ZQ6YB2Z5VB9V3K9F5C", ""),
            0,
            |_| panic!("nothing is signed for a request that cannot be sent"),
        )
        .await;
        assert!(
            matches!(refused, Err(PushTokenError::Request(_))),
            "{refused:?}"
        );
    }

    /// The token never reaches a `Debug` line.
    #[test]
    fn debug_redacts_the_token() {
        let printed = format!("{:?}", registration("dev", "secret-token"));
        assert!(!printed.contains("secret-token"), "{printed}");
        assert!(printed.contains("dev"), "{printed}");
    }
}
