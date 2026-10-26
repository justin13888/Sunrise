//! Each stream's latest snapshot record, pinned (`/api/v1/snapshots/...`).
//!
//! Per [ADR-0068](../../../../docs/11-adr/0068-a-streams-snapshot-travels-as-a-pinned-blob.md)
//! and `docs/06-server/api.md` §Stream snapshots. A snapshot record (ADR-0059
//! §4) is the one thing a device past the relay's op retention can catch up
//! from, and it is about the size of the stream's merge state: past a few
//! thousand tasks it does not fit in a 4 MiB frame. So it does not travel as a
//! frame. The stream's compactor uploads it through the ordinary blob routes
//! and then pins it here:
//!
//! 1. `POST /blobs/init`, `PUT` each chunk, `POST /blobs/finalize`, exactly as
//!    an attachment's ciphertext travels.
//! 2. `PUT /snapshots/{stream_id}` with the committed `blob_id` makes it the
//!    stream's latest, and the relay deletes the blob the previous pin named.
//!
//! A device that needs it asks `GET /snapshots/{stream_id}` for the pin and
//! fetches the blob with `GET /blobs/{blob_id}`.
//!
//! # What the relay reads
//!
//! The record is sealed and signed by its writer, and the relay checks
//! neither. It reads one thing out of the blob before pinning it: the five
//! byte magic prefix every Sunrise structure carries in clear
//! (`docs/10-cross-cutting/protocol-versioning.md` §3), and only its kind.
//! That is what lets a later pin delete the blob an earlier one named without
//! ever deleting an attachment a buggy client pinned by mistake.
//!
//! # The latest is the last pinned
//!
//! The relay cannot order two records it cannot open, so a pin replaces the
//! one before it, whoever wrote either. Only a stream's compactor publishes,
//! so two writers race only while the election changes hands, and a reader
//! that applies an older record loses nothing: applying a snapshot is a join.

use crate::api::blobs::{account, not_found, refuse_if_pending};
use crate::api::error::ApiError;
use crate::api::ids::{hex_pattern, parse_blob_id, parse_hex16};
use crate::api::signed::{Signed, SignedParts};
use crate::blob::Manifest;
use crate::state::ServerState;
use crate::store::PinnedSnapshot;
use kynos::di::inject::Inject;
use kynos::extract::body::json::Json;
use kynos::extract::params::path::Path;
use kynos::openapi::Schema as OpenApiSchema;
use serde::{Deserialize, Serialize};
use sunrise_cbor::magic::{decode_prefix, MagicKind};

/// A stream id in a URL path: 32 lowercase hex characters, the spelling
/// `POST /blobs/init` and `DELETE /blobs/{blob_id}` already use. A newtype for
/// the reason `ids::UploadId` is one: a path parameter's pattern has to be
/// something its type states.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct StreamIdHex(pub String);

impl kynos::schema::Schema for StreamIdHex {
    fn schema(registry: &mut kynos::schema::registry::Registry) -> OpenApiSchema {
        hex_pattern(registry, "^[0-9a-f]{32}$")
    }
}

impl std::str::FromStr for StreamIdHex {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_owned()))
    }
}

impl std::fmt::Display for StreamIdHex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Its schema is one string, so one path segment carries it.
impl kynos::schema::ParamValue for StreamIdHex {}

/// `{stream_id}` in a snapshot path.
#[derive(Debug, Clone, Deserialize, kynos::PathParams, kynos::Schema)]
pub struct StreamPath {
    /// The stream whose snapshot this is.
    pub stream_id: StreamIdHex,
}

/// `PUT /api/v1/snapshots/{stream_id}` request body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct PinSnapshotRequest {
    /// The committed blob holding the record: `blb_` + 32 lowercase hex
    /// characters, as `POST /blobs/finalize` returned it.
    #[schema(pattern = "^blb_[0-9a-f]{32}$")]
    pub blob_id: String,
}

/// A stream's pinned snapshot: the body of both snapshot operations.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct StreamSnapshot {
    /// The stream, 32 lowercase hex characters.
    #[schema(pattern = "^[0-9a-f]{32}$")]
    pub stream_id: String,
    /// The blob holding the record; `GET /blobs/{blob_id}` reads it.
    #[schema(pattern = "^blb_[0-9a-f]{32}$")]
    pub blob_id: String,
    /// The record's size in bytes.
    pub size_bytes: u64,
    /// When it was pinned, ms since the epoch, by the relay's clock.
    pub published_at_ms: u64,
}

impl StreamSnapshot {
    fn of(pin: &PinnedSnapshot) -> Self {
        Self {
            stream_id: hex::encode(pin.stream_id),
            blob_id: format!("blb_{}", hex::encode(pin.blob_key)),
            size_bytes: pin.size_bytes,
            published_at_ms: pin.published_at_ms,
        }
    }
}

