//! Reading the log back: cursor-filtered replay, the gaps retention made
//! unrecoverable, and each device's high-water mark.

use std::collections::HashMap;

use rusqlite::{params, OptionalExtension};

use crate::relay::{CursorGap, StreamKey};
use crate::store::{Store, StoreError};

/// One replayed frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayFrame {
    /// Its durable `relay_frames.id`.
    ///
    /// The id is what an SSE `id:` field carries, so `Last-Event-ID` resumes
    /// from a position the server can act on rather than one it has to guess.
    pub id: u64,
    /// The frame, verbatim.
    pub bytes: Vec<u8>,
    /// The ops it carries, as counted when it was stored, so a replay adds
    /// them to `sunrise_sync_ops_delivered_total` without decoding it.
    pub n_ops: u64,
}

/// What a replay yields: the frames to send, and the gaps retention made
/// unrecoverable.
pub type Replay = (Vec<ReplayFrame>, Vec<CursorGap>);

impl Store {
    /// Every retained frame this subscriber's cursors do not already cover, in
    /// arrival order, plus the gaps retention has made unrecoverable.
    ///
    /// Filtering matches the ring's rule exactly: a frame is skipped only when
    /// it has heads and *every* head is at or below the matching cursor. A
    /// frame whose heads could not be read is never skipped, because
    /// over-delivery costs one idempotent no-op and under-delivery is data
    /// loss.
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] if any read fails.
    pub fn relay_replay(
        &self,
        key: StreamKey,
        cursors: &HashMap<[u8; 16], u64>,
    ) -> Result<(Vec<Vec<u8>>, Vec<CursorGap>), StoreError> {
        let (frames, gaps) = self.relay_replay_after(key, 0, cursors)?;
        Ok((frames.into_iter().map(|f| f.bytes).collect(), gaps))
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
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] if any read fails.
    pub fn relay_replay_after(
        &self,
        key: StreamKey,
        after_id: u64,
        cursors: &HashMap<[u8; 16], u64>,
    ) -> Result<Replay, StoreError> {
        let (account_h, stream_id) = key;
        let conn = self.tx("store.relay_replay_after");

        let mut stmt = conn.prepare(
            "SELECT f.id, f.bytes, f.n_ops FROM relay_frames f
             WHERE f.account_h = ?1 AND f.stream_id = ?2
             ORDER BY f.id",
        )?;
        let rows = stmt
            .query_map(params![&account_h[..], &stream_id[..]], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut heads_stmt =
            conn.prepare("SELECT device_id, max_seq FROM relay_frame_heads WHERE frame_id = ?1")?;
        let mut out = Vec::new();
        for (id, bytes, n_ops) in rows {
            let frame = ReplayFrame {
                id: u64::try_from(id).unwrap_or(0),
                bytes,
                n_ops: u64::try_from(n_ops).unwrap_or(0),
            };
            // A reconnect that names where it got to has said everything the
            // cursors would: it is reporting delivery on this stream rather
            // than application in its op log, which is the stricter of the two.
            if after_id > 0 {
                if frame.id > after_id {
                    out.push(frame);
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
            out.push(frame);
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

    /// The highest sequence this channel has ever held for each device.
    ///
    /// The maximum over two sources, because either alone is a wrong answer.
    /// `relay_frame_heads` covers what is retained; `relay_evicted` covers what
    /// retention has already deleted and outlives the frames it names, so a
    /// channel that has rolled still knows an op was once stored here. Reading
    /// only the first would report a re-send of an evicted op as new work.
    ///
    /// A device absent from the result has never been seen on this channel,
    /// which is a different statement from a head of zero and is why the map
    /// carries no default.
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] if either read fails.
    pub fn relay_device_heads(&self, key: StreamKey) -> Result<HashMap<[u8; 16], u64>, StoreError> {
        let (account_h, stream_id) = key;
        let conn = self.tx("store.relay_device_heads");
        let mut out: HashMap<[u8; 16], u64> = HashMap::new();

        let mut stmt = conn.prepare(
            "SELECT h.device_id, MAX(h.max_seq) FROM relay_frame_heads h
             JOIN relay_frames f ON f.id = h.frame_id
             WHERE f.account_h = ?1 AND f.stream_id = ?2
             GROUP BY h.device_id",
        )?;
        let retained = stmt
            .query_map(params![&account_h[..], &stream_id[..]], |r| {
                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut gone_stmt = conn.prepare(
            "SELECT device_id, evicted_through FROM relay_evicted
             WHERE account_h = ?1 AND stream_id = ?2",
        )?;
        let evicted = gone_stmt
            .query_map(params![&account_h[..], &stream_id[..]], |r| {
                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        for (dev, seq) in retained.into_iter().chain(evicted) {
            let Some(device_id) = to_id(&dev) else {
                continue;
            };
            let seq = u64::try_from(seq).unwrap_or(0);
            let slot = out.entry(device_id).or_insert(0);
            *slot = (*slot).max(seq);
        }
        Ok(out)
    }

    /// Number of retained frames for a channel (tests and diagnostics).
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] if the count cannot be read.
    pub fn relay_len(&self, key: StreamKey) -> Result<usize, StoreError> {
        let (account_h, stream_id) = key;
        let conn = self.tx("store.relay_len");
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
    use super::super::test_support::{big, cursors, head, store, ACC, KEY};
    use super::super::DurableCaps;
    use super::*;

    #[test]
    fn replays_everything_when_the_subscriber_has_no_cursor() {
        let s = store();
        for seq in 1..=3 {
            s.relay_append(
                KEY,
                &[u8::try_from(seq).unwrap(); 8],
                &head(seq),
                1,
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

    /// A replayed frame carries the op count it was stored with, so a replay
    /// counts what it delivers without decoding it.
    #[test]
    fn a_replayed_frame_carries_the_op_count_it_was_stored_with() {
        let s = store();
        s.relay_append(KEY, b"three", &head(1), 3, None, 0, 1000, big())
            .unwrap();
        s.relay_append(KEY, b"opaque", &[], 0, None, 0, 1000, big())
            .unwrap();
        let (frames, _) = s.relay_replay_after(KEY, 0, &HashMap::new()).unwrap();
        assert_eq!(frames.iter().map(|f| f.n_ops).collect::<Vec<_>>(), [3, 0]);
    }

    #[test]
    fn a_cursor_narrows_the_replay_to_what_was_missed() {
        let s = store();
        for seq in 1..=5 {
            s.relay_append(
                KEY,
                &[u8::try_from(seq).unwrap(); 8],
                &head(seq),
                1,
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
        s.relay_append(KEY, b"mine", &head(1), 1, None, 0, 1000, big())
            .unwrap();
        let other = (ACC, [0x99; 16]);
        s.relay_append(other, b"theirs", &head(1), 1, None, 0, 1000, big())
            .unwrap();
        let (frames, _) = s.relay_replay(KEY, &HashMap::new()).unwrap();
        assert_eq!(frames, vec![b"mine".to_vec()]);
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
                1,
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
    fn an_unparseable_frame_is_never_filtered_out() {
        let s = store();
        // No heads: the relay could not read the routing header.
        s.relay_append(KEY, b"opaque", &[], 0, None, 0, 1000, big())
            .unwrap();
        let (frames, _) = s.relay_replay(KEY, &cursors(u64::MAX)).unwrap();
        assert_eq!(
            frames.len(),
            1,
            "over-delivery is an idempotent no-op; under-delivery is data loss"
        );
    }
}
