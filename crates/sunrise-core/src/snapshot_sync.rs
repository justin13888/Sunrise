//! A stream's snapshot record, carried to the devices that need it (issue
//! #462, [ADR-0068](../../../docs/11-adr/0068-a-streams-snapshot-travels-as-a-pinned-blob.md)).
//!
//! The record is written and applied by the engine (ADR-0059 §4). This module
//! is the two legs of its journey, both driven by the sync driver over the one
//! transport it holds:
//!
//! - **Publish.** The stream's compactor uploads its latest record through the
//!   blob routes, chunked at the relay's chunk ceiling, and pins it as the
//!   stream's latest. A record is a blob and not a frame because it outgrows
//!   the 4 MiB frame cap past a few thousand tasks.
//! - **Recover.** A device the relay answers with `SYNC_CURSOR_GAP` — one
//!   offline past the relay's op retention, or a new device joining an old
//!   account, whose empty cursors are below everything retained — fetches the
//!   stream's pinned record, checks it is the blob the pin names, and applies
//!   it. Applying it raises the stream's floors and cursors to its frontier,
//!   so the re-subscribe that follows asks the relay only for what it still
//!   holds.
//!
//! What the record proves is the engine's to check: its signature, its
//! writer, its frontier against this replica's own chain. This module checks
//! only that the bytes are the ones the relay pinned.

use sunrise_sync::{Transport, TransportError};

use crate::core::{Core, CoreError};
use crate::engine::{hex_short, EngineError, SnapshotApplied};

/// The largest chunk a record is uploaded in: the relay's per-chunk ceiling
/// (`crates/sunrise-server/src/api/blobs.rs#MAX_CHUNK_BYTES`).
const CHUNK_BYTES: usize = 1024 * 1024;

/// A record this replica should publish.
#[derive(Debug, Clone)]
pub(crate) struct Publishable {
    /// The stream it is a snapshot of.
    pub(crate) stream: [u8; 16],
    /// Its content address on the relay, [`blob_id_of`] the record.
    pub(crate) blob_id: [u8; 16],
    /// The record, verbatim.
    pub(crate) record: Vec<u8>,
}

/// A record's address on the relay: the first sixteen bytes of BLAKE3 over
/// it, which is what the relay content-addresses a committed blob by.
pub(crate) fn blob_id_of(record: &[u8]) -> [u8; 16] {
    let hash = sunrise_crypto::ciphertext_hash(std::iter::once(record));
    let mut id = [0u8; 16];
    id.copy_from_slice(&hash[..16]);
    id
}

/// Publish every snapshot record this replica should (see
/// [`Core::snapshots_to_publish`]).
///
/// Failures are logged and left for the next tick, which asks again: nothing
/// is queued, because the record itself is the durable state and the relay's
/// pin is what says whether it arrived.
pub(crate) async fn publish<T: Transport + ?Sized>(core: &Core, transport: &mut T) {
    let Ok(due) = core.snapshots_to_publish() else {
        return;
    };
    for Publishable {
        stream,
        blob_id,
        record,
    } in due
    {
        match publish_one(transport, &stream, &blob_id, &record).await {
            Ok(uploaded) => {
                core.note_snapshot_published(stream, blob_id);
                if uploaded {
                    tracing::info!(
                        ev = "sync.snapshot.published",
                        stream_h = hex_short(&stream),
                        blob_h = hex_short(&blob_id),
                        n_bytes = record.len() as u64,
                        "this stream's snapshot record is pinned on the relay for devices that need it"
                    );
                }
            }
            Err(TransportError::Unsupported) => return,
            Err(e) => tracing::warn!(
                ev = "sync.snapshot.publish_failed",
                err_code = "SYNC_NETWORK_UNAVAILABLE",
                err_kind = "transient",
                retryable = true,
                stream_h = hex_short(&stream),
                cause = %e,
                "a snapshot record did not reach the relay; the next tick tries again"
            ),
        }
    }
}