/// Make a committed blob its stream's latest snapshot record.
///
/// The blob must be this account's, readable, and start with the snapshot
/// record's magic prefix. The blob the previous pin named is deleted once
/// nothing else names it: the relay keeps one record per stream.
#[kynos::put("/api/v1/snapshots/{stream_id}", operation_id = "pinStreamSnapshot")]
pub async fn pin(
    Inject(state): Inject<ServerState>,
    Path(path): Path<StreamPath>,
    Signed {
        caller,
        value: body,
    }: Signed<PinSnapshotRequest>,
) -> Result<Json<StreamSnapshot>, ApiError> {
    let stream_id = parse_hex16(&path.stream_id.0, "stream_id")?;
    let blob = parse_blob_id(&body.blob_id)?;
    refuse_if_pending(&state, &caller).await?;
    let account_key = account(&caller);
    // The same `404` as a fetch: no oracle for another account's ciphertext.
    let Manifest {
        chunk_count,
        size_bytes,
    } = state
        .blobs
        .manifest(account_key, blob)
        .await?
        .ok_or_else(not_found)?;
    let first = state
        .blobs
        .get_committed(account_key, blob, 0)
        .await?
        .ok_or_else(not_found)?;
    if chunk_count == 0 || !matches!(decode_prefix(&first), Ok(p) if p.kind == MagicKind::Snapshot)
    {
        return Err(ApiError::validation(
            "blob_id does not name a snapshot record",
        ));
    }

    let account_id = &caller.principal.account.account_id;
    let pinned = PinnedSnapshot {
        stream_id,
        blob_key: blob,
        size_bytes,
        published_by: caller.device.as_ref().map(|d| d.device_id.clone()),
        published_at_ms: state.clock.now_ms(),
    };
    let released = state.store.pin_snapshot(account_id, &pinned).await?;
    tracing::info!(
        ev = "srv.snapshot.pinned",
        account_h = %crate::logging::account_h(account_id),
        stream_h = %crate::logging::id_h(&stream_id),
        blob_h = %crate::logging::id_h(&blob),
        n_bytes = size_bytes,
        "a stream's latest snapshot record is pinned"
    );
    state.metrics.incr("sunrise_snapshot_pin_total");
    if let Some(old) = released {
        // Best effort, after the pin committed: a blob left behind costs
        // storage, never correctness, because nothing names it any more.
        match state.blobs.delete_committed(account_key, old).await {
            Ok(()) => {
                let _ = state.store.forget_blob(account_id, &old).await;
            }
            Err(e) => tracing::warn!(
                ev = "srv.snapshot.release_failed",
                account_h = %crate::logging::account_h(account_id),
                blob_h = %crate::logging::id_h(&old),
                cause = %e,
                "a replaced snapshot record could not be deleted; it is unreferenced"
            ),
        }
    }
    Ok(Json(StreamSnapshot::of(&pinned)))
}

/// The stream's pinned snapshot, or `404` when none is.
#[kynos::get("/api/v1/snapshots/{stream_id}", operation_id = "fetchStreamSnapshot")]
pub async fn latest(
    Inject(state): Inject<ServerState>,
    Path(path): Path<StreamPath>,
    SignedParts(caller): SignedParts,
) -> Result<Json<StreamSnapshot>, ApiError> {
    let stream_id = parse_hex16(&path.stream_id.0, "stream_id")?;
    let account_id = &caller.principal.account.account_id;
    let pin = state
        .store
        .pinned_snapshot(account_id, &stream_id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(StreamSnapshot::of(&pin)))
}

#[cfg(test)]
mod tests {
    use crate::api::error::codes;
    use crate::api::testing::{Client, Res, BEARER, SECOND_BEARER};
    use kynos::http::{Method, StatusCode};

    const STREAM: &str = "0123456789abcdef0123456789abcdef";

    fn hex_hash(bytes: &[u8]) -> String {
        hex::encode(blake3::hash(bytes).as_bytes())
    }

    /// A record as the relay sees one: the snapshot magic, then opaque bytes.
    fn record(tag: u8) -> Vec<u8> {
        let mut r = b"SR\x04\x00\x01".to_vec();
        r.extend(std::iter::repeat_n(tag, 64));
        r
    }

    /// The three blob steps, returning the committed blob id.
    async fn upload(client: &Client, bearer: &str, bytes: &[u8]) -> String {
        let res = client
            .send_as(
                Method::POST,
                "/api/v1/blobs/init",
                Some(bearer),
                Some(&serde_json::json!({
                    "stream_id": STREAM, "chunk_count": 1, "size_bytes": bytes.len(),
                })),
            )
            .await;
        res.assert_status(StatusCode::OK);
        let upload_id = res.json()["upload_id"].as_str().unwrap().to_owned();
        client
            .send_bytes(
                Method::PUT,
                &format!("/api/v1/blobs/{upload_id}/0"),
                "application/octet-stream",
                bytes,
                &[("authorization", bearer)],
            )
            .await
            .assert_status(StatusCode::NO_CONTENT);
        let res = client
            .send_as(
                Method::POST,
                "/api/v1/blobs/finalize",
                Some(bearer),
                Some(&serde_json::json!({
                    "upload_id": upload_id,
                    "content_hash": hex_hash(bytes),
                    "chunk_hashes": [hex_hash(bytes)],
                })),
            )
            .await;
        res.assert_status(StatusCode::OK);
        res.json()["blob_id"].as_str().unwrap().to_owned()
    }

