//! Relay viewer grants: an owner lets one other account read one Stream
//! (ADR-0074).
//!
//! [`create`] registers a grant on the caller's own Stream under the hash of a
//! claim token; [`claim`] binds it to the first other account to present the
//! token; [`list`] shows the caller's grants on both sides; [`withdraw`]
//! removes one, by either party. A session reads through a grant by naming it
//! in a subscription ([`super::StreamSubscription::grant`]), which
//! `in_force` checks there and on every re-check of an open stream.
//!
//! The grant decides whom the relay hands a channel's ciphertext and metadata
//! to, and nothing about who can read it: the Stream key reaches the recipient
//! sealed to its identity by the owner's device. A relay that ignores every
//! check here exposes metadata, never content.

use crate::api::account_deletion::refuse_if_pending;
use crate::api::error::{codes, ApiError};
use crate::api::ids::parse_hex16;
use crate::api::signed::{Signed, SignedParts};
use crate::state::ServerState;
use crate::store::{GrantCreated, ViewerGrant, MAX_GRANTS_PER_OWNER};
use kynos::di::inject::Inject;
use kynos::extract::body::json::Json;
use kynos::extract::params::path::Path;
use kynos::response::status::{Created, NoContent};
use serde::{Deserialize, Serialize};

/// The domain the claim token is hashed under before the relay stores it.
const CLAIM_DOMAIN: &str = "sunrise.relay.viewer_grant.claim.v1";

/// `POST /api/v1/sync/grants` request body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
pub struct CreateGrantRequest {
    /// The caller's Stream to grant, 32 lowercase hex characters.
    #[schema(pattern = "^[0-9a-f]{32}$")]
    pub stream_id: String,
    /// The claim token, 32 random bytes the owner's device drew, as 64
    /// lowercase hex characters. The relay keeps only its hash; whoever
    /// presents it first, other than the owner, is bound as the recipient.
    #[schema(pattern = "^[0-9a-f]{64}$")]
    pub claim: String,
    /// When the relay stops serving the grant, ms since the epoch; absent
    /// for never. Must be in the future.
    #[serde(default)]
    pub expires_at_ms: Option<u64>,
}

/// `POST /api/v1/sync/grants/claim` request body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
pub struct ClaimGrantRequest {
    /// The claim token the owner registered, 64 lowercase hex characters.
    #[schema(pattern = "^[0-9a-f]{64}$")]
    pub claim: String,
}

/// One viewer grant, as either party sees it. The other party's account is
/// never named.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
pub struct GrantView {
    /// The grant, 32 lowercase hex characters: what a subscription names.
    #[schema(pattern = "^[0-9a-f]{32}$")]
    pub grant_id: String,
    /// The owner's Stream the grant reads, 32 lowercase hex characters.
    #[schema(pattern = "^[0-9a-f]{32}$")]
    pub stream_id: String,
    /// When the relay stops serving it, ms since the epoch; absent for never.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    /// When the owner registered it, by the relay's clock.
    pub created_ms: u64,
    /// When it was claimed, by the relay's clock; absent while unclaimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_ms: Option<u64>,
}

impl GrantView {
    fn of(g: &ViewerGrant) -> Self {
        Self {
            grant_id: hex::encode(g.grant_id),
            stream_id: hex::encode(g.stream_id),
            expires_at_ms: g.expires_at_ms,
            created_ms: g.created_ms,
            claimed_ms: g.claimed_ms,
        }
    }
}

/// `GET /api/v1/sync/grants` response body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
pub struct GrantList {
    /// The grants on the caller's own Streams, oldest first.
    pub owned: Vec<GrantView>,
    /// The grants the caller holds on other accounts' Streams, oldest first.
    pub held: Vec<GrantView>,
}

/// `{grant_id}` in a withdrawal path.
#[derive(Debug, Clone, Deserialize, kynos::PathParams, kynos::Schema)]
pub struct GrantIdPath {
    /// The grant, 32 lowercase hex characters.
    pub grant_id: String,
}

