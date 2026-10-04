//! Client op-log compaction below a per-device floor (issue #330, ADR-0059,
//! `docs/04-storage/compaction.md`).
//!
//! # The floor
//!
//! Each `(stream, device)` prefix may have a [`Floor`]: every op of that
//! device in that stream at or below `seq` is *covered*. Its effect is in the
//! per-field merge state (ADR-0044) or its row is still held, and the floor's
//! chain root `root(device, seq)` (ADR-0043) stands for the whole prefix below
//! it. So a position below the floor is neither held nor missing, and the op
//! log reads the floor wherever it would have read the op at that position:
//!
//! - the contiguous prefix starts at `seq + 1` (`oplog::upsert_sync_cursor`);
//! - the chain root resumes from the floor's root (`chain::fold_chain`), and
//!   `root(device, seq)` and the op hash at `seq` read from it when the row is
//!   gone;
//! - a link or a head naming a covered position is not an expectation
//!   (`chain::check_links`), and a delivery at a covered position is a
//!   duplicate (`sync::apply_remote_all`);
//! - this device's next `seq` never falls to or below its own floor.
//!
//! This is the answer to the second blocker the compaction document named: a
//! hole below a floor is not a gap, because the chain root at the floor
//! commits to every op under it and a peer's digest is compared against it.
//!
//! # Compaction
//!
//! [`Engine::compact_op_log`] raises each floor to the highest position that
//! every known device has acknowledged in its stream digest, that is older
//! than the retention window, and that is strictly below this replica's own
//! tip, then deletes the ops under it whose whole effect is in the merge
//! state: the entity writes and old digests. It never deletes a parked op, an
//! op of a kind whose effect lives anywhere else (control ops, focus and
//! review records), an op still waiting in the outbox, or an op that a
//! missing op's expectation was named by.
//!
//! # Known devices and the compactor
//!
//! A device is known in a stream when it is not revoked, was heard from within
//! the policy's window, and is either a member of the account or has written
//! in the stream. Its acknowledgement of another device's prefix is the last
//! frontier it published there ([`record_peer_frontier`]); this replica's own
//! is its sync cursor. The known device with the least id is the stream's
//! compactor, and only the compactor writes a snapshot of the stream.

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::collections::BTreeSet;
use sunrise_cbor::hlc::Hlc;
use sunrise_crypto::decode_envelope;
use sunrise_id::registry::Merge;

use super::chain::{fold_chain, held_op_hash, root_at};
use super::ids::hex_short;
use super::merge::fold_rows;
use super::{Engine, EngineError};
use sunrise_storage::Db;

/// The op-log `inner_kind` of a stream digest, which carries no entity state
/// and is superseded by the next one: compactable once acknowledged.
const DIGEST_KIND: &str = "stream.digest";

/// The op-log kinds whose whole effect is in the merge state (ADR-0044), and
/// so the only kinds compaction deletes: every op, legacy full-state or
/// `Patch`, of an entity the registry merges field by field and projects to a
/// row, and the stream digest. Everything else is kept. Control ops carry
/// trust and keys, focus and review ops write append-only records, and a kind
/// this build does not know is parked.
///
/// Read off the registry rather than listed, so an entity added to it is
/// compacted, or not, by the rule rather than by somebody remembering to.
pub(super) fn compactable_kinds() -> Vec<String> {
    let mut kinds = vec![DIGEST_KIND.to_owned()];
    for spec in &sunrise_id::registry::ENTITIES {
        if spec.merge != Merge::Lww || spec.storage().is_none() {
            continue;
        }
        kinds.extend(spec.ops.iter().map(|op| op.inner_kind.to_owned()));
        kinds.push(format!("{}.patch", spec.tag));
    }
    kinds
}

