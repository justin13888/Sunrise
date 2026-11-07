//! The pairing rendezvous: where two devices meet to run the Noise handshake
//! without a person carrying the bytes between them.
//!
//! Per `docs/03-crypto/pairing-and-onboarding.md` §Relay framing for Noise.
//! Until this module the relay had no route keyed by `pair_id`, so every one
//! of a pairing's six messages — three Noise handshake messages, then the
//! offer, the cert request and the grant — crossed as text a user copied from
//! one screen to the other.
//!
//! # Shape
//!
//! Three typed `POST`s, each authenticated by the account bearer alone:
//!
//! - `POST /api/v1/pairing/send` buffers one message from one role.
//! - `POST /api/v1/pairing/receive` returns what the *other* role has sent
//!   since a cursor the caller holds.
//! - `POST /api/v1/pairing/abort` drops the session.
//! - `POST /api/v1/pairing/resolve` turns the six-digit code a session was
//!   given into its `pair_id`, for an existing device with no camera.
//!
//! Polling rather than a stream: a pairing is six messages over a few seconds
//! of human attention, and an SSE channel per pairing would be a second event
//! transport for a flow that moves less data than one sync batch. ADR-0023
//! retired the WebSocket the spec's framing section describes; these routes
//! carry the same payloads, base64url, with the role in the body rather than in
//! a frame header.
//!
//! # What the relay knows
//!
//! Nothing it can read. Every message is a Noise handshake or transport
//! message; the relay checks only that it is base64url, non-empty and at most
//! [`MAX_PAIR_MESSAGE`] bytes.
//!
//! # Who may touch a session
//!
//! The device being added opens it, with its first handshake message. The
//! session is then bound to the account whose bearer opened it, and a caller
//! from any other account is told the session is gone — the same answer as
//! for one that never existed, so the route is not an oracle for which pair
//! ids are live. The device being added holds no device key yet, so no device
//! binding is demanded; one that is supplied is still verified, as on every
//! bootstrap route.
//!
//! # Bounds
//!
//! - **Three messages per role**, which is exactly what the protocol sends:
//!   the new device writes handshake messages 1 and 3 and the cert request,
//!   the existing device handshake message 2, the offer and the grant. A
//!   fourth drops the session.
//! - **Each send names its index**, so a retry after a lost answer is
//!   acknowledged rather than buffered twice. An index that skips ahead or
//!   names a slot holding a different message drops the session: the two
//!   sides no longer agree on where the exchange is.
//! - **[`SESSION_TTL_MS`], 300 s from the first message**, which is the QR's
//!   lifetime. The spec's separate 60 s handshake window is folded into it:
//!   the SAS screen alone may take 90 s, so a window that started when the
//!   existing device scanned would cut off a pairing the spec allows.
//! - **Pair attempts**, counted when a session is opened, per
//!   `sunrise_pairing::AttemptLimit`: 10 an hour and 30 a day per account, and
//!   60 an hour per client address. A refusal is `429 RATE_LIMITED` with
//!   `Retry-After`, and no session is opened.
//! - **Code lookups** are charged to ledgers of their own under the same two
//!   limits, hit or miss, so guessing at codes costs what opening sessions
//!   does.
//!
//! # The typed code
//!
//! A device with no camera cannot read a QR, and a 22-character `pair_id`
//! is too much to type. So the session gets a six-digit code when it opens,
//! returned to the new device in the opening send's answer, and the existing
//! device trades it for the `pair_id` at `resolve` (the spec's §Flow step 1,
//! "a QR (and fallback 6-digit code)").
//!
//! - **Within one account.** A code resolves only for the account that opened
//!   the session, and a code unknown there gets the same `404` as one from
//!   another account. Six digits need to tell apart only the few sessions one
//!   account can hold under its ten attempts an hour, not every live session
//!   on the relay.
//! - **No collision inside an account.** The code is drawn from the `pair_id`
//!   (random), then stepped past any code a live session of the same account
//!   already holds.
//! - **Spent when the existing device joins.** Once it has sent its first
//!   message the code stops resolving, so a second lookup cannot race the
//!   device that already joined.
//! - **Nothing is authenticated by it.** It is a routing label like the
//!   `pair_id`; a pairing by code carries no `n_static_pub`, and the SAS is its
//!   whole authentication (§Out-of-band channel security).

