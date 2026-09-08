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

use std::collections::HashMap;

use rusqlite::{params, OptionalExtension};

use crate::relay::{CursorGap, FrameHead};
use crate::store::{Store, StoreError};

/// Default per-channel age bound: 30 days, matching every other retention
/// window in the storage spec.
pub const DEFAULT_MAX_AGE_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// Default per-channel size bound.
///
/// 256 MiB of *ciphertext ops* is a very long history for one stream — the
/// in-memory ring's own bound is 16 MiB — while keeping the disk worst case
/// something a self-hoster can reason about.
pub const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Retention bounds for the durable log. Injectable so a test can reach past
/// them without writing 256 MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurableCaps {
    /// Per-channel ciphertext budget in bytes.
    pub max_bytes: u64,
    /// Per-channel age bound in milliseconds.
    pub max_age_ms: u64,
}

impl Default for DurableCaps {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
            max_age_ms: DEFAULT_MAX_AGE_MS,
        }
    }
}

/// A channel key: `(account_hash, stream_id)`, the same key the ring uses.
pub type ChannelKey = ([u8; 16], [u8; 16]);

/// One replayed frame: its durable `relay_frames.id` and its verbatim bytes.
///
/// The id is what an SSE `id:` field carries, so `Last-Event-ID` resumes from a
/// position the server can act on rather than one it has to guess.
pub type ReplayFrame = (u64, Vec<u8>);

/// What a replay yields: the frames to send, and the gaps retention made
/// unrecoverable.
pub type Replay = (Vec<ReplayFrame>, Vec<CursorGap>);

/// What [`Store::relay_append`] did with a batch.
///
/// Both arms carry `first_seen_ms`, and both are acked: the field's contract is
/// "when the server *first* saw this batch", so a re-send is answered with the
/// original timestamp rather than with now. A refusal would be worse than
/// useless — the client deletes an acked batch from its outbox and keeps an
/// un-acked one forever, so the only safe answer to "I already have this" is
/// the same ack the first copy got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appended {
    /// The batch was not in the window; it is now stored and must be fanned out.
    Fresh {
        /// Server wall-clock at which this batch was stored.
        first_seen_ms: u64,
    },
    /// The batch is already stored. Nothing was written and nothing must be
    /// republished: the subscribers that were live for the first copy already
    /// received it, and the ones that were not will get it from the durable
    /// replay.
    Duplicate {
        /// Server wall-clock at which the *first* copy was stored.
        first_seen_ms: u64,
    },
}

