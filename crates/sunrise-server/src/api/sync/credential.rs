//! Establishing and renewing the negotiated credential.
//!
//! The only symbols that read or write `state.sessions`, and the only ones that
//! call the token verifier: [`session`] mints a session from a `Hello`
//! negotiation, [`refresh`] replaces the bearer behind one without ending it,
//! and [`resolve`] is the ownership check every other operation in this surface
//! performs before it touches a session. [`account_hash`] is here because the
//! relay channel namespace is fixed at establishment and nowhere else.

use crate::api::error::ApiError;
use crate::api::signed::Signed;
use crate::state::ServerState;
use crate::sync_session::Session;
use kynos::di::inject::Inject;
use kynos::extract::body::json::Json;
use kynos::extract::params::header::Headers;
use kynos::response::status::Created;
use serde::{Deserialize, Serialize};
use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_FLOOR, WIRE_PROTO_V};
use sunrise_wire_protocol::{
    Capability, CapabilityBits, Hello, REQUIRED_CLIENT_BITS, REQUIRED_SERVER_BITS,
};

/// `POST /api/v1/sync/session` request body — the `Hello` fields.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct SessionRequest {
    /// Client app version.
    pub client_app_v: String,
    /// Platform string.
    pub client_platform: String,
    /// Ascending list of wire-protocol versions the client speaks.
    pub wire_proto_supported: Vec<u32>,
    /// Lowest document schema the client can produce.
    pub doc_schema_min: u32,
    /// Highest document schema the client can read.
    pub doc_schema_max: u32,
    /// Crypto suites the client speaks.
    pub crypto_suite_supported: Vec<u32>,
    /// Bitfield of optional features.
    pub capabilities: u64,
    /// ULID of the session trace.
    pub trace: String,
}

/// `POST /api/v1/sync/session` response body — `HelloAck` plus the id.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct SessionResponse {
    /// The session id, presented as `X-Sunrise-Session` on every later call.
    pub session_id: String,
    /// Server app version.
    pub server_app_v: String,
    /// Negotiated wire protocol version.
    pub wire_proto: u32,
    /// Negotiated crypto suite.
    pub crypto_suite: u32,
    /// The server's accepted document-schema floor.
    pub doc_schema_floor: u32,
    /// The capability bitfield both sides agreed on.
    pub capabilities: u64,
    /// Server time, advisory, for clock-skew detection.
    pub server_time_ms: u64,
}

/// The session id, presented on every operation after establishment.
#[derive(Debug, Clone, kynos::HeaderParams)]
pub struct SessionHeader {
    /// The id `POST /sync/session` returned.
    #[header(rename = "X-Sunrise-Session")]
    pub session: Option<String>,
}

/// Establish a sync session: negotiate versions and capabilities.
///
/// Replaces the `Hello`/`HelloAck` exchange. `Hello::negotiate` is called
/// unchanged, so a client that cannot agree on a wire version, a crypto suite
/// or a required capability is refused here for the same reason and with the
/// same mapping it was refused at the socket.
///
/// A refusal is a `400` carrying the negotiation's **own** code —
/// `SYNC_PROTOCOL_VERSION_MISMATCH`, `CRYPTO_SUITE_MISMATCH`,
/// `DOC_SCHEMA_TOO_OLD` or `CAPABILITY_REQUIRED_MISSING` — because the four
/// mean four different things to whoever is looking at the screen, and
/// telling a self-hoster to update their app when the relay is the stale half
/// is the concrete failure of collapsing them.

#[kynos::post("/api/v1/sync/session", operation_id = "openSyncSession")]
pub async fn session(
    Inject(state): Inject<ServerState>,
    Signed {
        caller,
        value: body,
    }: Signed<SessionRequest>,
) -> Result<Created<Json<SessionResponse>>, ApiError> {
    let now_ms = state.clock.now_ms();
    // Hung off the busiest path rather than a timer task whose only job is to
    // take a lock occasionally.
    state.sessions.collect(now_ms);

    let hello = Hello {
        client_app_v: body.client_app_v,
        client_platform: body.client_platform,
        wire_proto_supported: body.wire_proto_supported,
        doc_schema_min: body.doc_schema_min,
        doc_schema_max: body.doc_schema_max,
        crypto_suite_supported: body.crypto_suite_supported,
        capabilities: body.capabilities,
        trace: body.trace,
    };
    // `SrvTokenRefresh` is optional, so it rides on top of the required set.
    // The client only asks for a refresh if it sees this bit come back agreed,
    // which is what stops one being silently swallowed by an older server.
    let server_caps = REQUIRED_CLIENT_BITS.0
        | REQUIRED_SERVER_BITS.0
        | CapabilityBits::EMPTY.with(Capability::SrvTokenRefresh).0;
    let ack = hello
        .negotiate(
            state.config.server_app_v.clone(),
            &[u32::from(WIRE_PROTO_V)],
            &[u32::from(CRYPTO_SUITE_V)],
            u32::from(DOC_SCHEMA_FLOOR),
            server_caps,
            now_ms,
        )
        .map_err(|e| {
            // The refusal's own code, not `VALIDATION_INVALID`. The four
            // negotiation failures mean four different things to a user —
            // update the app, update the relay, this device's data is too old,
            // this relay is missing a feature the vault requires — and a
            // client that receives one code for all four can only tell them
            // apart by matching on `e.to_string()`, which breaks on any
            // rewording. `NegotiationError::as_error_code` has always computed
            // the distinction; this is the call that delivers it.
            let code = e.as_error_code();
            state.metrics.incr("sunrise_sync_negotiate_refused_total");
            tracing::warn!(
                ev = "srv.sync.negotiate_refused",
                err_code = %code,
                err_kind = "permanent",
                retryable = false,
                cause = %e,
                "session refused"
            );
            ApiError::validation_coded(code.as_str(), e.to_string())
        })?;

    let account = account_hash(&caller.principal.account.account_id);
    let stored = Session {
        account,
        account_id: caller.principal.account.account_id.clone(),
        subject: caller.principal.subject.clone(),
        device_id: caller.device.as_ref().map(|d| d.device_id.clone()),
        deadline_ms: caller.principal.expires_at_ms,
        conn: state.relay.next_conn(),
        negotiated: ack.clone(),
        streams: Vec::new(),
        subscribe_unserved: false,
        seen_ms: now_ms,
    };

    let session_id = state
        .sessions
        .insert(stored)
        .map_err(|_| ApiError::internal())?;

    state.metrics.incr("sunrise_sync_session_total");
    tracing::info!(
        ev = "srv.sync.session_open",
        account_h = %crate::logging::account_h(&caller.principal.account.account_id),
        "sync session established"
    );
    Ok(Created::at(
        "/api/v1/sync/events",
        Json(SessionResponse {
            session_id,
            server_app_v: ack.server_app_v,
            wire_proto: ack.wire_proto,
            crypto_suite: ack.crypto_suite,
            doc_schema_floor: ack.doc_schema_floor,
            capabilities: ack.capabilities,
            server_time_ms: ack.server_time_ms,
        }),
    ))
}

