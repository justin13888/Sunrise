//! Relay viewer grants: register, claim, list and withdraw one (ADR-0074 §1).
//!
//! The four routes under `/api/v1/sync/grants` had no caller: an owner could
//! not register a grant on its Stream, and a recipient could not claim one.
//! These are the calls, over the generated client like every other route here.
//!
//! # What a grant is, and is not
//!
//! The relay's routing record only: which other account it hands one
//! Stream's ciphertext to. The Stream key reaches the recipient sealed to its
//! identity, in the `sunrise://share/<token>` link beside the claim token, and
//! the relay sees neither (`docs/03-crypto/sharing-with-others.md` §Granting
//! access, step 4). Nothing here touches a key.
//!
//! # Signed, every one
//!
//! All four routes take the ADR-0022 device binding: the two posts sign the
//! RFC 8785 canonical form of the body they send, through
//! [`sunrise_http_sig::sign_with`], and the list and the withdrawal sign an
//! empty body over their own request target. The signature is an operation the
//! caller hands in, for the reason [`crate::register_push_token`] gives:
//! `D_S_priv` never leaves the vault.
//!
//! # The claim token is a credential
//!
//! Whoever presents it first, other than the owner, is bound as the
//! recipient. So it is taken as bytes, encoded once on the way out, and never
//! reaches a `Debug` line or an error's text.

use crate::api;

/// The collection route, exactly as the relay sees it in the request target
/// the signature covers.
const GRANTS_PATH: &str = "/api/v1/sync/grants";

/// The claim route.
const CLAIM_PATH: &str = "/api/v1/sync/grants/claim";

/// Who is calling: the relay origin, the account's bearer and the relay's id
/// for this device, which every request here is signed as.
#[derive(Clone)]
pub struct GrantCaller<'a> {
    /// The relay origin, as [`crate::bootstrap()`] takes it.
    pub base_url: &'a str,
    /// The account's bearer token.
    pub bearer: &'a str,
    /// The ULID `POST /api/v1/devices` returned for this device: the
    /// `X-Sunrise-Device` each request is signed as.
    pub relay_device_id: &'a str,
}

// Hand-written: the bearer is a credential.
impl std::fmt::Debug for GrantCaller<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrantCaller")
            .field("base_url", &self.base_url)
            .field("bearer", &"<redacted>")
            .field("relay_device_id", &self.relay_device_id)
            .finish()
    }
}

/// One viewer grant, as either party sees it. The other party's account is
/// never named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewerGrant {
    /// The relay's id for the grant: what a subscription names.
    pub grant_id: [u8; 16],
    /// The owner's Stream the grant reads.
    pub stream_id: [u8; 16],
    /// When the relay stops serving it, ms since the epoch; `None` for never.
    pub expires_at_ms: Option<u64>,
    /// When the owner registered it, by the relay's clock.
    pub created_ms: u64,
    /// When it was claimed, by the relay's clock; `None` while unclaimed.
    pub claimed_ms: Option<u64>,
}

/// `GET /api/v1/sync/grants`: the grants on the caller's own Streams, and the
/// ones it holds on other accounts' Streams, each oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewerGrants {
    /// The grants this account registered.
    pub owned: Vec<ViewerGrant>,
    /// The grants this account claimed.
    pub held: Vec<ViewerGrant>,
}

