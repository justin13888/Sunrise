//! Durable, bounded, per-channel ciphertext log behind the in-memory relay
//! ring.
//!
//! # Why this exists
//!
//! The retained ring in [`crate::relay`] is memory-only. That left two ways to
//! lose data outright:
//!
//! - A device offline long enough for the ring to turn over came back to a
//!   `SYNC_CURSOR_GAP` it could not resolve, because nothing else held the ops.
//! - A relay **restart** dropped every retained frame *and* every
//!   `evicted_through` watermark together. A fresh hub cannot tell "never held
//!   it" from "evicted it", so it reported no gap at all: the returning device
//!   was told it was caught up while ops were missing. No client-side change
//!   can fix that one — the client is being told a falsehood by the only party
//!   that knows better.
//!
//! This module makes the disk the authority for replay and leaves the ring as
//! what it is good at: live fan-out to already-connected sessions.
//!
//! # What the relay may look at
//!
//! Frames are stored as the verbatim bytes the relay received and forwards.
//! The only thing ever parsed out of them is the per-op routing head
//! (`device_id`, `seq`) via [`sunrise_cbor::decode_envelope_header`], which
//! reads the envelope's cleartext routing fields and never touches the
//! ciphertext field. Storing opaque bytes it already relays is not a new
//! privilege: `docs/06-server/overview.md` lists reading op *contents* as the
//! non-responsibility, and durable encrypted op storage as an explicit
//! responsibility.
//!
//! # Retention
//!
//! Two bounds per channel, both enforced on append, whichever bites first:
//!
//! - **Age**, [`DEFAULT_MAX_AGE_MS`] (30 days) — the number every other
//!   retention window in `docs/06-server/relay-and-blob-storage.md` already
//!   uses. A device gone longer than that is a re-pair, not a resync.
//! - **Size**, [`DEFAULT_MAX_BYTES`] per channel — so one busy stream cannot
//!   fill an operator's disk, and the worst case is bounded by
//!   `channels × max_bytes` rather than by how long the server has been up.
//!
//! Deleting under either bound raises `evicted_through` exactly as the ring
//! does, so a cursor below it still produces the typed `SYNC_CURSOR_GAP`. The
//! error does not go away; it stops being routine.

use rusqlite::params;

mod append;
mod replay;
mod schema;

pub use append::{Appended, DurableCaps, DEFAULT_MAX_AGE_MS, DEFAULT_MAX_BYTES};
pub use replay::{Replay, ReplayFrame};
pub(crate) use schema::{frame_op_count, SCHEMA};

/// The relay's namespace for an account, which keys `relay_frames.account_h`;
/// defined with every other per-account store's key in `account_ns`.
pub use crate::account_ns::account_key;

/// Delete every row this module holds for one account, inside the caller's
/// transaction, returning the frames removed.
///
/// No foreign key reaches these tables from `accounts` — they are keyed by the
/// hash, not the id — so account erasure has to name them. Heads and batches go
/// with their frames by cascade; the batches are deleted explicitly as well so
/// the erasure does not lean on `PRAGMA foreign_keys` alone.
pub(crate) fn erase_account(
    tx: &rusqlite::Transaction<'_>,
    account_h: [u8; 16],
) -> rusqlite::Result<u64> {
    tx.execute(
        "DELETE FROM relay_batches WHERE account_h = ?1",
        params![&account_h[..]],
    )?;
    tx.execute(
        "DELETE FROM relay_evicted WHERE account_h = ?1",
        params![&account_h[..]],
    )?;
    let frames = tx.execute(
        "DELETE FROM relay_frames WHERE account_h = ?1",
        params![&account_h[..]],
    )?;
    Ok(frames as u64)
}

/// Frames and ciphertext bytes the durable log holds, for `admin stats`.
pub(crate) fn totals(conn: &rusqlite::Connection) -> rusqlite::Result<(u64, u64)> {
    conn.query_row(
        "SELECT COUNT(*), IFNULL(SUM(n_bytes), 0) FROM relay_frames",
        [],
        |r| {
            Ok((
                u64::try_from(r.get::<_, i64>(0)?).unwrap_or(0),
                u64::try_from(r.get::<_, i64>(1)?).unwrap_or(0),
            ))
        },
    )
}

/// The fixtures [`append`]'s and [`replay`]'s tests share.
#[cfg(test)]
mod test_support {
    use std::collections::HashMap;

    use super::DurableCaps;
    use crate::relay::{FrameHead, StreamKey};
    use crate::store::Store;

    pub(super) const ACC: [u8; 16] = [0xa1; 16];
    pub(super) const STREAM: [u8; 16] = [0x11; 16];
    pub(super) const DEV: [u8; 16] = [0x22; 16];
    pub(super) const KEY: StreamKey = (ACC, STREAM);

    pub(super) fn store() -> Store {
        Store::open(None).unwrap()
    }

    pub(super) fn head(seq: u64) -> Vec<FrameHead> {
        vec![FrameHead {
            device_id: DEV,
            max_seq: seq,
        }]
    }

    pub(super) fn cursors(n: u64) -> HashMap<[u8; 16], u64> {
        let mut m = HashMap::new();
        m.insert(DEV, n);
        m
    }

    pub(super) fn big() -> DurableCaps {
        DurableCaps {
            max_bytes: u64::MAX,
            max_age_ms: u64::MAX,
        }
    }
}
