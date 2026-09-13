//! The upstream publish path and its content addressing.
//!
//! Every symbol here exists to turn one request body into a relay frame plus
//! the routing heads that frame is filtered on: [`ops`] encodes and appends,
//! [`frame_heads`] reads the per-device high-water marks out of the envelopes,
//! [`batch_ops_hash`] is the dedup key, and [`overlaps_stored`] with
//! [`frame_floors`] is the measurement ADR-0033's revisit trigger asks for.

use crate::api::error::ApiError;
use crate::api::signed::Signed;
use crate::relay::{FrameHead, RelayFrame};
use crate::relay_log::Appended;
use crate::state::ServerState;
use base64::Engine as _;
use kynos::di::inject::Inject;
use kynos::extract::body::json::Json;
use kynos::extract::params::header::Headers;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use sunrise_error::ErrorCode;
use sunrise_wire_protocol::{encode_frame, FrameFlags, MsgKind, OpBatchPayload};

use super::credential::{resolve, SessionHeader};
use super::cursors::parse_id;

/// `POST /api/v1/sync/ops` request body — one `OpBatch`.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct OpsRequest {
    /// The stream the batch targets, 32 hex characters.
    pub stream_id: String,
    /// Client-generated idempotency key for the batch.
    pub batch_id: u64,
    /// The already-encoded `OpEnvelope`s, base64 standard, one per element.
    /// The server never decodes their ciphertext; it reads only the cleartext
    /// routing head to tag the frame for cursor filtering.
    pub ops: Vec<String>,
}

/// `POST /api/v1/sync/ops` response body — the `Ack`.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct OpsResponse {
    /// Echoes the acked batch's `batch_id`.
    pub batch_id: u64,
    /// The stream the batch targeted.
    pub stream_id: String,
    /// Server wall-clock at which the batch was first seen, for clock-skew
    /// clamping.
    pub server_first_seen_ms: u64,
}