/// Why a grant call did not do what it asked.
///
/// One arm per thing a caller does differently: a request that cannot be
/// built will not be built next time; a grant the relay does not hold for
/// this account is gone, or was never this account's to claim; a conflict is
/// a fresh token or a withdrawal away; anything else may pass on a retry.
#[derive(Debug, thiserror::Error)]
pub enum GrantError {
    /// Nothing was sent: the origin or the device id cannot form a request.
    #[error("viewer grant request could not be built: {0}")]
    Request(String),
    /// `404 RELAY_GRANT_REVOKED`: no grant under that token or id for this
    /// account. A claim of a token nobody registered, of one another account
    /// claimed first, of the owner's own, and of an expired grant all answer
    /// this, by design: a claim says nothing about which tokens exist.
    #[error("the relay holds no viewer grant under that token or id for this account")]
    NotHeld,
    /// `409`: the claim token is registered to another grant, or the owner
    /// already holds the most grants the relay allows.
    #[error("the relay refused the viewer grant: {0}")]
    Conflict(String),
    /// A grant the relay answered with does not parse.
    #[error("the relay answered a malformed viewer grant: {0}")]
    Malformed(&'static str),
    /// Any other refusal or a transport failure, in the generated client's
    /// words.
    #[error("viewer grant call failed: {0}")]
    Relay(String),
}

/// Register a viewer grant on `stream_id`, one of the caller's Streams, under
/// `claim` — 32 bytes the caller drew at random — until `expires_at_ms`, or
/// for good when it is `None`.
///
/// Safe to repeat: the relay answers the grant it already made for the same
/// token, Stream and expiry, so a caller whose response was lost retries with
/// the same token.
///
/// `now_ms` is the instant the signed `Date` header names, from the caller's
/// injected clock; `sign` turns the canonical string into the 64 raw Ed25519
/// bytes `D_S_priv` makes of it.
///
/// # Errors
/// [`GrantError::Request`] for a device id or origin that cannot form a
/// request; [`GrantError::Conflict`] for a token another grant holds, or an
/// owner at its limit; [`GrantError::Relay`] otherwise.
pub async fn create_viewer_grant(
    caller: &GrantCaller<'_>,
    stream_id: [u8; 16],
    claim: &[u8; 32],
    expires_at_ms: Option<u64>,
    now_ms: u64,
    sign: impl FnOnce(&[u8]) -> [u8; 64],
) -> Result<ViewerGrant, GrantError> {
    let device = device_of(caller)?;
    // The description types the field as a signed integer; an expiry past
    // its range is no expiry the relay could store.
    let expires_at_ms = expires_at_ms
        .map(i64::try_from)
        .transpose()
        .map_err(|_| GrantError::Request("expiry out of range".to_owned()))?;
    let body = api::types::CreateGrantRequest {
        stream_id: lower_hex(&stream_id),
        claim: lower_hex(claim),
        expires_at_ms,
    };
    let date = sunrise_http_sig::date_header(now_ms);
    let signature = sunrise_http_sig::sign_with(sign, "POST", GRANTS_PATH, &date, Some(&body))
        .map_err(|e| GrantError::Request(e.to_string()))?;
    let view = client(caller)?
        .create_viewer_grant(
            api::CreateViewerGrantParams::default()
                .x_sunrise_device(device.to_owned())
                .x_sunrise_device_sig(signature)
                .date(date),
            &body,
        )
        .await
        .map_err(|e| refusal(&e))?
        .into_inner();
    grant_of(&view)
}

/// Claim the viewer grant registered under `claim`, as the caller's account.
///
/// The first account other than the owner to present the token is bound as
/// the recipient, and is answered the same grant on every later claim, so a
/// recipient whose response was lost claims again.
///
/// # Errors
/// [`GrantError::NotHeld`] for a token the relay will not bind to this
/// account, whichever reason it was; the rest as
/// [`create_viewer_grant`].
pub async fn claim_viewer_grant(
    caller: &GrantCaller<'_>,
    claim: &[u8; 32],
    now_ms: u64,
    sign: impl FnOnce(&[u8]) -> [u8; 64],
) -> Result<ViewerGrant, GrantError> {
    let device = device_of(caller)?;
    let body = api::types::ClaimGrantRequest {
        claim: lower_hex(claim),
    };
    let date = sunrise_http_sig::date_header(now_ms);
    let signature = sunrise_http_sig::sign_with(sign, "POST", CLAIM_PATH, &date, Some(&body))
        .map_err(|e| GrantError::Request(e.to_string()))?;
    let view = client(caller)?
        .claim_viewer_grant(
            api::ClaimViewerGrantParams::default()
                .x_sunrise_device(device.to_owned())
                .x_sunrise_device_sig(signature)
                .date(date),
            &body,
        )
        .await
        .map_err(|e| refusal(&e))?
        .into_inner();
    grant_of(&view)
}

/// The viewer grants the caller's account owns and the ones it holds.
///
/// # Errors
/// As [`create_viewer_grant`], less the conflict.
pub async fn list_viewer_grants(
    caller: &GrantCaller<'_>,
    now_ms: u64,
    sign: impl FnOnce(&[u8]) -> [u8; 64],
) -> Result<ViewerGrants, GrantError> {
    let device = device_of(caller)?;
    let date = sunrise_http_sig::date_header(now_ms);
    let signature = sunrise_http_sig::sign_with(sign, "GET", GRANTS_PATH, &date, None::<&()>)
        .map_err(|e| GrantError::Request(e.to_string()))?;
    let list = client(caller)?
        .list_viewer_grants(
            api::ListViewerGrantsParams::default()
                .x_sunrise_device(device.to_owned())
                .x_sunrise_device_sig(signature)
                .date(date),
        )
        .await
        .map_err(|e| refusal(&e))?
        .into_inner();
    Ok(ViewerGrants {
        owned: list.owned.iter().map(grant_of).collect::<Result<_, _>>()?,
        held: list.held.iter().map(grant_of).collect::<Result<_, _>>()?,
    })
}

/// Withdraw the viewer grant `grant_id`: by its owner, or by its recipient
/// declining it.
///
/// A stream reading through it ends on the relay's next re-check. That stops
/// the relay serving the channel; what stops a revoked recipient reading what
/// comes next is the owner's key rotation
/// (`docs/03-crypto/sharing-with-others.md` §Revoking access, step 3).
///
/// # Errors
/// [`GrantError::NotHeld`] for a grant this account is not party to, or one
/// already withdrawn; the rest as [`create_viewer_grant`].
pub async fn withdraw_viewer_grant(
    caller: &GrantCaller<'_>,
    grant_id: [u8; 16],
    now_ms: u64,
    sign: impl FnOnce(&[u8]) -> [u8; 64],
) -> Result<(), GrantError> {
    let device = device_of(caller)?;
    let id = lower_hex(&grant_id);
    let path = format!("{GRANTS_PATH}/{id}");
    let date = sunrise_http_sig::date_header(now_ms);
    let signature = sunrise_http_sig::sign_with(sign, "DELETE", &path, &date, None::<&()>)
        .map_err(|e| GrantError::Request(e.to_string()))?;
    client(caller)?
        .withdraw_viewer_grant(
            &id,
            api::WithdrawViewerGrantParams::default()
                .x_sunrise_device(device.to_owned())
                .x_sunrise_device_sig(signature)
                .date(date),
        )
        .await
        .map_err(|e| refusal(&e))?;
    Ok(())
}

/// The device id every request is signed as, refused before anything is
/// signed when it is empty: an empty `X-Sunrise-Device` names no row, and the
/// relay answers that as a bad bearer, which would be undiagnosable here.
fn device_of<'a>(caller: &GrantCaller<'a>) -> Result<&'a str, GrantError> {
    let device = caller.relay_device_id.trim();
    if device.is_empty() {
        return Err(GrantError::Request(
            "no relay device id: register this device first".to_owned(),
        ));
    }
    Ok(device)
}