use crate::api::error::{codes, ApiError};
use crate::api::ratelimit::policy::address_key;
use crate::api::ratelimit::{RateLimited, Throttled};
use crate::api::signed::SignedBootstrap;
use crate::state::ServerState;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use kynos::di::inject::Inject;
use kynos::extract::body::json::Json;
use kynos::extract::describe::Describe;
use kynos::extract::FromRequestParts;
use kynos::http::forwarded::Forwarded;
use kynos::http::Parts;
use kynos::response::status::NoContent;
use kynos::router::operation::OperationCx;
use serde::{Deserialize, Serialize};

/// Largest message one role may buffer, decoded: the spec's 64 KiB.
pub const MAX_PAIR_MESSAGE: usize = 64 * 1024;

/// Messages one role sends in a pairing, and so the most it may buffer.
pub const MESSAGES_PER_ROLE: usize = 3;

/// How long a session lives from the message that opened it: the QR's 300 s.
pub const SESSION_TTL_MS: u64 = 300 * 1000;

/// A pair id as it travels: 16 bytes, base64url without padding.
const PAIR_ID_PATTERN: &str = "^[A-Za-z0-9_-]{22}$";

/// A typed code as it travels: six ASCII digits.
const CODE_PATTERN: &str = "^[0-9]{6}$";

/// Which side of a pairing sent a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, kynos::Schema)]
#[serde(rename_all = "snake_case")]
pub enum PairRole {
    /// The device being added. It shows the QR and opens the session.
    NewDevice,
    /// The device that already holds the vault. It scans the QR and joins.
    ExistingDevice,
}

impl PairRole {
    const fn index(self) -> usize {
        match self {
            Self::NewDevice => 0,
            Self::ExistingDevice => 1,
        }
    }

    const fn peer(self) -> Self {
        match self {
            Self::NewDevice => Self::ExistingDevice,
            Self::ExistingDevice => Self::NewDevice,
        }
    }
}

/// `POST /api/v1/pairing/send` request body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct PairSendRequest {
    /// The QR's `pair_id`: 16 bytes, base64url without padding.
    #[schema(pattern = "^[A-Za-z0-9_-]{22}$")]
    pub pair_id: String,
    /// The side sending.
    pub role: PairRole,
    /// How many messages this role sent before this one: 0, 1 or 2.
    ///
    /// What makes a retry safe. A send whose answer was lost is resent with
    /// the same index and the same message, and is acknowledged without being
    /// buffered twice — a duplicate would otherwise reach the other side as the
    /// protocol's next message and fail its decryption.
    pub index: u32,
    /// One Noise message, base64url without padding; at most 64 KiB decoded.
    pub message: String,
}

/// `POST /api/v1/pairing/send` response body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct PairSendResponse {
    /// How many messages this role has now sent in the session, this one
    /// included.
    pub sent: u32,
    /// When the session is dropped whatever happens, ms since the epoch.
    pub expires_at_ms: u64,
    /// The six digits the existing device may type instead of scanning, on
    /// the new device's sends until the existing device has joined. Absent
    /// otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(pattern = "^[0-9]{6}$")]
    pub code: Option<String>,
}

/// `POST /api/v1/pairing/resolve` request body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct PairResolveRequest {
    /// The six digits the new device shows beside its QR.
    #[schema(pattern = "^[0-9]{6}$")]
    pub code: String,
}

/// `POST /api/v1/pairing/resolve` response body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct PairResolveResponse {
    /// The session the code names, as the QR would have carried it.
    #[schema(pattern = "^[A-Za-z0-9_-]{22}$")]
    pub pair_id: String,
    /// When the session is dropped whatever happens, ms since the epoch.
    pub expires_at_ms: u64,
}

/// `POST /api/v1/pairing/receive` request body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct PairReceiveRequest {
    /// The QR's `pair_id`.
    #[schema(pattern = "^[A-Za-z0-9_-]{22}$")]
    pub pair_id: String,
    /// The side receiving. It is handed what the other side sent.
    pub role: PairRole,
    /// How many of the other side's messages the caller already holds.
    pub after: u32,
}

