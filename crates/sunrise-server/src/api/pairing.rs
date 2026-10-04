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
//! - **[`SESSION_TTL_MS`], 300 s from the first message**, which is the QR's
//!   lifetime. The spec's separate 60 s handshake window is folded into it:
//!   the SAS screen alone may take 90 s, so a window that started when the
//!   existing device scanned would cut off a pairing the spec allows.
//! - **Pair attempts**, counted when a session is opened, per
//!   `sunrise_pairing::AttemptLimit`: 10 an hour and 30 a day per account, and
//!   60 an hour per client address. A refusal is `429 RATE_LIMITED` with
//!   `Retry-After`, and no session is opened.

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
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use sunrise_pairing::{AttemptLimit, AttemptWindow};

/// Largest message one role may buffer, decoded: the spec's 64 KiB.
pub const MAX_PAIR_MESSAGE: usize = 64 * 1024;

/// Messages one role sends in a pairing, and so the most it may buffer.
pub const MESSAGES_PER_ROLE: usize = 3;

/// How long a session lives from the message that opened it: the QR's 300 s.
pub const SESSION_TTL_MS: u64 = 300 * 1000;

/// Live sessions the relay holds at once, across every account. Past it a new
/// session is refused as rate-limited until one expires, rather than letting
/// memory grow with whoever opens the most.
const MAX_SESSIONS: usize = 4096;

/// Keys an attempt ledger may hold before idle ones are swept.
const MAX_LEDGER_KEYS: usize = 65_536;

/// A pair id as it travels: 16 bytes, base64url without padding.
const PAIR_ID_PATTERN: &str = "^[A-Za-z0-9_-]{22}$";

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
        message,
        state.clock.now_ms(),
    );
    record(&state, &outcome);
    match outcome {
        Ok(sent) => Ok(Json(PairSendResponse {
            sent: sent.count,
            expires_at_ms: sent.expires_at_ms,
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
        _context: &ServerState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<Forwarded>()
                .and_then(Forwarded::client),
        ))
    }
}

impl Describe for ClientAddress {
    /// Nothing to describe: the address is the transport's, not a parameter.
    fn describe(operation: &mut OperationCx<'_>) {
        let _ = operation;
    }
}

/// Why the rendezvous refused a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// No live session under that id for this account: never opened, expired,
    /// aborted, overflowed, or another account's.
    Gone,
    /// A pair attempt over the limit, or the relay's session table is full.
    Limited {
        /// Milliseconds until a retry would be admitted, at least 1.
        retry_after_ms: u64,
    },
}

impl Refusal {
    const fn label(self) -> &'static str {
        match self {
            Self::Gone => "gone",
            Self::Limited { .. } => "rate_limited",
        }
    }
}

/// A message the rendezvous accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sent {
    /// Messages the sender has now sent in this session.
    pub count: u32,
    /// Whether this message opened the session.
    pub opened: bool,
    /// When the session is dropped.
    pub expires_at_ms: u64,
    /// Sessions the call found expired and dropped.
    pub expired: usize,
}

/// What a receive found, and what it swept on the way.
#[derive(Debug)]
pub struct Received {
    /// The other side's messages from the cursor on and the session's expiry,
    /// or the refusal.
    pub result: Result<(Vec<Vec<u8>>, u64), Refusal>,
    /// Sessions the call found expired and dropped.
    pub expired: usize,
}

/// The in-memory rendezvous: live sessions and the pair-attempt ledgers.
///
/// Memory, not the store: a session lives five minutes and holds ciphertext
/// nobody can use after it, so a restart losing every one costs the users in
/// the middle of a pairing one rescan, and persisting it would put pairing
/// traffic on disk for no reader.
///
/// Every method takes `now_ms`, so the policy is driven by a test clock rather
/// than by sleeping.
#[derive(Debug, Clone, Default)]
pub struct Rendezvous {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug, Default)]
struct Inner {
    sessions: HashMap<[u8; 16], Session>,
    by_account: HashMap<String, AttemptWindow>,
    by_address: HashMap<String, AttemptWindow>,
}

#[derive(Debug)]
struct Session {
    account_id: String,
    expires_at_ms: u64,
    /// Indexed by [`PairRole::index`]: what each side has sent.
    sent: [Vec<Vec<u8>>; 2],
}