fn client(caller: &GrantCaller<'_>) -> Result<api::Client, GrantError> {
    crate::client(caller.base_url, caller.bearer).map_err(GrantError::Request)
}

/// Classify a failed call by its status, never by its text: the generated
/// `Display` is the client's to change.
fn refusal<E>(e: &api::Error<E>) -> GrantError
where
    api::Error<E>: std::fmt::Display,
{
    let status = match e {
        api::Error::Api(v) => Some(v.status().as_u16()),
        api::Error::UnexpectedStatus { status, .. } => Some(status.as_u16()),
        _ => None,
    };
    match status {
        Some(404) => GrantError::NotHeld,
        Some(409) => GrantError::Conflict(e.to_string()),
        _ => GrantError::Relay(e.to_string()),
    }
}

/// The relay's view of a grant, its ids parsed.
fn grant_of(view: &api::types::GrantView) -> Result<ViewerGrant, GrantError> {
    Ok(ViewerGrant {
        grant_id: parse_hex16(&view.grant_id).ok_or(GrantError::Malformed("grant_id"))?,
        stream_id: parse_hex16(&view.stream_id).ok_or(GrantError::Malformed("stream_id"))?,
        expires_at_ms: view
            .expires_at_ms
            .map(u64::try_from)
            .transpose()
            .map_err(|_| GrantError::Malformed("expires_at_ms"))?,
        created_ms: u64::try_from(view.created_ms)
            .map_err(|_| GrantError::Malformed("created_ms"))?,
        claimed_ms: view
            .claimed_ms
            .map(u64::try_from)
            .transpose()
            .map_err(|_| GrantError::Malformed("claimed_ms"))?,
    })
}