/// `POST /api/v1/pairing/receive` response body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct PairReceiveResponse {
    /// The other side's messages from `after` on, oldest first, base64url.
    /// Empty when nothing new has arrived.
    pub messages: Vec<String>,
    /// When the session is dropped whatever happens, ms since the epoch.
    pub expires_at_ms: u64,
}

/// `POST /api/v1/pairing/abort` request body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct PairAbortRequest {
    /// The QR's `pair_id`.
    #[schema(pattern = "^[A-Za-z0-9_-]{22}$")]
    pub pair_id: String,
    /// The side giving up.
    pub role: PairRole,
}

/// Buffer one message for the other side.
///
/// The new device's first message opens the session and counts as a pair
/// attempt. The existing device can only join a session that is open.
#[kynos::post("/api/v1/pairing/send", operation_id = "sendPairingMessage")]
pub async fn send(
    Inject(state): Inject<ServerState>,
    ClientAddress(client): ClientAddress,
    SignedBootstrap {
        caller,
        value: body,
    }: SignedBootstrap<PairSendRequest>,
) -> Result<Json<PairSendResponse>, Throttled> {
    let pair_id = pair_id_of(&body.pair_id)?;
    let message = URL_SAFE_NO_PAD
        .decode(body.message.as_bytes())
        .map_err(|_| ApiError::validation("message must be base64url without padding"))?;
    if message.is_empty() || message.len() > MAX_PAIR_MESSAGE {
        return Err(ApiError::validation(format!(
            "message must be 1..={MAX_PAIR_MESSAGE} bytes decoded"
        ))
        .into());
    }
    let limits = state.config.limits.enabled.then(|| address_key(client));
    let outcome = state.pairing.send(
        &caller.principal.account.account_id,
        limits.as_deref(),
        pair_id,
        body.role,
        usize::try_from(body.index).unwrap_or(usize::MAX),
        message,
        state.clock.now_ms(),
    );
    record(&state, &outcome);
    match outcome {
        Ok(sent) => Ok(Json(PairSendResponse {
            sent: sent.count,
            expires_at_ms: sent.expires_at_ms,
            code: sent.code.map(code_text),
        })),
        Err(refusal) => Err(refused(refusal)),
    }
}

/// Collect what the other side has sent since `after`.
#[kynos::post("/api/v1/pairing/receive", operation_id = "receivePairingMessages")]
pub async fn receive(
    Inject(state): Inject<ServerState>,
    SignedBootstrap {
        caller,
        value: body,
    }: SignedBootstrap<PairReceiveRequest>,
) -> Result<Json<PairReceiveResponse>, Throttled> {
    let pair_id = pair_id_of(&body.pair_id)?;
    let outcome = state.pairing.receive(
        &caller.principal.account.account_id,
        pair_id,
        body.role,
        usize::try_from(body.after).unwrap_or(usize::MAX),
        state.clock.now_ms(),
    );
    record_expiries(&state, outcome.expired);
    match outcome.result {
        Ok((messages, expires_at_ms)) => Ok(Json(PairReceiveResponse {
            messages: messages.iter().map(|m| URL_SAFE_NO_PAD.encode(m)).collect(),
            expires_at_ms,
        })),
        Err(refusal) => {
            state
                .metrics
                .incr_with("sunrise_pairing_total", &[("result", refusal.label())]);
            Err(refused(refusal))
        }
    }
}

/// Drop the session: the spec's `pair_abort`, for a "Don't match" tap or a
/// cancelled screen.
///
/// Always `204`, whether or not there was a session to drop: an abort that
/// answered differently for a live session would be the oracle the other two
/// routes are careful not to be.
#[kynos::post("/api/v1/pairing/abort", operation_id = "abortPairing")]
pub async fn abort(
    Inject(state): Inject<ServerState>,
    SignedBootstrap {
        caller,
        value: body,
    }: SignedBootstrap<PairAbortRequest>,
) -> Result<NoContent, ApiError> {
    let pair_id = pair_id_of(&body.pair_id)?;
    let _ = body.role;
    if state.pairing.abort(
        &caller.principal.account.account_id,
        pair_id,
        state.clock.now_ms(),
    ) {
        state
            .metrics
            .incr_with("sunrise_pairing_total", &[("result", "aborted")]);
    }
    Ok(NoContent)
}