impl Store {
    /// Append one relayed frame and enforce retention for its channel, unless
    /// the batch is one this channel already holds.
    ///
    /// Called before the `Ack`, never after: acking an op the server has not
    /// durably stored would promise the client a durability that does not
    /// exist, and the client would then drop it from its outbox.
    ///
    /// `ops_h` is the content hash of the batch's ops, and passing it is what
    /// opts a batch into deduplication. `None` skips the check entirely, which
    /// is the right answer for a batch with no ops: an empty batch carries no
    /// content to be the same *as*, and collapsing several of them into one
    /// would silently drop keep-alive-shaped traffic the fan-out tests rely on.
    ///
    /// `batch_id` is recorded beside the hash for diagnostics only. It is
    /// deliberately not part of the key — see the `relay_batches` comment in
    /// [`crate::store`] for why keying on it loses data.
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] if the transaction cannot be committed, in which
    /// case nothing was written and the caller must refuse to ack.
    pub fn relay_append(
        &self,
        key: ChannelKey,
        bytes: &[u8],
        heads: &[FrameHead],
        ops_h: Option<&[u8; 32]>,
        batch_id: u64,
        now_ms: u64,
        caps: DurableCaps,
    ) -> Result<Appended, StoreError> {
        let (account_h, stream_id) = key;
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;

        // Inside the transaction, so the lookup and the insert cannot be
        // interleaved with another append of the same batch.
        if let Some(h) = ops_h {
            let seen: Option<i64> = tx
                .query_row(
                    "SELECT first_seen_ms FROM relay_batches
                     WHERE account_h = ?1 AND stream_id = ?2 AND ops_h = ?3",
                    params![&account_h[..], &stream_id[..], &h[..]],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(first_seen_ms) = seen {
                // Nothing to commit: the frame is already on disk from the
                // first copy, and rolling back leaves it exactly as it was.
                drop(tx);
                return Ok(Appended::Duplicate {
                    first_seen_ms: u64::try_from(first_seen_ms).unwrap_or(0),
                });
            }
        }

        tx.execute(
            "INSERT INTO relay_frames (account_h, stream_id, bytes, n_bytes, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                &account_h[..],
                &stream_id[..],
                bytes,
                i64::try_from(bytes.len()).unwrap_or(i64::MAX),
                i64::try_from(now_ms).unwrap_or(i64::MAX),
            ],
        )?;
        let frame_id = tx.last_insert_rowid();
        for h in heads {
            tx.execute(
                "INSERT OR REPLACE INTO relay_frame_heads (frame_id, device_id, max_seq)
                 VALUES (?1, ?2, ?3)",
                params![
                    frame_id,
                    &h.device_id[..],
                    i64::try_from(h.max_seq).unwrap_or(i64::MAX)
                ],
            )?;
        }
        if let Some(h) = ops_h {
            tx.execute(
                "INSERT INTO relay_batches
                   (account_h, stream_id, ops_h, frame_id, batch_id, first_seen_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    &account_h[..],
                    &stream_id[..],
                    &h[..],
                    frame_id,
                    i64::try_from(batch_id).unwrap_or(i64::MAX),
                    i64::try_from(now_ms).unwrap_or(i64::MAX),
                ],
            )?;
        }
        // After the insert, so the frame this call just stored is inside the
        // budget the eviction enforces. The cascade takes the batch rows of
        // whatever it deletes, which is what keeps the dedup window and the
        // retention window the same window.
        evict(&tx, account_h, stream_id, now_ms, caps)?;
        tx.commit()?;
        Ok(Appended::Fresh {
            first_seen_ms: now_ms,
        })
    }

    /// Every retained frame this subscriber's cursors do not already cover, in
    /// arrival order, plus the gaps retention has made unrecoverable.
    ///
    /// Filtering matches the ring's rule exactly: a frame is skipped only when
    /// it has heads and *every* head is at or below the matching cursor. A
    /// frame whose heads could not be read is never skipped, because
    /// over-delivery costs one idempotent no-op and under-delivery is data
    /// loss.
    pub fn relay_replay(
        &self,
        key: ChannelKey,
        cursors: &HashMap<[u8; 16], u64>,
    ) -> Result<(Vec<Vec<u8>>, Vec<CursorGap>), StoreError> {
        let (frames, gaps) = self.relay_replay_after(key, 0, cursors)?;
        Ok((frames.into_iter().map(|(_, bytes)| bytes).collect(), gaps))
    }

    /// [`relay_replay`](Self::relay_replay), keeping each frame's durable id and
    /// resuming after `after_id`.
    ///
    /// The id is `relay_frames.id`: durable, per-channel and monotonic in
    /// arrival order, which is exactly what an SSE `id:` field has to be for
    /// `Last-Event-ID` to mean anything. ADR-0023 calls resumption "a rename of
    /// machinery that exists", and this is the rename — the filtering rule below
    /// is untouched.
    ///
    /// `after_id` of 0 replays everything the cursors do not cover, which is a
    /// first connection. A non-zero one is a reconnect saying where it got to,
    /// and it takes precedence over the cursors for that reason: the client is
    /// reporting what it actually received on *this* stream rather than what it
    /// had applied when it subscribed.
    ///
    /// That precedence is safe only because the caller does not hand both a
    /// non-zero `after_id` and a cursor set the client has just restated. A
    /// `Last-Event-ID` presented on the first stream after a `Subscribe` is
    /// refused with `SYNC_RESUME_CONFLICT` in
    /// [`crate::api::sync::events`] before this is reached, so the two
    /// statements never race here and the id-only selection below cannot skip
    /// what a cursor asked for. Any future caller passing a non-zero
    /// `after_id` owes the same check.
    pub fn relay_replay_after(
        &self,
        key: ChannelKey,
        after_id: u64,
        cursors: &HashMap<[u8; 16], u64>,
    ) -> Result<Replay, StoreError> {
        let (account_h, stream_id) = key;
        let conn = self.conn.lock();

        let mut stmt = conn.prepare(
            "SELECT f.id, f.bytes FROM relay_frames f
             WHERE f.account_h = ?1 AND f.stream_id = ?2
             ORDER BY f.id",
        )?;
        let rows = stmt
            .query_map(params![&account_h[..], &stream_id[..]], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut heads_stmt =
            conn.prepare("SELECT device_id, max_seq FROM relay_frame_heads WHERE frame_id = ?1")?;
        let mut out = Vec::new();
        for (id, bytes) in rows {
            let seq = u64::try_from(id).unwrap_or(0);
            // A reconnect that names where it got to has said everything the
            // cursors would: it is reporting delivery on this stream rather
            // than application in its op log, which is the stricter of the two.
            if after_id > 0 {
                if seq > after_id {
                    out.push((seq, bytes));
                }
                continue;
            }
            let heads = heads_stmt
                .query_map(params![id], |r| {
                    Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            if covered(&heads, cursors) {
                continue;
            }
            out.push((seq, bytes));
        }

        let mut gap_stmt = conn.prepare(
            "SELECT device_id, evicted_through FROM relay_evicted
             WHERE account_h = ?1 AND stream_id = ?2 ORDER BY device_id",
        )?;
        let mut gaps = Vec::new();
        let evicted = gap_stmt
            .query_map(params![&account_h[..], &stream_id[..]], |r| {
                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (dev, through) in evicted {
            let Some(device_id) = to_id(&dev) else {
                continue;
            };
            let through = u64::try_from(through).unwrap_or(0);
            let cursor = cursors.get(&device_id).copied().unwrap_or(0);
            if cursor < through {
                gaps.push(CursorGap {
                    device_id,
                    cursor,
                    evicted_through: through,
                });
            }
        }
        Ok((out, gaps))
    }

    /// Number of retained frames for a channel (tests and diagnostics).
    pub fn relay_len(&self, key: ChannelKey) -> Result<usize, StoreError> {
        let (account_h, stream_id) = key;
        let conn = self.conn.lock();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM relay_frames WHERE account_h = ?1 AND stream_id = ?2",
                params![&account_h[..], &stream_id[..]],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        Ok(usize::try_from(n).unwrap_or(0))
    }
}

/// Enforce both retention bounds for one channel, raising `evicted_through`
/// for every device carried by a frame that is deleted.
fn evict(
    tx: &rusqlite::Transaction<'_>,
    account_h: [u8; 16],
    stream_id: [u8; 16],
    now_ms: u64,
    caps: DurableCaps,
) -> Result<(), StoreError> {
    let cutoff = i64::try_from(now_ms.saturating_sub(caps.max_age_ms)).unwrap_or(0);
    let mut doomed: Vec<i64> = {
        let mut s = tx.prepare(
            "SELECT id FROM relay_frames
             WHERE account_h = ?1 AND stream_id = ?2 AND created_ms < ?3",
        )?;
        let rows = s
            .query_map(params![&account_h[..], &stream_id[..], cutoff], |r| {
                r.get(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };

    // Size bound: drop oldest-first until under budget. The newest frame is
    // always kept, mirroring the ring — a single frame larger than the whole
    // budget is still delivered rather than silently dropped on arrival.
    {
        let mut s = tx.prepare(
            "SELECT id, n_bytes FROM relay_frames
             WHERE account_h = ?1 AND stream_id = ?2 ORDER BY id",
        )?;
        let rows = s
            .query_map(params![&account_h[..], &stream_id[..]], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut total: u64 = rows
            .iter()
            .map(|(_, n)| u64::try_from(*n).unwrap_or(0))
            .sum();
        let mut remaining = rows.len();
        for (id, n) in rows {
            if total <= caps.max_bytes || remaining <= 1 {
                break;
            }
            if !doomed.contains(&id) {
                doomed.push(id);
            }
            total = total.saturating_sub(u64::try_from(n).unwrap_or(0));
            remaining -= 1;
        }
    }

    for id in doomed {
        let heads: Vec<(Vec<u8>, i64)> = {
            let mut s =
                tx.prepare("SELECT device_id, max_seq FROM relay_frame_heads WHERE frame_id = ?1")?;
            let rows = s
                .query_map(params![id], |r| {
                    Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        for (dev, seq) in heads {
            // An op the relay could not parse contributes no head, so deleting
            // it raises no watermark — the same "fail toward over-delivery"
            // choice the ring makes.
            tx.execute(
                "INSERT INTO relay_evicted (account_h, stream_id, device_id, evicted_through)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(account_h, stream_id, device_id) DO UPDATE SET
                   evicted_through = MAX(evicted_through, excluded.evicted_through)",
                params![&account_h[..], &stream_id[..], dev, seq],
            )?;
        }
        tx.execute("DELETE FROM relay_frames WHERE id = ?1", params![id])?;
    }
    Ok(())
}

/// The ring's filtering rule, applied to rows read back from SQLite.
fn covered(heads: &[(Vec<u8>, i64)], cursors: &HashMap<[u8; 16], u64>) -> bool {
    if heads.is_empty() {
        return false;
    }
    heads.iter().all(|(dev, seq)| {
        to_id(dev).is_some_and(|id| {
            cursors
                .get(&id)
                .copied()
                .is_some_and(|c| u64::try_from(*seq).unwrap_or(u64::MAX) <= c)
        })
    })
}

fn to_id(v: &[u8]) -> Option<[u8; 16]> {
    <[u8; 16]>::try_from(v).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACC: [u8; 16] = [0xa1; 16];
    const STREAM: [u8; 16] = [0x11; 16];
    const DEV: [u8; 16] = [0x22; 16];
    const KEY: ChannelKey = (ACC, STREAM);

    fn store() -> Store {
        Store::open(None).unwrap()
    }

    fn head(seq: u64) -> Vec<FrameHead> {
        vec![FrameHead {
            device_id: DEV,
            max_seq: seq,
        }]
    }

    fn cursors(n: u64) -> HashMap<[u8; 16], u64> {
        let mut m = HashMap::new();
        m.insert(DEV, n);
        m
    }

    fn big() -> DurableCaps {
        DurableCaps {
            max_bytes: u64::MAX,
            max_age_ms: u64::MAX,
        }
    }

    #[test]
    fn replays_everything_when_the_subscriber_has_no_cursor() {
        let s = store();
        for seq in 1..=3 {
            s.relay_append(
                KEY,
                &[u8::try_from(seq).unwrap(); 8],
                &head(seq),
                None,
                0,
                1000,
                big(),
            )
            .unwrap();
        }
        let (frames, gaps) = s.relay_replay(KEY, &HashMap::new()).unwrap();
        assert_eq!(frames.len(), 3);
        assert!(gaps.is_empty());
    }

    #[test]
    fn a_cursor_narrows_the_replay_to_what_was_missed() {
        let s = store();
        for seq in 1..=5 {
            s.relay_append(
                KEY,
                &[u8::try_from(seq).unwrap(); 8],
                &head(seq),
                None,
                0,
                1000,
                big(),
            )
            .unwrap();
        }
        let (frames, gaps) = s.relay_replay(KEY, &cursors(3)).unwrap();
        assert_eq!(frames.len(), 2, "only seq 4 and 5 are still needed");
        assert!(gaps.is_empty(), "nothing was evicted, so nothing is a gap");
    }

    #[test]
    fn channels_do_not_bleed_into_each_other() {
        let s = store();
        s.relay_append(KEY, b"mine", &head(1), None, 0, 1000, big())
            .unwrap();
        let other = (ACC, [0x99; 16]);
        s.relay_append(other, b"theirs", &head(1), None, 0, 1000, big())
            .unwrap();
        let (frames, _) = s.relay_replay(KEY, &HashMap::new()).unwrap();
        assert_eq!(frames, vec![b"mine".to_vec()]);
    }

    #[test]
    fn the_size_bound_evicts_oldest_first_and_raises_the_watermark() {
        let s = store();
        let caps = DurableCaps {
            max_bytes: 20,
            max_age_ms: u64::MAX,
        };
        // Each frame is 8 bytes, so a 20-byte budget holds two.
        for seq in 1..=5 {
            s.relay_append(
                KEY,
                &[u8::try_from(seq).unwrap(); 8],
                &head(seq),
                None,
                0,
                1000,
                caps,
            )
            .unwrap();
        }
        assert_eq!(s.relay_len(KEY).unwrap(), 2, "budget holds two frames");

        // A subscriber at seq 1 has lost 2 and 3 for good.
        let (_, gaps) = s.relay_replay(KEY, &cursors(1)).unwrap();
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].device_id, DEV);
        assert_eq!(gaps[0].cursor, 1);
        assert_eq!(gaps[0].evicted_through, 3);
    }

    #[test]
    fn being_caught_up_past_everything_evicted_is_not_a_gap() {
        let s = store();
        let caps = DurableCaps {
            max_bytes: 20,
            max_age_ms: u64::MAX,
        };
        for seq in 1..=5 {
            s.relay_append(
                KEY,
                &[u8::try_from(seq).unwrap(); 8],
                &head(seq),
                None,
                0,
                1000,
                caps,
            )
            .unwrap();
        }
        // Otherwise every healthy long-lived session would be told to resync
        // each time retention turned over.
        let (frames, gaps) = s.relay_replay(KEY, &cursors(5)).unwrap();
        assert!(gaps.is_empty(), "caught up past the watermark is not a gap");
        assert!(frames.is_empty());
    }

    #[test]
    fn the_age_bound_evicts_and_is_reported_as_a_gap() {
        let s = store();
        let caps = DurableCaps {
            max_bytes: u64::MAX,
            max_age_ms: 1000,
        };
        s.relay_append(KEY, b"old", &head(1), None, 0, 1_000, caps)
            .unwrap();
        // Far enough past the bound that the first frame is out of window.
        s.relay_append(KEY, b"new", &head(2), None, 0, 10_000, caps)
            .unwrap();
        assert_eq!(s.relay_len(KEY).unwrap(), 1);
        let (_, gaps) = s.relay_replay(KEY, &HashMap::new()).unwrap();
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].evicted_through, 1);
    }

    #[test]
    fn an_unparseable_frame_is_never_filtered_out() {
        let s = store();
        // No heads: the relay could not read the routing header.
        s.relay_append(KEY, b"opaque", &[], None, 0, 1000, big())
            .unwrap();
        let (frames, _) = s.relay_replay(KEY, &cursors(u64::MAX)).unwrap();
        assert_eq!(
            frames.len(),
            1,
            "over-delivery is an idempotent no-op; under-delivery is data loss"
        );
    }

    #[test]
    fn evicting_an_opaque_frame_raises_no_watermark() {
        let s = store();
        let caps = DurableCaps {
            max_bytes: 8,
            max_age_ms: u64::MAX,
        };
        s.relay_append(KEY, b"opaque12", &[], None, 0, 1000, caps)
            .unwrap();
        s.relay_append(KEY, b"opaque34", &[], None, 0, 1000, caps)
            .unwrap();
        let (_, gaps) = s.relay_replay(KEY, &HashMap::new()).unwrap();
        assert!(
            gaps.is_empty(),
            "a frame with no readable heads cannot prove anything was lost"
        );
    }

    #[test]
    fn the_newest_frame_survives_a_budget_smaller_than_itself() {
        let s = store();
        let caps = DurableCaps {
            max_bytes: 1,
            max_age_ms: u64::MAX,
        };
        s.relay_append(KEY, &[7u8; 64], &head(1), None, 0, 1000, caps)
            .unwrap();
        assert_eq!(s.relay_len(KEY).unwrap(), 1);
    }

    /// The dedup window is the retention window, and nothing wires them
    /// together but the cascade — so this is the test that proves the cascade
    /// fires. A batch remembered past the frame it named would refuse to
    /// re-store ops the relay no longer holds, and the client would delete
    /// them from its outbox on the strength of that ack.
    #[test]
    fn eviction_forgets_the_batch_it_deduped_on() {
        let s = store();
        let caps = DurableCaps {
            max_bytes: 20,
            max_age_ms: u64::MAX,
        };
        let a: [u8; 32] = [0xaa; 32];
        let b: [u8; 32] = [0xbb; 32];

        assert!(matches!(
            s.relay_append(KEY, &[1u8; 16], &head(1), Some(&a), 1, 1000, caps)
                .unwrap(),
            Appended::Fresh { .. }
        ));
        assert!(matches!(
            s.relay_append(KEY, &[1u8; 16], &head(1), Some(&a), 1, 1000, caps)
                .unwrap(),
            Appended::Duplicate { .. }
        ));

        // 16-byte frames under a 20-byte budget: appending B evicts A.
        assert!(matches!(
            s.relay_append(KEY, &[3u8; 16], &head(3), Some(&b), 2, 2000, caps)
                .unwrap(),
            Appended::Fresh { .. }
        ));
        assert_eq!(s.relay_len(KEY).unwrap(), 1, "the budget holds one frame");

        assert!(
            matches!(
                s.relay_append(KEY, &[1u8; 16], &head(1), Some(&a), 1, 3000, caps)
                    .unwrap(),
                Appended::Fresh { .. }
            ),
            "a batch whose frame retention deleted must be storable again"
        );
    }

    /// The stored timestamp, not the one on the wire when the copy arrived.
    #[test]
    fn a_duplicate_reports_when_the_first_copy_landed() {
        let s = store();
        let h: [u8; 32] = [0xcc; 32];
        let first = s
            .relay_append(KEY, b"batch", &head(1), Some(&h), 1, 1_000, big())
            .unwrap();
        assert_eq!(
            first,
            Appended::Fresh {
                first_seen_ms: 1_000
            }
        );

        // A reconnect re-drains the outbox under a fresh counter, so the same
        // ops arrive again as batch 1 of a new session.
        let again = s
            .relay_append(KEY, b"batch", &head(1), Some(&h), 1, 9_000, big())
            .unwrap();
        assert_eq!(
            again,
            Appended::Duplicate {
                first_seen_ms: 1_000
            }
        );
        assert_eq!(s.relay_len(KEY).unwrap(), 1, "no second frame was stored");
    }

    /// `None` is the empty-batch case, and it must never collapse.
    #[test]
    fn a_batch_that_opts_out_is_appended_every_time() {
        let s = store();
        for _ in 0..3 {
            assert!(matches!(
                s.relay_append(KEY, b"same", &[], None, 0, 1000, big())
                    .unwrap(),
                Appended::Fresh { .. }
            ));
        }
        assert_eq!(s.relay_len(KEY).unwrap(), 3);
    }
}
