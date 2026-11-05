//! `DELETE /api/v1/blobs/{blob_id}`: the tombstone a detaching op asks for.

use super::{account, not_found, BlobIdPath};
use crate::api::error::ApiError;
use crate::api::ids::{parse_blob_id, parse_hex16};
use crate::api::signed::Signed;
use crate::state::ServerState;
use kynos::di::inject::Inject;
use kynos::extract::body::json::Json;
use kynos::extract::params::path::Path;
use serde::{Deserialize, Serialize};

/// `DELETE /api/v1/blobs/{blob_id}` request body: the op that detached the
/// blob, named by the cleartext routing head the relay already reads off
/// every op.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct BlobDeleteRequest {
    /// The stream the detaching op was published in, 32 lowercase hex.
    #[schema(pattern = "^[0-9a-f]{32}$")]
    pub stream_id: String,
    /// The device that published it — the op's routing `device_id`, as
    /// subscribe cursors spell it: 32 lowercase hex.
    #[schema(pattern = "^[0-9a-f]{32}$")]
    pub device_id: String,
    /// The op's `seq` from that device.
    pub seq: u64,
}

/// `DELETE /api/v1/blobs/{blob_id}` response body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct BlobDeleteResponse {
    /// The earliest the ciphertext may be reclaimed, ms since the epoch. It
    /// is reclaimed on the first maintenance pass after this at which every
    /// active device has acknowledged the op.
    pub collect_after_ms: u64,
}

/// Tombstone a committed blob.
///
/// The ciphertext stays readable until it is collected: a device that has not
/// yet applied the detach may still fetch it, which is the case the quorum in
/// `docs/02-domain/attachments.md` §Deletion exists for. Tombstoning a blob
/// already tombstoned keeps the first tombstone and its clock.
#[kynos::delete("/api/v1/blobs/{blob_id}", operation_id = "deleteBlob")]
pub async fn delete(
    Inject(state): Inject<ServerState>,
    Path(path): Path<BlobIdPath>,
    Signed {
        caller,
        value: body,
    }: Signed<BlobDeleteRequest>,
) -> Result<kynos::response::status::Accepted<Json<BlobDeleteResponse>>, ApiError> {
    let blob = parse_blob_id(&path.blob_id.0)?;
    let stream_id = parse_hex16(&body.stream_id, "stream_id")?;
    let origin_device = parse_hex16(&body.device_id, "device_id")?;
    // The same `404` as a fetch, for the same reason: no oracle for whether
    // another account holds this ciphertext.
    state
        .blobs
        .manifest(account(&caller), blob)
        .await?
        .ok_or_else(not_found)?;

    let now_ms = state.clock.now_ms();
    let account_id = &caller.principal.account.account_id;
    state
        .store
        .tombstone_blob(
            account_id,
            &crate::store::NewTombstone {
                blob_key: blob,
                stream_id,
                origin_device,
                seq: body.seq,
                deleted_by: caller.device.as_ref().map(|d| d.device_id.clone()),
            },
            now_ms,
        )
        .await?;
    tracing::info!(
        ev = "srv.blob.tombstoned",
        account_h = %crate::logging::account_h(account_id),
        blob_h = %crate::logging::id_h(&blob),
        "blob tombstoned; collected once its grace period and quorum hold"
    );
    Ok(kynos::response::status::Accepted::new(Json(
        BlobDeleteResponse {
            collect_after_ms: now_ms.saturating_add(state.config.retention().gc_grace_ms),
        },
    )))
}