/// The `IN (...)` list of [`compactable_kinds`], for a statement. Each kind is
/// a registry literal of `[a-z_.]`, so quoting it is safe.
fn compactable_sql() -> String {
    compactable_kinds()
        .iter()
        .map(|k| format!("'{k}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// One `(stream, device)` floor: see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Floor {
    /// Every op at or below this seq is covered.
    pub(super) seq: u64,
    /// The `op_hash` of the op at `seq`.
    pub(super) op_hash: [u8; 32],
    /// `root(device, seq)`.
    pub(super) root: [u8; 32],
    /// The stamp of the op at `seq`, which bounds every op the floor covers.
    pub(super) hlc: Hlc,
}

fn i64_of(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn u64_of(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

/// The floor of `(stream_id, device_id)`, if it has one.
pub(super) fn floor_of(
    conn: &Connection,
    stream_id: &[u8; 16],
    device_id: &[u8; 16],
) -> rusqlite::Result<Option<Floor>> {
    let row: Option<Option<Floor>> = conn
        .query_row(
            "SELECT seq, op_hash, root, hlc_ms, hlc_logical FROM compaction_floor
             WHERE stream_id = ?1 AND device_id = ?2",
            params![&stream_id[..], &device_id[..]],
            |r| {
                let h: Vec<u8> = r.get(1)?;
                let root: Vec<u8> = r.get(2)?;
                Ok(
                    match (
                        <[u8; 32]>::try_from(h.as_slice()),
                        <[u8; 32]>::try_from(root.as_slice()),
                    ) {
                        (Ok(op_hash), Ok(root)) => Some(Floor {
                            seq: u64_of(r.get(0)?),
                            op_hash,
                            root,
                            hlc: Hlc {
                                physical_ms: u64_of(r.get(3)?),
                                logical: u32::try_from(r.get::<_, i64>(4)?).unwrap_or(0),
                            },
                        }),
                        _ => None,
                    },
                )
            },
        )
        .optional()?;
    Ok(row.flatten())
}

/// The floor's `seq`, or 0 where there is none.
pub(super) fn floor_seq(
    conn: &Connection,
    stream_id: &[u8; 16],
    device_id: &[u8; 16],
) -> rusqlite::Result<u64> {
    Ok(floor_of(conn, stream_id, device_id)?.map_or(0, |f| f.seq))
}

/// Raise the floor of `(stream_id, device_id)` to `floor`. A floor never
/// falls: returns `false` and writes nothing when the held floor is at or
/// above `floor.seq`.
pub(super) fn raise_floor(
    tx: &Transaction<'_>,
    stream_id: &[u8; 16],
    device_id: &[u8; 16],
    floor: &Floor,
) -> rusqlite::Result<bool> {
    if floor_seq(tx, stream_id, device_id)? >= floor.seq {
        return Ok(false);
    }
    tx.execute(
        "INSERT INTO compaction_floor
           (stream_id, device_id, seq, op_hash, root, hlc_ms, hlc_logical)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(stream_id, device_id) DO UPDATE SET
           seq = excluded.seq, op_hash = excluded.op_hash, root = excluded.root,
           hlc_ms = excluded.hlc_ms, hlc_logical = excluded.hlc_logical",
        params![
            &stream_id[..],
            &device_id[..],
            i64_of(floor.seq),
            &floor.op_hash[..],
            &floor.root[..],
            i64_of(floor.hlc.physical_ms),
            i64::from(floor.hlc.logical),
        ],
    )?;
    Ok(true)
}

/// The greatest stamp any floor bounds, for [`Engine::prime_hlc`].
pub(super) fn max_floor_hlc(conn: &Connection) -> rusqlite::Result<Option<Hlc>> {
    conn.query_row(
        "SELECT hlc_ms, hlc_logical FROM compaction_floor
         ORDER BY hlc_ms DESC, hlc_logical DESC LIMIT 1",
        [],
        |r| {
            Ok(Hlc {
                physical_ms: u64_of(r.get(0)?),
                logical: u32::try_from(r.get::<_, i64>(1)?).unwrap_or(0),
            })
        },
    )
    .optional()
}

/// Record `peer`'s published frontier in `stream_id` as its acknowledgement
/// of each device's prefix. Never lowers what a later digest already said: a
/// relay can deliver an older digest after a newer one.
pub(super) fn record_peer_frontier(
    tx: &Transaction<'_>,
    stream_id: &[u8; 16],
    peer: &[u8; 16],
    entries: &[([u8; 16], u64)],
    at_ms: u64,
) -> rusqlite::Result<()> {
    for (device, seq) in entries {
        tx.execute(
            "INSERT INTO peer_frontiers (stream_id, peer_device_id, device_id, seq, recorded_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(stream_id, peer_device_id, device_id) DO UPDATE SET
               seq = MAX(seq, excluded.seq),
               recorded_at_ms = MAX(recorded_at_ms, excluded.recorded_at_ms)",
            params![
                &stream_id[..],
                &peer[..],
                &device[..],
                i64_of(*seq),
                i64_of(at_ms)
            ],
        )?;
    }
    Ok(())
}

/// What [`Engine::compact_op_log`] may fold, and when.
///
/// Every field is a bound a deployment may move; [`Default`] is the design of
/// record's numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionPolicy {
    /// An op is folded only once its stamp is older than this. Default: 30
    /// days, the relay's own retention window.
    pub retention_ms: u64,
    /// A device not heard from for this long is no longer a known device, and
    /// stops holding compaction back. Default: 30 days.
    pub known_device_window_ms: u64,
    /// The compactor rewrites a stream's snapshot at most this often. Default:
    /// one day.
    pub snapshot_every_ms: u64,
}

/// One day, in milliseconds.
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            retention_ms: 30 * DAY_MS,
            known_device_window_ms: 30 * DAY_MS,
            snapshot_every_ms: DAY_MS,
        }
    }
}

