//! The account's sealed recovery blob (`/api/v1/accounts/me/recovery_blob`):
//! the stepped-up fetch a recovery starts from, and the replacement a device
//! makes when it issues a new code (ADR-0070).
//!
//! Split out of `accounts` because the two routes share a reason to change —
//! what a recovery needs from the relay — that the account record does not.
//! The step-up both the fetch and account deletion sit behind stays there.

use crate::api::accounts::require_step_up;
use crate::api::error::ApiError;
use crate::api::signed::{Signed, SignedParts};
use crate::state::ServerState;
use crate::store::RETIRED_CHECK_LEN;
use base64::Engine as _;
use kynos::di::inject::Inject;
use kynos::extract::body::json::Json;
use serde::{Deserialize, Serialize};
use sunrise_cbor::magic::{decode_prefix, MagicKind};

/// `GET /api/v1/accounts/me/recovery_blob` response body.
///
/// The blob is the answer, and the salt and nonce the client needs to open it
/// are inside it — `magic(5) || salt(16) || nonce(24) || ct`, per
/// `docs/03-crypto/recovery.md`. Reporting them separately would be the server
/// describing a format it does not parse.
///
/// The other two members are what a recovery needs once the account has
/// rotated its identity (ADR-0070), served here because this is the one route
/// a recovering device can call: it holds a stepped-up bearer and no device
/// binding yet. Both are omitted on an account that never rotated.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct RecoveryBlobResponse {
    /// The sealed blob, base64url no-pad, exactly as it was uploaded.
    pub recovery_blob: String,
    /// The account's identity chain record, base64url no-pad, as a device
    /// pinned it with `PUT /accounts/me/identity_chain`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_chain: Option<String>,
    /// What is left of each blob a replacement retired, newest first: enough
    /// to tell the code that opened one from any other code, and no key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retired: Vec<RetiredRecoveryCode>,
}

/// One retired recovery code, as `GET /accounts/me/recovery_blob` reports it.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct RetiredRecoveryCode {
    /// The replaced blob's first 49 bytes, base64url no-pad: its magic, salt,
    /// nonce and four bytes of ciphertext over a fixed plaintext prefix.
    pub check: String,
    /// When the blob was replaced, milliseconds since the epoch, by the
    /// relay's clock.
    pub retired_at_ms: u64,
}

/// `PUT /api/v1/accounts/me/recovery_blob` request body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct ReplaceRecoveryBlobRequest {
    /// The new sealed blob, base64url no-pad. Opaque to the server beyond its
    /// magic prefix.
    pub recovery_blob: String,
}

/// `PUT /api/v1/accounts/me/recovery_blob` response body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct RecoveryBlobReplaced {
    /// Whether a different blob was in place and is now retired. `false` for
    /// a first blob, and for a retry of the blob already in place.
    pub retired: bool,
}

/// The calling account's sealed recovery blob.
///
/// # Why this route exists
///
/// It is the one thing missing between a user who has lost every device and
/// their data. `POST /api/v1/accounts` stored `recovery_blob` and nothing read
/// it back — `docs/03-crypto/recovery.md` §Implementation status called it "a
/// write-only column" — so the recovery flow's step 2 had no route to call and
/// a recovery code was not usable even once one existed.
///
/// # The step-up, and why it is not the email OTP the spec asked for
///
/// `recovery.md`:103 specifies a 6-digit email OTP in front of this fetch, and
/// `docs/00-product/non-goals.md` forbids implementing OTP delivery. #56
/// resolves the contradiction the way the rest of the product resolves every
/// authentication question: the IdP performs the ceremony and this server reads
/// the claims that describe it. The client re-runs its authorization request
/// with `max_age=0` — or `prompt=login`, or an operator-configured
/// `acr_values` — and the resulting token carries an `auth_time` this route
/// checks. [`crate::auth::step_up`] carries the full reasoning, including why
/// `exp` cannot stand in for `auth_time`.
///
/// **An ordinary bearer is refused.** That is the whole point: a stolen session
/// is exactly an ordinary bearer, and the blob plus a guessable recovery code
/// is the account.
///
/// # `403` rather than `401`
///
/// The credential is not wrong; a stronger one is required. A `401` drives a
/// client's token-refresh path, and a refresh mints a token with the same
/// `auth_time` — so the client would refresh into the identical refusal
/// forever, which is precisely the failure `AUTH_DEVICE_SIG_INVALID` exists to
/// avoid one route away. RFC 9470 defines a `401` with
/// `insufficient_user_authentication` for this, and this surface's error type
/// carries no `WWW-Authenticate` parameters to make that answer actionable, so
/// the code says it instead.
#[kynos::get("/api/v1/accounts/me/recovery_blob", operation_id = "getRecoveryBlob")]
pub async fn recovery_blob(
    Inject(state): Inject<ServerState>,
    SignedParts(caller): SignedParts,
) -> Result<Json<RecoveryBlobResponse>, ApiError> {
    let principal = &caller.principal;

    if let Err(refused) = require_step_up(&state, principal, "the recovery blob") {
        state.metrics.incr("sunrise_recovery_step_up_refused_total");
        return Err(refused);
    }

    let blob = state
        .store
        .recovery_blob(&principal.account.account_id)
        .await?
        .ok_or_else(|| {
            ApiError::not_found(
                crate::api::error::codes::RECOVERY_BLOB_NOT_FOUND,
                "this account has no recovery blob".to_owned(),
            )
        })?;

    let account_id = &principal.account.account_id;
    let identity_chain = state
        .store
        .pinned_identity_chain(account_id)
        .await?
        .map(|pin| BLOB_B64.encode(pin.chain));
    let retired = state
        .store
        .retired_recovery_blobs(account_id)
        .await?
        .into_iter()
        .map(|r| RetiredRecoveryCode {
            check: BLOB_B64.encode(r.check),
            retired_at_ms: r.retired_at_ms,
        })
        .collect();
    state.metrics.incr("sunrise_recovery_blob_fetch_total");
    Ok(Json(RecoveryBlobResponse {
        recovery_blob: blob,
        identity_chain,
        retired,
    }))
}