/// Upload and pin one record. `Ok(false)` when the relay already pinned it.
async fn publish_one<T: Transport + ?Sized>(
    transport: &mut T,
    stream: &[u8; 16],
    blob_id: &[u8; 16],
    record: &[u8],
) -> Result<bool, TransportError> {
    if transport
        .snapshot_latest(stream)
        .await?
        .is_some_and(|pin| pin.blob_id == *blob_id)
    {
        return Ok(false);
    }
    let chunks: Vec<&[u8]> = record.chunks(CHUNK_BYTES).collect();
    let count = u32::try_from(chunks.len()).unwrap_or(u32::MAX);
    let upload = transport
        .blob_init(stream, count, record.len() as u64)
        .await?;
    for (idx, chunk) in chunks.iter().enumerate() {
        let idx = u32::try_from(idx).unwrap_or(u32::MAX);
        transport.blob_put_chunk(&upload, idx, chunk).await?;
    }
    let hashes: Vec<[u8; 32]> = chunks
        .iter()
        .map(|c| sunrise_crypto::ciphertext_hash(std::iter::once(*c)))
        .collect();
    let whole = sunrise_crypto::ciphertext_hash(std::iter::once(record));
    let commit = transport.blob_finalize(&upload, &whole, &hashes).await?;
    transport.snapshot_pin(stream, &commit.blob_id).await?;
    Ok(true)
}

/// What catching one gapped stream up from its snapshot came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Recovery {
    /// The record was applied: re-subscribe from the cursors it set.
    Applied,
    /// Not now, but it may work later: the record needs a key or a cert that
    /// another stream's catch-up brings, or the relay could not be reached.
    Deferred(&'static str),
    /// Nothing this session can do: the relay pins no record, the one it
    /// pins adds nothing, or it was refused.
    Failed(&'static str),
}

/// Fetch, check and apply `stream`'s pinned snapshot record.
pub(crate) async fn recover<T: Transport + ?Sized>(
    core: &Core,
    transport: &mut T,
    stream: [u8; 16],
) -> Recovery {
    let pin = match transport.snapshot_latest(&stream).await {
        Ok(Some(pin)) => pin,
        Ok(None) => return Recovery::Failed("no_snapshot"),
        Err(TransportError::Unsupported) => return Recovery::Failed("unsupported"),
        Err(_) => return Recovery::Deferred("transport"),
    };
    let record = match transport.blob_fetch(&pin.blob_id).await {
        Ok(Some(bytes)) => bytes,
        // Replaced by a newer pin between the two reads; the next gap asks
        // again and finds that one.
        Ok(None) => return Recovery::Deferred("not_on_relay"),
        Err(TransportError::Unsupported) => return Recovery::Failed("unsupported"),
        Err(_) => return Recovery::Deferred("transport"),
    };
    if blob_id_of(&record) != pin.blob_id {
        return Recovery::Failed("rejected");
    }
    let outcome = match core.apply_stream_snapshot(&record) {
        Ok(SnapshotApplied::Applied { .. }) => Recovery::Applied,
        Ok(SnapshotApplied::Covered) => Recovery::Failed("covered"),
        Ok(SnapshotApplied::NoKey) => Recovery::Deferred("no_key"),
        Ok(SnapshotApplied::Pending { .. }) => Recovery::Deferred("pending"),
        Err(CoreError::Engine(EngineError::UnknownDevice)) => Recovery::Deferred("unknown_device"),
        Err(_) => Recovery::Failed("rejected"),
    };
    match outcome {
        Recovery::Applied => tracing::info!(
            ev = "sync.snapshot.recovered",
            stream_h = hex_short(&stream),
            blob_h = hex_short(&pin.blob_id),
            n_bytes = record.len() as u64,
            "a stream the relay could no longer replay was caught up from its snapshot record"
        ),
        Recovery::Deferred(reason) => tracing::warn!(
            ev = "sync.snapshot.deferred",
            stream_h = hex_short(&stream),
            reason,
            "a stream's snapshot record cannot be applied yet; the next gap report tries again"
        ),
        Recovery::Failed(_) => {}
    }
    outcome
}
