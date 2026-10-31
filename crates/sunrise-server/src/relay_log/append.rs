//! Appending a frame: batch deduplication and the two retention bounds, applied
//! in one transaction.

use rusqlite::{params, OptionalExtension};

use crate::relay::{FrameHead, StreamKey};
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
        /// Frames retention deleted from the channel to make room, by either
        /// bound, in the same transaction: `sunrise_relay_log_evicted_total`.
        evicted: u64,
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
    /// `relay_log::SCHEMA` for why keying on it loses data.
    ///
    /// `n_ops` is how many ops the frame carries, stored beside it so a
    /// replay can count what it delivers without decoding the frame.
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] if the transaction cannot be committed, in which
    /// case nothing was written and the caller must refuse to ack.
    #[allow(clippy::too_many_arguments)]
    pub fn relay_append(
        &self,
        key: StreamKey,
        bytes: &[u8],
        heads: &[FrameHead],
        n_ops: u64,
        ops_h: Option<&[u8; 32]>,
        batch_id: u64,
        now_ms: u64,
        caps: DurableCaps,
    ) -> Result<Appended, StoreError> {
        let (account_h, stream_id) = key;
        let mut conn = self.tx("store.relay_append");
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
            "INSERT INTO relay_frames (account_h, stream_id, bytes, n_bytes, created_ms, n_ops)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                &account_h[..],
                &stream_id[..],
                bytes,
                i64::try_from(bytes.len()).unwrap_or(i64::MAX),
                i64::try_from(now_ms).unwrap_or(i64::MAX),
                i64::try_from(n_ops).unwrap_or(i64::MAX),
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
        let evicted = evict(&tx, account_h, stream_id, now_ms, caps)?;
        tx.commit()?;
        Ok(Appended::Fresh {
            first_seen_ms: now_ms,
            evicted,
        })
    }
}

/// Enforce both retention bounds for one channel, raising `evicted_through`
/// for every device carried by a frame that is deleted, and return how many
/// frames went.
fn evict(
    tx: &rusqlite::Transaction<'_>,
    account_h: [u8; 16],
    stream_id: [u8; 16],
    now_ms: u64,
    caps: DurableCaps,
) -> Result<u64, StoreError> {
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

    let evicted = doomed.len() as u64;
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
    Ok(evicted)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::super::test_support::{big, cursors, head, store, DEV, KEY};
    use super::*;

    #[test]
    fn the_size_bound_evicts_oldest_first_and_raises_the_watermark() {
        let s = store();
        let caps = DurableCaps {
            max_bytes: 20,
            max_age_ms: u64::MAX,
        };
        // Each frame is 8 bytes, so a 20-byte budget holds two.
        let mut evicted = Vec::new();
        for seq in 1..=5 {
            let appended = s
                .relay_append(
                    KEY,
                    &[u8::try_from(seq).unwrap(); 8],
                    &head(seq),
                    1,
                    None,
                    0,
                    1000,
                    caps,
                )
                .unwrap();
            let Appended::Fresh { evicted: n, .. } = appended else {
                panic!("a batch with no content key is never a duplicate");
            };
            evicted.push(n);
        }
        assert_eq!(s.relay_len(KEY).unwrap(), 2, "budget holds two frames");
        // Each append past the second deletes exactly the oldest, and says so.
        assert_eq!(evicted, [0, 0, 1, 1, 1]);

        // A subscriber at seq 1 has lost 2 and 3 for good.
        let (_, gaps) = s.relay_replay(KEY, &cursors(1)).unwrap();
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].device_id, DEV);
        assert_eq!(gaps[0].cursor, 1);
        assert_eq!(gaps[0].evicted_through, 3);
    }

    #[test]
    fn the_age_bound_evicts_and_is_reported_as_a_gap() {
        let s = store();
        let caps = DurableCaps {
            max_bytes: u64::MAX,
            max_age_ms: 1000,
        };
        s.relay_append(KEY, b"old", &head(1), 1, None, 0, 1_000, caps)
            .unwrap();
        // Far enough past the bound that the first frame is out of window.
        s.relay_append(KEY, b"new", &head(2), 1, None, 0, 10_000, caps)
            .unwrap();
        assert_eq!(s.relay_len(KEY).unwrap(), 1);
        let (_, gaps) = s.relay_replay(KEY, &HashMap::new()).unwrap();
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].evicted_through, 1);
    }

    #[test]
    fn evicting_an_opaque_frame_raises_no_watermark() {
        let s = store();
        let caps = DurableCaps {
            max_bytes: 8,
            max_age_ms: u64::MAX,
        };
        s.relay_append(KEY, b"opaque12", &[], 0, None, 0, 1000, caps)
            .unwrap();
        s.relay_append(KEY, b"opaque34", &[], 0, None, 0, 1000, caps)
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
        s.relay_append(KEY, &[7u8; 64], &head(1), 1, None, 0, 1000, caps)
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
            s.relay_append(KEY, &[1u8; 16], &head(1), 1, Some(&a), 1, 1000, caps)
                .unwrap(),
            Appended::Fresh { .. }
        ));
        assert!(matches!(
            s.relay_append(KEY, &[1u8; 16], &head(1), 1, Some(&a), 1, 1000, caps)
                .unwrap(),
            Appended::Duplicate { .. }
        ));

        // 16-byte frames under a 20-byte budget: appending B evicts A.
        assert!(matches!(
            s.relay_append(KEY, &[3u8; 16], &head(3), 1, Some(&b), 2, 2000, caps)
                .unwrap(),
            Appended::Fresh { .. }
        ));
        assert_eq!(s.relay_len(KEY).unwrap(), 1, "the budget holds one frame");

        assert!(
            matches!(
                s.relay_append(KEY, &[1u8; 16], &head(1), 1, Some(&a), 1, 3000, caps)
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
            .relay_append(KEY, b"batch", &head(1), 1, Some(&h), 1, 1_000, big())
            .unwrap();
        assert_eq!(
            first,
            Appended::Fresh {
                first_seen_ms: 1_000,
                evicted: 0,
            }
        );

        // A reconnect re-drains the outbox under a fresh counter, so the same
        // ops arrive again as batch 1 of a new session.
        let again = s
            .relay_append(KEY, b"batch", &head(1), 1, Some(&h), 1, 9_000, big())
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
                s.relay_append(KEY, b"same", &[], 0, None, 0, 1000, big())
                    .unwrap(),
                Appended::Fresh { .. }
            ));
        }
        assert_eq!(s.relay_len(KEY).unwrap(), 3);
    }
}
