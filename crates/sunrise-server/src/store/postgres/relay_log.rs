//! The durable relay log on Postgres: append with its dedup, retention and
//! notice in one transaction, and the cursor-filtered replay — the contracts
//! of `crate::relay_log`, with two additions a shared log needs.
//!
//! **Commit order is id order, per channel.** An append first takes
//! `pg_advisory_xact_lock(CHANNEL, <hash of the channel>)`, held until it
//! commits, and only then draws its frame id. Without it two appends to one
//! channel could draw 10 and 11 and commit 11 first; a node that had already
//! read "everything after 10" would then never read 10, and a client resuming
//! after 11 would never be sent it. With it, every frame of a channel that
//! commits later has a larger id, so "after the last id I saw" is a complete
//! description of what is new. Channels whose hashes collide only share a
//! lock, which costs them some concurrency and nothing else.
//!
//! **The append announces itself.** `pg_notify('sunrise_relay',
//! '<account_h>:<stream_id>:<frame_id>')` runs inside the append's
//! transaction, and Postgres delivers a notice only when its transaction
//! commits, so no node hears of a frame that rolled back. The notice carries
//! no frame bytes: a listener reads the log (`fanout.rs`).

use std::collections::HashMap;

use tokio_postgres::Transaction;

use super::{signed, unavailable, unsigned, PgStore};
use crate::relay::{ConnId, CursorGap, FrameHead, StreamKey};
use crate::relay_log::{Appended, DurableCaps, Replay, ReplayFrame};
use crate::store::tx::op;
use crate::store::MetadataError;

/// The first key of every channel's two-key advisory lock: "SRLY".
const CHANNEL_LOCK: i32 = 0x5352_4c59;

/// The notification channel every append announces on.
pub(super) const NOTIFY_CHANNEL: &str = "sunrise_relay";

/// The second key of `key`'s channel lock: the first four bytes of a BLAKE3
/// of the channel.
fn channel_lock(key: StreamKey) -> i32 {
    let mut h = blake3::Hasher::new();
    h.update(&key.0);
    h.update(&key.1);
    let b = h.finalize();
    i32::from_le_bytes([
        b.as_bytes()[0],
        b.as_bytes()[1],
        b.as_bytes()[2],
        b.as_bytes()[3],
    ])
}

/// The notice an append of frame `id` to `key` sends.
pub(super) fn notice(key: StreamKey, id: i64) -> String {
    format!("{}:{}:{id}", hex::encode(key.0), hex::encode(key.1))
}

/// A notice's channel and frame id, or `None` for one this relay did not send.
pub(super) fn parse_notice(payload: &str) -> Option<(StreamKey, u64)> {
    let mut parts = payload.split(':');
    let account = hex::decode(parts.next()?).ok()?.try_into().ok()?;
    let stream = hex::decode(parts.next()?).ok()?.try_into().ok()?;
    let id = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some(((account, stream), id))
}

fn to_id(v: &[u8]) -> Option<[u8; 16]> {
    v.try_into().ok()
}