/// `POST /api/v1/sync/session/refresh` request body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct RefreshRequest {
    /// The replacement bearer.
    pub token: String,
}

/// `POST /api/v1/sync/session/refresh` response body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct RefreshResponse {
    /// The new deadline, or 0 where the verifier issues none.
    pub expires_at_ms: u64,
}

/// Extend a session without re-establishing it.
///
/// The replacement must name the **same principal and the same device**. A
/// refresh that changed either would let one credential hand a live op stream
/// to another, which is the whole reason the check exists rather than simply
/// trusting a valid token.
///
/// That mismatch does more than refuse the refresh: it **ends the session**
/// before answering `401`, so the caller has to establish a new one rather
/// than retry with the credential it still holds. The two failure paths are
/// asymmetric on purpose. A token that fails *verification* says nothing about
/// who holds the session, so that one is recoverable and the session keeps the
/// credential it already has; a token that verifies and names *someone else*
/// means the channel namespace fixed at establishment no longer matches the
/// holder, and there is nothing left worth serving on it.
#[kynos::post("/api/v1/sync/session/refresh", operation_id = "refreshSyncSession")]
pub async fn refresh(
    Inject(state): Inject<ServerState>,
    Headers(header): Headers<SessionHeader>,
    Signed {
        caller,
        value: body,
    }: Signed<RefreshRequest>,
) -> Result<Json<RefreshResponse>, ApiError> {
    let now_ms = state.clock.now_ms();
    let (id, session) = resolve(&state, &header, &caller, now_ms)?;

    let account_h = crate::logging::account_h(&session.account_id);
    let verified = state
        .token_verifier
        .verify(&body.token)
        .await
        .map_err(|e| {
            // Recoverable: the session keeps the credential it still has.
            tracing::warn!(
                ev = "srv.sync.refresh_rejected",
                err_kind = "user",
                account_h = %account_h,
                cause = %e,
                "refresh token failed verification"
            );
            ApiError::unauthenticated()
        })?;
    if verified.subject.principal_key() != session.subject.principal_key()
        || verified.subject.device_id != session.subject.device_id
    {
        // A session handed another principal's token is not one to keep
        // serving: the channel namespace was fixed at establishment.
        tracing::warn!(
            ev = "srv.sync.refresh_identity_mismatch",
            err_kind = "user",
            account_h = %account_h,
            "refresh token names a different principal or device; ending the session"
        );
        state.sessions.remove(&id);
        return Err(ApiError::unauthenticated());
    }

    let deadline = verified.expires_at_ms;
    state
        .sessions
        .update(&id, now_ms, |s| s.deadline_ms = deadline);
    state.metrics.incr("sunrise_sync_refresh_total");
    tracing::debug!(
        ev = "srv.sync.refreshed",
        account_h = %account_h,
        "session deadline extended without a reconnect"
    );
    Ok(Json(RefreshResponse {
        expires_at_ms: deadline.unwrap_or(0),
    }))
}

/// Resolve the session the header names, checking it belongs to the caller.
///
/// The ownership check is the load-bearing half. A session id is a
/// bearer-equivalent, so without it any authenticated account presenting
/// another's id would be handed that account's op stream.
pub(super) fn resolve(
    state: &ServerState,
    header: &SessionHeader,
    caller: &crate::api::signed::Caller,
    now_ms: u64,
) -> Result<(String, Session), ApiError> {
    let id = header
        .session
        .as_deref()
        .ok_or_else(|| ApiError::validation("X-Sunrise-Session is required"))?;
    let session = state
        .sessions
        .get(id, now_ms)
        .ok_or_else(ApiError::unauthenticated)?;
    if session.subject.principal_key() != caller.principal.subject.principal_key() {
        return Err(ApiError::unauthenticated());
    }
    Ok((id.to_owned(), session))
}

/// The relay channel namespace for an account.
///
/// The account id never travels to another account, and the hash is what keys
/// the channel, so a stream is addressable only by someone who already knows
/// whose it is.
fn account_hash(account_id: &str) -> [u8; 16] {
    let h = blake3::hash(account_id.as_bytes());
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.as_bytes()[..16]);
    out
}