impl Rendezvous {
    /// An empty rendezvous.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Buffer `message` from `role`.
    ///
    /// `address` is the client's rate-limit key, or `None` where limits are
    /// off; with `None` neither attempt limit is charged.
    ///
    /// # Errors
    ///
    /// [`Refusal::Gone`] when the existing device names no live session of its
    /// account, when the new device names another account's, and when a role
    /// sends a fourth message — which also drops the session.
    /// [`Refusal::Limited`] when opening a session is over an attempt limit.
    pub fn send(
        &self,
        account_id: &str,
        address: Option<&str>,
        pair_id: [u8; 16],
        role: PairRole,
        message: Vec<u8>,
        now_ms: u64,
    ) -> Result<Sent, Refusal> {
        let mut inner = self.inner.lock();
        let expired = inner.reap(now_ms);
        let opened = !inner.sessions.contains_key(&pair_id);
        if opened {
            if role != PairRole::NewDevice {
                return Err(Refusal::Gone);
            }
            if inner.sessions.len() >= MAX_SESSIONS {
                return Err(Refusal::Limited {
                    retry_after_ms: inner.next_expiry(now_ms),
                });
            }
            if let Some(address) = address {
                inner.admit(account_id, address, now_ms)?;
            }
            inner.sessions.insert(
                pair_id,
                Session {
                    account_id: account_id.to_owned(),
                    expires_at_ms: now_ms + SESSION_TTL_MS,
                    sent: [Vec::new(), Vec::new()],
                },
            );
        }
        let Some(session) = inner
            .sessions
            .get_mut(&pair_id)
            .filter(|s| s.account_id == account_id)
        else {
            return Err(Refusal::Gone);
        };
        let mine = &mut session.sent[role.index()];
        if mine.len() >= MESSAGES_PER_ROLE {
            inner.sessions.remove(&pair_id);
            return Err(Refusal::Gone);
        }
        mine.push(message);
        Ok(Sent {
            count: u32::try_from(mine.len()).unwrap_or(u32::MAX),
            opened,
            expires_at_ms: session.expires_at_ms,
            expired,
        })
    }

    /// What `role`'s peer has sent, from index `after` on.
    #[must_use]
    pub fn receive(
        &self,
        account_id: &str,
        pair_id: [u8; 16],
        role: PairRole,
        after: usize,
        now_ms: u64,
    ) -> Received {
        let mut inner = self.inner.lock();
        let expired = inner.reap(now_ms);
        let result = inner
            .sessions
            .get(&pair_id)
            .filter(|s| s.account_id == account_id)
            .map(|s| {
                let theirs = &s.sent[role.peer().index()];
                let fresh = theirs.get(after..).unwrap_or_default().to_vec();
                (fresh, s.expires_at_ms)
            })
            .ok_or(Refusal::Gone);
        Received { result, expired }
    }

    /// Drop the session, if this account holds it. Whether one was dropped.
    pub fn abort(&self, account_id: &str, pair_id: [u8; 16], now_ms: u64) -> bool {
        let mut inner = self.inner.lock();
        inner.reap(now_ms);
        let ours = inner
            .sessions
            .get(&pair_id)
            .is_some_and(|s| s.account_id == account_id);
        if ours {
            inner.sessions.remove(&pair_id);
        }
        ours
    }

    /// Sessions currently live, expired ones included until the next call.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().sessions.len()
    }
}

impl Inner {
    /// Drop every expired session; how many went.
    fn reap(&mut self, now_ms: u64) -> usize {
        let before = self.sessions.len();
        self.sessions.retain(|_, s| s.expires_at_ms > now_ms);
        before - self.sessions.len()
    }

    /// Milliseconds until the soonest session expires, at least 1.
    fn next_expiry(&self, now_ms: u64) -> u64 {
        self.sessions
            .values()
            .map(|s| s.expires_at_ms.saturating_sub(now_ms))
            .min()
            .unwrap_or(SESSION_TTL_MS)
            .max(1)
    }

    /// Charge one pair attempt to both ledgers, or refuse it with the longer
    /// of the two waits. Neither is charged on a refusal.
    fn admit(&mut self, account_id: &str, address: &str, now_ms: u64) -> Result<(), Refusal> {
        sweep(&mut self.by_account, now_ms);
        sweep(&mut self.by_address, now_ms);
        let mut account = self.by_account.get(account_id).cloned().unwrap_or_default();
        let mut client = self.by_address.get(address).cloned().unwrap_or_default();
        let waits = [
            account.try_admit(AttemptLimit::PER_ACCOUNT, now_ms).err(),
            client.try_admit(AttemptLimit::PER_ADDRESS, now_ms).err(),
        ];
        if let Some(retry_after_ms) = waits.into_iter().flatten().max() {
            return Err(Refusal::Limited { retry_after_ms });
        }
        self.by_account.insert(account_id.to_owned(), account);
        self.by_address.insert(address.to_owned(), client);
        Ok(())
    }
}

/// Forget the ledgers of keys that have made no attempt for a day, once the
/// map is large enough to be worth the walk.
fn sweep(ledgers: &mut HashMap<String, AttemptWindow>, now_ms: u64) {
    if ledgers.len() >= MAX_LEDGER_KEYS {
        ledgers.retain(|_, w| !w.is_idle(now_ms));
    }
}

#[cfg(test)]
#[path = "pairing_tests.rs"]
mod tests;