    async fn pin(client: &Client, bearer: &str, blob_id: &str) -> Res {
        client
            .send_as(
                Method::PUT,
                &format!("/api/v1/snapshots/{STREAM}"),
                Some(bearer),
                Some(&serde_json::json!({ "blob_id": blob_id })),
            )
            .await
    }

    async fn latest(client: &Client, bearer: &str) -> Res {
        client
            .send_as(
                Method::GET,
                &format!("/api/v1/snapshots/{STREAM}"),
                Some(bearer),
                None,
            )
            .await
    }

    /// Pin a record, read the pin back, and fetch the record it names.
    #[tokio::test]
    async fn a_pinned_record_is_found_and_fetched() {
        let (client, _guard) = Client::with_blob_root();
        let bytes = record(1);
        let blob_id = upload(&client, BEARER, &bytes).await;

        let res = pin(&client, BEARER, &blob_id).await;
        res.assert_status(StatusCode::OK);
        assert_eq!(res.json()["blob_id"], blob_id.as_str());
        assert_eq!(res.json()["size_bytes"], bytes.len());

        let res = latest(&client, BEARER).await;
        res.assert_status(StatusCode::OK);
        assert_eq!(res.json()["stream_id"], STREAM);
        assert_eq!(res.json()["blob_id"], blob_id.as_str());
        let fetched = client
            .send(Method::GET, &format!("/api/v1/blobs/{blob_id}"), None)
            .await;
        fetched.assert_status(StatusCode::OK);
        assert_eq!(fetched.bytes, bytes);
    }

    /// The relay keeps one record per stream: a newer pin deletes the blob
    /// the older one named.
    #[tokio::test]
    async fn a_newer_pin_replaces_the_older_and_deletes_its_blob() {
        let (client, _guard) = Client::with_blob_root();
        let first = upload(&client, BEARER, &record(1)).await;
        let second = upload(&client, BEARER, &record(2)).await;
        pin(&client, BEARER, &first)
            .await
            .assert_status(StatusCode::OK);
        pin(&client, BEARER, &second)
            .await
            .assert_status(StatusCode::OK);

        assert_eq!(
            latest(&client, BEARER).await.json()["blob_id"],
            second.as_str()
        );
        client
            .send(Method::GET, &format!("/api/v1/blobs/{first}"), None)
            .await
            .assert_status(StatusCode::NOT_FOUND);
        client
            .send(Method::GET, &format!("/api/v1/blobs/{second}"), None)
            .await
            .assert_status(StatusCode::OK);
        assert_eq!(client.metrics.get("sunrise_snapshot_pin_total"), 2);
    }

    /// Only a snapshot record is pinned, so a later pin can never delete an
    /// attachment; a blob that is not there is the fetch route's 404.
    #[tokio::test]
    async fn only_a_committed_snapshot_record_is_pinned() {
        let (client, _guard) = Client::with_blob_root();
        let attachment = upload(&client, BEARER, b"opaque attachment ciphertext").await;
        let res = pin(&client, BEARER, &attachment).await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(res.json()["code"], codes::VALIDATION_INVALID);

        let res = pin(&client, BEARER, &format!("blb_{}", "0".repeat(32))).await;
        res.assert_status(StatusCode::NOT_FOUND);
        assert_eq!(res.json()["code"], codes::BLOB_NOT_FOUND);

        let res = latest(&client, BEARER).await;
        res.assert_status(StatusCode::NOT_FOUND);
        assert_eq!(res.json()["code"], codes::BLOB_NOT_FOUND);
    }

    /// A stream id with a second spelling would be a second pin.
    #[tokio::test]
    async fn an_uppercase_stream_id_is_refused() {
        let (client, _guard) = Client::with_blob_root();
        let res = client
            .send(
                Method::GET,
                &format!("/api/v1/snapshots/{}", STREAM.to_uppercase()),
                None,
            )
            .await;
        res.assert_status(StatusCode::BAD_REQUEST);
    }

    /// Another account neither sees the pin nor can pin a blob it does not
    /// hold.
    #[tokio::test]
    async fn a_pin_is_the_accounts_own() {
        let (client, _guard) = Client::with_blob_root_and_verifier();
        let blob_id = upload(&client, BEARER, &record(7)).await;
        pin(&client, BEARER, &blob_id)
            .await
            .assert_status(StatusCode::OK);

        latest(&client, SECOND_BEARER)
            .await
            .assert_status(StatusCode::NOT_FOUND);
        pin(&client, SECOND_BEARER, &blob_id)
            .await
            .assert_status(StatusCode::NOT_FOUND);
        assert_eq!(
            latest(&client, BEARER).await.json()["blob_id"],
            blob_id.as_str()
        );
    }
}