impl PgStore {
    /// `Store::relay_append`, under the channel's lock, with its notice.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn relay_append_from(
        &self,
        from: ConnId,
        key: StreamKey,
        bytes: &[u8],
        heads: &[FrameHead],
        n_ops: u64,
        ops_h: Option<&[u8; 32]>,
        batch_id: u64,
        now_ms: u64,
        caps: DurableCaps,
    ) -> Result<Appended, MetadataError> {
        let _op = op("store.relay_append");
        let (account_h, stream_id) = (&key.0[..], &key.1[..]);
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(unavailable)?;
        tx.execute(
            "SELECT pg_advisory_xact_lock($1, $2)",
            &[&CHANNEL_LOCK, &channel_lock(key)],
        )
        .await
        .map_err(unavailable)?;
        if let Some(h) = ops_h {
            let seen = tx
                .query_opt(
                    "SELECT first_seen_ms FROM relay_batches
                     WHERE account_h = $1 AND stream_id = $2 AND ops_h = $3",
                    &[&account_h, &stream_id, &&h[..]],
                )
                .await
                .map_err(unavailable)?;
            if let Some(row) = seen {
                return Ok(Appended::Duplicate {
                    first_seen_ms: unsigned(row.get(0)),
                });
            }
        }
        let frame_id: i64 = tx
            .query_one(
                "INSERT INTO relay_frames
                     (account_h, stream_id, bytes, n_bytes, created_ms, n_ops, from_conn)
                 VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
                &[
                    &account_h,
                    &stream_id,
                    &bytes,
                    &i64::try_from(bytes.len()).unwrap_or(i64::MAX),
                    &signed(now_ms),
                    &signed(n_ops),
                    // The bits, not the value: a connection id is a label.
                    &i64::from_ne_bytes(from.to_ne_bytes()),
                ],
            )
            .await
            .map_err(unavailable)?
            .get(0);
        for h in heads {
            tx.execute(
                "INSERT INTO relay_frame_heads (frame_id, device_id, max_seq) VALUES ($1, $2, $3)
                 ON CONFLICT (frame_id, device_id) DO UPDATE SET max_seq = EXCLUDED.max_seq",
                &[&frame_id, &&h.device_id[..], &signed(h.max_seq)],
            )
            .await
            .map_err(unavailable)?;
        }
        if let Some(h) = ops_h {
            tx.execute(
                "INSERT INTO relay_batches
                     (account_h, stream_id, ops_h, frame_id, batch_id, first_seen_ms)
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[
                    &account_h,
                    &stream_id,
                    &&h[..],
                    &frame_id,
                    &signed(batch_id),
                    &signed(now_ms),
                ],
            )
            .await
            .map_err(unavailable)?;
        }
        let evicted = evict(&tx, key, now_ms, caps).await?;
        tx.execute(
            "SELECT pg_notify($1, $2)",
            &[&self.connect().notify(), &notice(key, frame_id)],
        )
        .await
        .map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)?;
        Ok(Appended::Fresh {
            first_seen_ms: now_ms,
            evicted,
            frame_id: unsigned(frame_id),
        })
    }

    /// `Store::relay_replay_after`.
    pub(super) async fn relay_replay_after(
        &self,
        key: StreamKey,
        after_id: u64,
        cursors: &HashMap<[u8; 16], u64>,
    ) -> Result<Replay, MetadataError> {
        let _op = op("store.relay_replay_after");
        let (account_h, stream_id) = (&key.0[..], &key.1[..]);
        let mut c = self.client().await?;
        // One snapshot for the frames, their heads and the gaps, so a frame
        // evicted between two of the reads is not half reported.
        let tx = c
            .build_transaction()
            .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .await
            .map_err(unavailable)?;
        let after = signed(after_id);
        let rows = tx
            .query(
                "SELECT id, bytes, n_ops FROM relay_frames
                 WHERE account_h = $1 AND stream_id = $2 AND id > $3 ORDER BY id",
                &[&account_h, &stream_id, &after],
            )
            .await
            .map_err(unavailable)?;
        let mut heads: HashMap<i64, Vec<(Vec<u8>, i64)>> = HashMap::new();
        if after_id == 0 {
            for r in tx
                .query(
                    "SELECT h.frame_id, h.device_id, h.max_seq FROM relay_frame_heads h
                     JOIN relay_frames f ON f.id = h.frame_id
                     WHERE f.account_h = $1 AND f.stream_id = $2",
                    &[&account_h, &stream_id],
                )
                .await
                .map_err(unavailable)?
            {
                heads
                    .entry(r.get(0))
                    .or_default()
                    .push((r.get(1), r.get(2)));
            }
        }
        let mut frames = Vec::new();
        for r in rows {
            let id: i64 = r.get(0);
            // A non-zero `after_id` selects on the id alone, as on SQLite.
            if after_id == 0 && covered(heads.get(&id).map_or(&[], Vec::as_slice), cursors) {
                continue;
            }
            frames.push(ReplayFrame {
                id: unsigned(id),
                bytes: r.get(1),
                n_ops: unsigned(r.get(2)),
            });
        }
        let mut gaps = Vec::new();
        for r in tx
            .query(
                "SELECT device_id, evicted_through FROM relay_evicted
                 WHERE account_h = $1 AND stream_id = $2 ORDER BY device_id",
                &[&account_h, &stream_id],
            )
            .await
            .map_err(unavailable)?
        {
            let Some(device_id) = to_id(r.get(0)) else {
                continue;
            };
            let through = unsigned(r.get(1));
            let cursor = cursors.get(&device_id).copied().unwrap_or(0);
            if cursor < through {
                gaps.push(CursorGap {
                    device_id,
                    cursor,
                    evicted_through: through,
                });
            }
        }
        tx.commit().await.map_err(unavailable)?;
        Ok((frames, gaps))
    }

    /// `Store::relay_device_heads`: the larger of what is retained and what
    /// retention already deleted, per device.
    pub(super) async fn relay_device_heads(
        &self,
        key: StreamKey,
    ) -> Result<HashMap<[u8; 16], u64>, MetadataError> {
        let _op = op("store.relay_device_heads");
        let rows = self
            .client()
            .await?
            .query(
                "SELECT device_id, MAX(seq) FROM (
                     SELECT h.device_id, h.max_seq AS seq FROM relay_frame_heads h
                     JOIN relay_frames f ON f.id = h.frame_id
                     WHERE f.account_h = $1 AND f.stream_id = $2
                     UNION ALL
                     SELECT device_id, evicted_through FROM relay_evicted
                     WHERE account_h = $1 AND stream_id = $2
                 ) s GROUP BY device_id",
                &[&&key.0[..], &&key.1[..]],
            )
            .await
            .map_err(unavailable)?;
        Ok(rows
            .iter()
            .filter_map(|r| to_id(r.get(0)).map(|d| (d, unsigned(r.get(1)))))
            .collect())
    }

    pub(super) async fn relay_len(&self, key: StreamKey) -> Result<usize, MetadataError> {
        let _op = op("store.relay_len");
        let n: i64 = self
            .client()
            .await?
            .query_one(
                "SELECT COUNT(*) FROM relay_frames WHERE account_h = $1 AND stream_id = $2",
                &[&&key.0[..], &&key.1[..]],
            )
            .await
            .map_err(unavailable)?
            .get(0);
        Ok(usize::try_from(n).unwrap_or(0))
    }

    /// Every frame of `key` after `after_id`, in id order, as the live path
    /// publishes them: what the bus reads on a notice.
    pub(super) async fn relay_frames_after(
        &self,
        key: StreamKey,
        after_id: u64,
    ) -> Result<Vec<crate::relay::RelayFrame>, MetadataError> {
        let _op = op("store.relay_frames_after");
        let (account_h, stream_id) = (&key.0[..], &key.1[..]);
        let after = signed(after_id);
        let c = self.client().await?;
        let rows = c
            .query(
                "SELECT id, bytes, n_ops, from_conn FROM relay_frames
                 WHERE account_h = $1 AND stream_id = $2 AND id > $3 ORDER BY id",
                &[&account_h, &stream_id, &after],
            )
            .await
            .map_err(unavailable)?;
        let mut heads: HashMap<i64, Vec<FrameHead>> = HashMap::new();
        if !rows.is_empty() {
            for r in c
                .query(
                    "SELECT h.frame_id, h.device_id, h.max_seq FROM relay_frame_heads h
                     JOIN relay_frames f ON f.id = h.frame_id
                     WHERE f.account_h = $1 AND f.stream_id = $2 AND f.id > $3",
                    &[&account_h, &stream_id, &after],
                )
                .await
                .map_err(unavailable)?
            {
                if let Some(device_id) = to_id(r.get(1)) {
                    heads.entry(r.get(0)).or_default().push(FrameHead {
                        device_id,
                        max_seq: unsigned(r.get(2)),
                    });
                }
            }
        }
        Ok(rows
            .iter()
            .map(|r| {
                let id: i64 = r.get(0);
                crate::relay::RelayFrame {
                    from: u64::from_ne_bytes(r.get::<_, i64>(3).to_ne_bytes()),
                    id: unsigned(id),
                    bytes: r.get(1),
                    n_ops: unsigned(r.get(2)),
                    heads: heads.remove(&id).unwrap_or_default(),
                    fanout: None,
                }
            })
            .collect())
    }

    /// The newest frame id `key` holds, 0 for none: where a channel that
    /// gains its first live subscriber starts being read from.
    pub(super) async fn relay_last_id(&self, key: StreamKey) -> Result<u64, MetadataError> {
        let _op = op("store.relay_last_id");
        let id: Option<i64> = self
            .client()
            .await?
            .query_one(
                "SELECT MAX(id) FROM relay_frames WHERE account_h = $1 AND stream_id = $2",
                &[&&key.0[..], &&key.1[..]],
            )
            .await
            .map_err(unavailable)?
            .get(0);
        Ok(id.map_or(0, unsigned))
    }
}