/// Publish one batch of ops.
///
/// Durable first, and only then the ack. Acking an op the server has not
/// committed would promise a durability that does not exist — and the client
/// drops an acked op from its outbox, so the op would be gone from both sides
/// at once. A storage failure is reported as a 503 with nothing acked, which
/// leaves the batch in the outbox to retry.
///
/// **Re-submitting a batch is safe and is not a second append.** A batch whose
/// ops the channel already holds is acked without being stored or fanned out
/// again, and the ack carries `server_first_seen_ms` from the **first** copy —
/// so a client that re-sends must not read that field as "now". The client
/// cannot avoid re-sending: a session that dies between the append and the ack
/// leaves the batch in the outbox, and every reconnect re-drains it.
///
/// The key is the batch's **content**, not its `batch_id`, and it is the
/// *whole* batch. A re-send that adds an op — one the user made between a lost
/// ack and the reconnect — is different content and therefore a fresh append,
/// so the ops it repeats are stored twice. Re-applying them is harmless,
/// because ops are idempotent; what it costs is the relay's disk and fan-out.
/// A client that wants to avoid that persists its batch partition rather than
/// re-grouping its outbox.
//
// See `batch_ops_hash` for why the key is the content rather than the
// `batch_id`.
//
// The key is the *whole* batch, which bounds what "already seen" can mean. Two
// of the three re-send shapes are covered: an in-session retransmit replays
// `InflightBatch.frame` verbatim, and a reconnect that adds nothing to the
// outbox re-drains the same ops. An **active** client is not.
// `Core::sync_outbox_grouped` puts every unacked op for a stream into one batch
// with no size cap, so a user who edits between a lost ack and the reconnect
// makes session 2 send `[O1, O2]` where session 1 sent `[O1]`: different
// content, a `Fresh` append, and `O1` stored twice. Re-applying it is harmless
// — ops are idempotent — but the relay pays the disk and fan-out cost, and
// nothing upstream prevents it. That is the stated guarantee rather than a
// pending gap: ADR-0033 (`docs/11-adr/0033-relay-batch-dedup-is-whole-batch.md`)
// rejects a per-op key here — the relay stores frames, not ops, so saving the
// disk would mean filtering an op out and re-deriving the heads, and a per-op
// table would need its own tie to retention, because forgetting an op id is
// precisely what lets a legitimate replay through — and names the client-side
// fix (persist the partition in the outbox) as the cheaper one if it is ever
// measured to matter.
//
// The paragraphs above this one are `///` and reach the published
// `description`, because a client author needs them to write a correct
// retry. Everything below stays `//`: it is the argument for the design
// rather than the contract, and an operation description is not where a
// reader should meet ADR-0033.
#[kynos::post("/api/v1/sync/ops", operation_id = "publishOps")]
pub async fn ops(
    Inject(state): Inject<ServerState>,
    Headers(header): Headers<SessionHeader>,
    Signed {
        caller,
        value: body,
    }: Signed<OpsRequest>,
) -> Result<Json<OpsResponse>, ApiError> {
    let now_ms = state.clock.now_ms();
    let (_, session) = resolve(&state, &header, &caller, now_ms)?;
    let stream_id = parse_id(&body.stream_id, "stream_id")?;

    let ops = body
        .ops
        .iter()
        .map(|o| {
            base64::engine::general_purpose::STANDARD
                .decode(o)
                .map_err(|_| ApiError::validation("ops entries must be base64"))
        })
        .collect::<Result<Vec<Vec<u8>>, ApiError>>()?;

    // Rebuild the frame the socket used to receive, so everything downstream —
    // durable append, head extraction, fan-out, replay — is the code that
    // already exists rather than a second encoding of the same thing.
    let batch = OpBatchPayload {
        ops,
        batch_id: body.batch_id,
        stream_id,
    };
    let payload = batch
        .encode()
        .map_err(|_| ApiError::validation("batch could not be encoded"))?;
    let frame = encode_frame(MsgKind::OpBatch, FrameFlags::EMPTY, &payload)
        .map_err(|_| ApiError::validation("batch could not be framed"))?;

    let heads = frame_heads(&batch);
    let ops_h = batch_ops_hash(&batch.ops);

    // Read before the append, and only useful before it: afterwards this
    // batch's own ops are what the channel holds, and every append would look
    // like a re-send of itself. One indexed read on the publish path is the
    // price of the measurement ADR-0033's revisit trigger names; a read that
    // fails costs the measurement and never the append, which is about to fail
    // on its own and say so.
    let stored_heads = state
        .store
        .relay_device_heads((session.account, stream_id))
        .unwrap_or_default();

    let appended = state
        .store
        .relay_append(
            (session.account, stream_id),
            &frame,
            &heads,
            ops_h.as_ref(),
            body.batch_id,
            now_ms,
            state.durable_caps,
        )
        .map_err(|e| {
            tracing::error!(
                ev = "srv.relay.append_failed",
                err_code = %ErrorCode::RelayStorageUnavailable,
                err_kind = "transient",
                retryable = true,
                result = "failed",
                stream_h = %crate::logging::id_h(&stream_id),
                cause = %e,
                "could not persist an op batch; refusing to ack it"
            );
            state.metrics.incr("sunrise_relay_append_failed_total");
            ApiError::unavailable("relay could not durably store the batch")
        })?;

    let first_seen_ms = match appended {
        Appended::Fresh { first_seen_ms } => {
            tracing::debug!(
                ev = "srv.relay.fanout",
                stream_h = %crate::logging::id_h(&stream_id),
                n_bytes = frame.len() as u64,
                "op batch republished"
            );
            state.relay.publish(
                (session.account, stream_id),
                RelayFrame {
                    from: session.conn,
                    bytes: frame,
                    heads,
                },
            );
            // Fresh by the whole-batch key, and yet carrying ops this channel
            // already holds: the re-partitioned re-send ADR-0033 accepted and
            // could not see. Counted, never refused — the batch is stored and
            // fanned out exactly as before.
            if overlaps_stored(&batch, &stored_heads) {
                state.metrics.incr("sunrise_relay_batch_overlap_total");
            }
            first_seen_ms
        }
        // No publish: a second fan-out would hand every live subscriber an op
        // it already applied. Idempotent, but it is bandwidth and log noise
        // proportional to how flaky the *sender's* link is.
        Appended::Duplicate { first_seen_ms } => {
            state.metrics.incr("sunrise_relay_batch_duplicate_total");
            tracing::debug!(
                ev = "srv.relay.batch_duplicate",
                stream_h = %crate::logging::id_h(&stream_id),
                batch_id = body.batch_id,
                first_seen_ms,
                "op batch already stored; re-acking the first copy"
            );
            first_seen_ms
        }
    };

    Ok(Json(OpsResponse {
        batch_id: body.batch_id,
        stream_id: body.stream_id,
        server_first_seen_ms: first_seen_ms,
    }))
}