/// The stored form of a claim token: its BLAKE3 under [`CLAIM_DOMAIN`], so a
/// copy of the database claims nothing.
fn claim_hash(token: &str) -> Result<[u8; 32], ApiError> {
    if token.len() != 64
        || !token
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(ApiError::validation(
            "claim must be 64 lowercase hex characters",
        ));
    }
    let mut raw = [0u8; 32];
    hex::decode_to_slice(token, &mut raw)
        .map_err(|_| ApiError::validation("claim must be 64 lowercase hex characters"))?;
    let mut h = blake3::Hasher::new_derive_key(CLAIM_DOMAIN);
    h.update(&raw);
    Ok(*h.finalize().as_bytes())
}

/// The `404` a claim or a withdrawal of a grant the caller is not party to
/// gets, whichever case it was.
fn not_held() -> ApiError {
    ApiError::not_found(
        codes::RELAY_GRANT_REVOKED,
        "no viewer grant under that token or id for this account",
    )
}

/// The `403` a subscription, a snapshot read or a blob read naming a grant
/// not in force for the caller gets, whichever case it was.
pub(crate) fn not_in_force() -> ApiError {
    ApiError::forbidden(
        codes::RELAY_GRANT_REVOKED,
        "no viewer grant in force for this account and stream",
    )
}

/// The grant under `grant_id` if `account_id` may read through it at
/// `now_ms`: claimed by that account, unexpired, and, where `stream_id` is
/// given, naming that Stream. A blob read passes `None`: a blob id names no
/// Stream (ADR-0074).
///
/// `None` for every other case, which the caller refuses or ends a stream on
/// with `RELAY_GRANT_REVOKED`, never saying which case it was. The one check
/// every grant-routed read and stream makes, so none can drift from another.
pub(crate) async fn in_force(
    state: &ServerState,
    grant_id: &[u8; 16],
    account_id: &str,
    stream_id: Option<&[u8; 16]>,
    now_ms: u64,
) -> Result<Option<ViewerGrant>, ApiError> {
    Ok(state
        .store
        .viewer_grant(grant_id)
        .await?
        .filter(|g| stream_id.is_none_or(|s| g.stream_id == *s) && g.serves(account_id, now_ms)))
}

/// Register a viewer grant on one of the caller's Streams.
///
/// The relay draws the grant's id. Registering the same token again for the
/// same Stream and expiry answers the grant it made, so a client whose
/// response was lost retries safely. A token another grant holds is `409`,
/// and so is an owner already holding the most grants it may.
///
/// The grant is the relay's routing record only. The Stream key travels to
/// the recipient sealed to its identity, beside the token; the relay sees
/// neither the key nor the recipient's identity.
#[kynos::post("/api/v1/sync/grants", operation_id = "createViewerGrant")]
pub async fn create(
    Inject(state): Inject<ServerState>,
    Signed {
        caller,
        value: body,
    }: Signed<CreateGrantRequest>,
) -> Result<Created<Json<GrantView>>, ApiError> {
    let stream_id = parse_hex16(&body.stream_id, "stream_id")?;
    let claim_h = claim_hash(&body.claim)?;
    let now_ms = state.clock.now_ms();
    if body.expires_at_ms.is_some_and(|at| at <= now_ms) {
        return Err(ApiError::validation("expires_at_ms must be in the future"));
    }
    refuse_if_pending(&state, &caller).await?;
    let mut grant_id = [0u8; 16];
    getrandom::getrandom(&mut grant_id)
        .map_err(|_| ApiError::unavailable("the relay could not draw a grant id"))?;
    let owner_id = &caller.principal.account.account_id;
    let grant = ViewerGrant {
        grant_id,
        owner_id: owner_id.clone(),
        stream_id,
        recipient_id: None,
        expires_at_ms: body.expires_at_ms,
        created_ms: now_ms,
        claimed_ms: None,
    };
    let made = match state
        .store
        .create_viewer_grant(&grant, &claim_h, now_ms)
        .await?
    {
        GrantCreated::Created(g) | GrantCreated::Exists(g) => g,
        GrantCreated::TokenTaken => {
            return Err(ApiError::conflict(
                codes::VALIDATION_INVALID,
                "that claim token is registered to another grant; draw a new one",
            ))
        }
        GrantCreated::TooMany => {
            return Err(ApiError::conflict(
                codes::VALIDATION_INVALID,
                format!(
                    "this account already holds {MAX_GRANTS_PER_OWNER} viewer grants; \
                     withdraw one first"
                ),
            ))
        }
    };
    tracing::info!(
        ev = "srv.grant.created",
        account_h = %crate::logging::account_h(owner_id),
        stream_h = %crate::logging::id_h(&stream_id),
        "a viewer grant is registered"
    );
    Ok(Created::at(
        "/api/v1/sync/grants",
        Json(GrantView::of(&made)),
    ))
}