/// Enforce both retention bounds for one channel, as `relay_log::append`'s
/// `evict` does, raising `evicted_through` for every device a deleted frame
/// carried, and return how many frames went.
async fn evict(
    tx: &Transaction<'_>,
    key: StreamKey,
    now_ms: u64,
    caps: DurableCaps,
) -> Result<u64, MetadataError> {
    let (account_h, stream_id) = (&key.0[..], &key.1[..]);
    let cutoff = signed(now_ms.saturating_sub(caps.max_age_ms));
    let rows = tx
        .query(
            "SELECT id, n_bytes, created_ms FROM relay_frames
             WHERE account_h = $1 AND stream_id = $2 ORDER BY id",
            &[&account_h, &stream_id],
        )
        .await
        .map_err(unavailable)?;
    let mut doomed: Vec<i64> = rows
        .iter()
        .filter(|r| r.get::<_, i64>(2) < cutoff)
        .map(|r| r.get(0))
        .collect();
    // Oldest first until under budget, always keeping the newest frame.
    let mut total: u64 = rows.iter().map(|r| unsigned(r.get(1))).sum();
    let mut remaining = rows.len();
    for r in &rows {
        if total <= caps.max_bytes || remaining <= 1 {
            break;
        }
        let id: i64 = r.get(0);
        if !doomed.contains(&id) {
            doomed.push(id);
        }
        total = total.saturating_sub(unsigned(r.get(1)));
        remaining -= 1;
    }
    if doomed.is_empty() {
        return Ok(0);
    }
    // A frame with no readable head raises no watermark, as on SQLite.
    tx.execute(
        "INSERT INTO relay_evicted (account_h, stream_id, device_id, evicted_through)
         SELECT $1, $2, device_id, MAX(max_seq) FROM relay_frame_heads
          WHERE frame_id = ANY($3) GROUP BY device_id
         ON CONFLICT (account_h, stream_id, device_id) DO UPDATE SET
           evicted_through = GREATEST(relay_evicted.evicted_through, EXCLUDED.evicted_through)",
        &[&account_h, &stream_id, &doomed],
    )
    .await
    .map_err(unavailable)?;
    tx.execute("DELETE FROM relay_frames WHERE id = ANY($1)", &[&doomed])
        .await
        .map_err(unavailable)?;
    Ok(doomed.len() as u64)
}

/// The ring's filtering rule: skipped only when it has heads and every one is
/// at or below the matching cursor.
fn covered(heads: &[(Vec<u8>, i64)], cursors: &HashMap<[u8; 16], u64>) -> bool {
    !heads.is_empty()
        && heads.iter().all(|(dev, seq)| {
            to_id(dev).is_some_and(|id| {
                cursors
                    .get(&id)
                    .is_some_and(|c| u64::try_from(*seq).unwrap_or(u64::MAX) <= *c)
            })
        })
}

#[cfg(test)]
mod tests {
    use super::{notice, parse_notice};

    /// A notice names its channel and frame and nothing else, and a payload
    /// this relay did not write is not read as one.
    #[test]
    fn a_notice_round_trips_and_a_foreign_one_is_ignored() {
        let key = ([0xa1; 16], [0x11; 16]);
        assert_eq!(parse_notice(&notice(key, 42)), Some((key, 42)));
        for foreign in ["", "x:y:1", "a1:11:1", &format!("{}:4", notice(key, 1))] {
            assert_eq!(parse_notice(foreign), None, "{foreign}");
        }
    }
}