/// What one [`Engine::compact_op_log`] run did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompactionReport {
    /// `(stream, device)` floors that rose.
    pub floors_raised: u64,
    /// Op rows deleted.
    pub ops_removed: u64,
    /// Snapshots this replica wrote as a stream's compactor.
    pub snapshots_written: u64,
}

/// When `device` was last heard from: the newest stamp among its tips, its
/// floors and the digests it published. `None` for a device never heard from.
fn last_heard(conn: &Connection, device: &[u8; 16]) -> rusqlite::Result<Option<u64>> {
    let v: Option<i64> = conn.query_row(
        "SELECT MAX(t) FROM (
            SELECT o.ts_ms AS t FROM sync_cursors c
              JOIN ops o ON o.stream_id = c.stream_id AND o.device_id = c.device_id
                        AND o.seq = c.last_applied_seq
             WHERE c.device_id = ?1
            UNION ALL
            SELECT hlc_ms FROM compaction_floor WHERE device_id = ?1
            UNION ALL
            SELECT recorded_at_ms FROM peer_frontiers WHERE peer_device_id = ?1
         )",
        params![&device[..]],
        |r| r.get(0),
    )?;
    Ok(v.and_then(|v| u64::try_from(v).ok()))
}

impl Engine {
    /// The known devices of `stream_id`, sorted by id, this replica always
    /// among them: see the module docs.
    pub(super) fn known_devices(
        &self,
        tx: &Transaction<'_>,
        stream_id: &[u8; 16],
        now_ms: u64,
        window_ms: u64,
    ) -> rusqlite::Result<BTreeSet<[u8; 16]>> {
        let me = self.keychain.device_id();
        let head = self.current_identity(tx)?.identity_id;
        let candidates: Vec<Vec<u8>> = {
            let mut stmt = tx.prepare(
                "SELECT d.device_id FROM devices d
                 WHERE (d.identity_id = ?1
                        OR EXISTS (SELECT 1 FROM sync_cursors c
                                   WHERE c.stream_id = ?2 AND c.device_id = d.device_id))
                   AND NOT EXISTS (SELECT 1 FROM device_revocations r
                                   WHERE r.device_id = d.device_id)",
            )?;
            let rows = stmt.query_map(params![&head[..], &stream_id[..]], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let horizon = now_ms.saturating_sub(window_ms);
        let mut known = BTreeSet::from([me]);
        for raw in candidates {
            let Ok(device) = <[u8; 16]>::try_from(raw.as_slice()) else {
                continue;
            };
            if device == me {
                continue;
            }
            if last_heard(tx, &device)?.is_some_and(|t| t >= horizon) {
                known.insert(device);
            }
        }
        Ok(known)
    }

    /// The highest seq of `device`'s prefix in `stream_id` that every device
    /// in `known` has acknowledged.
    fn acknowledged(
        &self,
        tx: &Transaction<'_>,
        stream_id: &[u8; 16],
        device: &[u8; 16],
        known: &BTreeSet<[u8; 16]>,
        own_cursor: u64,
    ) -> rusqlite::Result<u64> {
        let me = self.keychain.device_id();
        let mut ack = own_cursor;
        for k in known {
            if *k == me {
                continue;
            }
            let seq: Option<i64> = tx
                .query_row(
                    "SELECT seq FROM peer_frontiers
                     WHERE stream_id = ?1 AND peer_device_id = ?2 AND device_id = ?3",
                    params![&stream_id[..], &k[..], &device[..]],
                    |r| r.get(0),
                )
                .optional()?;
            ack = ack.min(seq.map_or(0, u64_of));
        }
        Ok(ack)
    }

    /// Fold this replica's op log below every floor the policy allows, and
    /// write a snapshot of each stream this replica is the compactor of and
    /// whose last snapshot is older than `policy.snapshot_every_ms`. See the
    /// module docs for what is deleted and what never is.
    ///
    /// Each stream is folded in its own transaction, so a failure leaves
    /// every stream before it compacted and every stream after it as it was.
    ///
    /// # Errors
    /// Storage failures.
    pub fn compact_op_log(
        &self,
        db: &mut Db,
        policy: &CompactionPolicy,
    ) -> Result<CompactionReport, EngineError> {
        let now_ms = self.clock.now_ms();
        let streams: Vec<[u8; 16]> = {
            let mut stmt = db
                .conn()
                .prepare("SELECT DISTINCT stream_id FROM sync_cursors ORDER BY stream_id")?;
            let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
            rows.filter_map(|r| r.ok().and_then(|v| <[u8; 16]>::try_from(v.as_slice()).ok()))
                .collect()
        };
        let mut report = CompactionReport::default();
        for stream_id in streams {
            let (raised, removed, compactor) =
                db.with_tx(|tx| self.compact_stream(tx, &stream_id, policy, now_ms))?;
            report.floors_raised += raised;
            report.ops_removed += removed;
            if compactor
                && self.snapshot_due(db, &stream_id, policy, now_ms)?
                && self.write_stream_snapshot(db, &stream_id)?.is_some()
            {
                report.snapshots_written += 1;
            }
        }
        if report.ops_removed > 0 || report.snapshots_written > 0 {
            tracing::info!(
                ev = "core.compaction.done",
                n_devices = report.floors_raised,
                n_ops = report.ops_removed,
                "the op log was folded below its acknowledged floors"
            );
        }
        Ok(report)
    }

    /// Whether this replica's stored snapshot of `stream_id` is missing or
    /// older than the policy allows.
    fn snapshot_due(
        &self,
        db: &Db,
        stream_id: &[u8; 16],
        policy: &CompactionPolicy,
        now_ms: u64,
    ) -> Result<bool, EngineError> {
        let last: Option<i64> = db
            .conn()
            .query_row(
                "SELECT generated_at_ms FROM stream_snapshots
                 WHERE stream_id = ?1 AND generated_by = ?2",
                params![&stream_id[..], &self.keychain.device_id()[..]],
                |r| r.get(0),
            )
            .optional()?;
        Ok(last.is_none_or(|t| now_ms.saturating_sub(u64_of(t)) >= policy.snapshot_every_ms))
    }

    /// Fold one stream. Returns the floors raised, the rows deleted, and
    /// whether this replica is the stream's compactor.
    fn compact_stream(
        &self,
        tx: &Transaction<'_>,
        stream_id: &[u8; 16],
        policy: &CompactionPolicy,
        now_ms: u64,
    ) -> rusqlite::Result<(u64, u64, bool)> {
        let me = self.keychain.device_id();
        let known = self.known_devices(tx, stream_id, now_ms, policy.known_device_window_ms)?;
        let compactor = known.first() == Some(&me);
        let cutoff = now_ms.saturating_sub(policy.retention_ms);
        let cursors: Vec<(Vec<u8>, i64)> = {
            let mut stmt = tx.prepare(
                "SELECT device_id, last_applied_seq FROM sync_cursors
                 WHERE stream_id = ?1 ORDER BY device_id",
            )?;
            let rows = stmt.query_map(params![&stream_id[..]], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let kinds = compactable_sql();
        let mut raised = 0;
        let mut removed = 0;
        for (raw, cursor) in cursors {
            let Ok(device) = <[u8; 16]>::try_from(raw.as_slice()) else {
                continue;
            };
            let cursor = u64_of(cursor);
            // Strictly below the tip, so the tip's row stays: it is what the
            // next op's link names and what `prime_hlc` restores the clock
            // from.
            let limit = self
                .acknowledged(tx, stream_id, &device, &known, cursor)?
                .min(cursor.saturating_sub(1));
            let current = floor_seq(tx, stream_id, &device)?;
            if limit <= current {
                continue;
            }
            let target: Option<(i64, Vec<u8>, i64)> = tx
                .query_row(
                    "SELECT seq, envelope, ts_ms FROM ops
                     WHERE stream_id = ?1 AND device_id = ?2 AND seq > ?3 AND seq <= ?4
                       AND ts_ms <= ?5
                     ORDER BY seq DESC LIMIT 1",
                    params![
                        &stream_id[..],
                        &device[..],
                        i64_of(current),
                        i64_of(limit),
                        i64_of(cutoff)
                    ],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((target, envelope, ts_ms)) = target else {
                continue;
            };
            let target = u64_of(target);
            // A prefix from before migration 0034 may not have its roots yet.
            fold_chain(tx, stream_id, &device, cursor, now_ms)?;
            let (Some(op_hash), Some(root)) = (
                held_op_hash(tx, stream_id, &device, target)?,
                root_at(tx, stream_id, &device, target)?,
            ) else {
                continue;
            };
            let hlc = decode_envelope(&envelope).map_or(
                Hlc {
                    physical_ms: u64_of(ts_ms),
                    logical: 0,
                },
                |env| env.hlc,
            );

            // Fold every entity row the deleted ops wrote into its field
            // state first: a local command writes its row and logs its op,
            // and the merge folds that row the next time an op touches the
            // entity, reading the op's stream and kind back from the log.
            // Once the op is gone it could not.
            let predicate = format!(
                "stream_id = ?1 AND device_id = ?2 AND seq <= ?3
                 AND applied_at IS NOT NULL
                 AND inner_kind IN ({kinds})
                 AND op_id NOT IN (SELECT op_id FROM parked_ops)
                 AND op_id NOT IN (SELECT op_id FROM outbox WHERE acked_at_ms IS NULL)
                 AND op_id NOT IN (SELECT named_by FROM chain_expected)"
            );
            let key = params![&stream_id[..], &device[..], i64_of(target)];
            let targets: Vec<(String, Vec<u8>)> = {
                let mut stmt = tx.prepare(&format!(
                    "SELECT DISTINCT target_kind, target_id FROM ops
                     WHERE {predicate} AND target_id IS NOT NULL
                       AND inner_kind <> '{DIGEST_KIND}'"
                ))?;
                let rows = stmt.query_map(key, |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            fold_rows(tx, &targets)?;

            raise_floor(
                tx,
                stream_id,
                &device,
                &Floor {
                    seq: target,
                    op_hash,
                    root,
                    hlc,
                },
            )?;
            raised += 1;
            tx.execute(
                &format!(
                    "DELETE FROM outbox WHERE op_id IN (SELECT op_id FROM ops WHERE {predicate})"
                ),
                key,
            )?;
            tx.execute(
                &format!(
                    "DELETE FROM op_dep WHERE op_id IN (SELECT op_id FROM ops WHERE {predicate})"
                ),
                key,
            )?;
            let n = tx.execute(&format!("DELETE FROM ops WHERE {predicate}"), key)?;
            removed += n as u64;
            tracing::debug!(
                ev = "core.compaction.floor",
                stream_h = hex_short(stream_id),
                subject_h = hex_short(&device),
                seq = target,
                n_ops = n,
                "a device's prefix was folded below its acknowledged floor"
            );
        }
        Ok((raised, removed, compactor))
    }
}