/// The alphabet every recovery blob and identity chain travels in.
const BLOB_B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Replace the calling account's sealed recovery blob (ADR-0070).
///
/// What a device that rotated the identity without carrying the recovery code
/// sends, with the blob it sealed under the new code. The blob in place is not
/// kept: only its first [`RETIRED_CHECK_LEN`] bytes are, as a retired check,
/// so a recovery with the old code can be told the code was replaced rather
/// than that it was mistyped, and gets nothing that opens.
///
/// **A device binding is required**, whatever `require_device_sig` says: a
/// blob is replaced by a registered device of the account, never by a bearer
/// alone. A replacement cannot read anything, but it can make the user's
/// written-down code stop working, which is worth the device signature.
///
/// The new blob must start with the recovery blob's magic prefix (kind
/// `0x03`); nothing else about it is read.
#[kynos::put(
    "/api/v1/accounts/me/recovery_blob",
    operation_id = "replaceRecoveryBlob"
)]
pub async fn replace_recovery_blob(
    Inject(state): Inject<ServerState>,
    Signed {
        caller,
        value: body,
    }: Signed<ReplaceRecoveryBlobRequest>,
) -> Result<Json<RecoveryBlobReplaced>, ApiError> {
    if caller.device.is_none() {
        return Err(ApiError::device_sig_invalid());
    }
    crate::api::blobs::refuse_if_pending(&state, &caller).await?;
    let account_id = &caller.principal.account.account_id;
    let offered = body.recovery_blob.trim();
    let bytes = BLOB_B64
        .decode(offered)
        .map_err(|_| ApiError::validation("recovery_blob is not base64url"))?;
    if bytes.len() <= RETIRED_CHECK_LEN
        || !matches!(decode_prefix(&bytes), Ok(p) if p.kind == MagicKind::RecoveryBlob)
    {
        return Err(ApiError::validation(
            "recovery_blob is not a sealed recovery blob",
        ));
    }
    let existing = state.store.recovery_blob(account_id).await?;
    if existing.as_deref() == Some(offered) {
        return Ok(Json(RecoveryBlobReplaced { retired: false }));
    }
    let check = existing
        .as_deref()
        .and_then(|old| BLOB_B64.decode(old).ok())
        .filter(|old| old.len() >= RETIRED_CHECK_LEN)
        .map(|mut old| {
            old.truncate(RETIRED_CHECK_LEN);
            old
        });
    state
        .store
        .replace_recovery_blob(account_id, offered, check.as_deref(), state.clock.now_ms())
        .await?;
    tracing::info!(
        ev = "srv.recovery_blob.replaced",
        account_h = %crate::logging::account_h(account_id),
        result = if check.is_some() { "retired" } else { "first" },
        "the account's recovery blob was replaced by one of its devices"
    );
    state.metrics.incr("sunrise_recovery_blob_replace_total");
    Ok(Json(RecoveryBlobReplaced {
        retired: check.is_some(),
    }))
}