/// Whether this batch re-sends an op the channel already holds.
///
/// The question ADR-0033's revisit trigger asks, and the one the whole-batch
/// content key cannot answer: a re-send that re-partitions its ops hashes
/// differently, so it is stored and fanned out as new work.
/// `sunrise_relay_batch_duplicate_total` counts the re-sends the key *did*
/// catch, which is the case already handled; this is the near miss.
///
/// Answered from the sequence numbers rather than from per-op identity,
/// because per-op identity is exactly what the relay declined to store. A
/// device's ops reach a channel in sequence order, so the channel's highest
/// sequence for a device is also the boundary of what it has already been
/// sent: an op at or below it has been here before. That reads a re-partition
/// exactly, and it is one-directional about the failure it can have — a batch
/// mixing old ops with new ones is counted, and nothing new is ever counted as
/// old.
fn overlaps_stored(batch: &OpBatchPayload, stored: &HashMap<[u8; 16], u64>) -> bool {
    frame_floors(batch)
        .iter()
        .any(|(device, floor)| stored.get(device).is_some_and(|head| floor <= head))
}

/// The lowest sequence this batch carries for each device.
///
/// The mirror of [`frame_heads`], and undecodable ops are skipped by both: a
/// frame the relay cannot parse is never filtered on, and it is not measured
/// on either.
fn frame_floors(batch: &OpBatchPayload) -> HashMap<[u8; 16], u64> {
    let mut lowest: HashMap<[u8; 16], u64> = HashMap::new();
    for op in &batch.ops {
        if let Ok(head) = sunrise_cbor::decode_envelope_header(op) {
            let slot = lowest.entry(head.device_id).or_insert(u64::MAX);
            *slot = (*slot).min(head.seq);
        }
    }
    lowest
}

/// Domain-separated content hash of a batch's ops, or `None` for an empty one.
///
/// The relay dedups on this rather than on the request's `batch_id` because the
/// `batch_id` is not durable: `sync_driver.rs` initialises the counter inside
/// `session()`, so it restarts at 1 on every reconnect and a key containing it
/// would read session 2's first batch as session 1's — dropping it while acking
/// it, which is how an acked batch disappears from the client's outbox and the
/// server at once.
///
/// The hash covers the op count and each op's length before its bytes, so no
/// re-partitioning of the same concatenated ops collides with another batch.
/// `None` for an empty batch: there is no content to be the same as, and three
/// empty batches are three events.
fn batch_ops_hash(ops: &[Vec<u8>]) -> Option<[u8; 32]> {
    if ops.is_empty() {
        return None;
    }
    let mut h = blake3::Hasher::new();
    h.update(b"sunrise.relay.batch.v1");
    h.update(&(ops.len() as u64).to_le_bytes());
    for op in ops {
        h.update(&(op.len() as u64).to_le_bytes());
        h.update(op);
    }
    Some(*h.finalize().as_bytes())
}

/// The per-device high-water marks a batch carries, read from the envelopes'
/// cleartext routing headers. The ciphertext is never touched.
fn frame_heads(batch: &OpBatchPayload) -> Vec<FrameHead> {
    let mut highest: HashMap<[u8; 16], u64> = HashMap::new();
    for op in &batch.ops {
        if let Ok(head) = sunrise_cbor::decode_envelope_header(op) {
            let slot = highest.entry(head.device_id).or_insert(0);
            *slot = (*slot).max(head.seq);
        }
    }
    let mut out: Vec<FrameHead> = highest
        .into_iter()
        .map(|(device_id, max_seq)| FrameHead { device_id, max_seq })
        .collect();
    out.sort_by_key(|h| h.device_id);
    out
}
