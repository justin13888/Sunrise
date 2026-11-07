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

/// How many six-digit codes there are.
const CODE_SPACE: u32 = 1_000_000;

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
    /// Whether this was a resend of a message already held, and so changed
    /// nothing.
    pub replayed: bool,
    /// When the session is dropped.
    pub expires_at_ms: u64,
    /// Sessions the call found expired and dropped.
    pub expired: usize,
    /// The session's typed code, to the new device until the existing device
    /// has joined.
    pub code: Option<u32>,
}

/// What a code lookup found, and what it swept on the way.
#[derive(Debug)]
pub struct Resolved {
    /// The session's pair id and expiry, or the refusal.
    pub result: Result<([u8; 16], u64), Refusal>,
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
    lookups_by_account: HashMap<String, AttemptWindow>,
    lookups_by_address: HashMap<String, AttemptWindow>,
}

#[derive(Debug)]
struct Session {
    account_id: String,
    expires_at_ms: u64,
    /// The typed code, until the existing device sends its first message.
    code: Option<u32>,
    /// Indexed by [`PairRole::index`]: what each side has sent.
    sent: [Vec<Vec<u8>>; 2],
}

impl Rendezvous {
    /// An empty rendezvous.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Buffer `message` as `role`'s message number `index`.
    ///
    /// `address` is the client's rate-limit key, or `None` where limits are
    /// off; with `None` neither attempt limit is charged. Resending the message
    /// already held at `index` is acknowledged and changes nothing.
    ///
    /// # Errors
    ///
    /// [`Refusal::Gone`] when no live session of this account is named and the
    /// message cannot open one — it is not the new device's first — and when
    /// `index` is past the next free slot, past the protocol's three, or holds
    /// a different message; those three also drop the session.
    /// [`Refusal::Limited`] when opening a session is over an attempt limit or
    /// the session table is full.
    #[allow(clippy::too_many_arguments)]
    pub fn send(
        &self,
        account_id: &str,
        address: Option<&str>,
        pair_id: [u8; 16],
        role: PairRole,
        index: usize,
        message: Vec<u8>,
        now_ms: u64,
    ) -> Result<Sent, Refusal> {
        let mut inner = self.inner.lock();
        let expired = inner.reap(now_ms);
        let opened = !inner.sessions.contains_key(&pair_id);
        if opened {
            // Only the new device's first message opens a session. A later
            // message naming a session that is gone — expired, say — must not
            // open a fresh one and leave the sender waiting on a peer that
            // will never come.
            if role != PairRole::NewDevice || index != 0 {
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
            let code = inner.free_code(account_id, pair_id);
            inner.sessions.insert(
                pair_id,
                Session {
                    account_id: account_id.to_owned(),
                    expires_at_ms: now_ms + SESSION_TTL_MS,
                    code: Some(code),
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
        let replayed = mine.get(index).map(|held| *held == message);
        match replayed {
            // A retry of a send whose answer was lost: acknowledged, not
            // buffered twice.
            Some(true) => {}
            // The index is taken by a different message, or is past the next
            // free one, or past the protocol's three: the two sides no longer
            // agree on where the exchange is, and nothing either sends now
            // could decrypt. The spec drops the session on overflow; the same
            // answer serves the rest.
            Some(false) => {
                inner.sessions.remove(&pair_id);
                return Err(Refusal::Gone);
            }
            None if index != mine.len() || index >= MESSAGES_PER_ROLE => {
                inner.sessions.remove(&pair_id);
                return Err(Refusal::Gone);
            }
            None => mine.push(message),
        }
        let count = u32::try_from(mine.len()).unwrap_or(u32::MAX);
        if role == PairRole::ExistingDevice {
            // The existing device has joined: the code has done its work, and
            // a second lookup must not hand the session to anyone else.
            session.code = None;
        }
        Ok(Sent {
            count,
            opened,
            replayed: replayed == Some(true),
            expires_at_ms: session.expires_at_ms,
            expired,
            code: session.code.filter(|_| role == PairRole::NewDevice),
        })
    }

    /// The session `account_id` holds under `code`, charging one lookup to
    /// `address` and the account where `address` is given.
    ///
    /// # Errors
    ///
    /// [`Refusal::Limited`] when the lookup is over either limit, and
    /// [`Refusal::Gone`] when no live session of this account holds `code`.
    pub fn resolve(
        &self,
        account_id: &str,
        address: Option<&str>,
        code: u32,
        now_ms: u64,
    ) -> Resolved {
        let mut inner = self.inner.lock();
        let expired = inner.reap(now_ms);
        if let Some(address) = address {
            if let Err(refusal) = inner.admit_lookup(account_id, address, now_ms) {
                return Resolved {
                    result: Err(refusal),
                    expired,
                };
            }
        }
        let result = inner
            .sessions
            .iter()
            .find(|(_, s)| s.account_id == account_id && s.code == Some(code))
            .map(|(id, s)| (*id, s.expires_at_ms))
            .ok_or(Refusal::Gone);
        Resolved { result, expired }
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
        admit(
            &mut self.by_account,
            &mut self.by_address,
            account_id,
            address,
            now_ms,
        )
    }

    /// [`Self::admit`], for a code lookup: the same limits, in ledgers of
    /// their own, so looking codes up and opening sessions do not share one
    /// budget.
    fn admit_lookup(
        &mut self,
        account_id: &str,
        address: &str,
        now_ms: u64,
    ) -> Result<(), Refusal> {
        admit(
            &mut self.lookups_by_account,
            &mut self.lookups_by_address,
            account_id,
            address,
            now_ms,
        )
    }

    /// A code for a new session of `account_id`: drawn from the random
    /// `pair_id`, then stepped past every code a live session of the same
    /// account holds. The table holds far fewer than [`CODE_SPACE`] sessions,
    /// so the step ends.
    fn free_code(&self, account_id: &str, pair_id: [u8; 16]) -> u32 {
        let [a, b, c, d, ..] = pair_id;
        let mut code = u32::from_be_bytes([a, b, c, d]) % CODE_SPACE;
        while self
            .sessions
            .values()
            .any(|s| s.account_id == account_id && s.code == Some(code))
        {
            code = (code + 1) % CODE_SPACE;
        }
        code
    }
}

/// Charge one attempt to both ledgers, or refuse it with the longer of the
/// two waits. Neither is charged on a refusal.
fn admit(
    by_account: &mut HashMap<String, AttemptWindow>,
    by_address: &mut HashMap<String, AttemptWindow>,
    account_id: &str,
    address: &str,
    now_ms: u64,
) -> Result<(), Refusal> {
    sweep(by_account, now_ms);
    sweep(by_address, now_ms);
    let mut account = by_account.get(account_id).cloned().unwrap_or_default();
    let mut client = by_address.get(address).cloned().unwrap_or_default();
    let waits = [
        account.try_admit(AttemptLimit::PER_ACCOUNT, now_ms).err(),
        client.try_admit(AttemptLimit::PER_ADDRESS, now_ms).err(),
    ];
    if let Some(retry_after_ms) = waits.into_iter().flatten().max() {
        return Err(Refusal::Limited { retry_after_ms });
    }
    by_account.insert(account_id.to_owned(), account);
    by_address.insert(address.to_owned(), client);
    Ok(())
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