/// Claim a viewer grant by its token.
///
/// The first account other than the owner to present the token is bound as
/// the recipient, and is answered again on a later claim. Any other account,
/// the owner, a token nobody registered and an expired grant all get the same
/// `404 RELAY_GRANT_REVOKED`, so a claim says nothing about which tokens exist.
#[kynos::post("/api/v1/sync/grants/claim", operation_id = "claimViewerGrant")]
pub async fn claim(
    Inject(state): Inject<ServerState>,
    Signed {
        caller,
        value: body,
    }: Signed<ClaimGrantRequest>,
) -> Result<Json<GrantView>, ApiError> {
    let claim_h = claim_hash(&body.claim)?;
    refuse_if_pending(&state, &caller).await?;
    let account_id = &caller.principal.account.account_id;
    let Some(grant) = state
        .store
        .claim_viewer_grant(&claim_h, account_id, state.clock.now_ms())
        .await?
    else {
        return Err(not_held());
    };
    tracing::info!(
        ev = "srv.grant.claimed",
        account_h = %crate::logging::account_h(account_id),
        stream_h = %crate::logging::id_h(&grant.stream_id),
        "a viewer grant is claimed"
    );
    Ok(Json(GrantView::of(&grant)))
}

/// The viewer grants the caller owns and the ones it holds.
#[kynos::get("/api/v1/sync/grants", operation_id = "listViewerGrants")]
pub async fn list(
    Inject(state): Inject<ServerState>,
    SignedParts(caller): SignedParts,
) -> Result<Json<GrantList>, ApiError> {
    let account_id = &caller.principal.account.account_id;
    let (owned, held): (Vec<ViewerGrant>, Vec<ViewerGrant>) = state
        .store
        .viewer_grants(account_id)
        .await?
        .into_iter()
        .partition(|g| g.owner_id == *account_id);
    Ok(Json(GrantList {
        owned: owned.iter().map(GrantView::of).collect(),
        held: held.iter().map(GrantView::of).collect(),
    }))
}

/// Withdraw a viewer grant: by its owner, or by its recipient declining it.
///
/// A stream reading through it ends with `RELAY_GRANT_REVOKED` on its next
/// re-check. That stops the relay serving the owner's channel; what stops the
/// recipient reading what comes next is the owner's key rotation.
#[kynos::delete("/api/v1/sync/grants/{grant_id}", operation_id = "withdrawViewerGrant")]
pub async fn withdraw(
    Inject(state): Inject<ServerState>,
    Path(path): Path<GrantIdPath>,
    SignedParts(caller): SignedParts,
) -> Result<NoContent, ApiError> {
    let grant_id = parse_hex16(&path.grant_id, "grant_id")?;
    let account_id = &caller.principal.account.account_id;
    if !state
        .store
        .withdraw_viewer_grant(&grant_id, account_id)
        .await?
    {
        return Err(not_held());
    }
    tracing::info!(
        ev = "srv.grant.withdrawn",
        account_h = %crate::logging::account_h(account_id),
        "a viewer grant is withdrawn"
    );
    Ok(NoContent)
}

#[cfg(test)]
mod tests {
    /// The claim domain is part of every stored hash: renaming it would
    /// orphan every unclaimed grant.
    #[test]
    fn the_claim_hash_is_domain_separated() {
        let token = "ab".repeat(32);
        let mut h = blake3::Hasher::new_derive_key("sunrise.relay.viewer_grant.claim.v1");
        h.update(&[0xab; 32]);
        assert_eq!(super::claim_hash(&token).unwrap(), *h.finalize().as_bytes());
        assert_ne!(
            super::claim_hash(&token).unwrap(),
            *blake3::hash(&[0xab; 32]).as_bytes()
        );
        assert!(super::claim_hash(&"AB".repeat(32)).is_err());
        assert!(super::claim_hash(&"ab".repeat(31)).is_err());
    }
}
