//! The durable log's frozen DDL, and the one-time op count the migration that
//! extended it needs.

/// The tables [`crate::relay_log`] is, apart from migrations, the only reader
/// and writer of.
///
/// Declared here rather than with the account and device DDL because the
/// statements over them are in this module: a table whose schema lives in one
/// module and whose only queries live in another is a seam nothing enforces.
/// Migration 0001 applies it, so it is frozen: change these tables by a new
/// migration.
///
/// `bytes` is the verbatim wire frame: ciphertext the relay forwards and never
/// opens.
pub(crate) const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS relay_frames (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    account_h  BLOB NOT NULL,
    stream_id  BLOB NOT NULL,
    bytes      BLOB NOT NULL,
    n_bytes    INTEGER NOT NULL,
    created_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS relay_frames_by_channel
    ON relay_frames(account_h, stream_id, id);

-- Routing heads, read from each op's cleartext envelope header. This is the
-- only part of a frame the relay ever parses, and it is what makes
-- cursor-filtered replay possible without opening the ciphertext.
CREATE TABLE IF NOT EXISTS relay_frame_heads (
    frame_id  INTEGER NOT NULL REFERENCES relay_frames(id) ON DELETE CASCADE,
    device_id BLOB NOT NULL,
    max_seq   INTEGER NOT NULL,
    PRIMARY KEY (frame_id, device_id)
);

-- Per-channel, per-device high-water mark of what retention has deleted.
-- Survives restart, which is the whole point: an in-memory watermark cannot
-- tell 'never held it' from 'evicted it', so a fresh process reported no gaps
-- and the loss was silent.
CREATE TABLE IF NOT EXISTS relay_evicted (
    account_h       BLOB NOT NULL,
    stream_id       BLOB NOT NULL,
    device_id       BLOB NOT NULL,
    evicted_through INTEGER NOT NULL,
    PRIMARY KEY (account_h, stream_id, device_id)
);

-- One row per op batch the relay has already appended, keyed by the CONTENT of
-- the batch rather than by its `batch_id`. The client's counter is per session
-- (`sync_driver.rs` starts it at 0 inside `session()`), so it restarts at 1 on
-- every reconnect: a `UNIQUE (account_h, device_id, batch_id)` would drop
-- session 2's batch 1 as a duplicate *while acking it*, and an acked batch is
-- deleted from the client's outbox. That is silent data loss. Content is the
-- only key that survives a reconnect, and a reconnect re-draining the outbox
-- is exactly the churn this table exists to absorb.
--
-- `frame_id` is what bounds it: `ON DELETE CASCADE` plus `PRAGMA foreign_keys`
-- means a batch is forgotten the moment retention deletes the frame it named,
-- so the dedup window is the retention window, kept in step for free and with
-- no second sweep to write.
CREATE TABLE IF NOT EXISTS relay_batches (
    account_h     BLOB NOT NULL,
    stream_id     BLOB NOT NULL,
    ops_h         BLOB NOT NULL,
    frame_id      INTEGER NOT NULL REFERENCES relay_frames(id) ON DELETE CASCADE,
    batch_id      INTEGER NOT NULL,
    first_seen_ms INTEGER NOT NULL,
    PRIMARY KEY (account_h, stream_id, ops_h)
);
CREATE INDEX IF NOT EXISTS relay_batches_by_frame ON relay_batches(frame_id);";

/// How many ops a stored frame carries: the length of its `OpBatch`
/// payload's op list, and 0 for any other frame or one that does not decode.
///
/// Used once per frame, by the migration that gave `relay_frames` its
/// `n_ops` column, for the frames stored before it. Every frame stored since
/// is counted by its publisher, which has already decoded the batch.
pub(crate) fn frame_op_count(bytes: &[u8]) -> u64 {
    use sunrise_wire_protocol::{decode_frame, MsgKind, OpBatchPayload};
    match decode_frame(bytes) {
        Ok((header, payload)) if header.msg_kind == MsgKind::OpBatch => {
            OpBatchPayload::decode(&payload).map_or(0, |batch| batch.ops.len() as u64)
        }
        _ => 0,
    }
}
