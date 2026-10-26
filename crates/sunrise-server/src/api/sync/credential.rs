//! Establishing and renewing the negotiated credential.
//!
//! The only symbols that read or write `state.sessions`, and the only ones that
//! call the token verifier: [`session`] mints a session from a `Hello`
//! negotiation, [`refresh`] replaces the bearer behind one without ending it,
//! and [`resolve`] is the ownership check every other operation in this surface
//! performs before it touches a session. [`account_hash`] is here because the
//! relay channel namespace is fixed at establishment and nowhere else.
//!
//! Every body on this surface — here, in `cursors` and in `publish` — is open
//! to extension: none is `deny_unknown_fields`, so a field a newer client adds
//! is ignored by a relay that predates it rather than refused before the
//! capability bitfield is ever read. Something a peer must understand is
//! expressed as a capability bit or a version the relay compares, never as a
//! decode failure (protocol-versioning.md §4 and §6; issue #370). The suite's
//! `a_newer_client*` tests pin it for each request body.

use crate::api::error::ApiError;
use crate::api::ratelimit::policy::Budget;
use crate::api::ratelimit::Throttled;
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
) -> Result<Created<Json<SessionResponse>>, Throttled> {
    let now_ms = state.clock.now_ms();
    // Before any work: a client re-establishing in a tight loop is the case
    // this budget exists for, and negotiating for it first would be the work
    // it is meant to spare.
    state
        .limiter
        .charge(&state, "/api/v1/sync/session", Budget::Sessions, &caller, 1)
        .await?;
    crate::api::account_deletion::refuse_if_pending_deletion(
        &state,
        &caller.principal.account.account_id,
    )
    .await?;
    // Hung off the busiest path rather than a timer task whose only job is to
    // take a lock occasionally.
    state
        .sessions
        .collect(now_ms)
        .await
        .map_err(ApiError::from)?;

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
    // `SrvEnvelopeFloor` is optional for the same reason: this relay's header
    // decoder routes a newer envelope container at a floor it implements
    // (ADR-0045 §5), and an older relay's does not.
    let server_caps = REQUIRED_CLIENT_BITS.0
        | REQUIRED_SERVER_BITS.0
        | CapabilityBits::EMPTY
            .with(Capability::SrvTokenRefresh)
            .with(Capability::SrvEnvelopeFloor)
            .0;
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
            state.metrics.incr_with(
                "sunrise_sync_negotiate_refused_total",
                &[("reason", code.as_str())],
            );
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
        .await
        .map_err(ApiError::from)?;

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
pub struct RefreshRequest {
    /// The replacement bearer.
    pub token: String,
}

/// `POST /api/v1/sync/session/refresh` response body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
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
    let (id, session) = resolve(&state, &header, &caller, now_ms).await?;

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
        // A removal the store refused is a `503`, not the `401`: the session
        // is still live, and the client's retry is what gets it ended.
        state.sessions.remove(&id).await?;
        return Err(ApiError::unauthenticated());
    }

    let deadline = verified.expires_at_ms;
    state
        .sessions
        .update(&id, now_ms, move |s| s.deadline_ms = deadline)
        .await?;
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
pub(super) async fn resolve(
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
        .await?
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

#[cfg(test)]
mod tests {
    use super::account_hash;
    use crate::relay_log::{account_key, DurableCaps};
    use crate::Store;

    /// Ids of every shape an account id could take: empty, ASCII, a UUID,
    /// multi-byte UTF-8, and lengths either side of BLAKE3's 64-byte block and
    /// 1024-byte chunk boundaries.
    fn ids() -> Vec<String> {
        let mut ids: Vec<String> = [
            "",
            "a",
            "alice",
            "acct_01HZX3K9Q2V7",
            "0b9f6c1e-3a4d-4e8b-9c2f-7d1a5e6b8c90",
            "ünïcødé-账户-🔑",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        for n in [63, 64, 65, 1023, 1024, 1025, 4096] {
            ids.push("x".repeat(n));
        }
        // A deterministic spread of arbitrary bytes-as-chars, so the check
        // covers ids nobody chose by hand without making the test random.
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..256 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let len = usize::try_from(seed % 97).unwrap();
            let id: String = (0..len)
                .map(|i| {
                    // Below the surrogate range, so every value is a char.
                    let c = seed.rotate_left(u32::try_from(i % 64).unwrap()) % 0xd800;
                    char::from_u32(u32::try_from(c).unwrap()).unwrap()
                })
                .collect();
            ids.push(id);
        }
        ids
    }

    /// The namespace a session's channels are keyed by is the namespace the
    /// relay log, erasure, maintenance and the blob tree use. If the two
    /// copies drifted, erasure would stop deleting what sessions wrote.
    #[test]
    fn the_session_namespace_is_the_relay_logs_account_key() {
        for id in ids() {
            assert_eq!(account_hash(&id), account_key(&id), "id {id:?}");
        }
    }

    /// Distinct ids land in distinct namespaces, so the equality above is not
    /// satisfied by two functions that both ignore their input.
    #[test]
    fn distinct_ids_get_distinct_namespaces() {
        let mut seen = std::collections::HashMap::new();
        for id in ids() {
            if let Some(other) = seen.insert(account_hash(&id), id.clone()) {
                assert_eq!(other, id, "two ids share a namespace");
            }
        }
        assert!(seen.len() > 200, "the id spread is not degenerate");
    }

    /// A frame written under the key a session's `account` field holds is
    /// deleted by erasing that account by id, and another account's frame is
    /// not.
    #[test]
    fn a_sessions_account_keys_erasure() {
        const STREAM: [u8; 16] = [0x11; 16];
        let store = Store::open(None).unwrap();
        let (alice, bob) = ("alice", "bob");
        for who in [alice, bob] {
            store
                .relay_append(
                    (account_hash(who), STREAM),
                    b"frame",
                    &[],
                    0,
                    None,
                    1,
                    1_000,
                    DurableCaps::default(),
                )
                .unwrap();
        }

        store.erase_account(alice).unwrap();

        assert_eq!(store.relay_len((account_hash(alice), STREAM)).unwrap(), 0);
        assert_eq!(
            store.relay_len((account_hash(bob), STREAM)).unwrap(),
            1,
            "another account's log is untouched"
        );
    }
}