/// Trade the six digits a new device shows for the session they name, within
/// the caller's account.
///
/// Every lookup is charged to the code-lookup ledgers, hit or miss. A code no
/// live session of this account holds — never given, expired, spent, or
/// another account's — is `404` with the same body in every case.
#[kynos::post("/api/v1/pairing/resolve", operation_id = "resolvePairingCode")]
pub async fn resolve(
    Inject(state): Inject<ServerState>,
    ClientAddress(client): ClientAddress,
    SignedBootstrap {
        caller,
        value: body,
    }: SignedBootstrap<PairResolveRequest>,
) -> Result<Json<PairResolveResponse>, Throttled> {
    let code = code_of(&body.code)?;
    let limits = state.config.limits.enabled.then(|| address_key(client));
    let outcome = state.pairing.resolve(
        &caller.principal.account.account_id,
        limits.as_deref(),
        code,
        state.clock.now_ms(),
    );
    record_expiries(&state, outcome.expired);
    match outcome.result {
        Ok((pair_id, expires_at_ms)) => Ok(Json(PairResolveResponse {
            pair_id: URL_SAFE_NO_PAD.encode(pair_id),
            expires_at_ms,
        })),
        Err(Refusal::Gone) => Err(ApiError::not_found(
            codes::RELAY_PAIR_SESSION_GONE,
            "no pairing is waiting under that code; check the digits, or start again from a new code",
        )
        .into()),
        Err(refusal) => Err(refused(refusal)),
    }
}

fn code_of(text: &str) -> Result<u32, ApiError> {
    Some(text)
        .filter(|t| t.len() == 6 && t.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|t| t.parse().ok())
        .ok_or_else(|| ApiError::validation(format!("code must match {CODE_PATTERN}")))
}

fn code_text(code: u32) -> String {
    format!("{code:06}")
}

fn pair_id_of(text: &str) -> Result<[u8; 16], ApiError> {
    URL_SAFE_NO_PAD
        .decode(text.as_bytes())
        .ok()
        .and_then(|raw| <[u8; 16]>::try_from(raw.as_slice()).ok())
        .ok_or_else(|| ApiError::validation(format!("pair_id must match {PAIR_ID_PATTERN}")))
}

fn refused(refusal: Refusal) -> Throttled {
    match refusal {
        Refusal::Gone => ApiError::not_found(
            codes::RELAY_PAIR_SESSION_GONE,
            "no live pairing session under that id; start again from a new code",
        )
        .into(),
        Refusal::Limited { retry_after_ms } => {
            RateLimited::after_secs(retry_after_ms.div_ceil(1000)).into()
        }
    }
}

fn record(state: &ServerState, outcome: &Result<Sent, Refusal>) {
    let result = match outcome {
        Ok(sent) => {
            record_expiries(state, sent.expired);
            if sent.replayed {
                // Counted when it was first accepted.
                return;
            }
            if sent.opened {
                "opened"
            } else {
                "relayed"
            }
        }
        Err(refusal) => refusal.label(),
    };
    state
        .metrics
        .incr_with("sunrise_pairing_total", &[("result", result)]);
}

fn record_expiries(state: &ServerState, expired: usize) {
    if expired > 0 {
        state.metrics.add_with(
            "sunrise_pairing_total",
            &[("result", "expired")],
            u64::try_from(expired).unwrap_or(u64::MAX),
        );
    }
}

/// The client address, as the router's trusted-proxy policy resolved it.
///
/// The same resolution the per-address admission limiter keys on, so the two
/// limits agree about who a client is.
#[derive(Debug, Clone, Copy)]
pub struct ClientAddress(pub Option<std::net::IpAddr>);

impl FromRequestParts<ServerState> for ClientAddress {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        context: &ServerState,
    ) -> Result<Self, Self::Rejection> {
        // kynos's own extractor for what the router resolved, infallible.
        let forwarded = Forwarded::from_request_parts(parts, context).await?;
        Ok(Self(forwarded.client()))
    }
}

impl Describe for ClientAddress {
    /// Nothing to describe: the address is the transport's, not a parameter.
    fn describe(operation: &mut OperationCx<'_>) {
        let _ = operation;
    }
}

#[path = "pairing_rendezvous.rs"]
mod rendezvous;
pub use rendezvous::{Received, Refusal, Rendezvous, Resolved, Sent};

#[cfg(test)]
#[path = "pairing_tests.rs"]
mod tests;