/// Lowercase hex, the only spelling the grant routes accept.
fn lower_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

/// Exactly 32 lowercase hex characters, as the relay serves an id.
fn parse_hex16(text: &str) -> Option<[u8; 16]> {
    fn nibble(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        }
    }
    let raw = text.as_bytes();
    if raw.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (slot, pair) in out.iter_mut().zip(raw.chunks_exact(2)) {
        *slot = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE: &str = "01HZY3W9ZQ6YB2Z5VB9V3K9F5C";

    fn caller(device: &str) -> GrantCaller<'_> {
        GrantCaller {
            base_url: "http://127.0.0.1:1",
            bearer: "secret-bearer",
            relay_device_id: device,
        }
    }

    fn unsigned(_: &[u8]) -> [u8; 64] {
        panic!("nothing is signed for a request that cannot be sent")
    }

    /// Every call refuses an empty device id before it signs or sends: the
    /// relay would answer it as a bad bearer.
    #[tokio::test]
    async fn an_empty_device_id_is_refused_before_the_request() {
        let c = caller("  ");
        let claim = [7u8; 32];
        assert!(matches!(
            create_viewer_grant(&c, [1; 16], &claim, None, 0, unsigned).await,
            Err(GrantError::Request(_))
        ));
        assert!(matches!(
            claim_viewer_grant(&c, &claim, 0, unsigned).await,
            Err(GrantError::Request(_))
        ));
        assert!(matches!(
            list_viewer_grants(&c, 0, unsigned).await,
            Err(GrantError::Request(_))
        ));
        assert!(matches!(
            withdraw_viewer_grant(&c, [2; 16], 0, unsigned).await,
            Err(GrantError::Request(_))
        ));
    }

    /// A relay that cannot be reached is the network's failure, signed first.
    #[tokio::test]
    async fn an_unreachable_relay_is_a_relay_failure() {
        let c = caller(DEVICE);
        let refused = claim_viewer_grant(&c, &[7; 32], 0, |_| [0; 64]).await;
        assert!(matches!(refused, Err(GrantError::Relay(_))), "{refused:?}");
    }

    /// The bodies carry the spelling the relay's patterns demand: lowercase
    /// hex, 32 characters for an id and 64 for a token.
    #[test]
    fn ids_and_tokens_travel_as_lowercase_hex() {
        let mut token = [0u8; 32];
        token[0] = 0xab;
        token[31] = 0x0f;
        let hex = lower_hex(&token);
        assert_eq!(hex.len(), 64);
        assert!(hex.starts_with("ab"), "{hex}");
        assert!(hex.ends_with("0f"), "{hex}");
        let id: [u8; 16] = core::array::from_fn(|i| u8::try_from(i * 17).unwrap_or(0));
        assert_eq!(parse_hex16(&lower_hex(&id)), Some(id));
    }

    /// The relay serves only lowercase hex of 16 bytes; anything else is a
    /// malformed answer rather than an id to guess at.
    #[test]
    fn a_served_id_is_exactly_lowercase_hex_of_sixteen_bytes() {
        assert_eq!(parse_hex16(&"AB".repeat(16)), None);
        assert_eq!(parse_hex16(&"ab".repeat(15)), None);
        assert_eq!(parse_hex16(&"zz".repeat(16)), None);
        let view = api::types::GrantView {
            grant_id: "ab".repeat(15),
            stream_id: "cd".repeat(16),
            expires_at_ms: None,
            created_ms: 1,
            claimed_ms: None,
        };
        assert!(matches!(
            grant_of(&view),
            Err(GrantError::Malformed("grant_id"))
        ));
    }

    /// Neither the bearer nor anything but the origin and device reaches a
    /// `Debug` line.
    #[test]
    fn debug_redacts_the_bearer() {
        let shown = format!("{:?}", caller(DEVICE));
        assert!(!shown.contains("secret-bearer"), "{shown}");
        assert!(shown.contains(DEVICE), "{shown}");
    }
}
