//! Rebuilding the projection from the local op log (issue #441,
//! `docs/04-storage/migrations.md` §Migration rigor rule 4).
//!
//! # What a rebuild re-derives
//!
//! The entity rows, their side tables (`task_contexts`, `task_blockers`,
//! `block_tasks`), the task rows of the full-text index, and the per-field
//! merge state they are projected from ([ADR-0044]), are all a function of the
//! entity ops this replica applied. [`Engine::rebuild_projection`] clears them
//! and folds those ops again, in one transaction:
//!
//! 1. **The compacted base.** An op below a `compaction_floor` has been folded
//!    and deleted ([ADR-0059]); a replica that joined a snapshot never held
//!    it. What those ops wrote is still in the merge state, attributed to
//!    them by stamp, so it is dumped first ([`dump_compacted_base`]).
//! 2. **Clear** every table [`TABLE_CLASSES`] classifies as a projection,
//!    keeping the rows named below.
//! 3. **Join the base back** with [`join_stream_state`], the snapshot join.
//! 4. **Refold the log.** Every applied entity op still in `ops` is read,
//!    sorted by its merge stamp `(hlc, device, seq, stream)`, opened, and
//!    folded through [`materialize_remote`], the step the receive path takes.
//!    Parked ops are not entity ops here: this build cannot read them, so the
//!    live projection never held them either, and their replay after an
//!    upgrade is [`Engine::replay_parked_ops`]'s.
//! 5. Put back the local columns named below.
//!
//! The merge is order-independent ([ADR-0044]), so the canonical order is
//! not what makes the result right. It is what makes the side effects of a
//! fold, such as the order rows are inserted in, the same on every rebuild.
//!
//! # What a rebuild keeps
//!
//! Everything else in the schema is source data or local state no op
//! derives, and is not touched: the op log, keys, identity and devices, sync
//! cursors and floors, the control-op folds (revocations, features), and
//! local-only tables such as `device_preferences` and `blob_cache`. Inside
//! the cleared tables three things survive, because no op derives them:
//!
//! - a placeholder `streams` row no op has stamped (`lww_device IS NULL`),
//!   which `ensure_stream_row` wrote when an op named a stream before its
//!   create; the lazily seeded Inbox is one;
//! - each routine's `materialized_until_ms`, how far this device has
//!   generated its occurrences, which is never on the wire;
//! - every `search_idx` row whose `kind` is not `task`, since only task rows
//!   are projected.
//!
//! [`TABLE_CLASSES`] classifies every table in the schema. A test fails when
//! a migration adds a table it does not classify, or when the registry
//! projects an entity to a table it does not call a projection.
//!
//! [ADR-0044]: ../../../../docs/11-adr/0044-per-field-ops.md
//! [ADR-0059]: ../../../../docs/11-adr/0059-client-op-log-compaction.md

use super::ids::{blob16, hex_short};
use super::lww::{materialize_remote, remap_legacy_inbox, LwwStamp};
use super::merge::{check_patch, dump_compacted_base, join_stream_state, PatchProblem, Stamp};
use super::snapshot::in_tx;
use super::{Engine, EngineError};
use crate::inner_op::{decode_inner_op, InnerOp};
use rusqlite::{params, Transaction};
use sunrise_crypto::decode_envelope;
use sunrise_crypto::op_envelope::open_envelope;
use sunrise_id::registry::{Merge, ENTITIES};
use sunrise_storage::Db;

/// What one [`Engine::rebuild_projection`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RebuildReport {
    /// Entity ops folded again from the log.
    pub ops_replayed: u64,
    /// Entity ops in the log this build refuses to fold: a `Patch` an older
    /// build applied that this build's registry parks or rejects. The
    /// receive path would not apply one either. Each is logged.
    pub ops_skipped: u64,
    /// Entities whose compacted state was carried across from the merge
    /// state, because compaction deleted the ops that wrote it.
    pub compacted_entities: u64,
}

/// How a table in the local schema relates to the op log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TableClass {
    /// A fold of entity ops: cleared and re-derived by a rebuild.
    Projection,
    /// Never cleared by a rebuild: the op log, keys and identity, sync and
    /// compaction bookkeeping, control-op folds, and local-only state.
    Source,
}

/// Every table the schema has, classified (`migrations.md` rule 4), in the
/// order a rebuild clears the projections. The registry also names the
/// projected entity tables; a test holds this list to it, so a registry
/// change that moved one is a failing test rather than an uncleared table.
pub(super) const TABLE_CLASSES: &[(&str, TableClass)] = {
    use TableClass::{Projection, Source};
    &[
        // The task search rows, and the side tables of entity rows.
        ("search_idx", Projection),
        ("task_contexts", Projection),
        ("task_blockers", Projection),
        ("block_tasks", Projection),
        // The per-field merge state.
        ("merge_entities", Projection),
        ("merge_registers", Projection),
        ("merge_map_entries", Projection),
        ("merge_orset_adds", Projection),
        ("merge_orset_removes", Projection),
        ("merge_counter_deltas", Projection),
        // Entity rows.
        ("tasks", Projection),
        ("contexts", Projection),
        ("routines", Projection),
        ("blocks", Projection),
        ("attachments", Projection),
        ("preferences", Projection),
        ("focus_sessions", Projection),
        ("focus_session_ends", Projection),
        ("focus_interruptions", Projection),
        ("review_snapshots", Projection),
        ("streams", Projection),
        // The op log and what hangs off it.
        ("ops", Source),
        ("op_dep", Source),
        ("outbox", Source),
        ("parked_ops", Source),
        ("deferred_ops", Source),
        ("sync_cursors", Source),
        ("compaction_floor", Source),
        ("peer_frontiers", Source),
        ("stream_snapshots", Source),
        ("chain_heads_sent", Source),
        ("chain_expected", Source),
        ("chain_claims", Source),
        ("fork_evidence", Source),
        ("chain_divergence", Source),
        // Keys, identity and devices, and the control-op folds over them.
        ("schema_meta", Source),
        ("local_identity", Source),
        ("identity", Source),
        ("identity_transitions", Source),
        ("stream_keys", Source),
        ("stream_key_senders", Source),
        ("key_envelope_recipients", Source),
        ("devices", Source),
        ("device_revocations", Source),
        ("device_revoke_ops", Source),
        ("device_read_bounds", Source),
        ("relay_revocation_intents", Source),
        ("vault_required_features", Source),
        ("device_features", Source),
        // Local-only state no op carries.
        ("device_preferences", Source),
        ("notes", Source),
        ("persons", Source),
        ("blob_uploads", Source),
        ("blob_fetches", Source),
        ("blob_cache", Source),
        ("blob_cache_backfill", Source),
    ]
};

/// The op-log `target_kind`s a rebuild folds: the tag of every entity that
/// merges as [`Merge::Lww`] or [`Merge::AppendOnly`] and is projected to a
/// table, read off the registry.
///
/// Selected by target and not by `inner_kind`, because the log's
/// `inner_kind` is not always the registry's: a defer is logged as
/// `task.defer`, and is a `TaskUpdate` on the wire.
fn replayable_tags() -> Vec<&'static str> {
    ENTITIES
        .iter()
        .filter(|spec| {
            matches!(spec.merge, Merge::Lww | Merge::AppendOnly) && spec.storage().is_some()
        })
        .map(|spec| spec.tag)
        .collect()
}

/// The one control op whose `target_kind` is an entity's tag: a stream
/// digest names the stream it describes.
const DIGEST_KIND: &str = "stream.digest";

/// One entity op to fold again: its merge stamp, and where its row is.
struct Pending {
    at: Stamp,
    rowid: i64,
}

/// The page cache a rebuild runs with, as `PRAGMA cache_size` takes it: a
/// negative number is KiB, so 128 MiB. See [`Engine::rebuild_projection`].
const REBUILD_CACHE_KIB: i64 = -131_072;

/// The local columns a rebuild puts back after the refold.
struct Kept {
    materialized_until: Vec<(Vec<u8>, i64)>,
}

impl Kept {
    fn save(tx: &Transaction<'_>) -> rusqlite::Result<Self> {
        let mut stmt = tx.prepare(
            "SELECT id, materialized_until_ms FROM routines
             WHERE materialized_until_ms <> 0 ORDER BY id",
        )?;
        let materialized_until = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Self { materialized_until })
    }

    fn restore(&self, tx: &Transaction<'_>) -> rusqlite::Result<()> {
        for (id, ms) in &self.materialized_until {
            tx.execute(
                "UPDATE routines SET materialized_until_ms = ?1 WHERE id = ?2",
                params![ms, id],
            )?;
        }
        Ok(())
    }
}

/// Clear every [`TableClass::Projection`] table, keeping the rows the module
/// docs name: placeholder stream rows, and search rows of other kinds.
fn clear(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    // Rows reference each other (a task its stream, a stream its parent).
    // Every reference is whole again once the refold has run, which is
    // before the transaction commits.
    tx.execute_batch("PRAGMA defer_foreign_keys = ON")?;
    for (table, class) in TABLE_CLASSES {
        if *class != TableClass::Projection {
            continue;
        }
        let sql = match *table {
            "streams" => "DELETE FROM streams WHERE lww_device IS NOT NULL".to_owned(),
            "search_idx" => "DELETE FROM search_idx WHERE kind = 'task'".to_owned(),
            other => format!("DELETE FROM {other}"),
        };
        tx.execute(&sql, [])?;
    }
    Ok(())
}

/// Stand a plain temporary table in for the task rows of `search_idx` while
/// the refold writes them.
///
/// The row writers keep the index current one task at a time, and each write
/// first deletes the task's row by `id`, a column FTS5 cannot index. That is
/// a scan of the whole index per write, which made a rebuild quadratic in the
/// vault's task count: 2.7 ms an op at 10k tasks and growing. A `TEMP` table
/// of the same name shadows the main one for every unqualified statement on
/// this connection, and is indexed on `(kind, id)`. [`unshadow_search`] then
/// copies its rows into the index in one statement, which is step 4 of the
/// design ("rebuild FTS"). It is created inside the transaction, so a rollback
/// drops it with everything else.
fn shadow_search(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute_batch(
        "CREATE TEMP TABLE search_idx
           (kind TEXT, id BLOB, stream_id BLOB, title TEXT, body TEXT, contexts TEXT);
         CREATE INDEX temp.search_idx_by_id ON search_idx (kind, id);",
    )
}

/// Move the shadowed task rows into the full-text index and drop the shadow.
fn unshadow_search(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute_batch(
        "INSERT INTO main.search_idx (kind, id, stream_id, title, body, contexts)
           SELECT kind, id, stream_id, title, body, contexts FROM temp.search_idx
           ORDER BY rowid;
         DROP TABLE temp.search_idx;",
    )
}

/// A stored op the rebuild cannot read. The log is damaged, and the rebuild
/// stops rather than project a vault without the op.
fn unreadable(stream: &[u8; 16], device: &[u8; 16], seq: u64, why: &str) -> EngineError {
    EngineError::Invalid(format!(
        "rebuild: the op at seq {seq} of device {} in stream {} {why}",
        hex_short(device),
        hex_short(stream)
    ))
}

impl Engine {
    /// Rebuild the projection from the local op log: clear every table the
    /// entity ops are folded into and fold them again, in one transaction.
    /// See the [module docs](self) for the steps and for what is kept.
    ///
    /// Nothing is sent and no clock is read: the result is a function of the
    /// vault's op log, its compacted merge state and the local rows it keeps.
    /// No [`DomainEvent`](crate::events::DomainEvent) is produced; a client
    /// re-reads what it shows.
    ///
    /// # Errors
    /// [`EngineError::Invalid`] when an applied op in the log cannot be
    /// opened or decoded, or the compacted state does not parse; storage
    /// failures. Either way the transaction rolls back and the projection is
    /// as it was.
    pub fn rebuild_projection(&self, db: &mut Db) -> Result<RebuildReport, EngineError> {
        // The whole refold is one transaction, and every page it dirties
        // stays dirty until the commit. Past the default page cache SQLite
        // spills them to the WAL mid-transaction, encrypting each, and reads
        // them back, decrypting each, which made the rebuild's cost per op
        // grow with the vault. A larger cache for its duration keeps them in
        // memory; the setting is put back after.
        let cache: i64 = db.conn().query_row("PRAGMA cache_size", [], |r| r.get(0))?;
        db.conn()
            .execute_batch(&format!("PRAGMA cache_size = {REBUILD_CACHE_KIB}"))?;
        let result = in_tx(db, |tx| self.rebuild_in(tx));
        db.conn()
            .execute_batch(&format!("PRAGMA cache_size = {cache}"))?;
        let report = result.inspect_err(|_| {
            // The rollback drops the shadow table with the rest; this makes
            // sure no later statement on this connection can resolve to it.
            let _ = db
                .conn()
                .execute_batch("DROP TABLE IF EXISTS temp.search_idx");
        })?;
        tracing::info!(
            ev = "core.rebuild.done",
            n_ops = report.ops_replayed,
            n_dropped = report.ops_skipped,
            "the projection was rebuilt from the op log"
        );
        Ok(report)
    }

    /// [`Self::rebuild_projection`] inside the caller's transaction.
    pub(super) fn rebuild_in(&self, tx: &Transaction<'_>) -> Result<RebuildReport, EngineError> {
        let base = dump_compacted_base(tx)?;
        let kept = Kept::save(tx)?;
        let pending = pending_ops(tx)?;
        clear(tx)?;
        shadow_search(tx)?;
        let compacted = join_stream_state(tx, &base)?;
        let mut report = RebuildReport {
            compacted_entities: compacted.len() as u64,
            ..RebuildReport::default()
        };
        self.refold(tx, pending, &mut report)?;
        unshadow_search(tx)?;
        kept.restore(tx)?;
        Ok(report)
    }

    /// Fold `pending` through the receive path's entity step, in order.
    fn refold(
        &self,
        tx: &Transaction<'_>,
        pending: Vec<Pending>,
        report: &mut RebuildReport,
    ) -> Result<(), EngineError> {
        let signers = self.device_signing_keys(tx)?;
        let mut stmt = tx.prepare("SELECT envelope FROM ops WHERE rowid = ?1")?;
        for op in pending {
            let (stream, device, seq) = (op.at.stream, op.at.device, op.at.seq);
            let bytes: Vec<u8> = stmt.query_row(params![op.rowid], |r| r.get(0))?;
            let env = decode_envelope(&bytes)
                .map_err(|_| unreadable(&stream, &device, seq, "does not decode"))?;
            let signer = signers
                .get(&env.device_id)
                .ok_or_else(|| unreadable(&stream, &device, seq, "has no known signer"))?;
            let keys = self.keychain.stream_keys_at(&env.stream_id, env.epoch);
            let inner_cbor = keys
                .iter()
                .map(Some)
                .chain([None])
                .find_map(|k| open_envelope(&env, signer, k).ok())
                .ok_or_else(|| unreadable(&stream, &device, seq, "does not open"))?;
            let mut inner = decode_inner_op(&inner_cbor)
                .map_err(|_| unreadable(&stream, &device, seq, "is not an op this build reads"))?;
            if inner.is_control() {
                // Its effect is a control fold, which a rebuild keeps.
                continue;
            }
            remap_legacy_inbox(&mut inner);
            if let InnerOp::Patch(p) = &inner {
                if let Err(problem) = check_patch(p) {
                    tracing::warn!(
                        ev = "core.rebuild.op_skipped",
                        reason = match problem {
                            PatchProblem::Park(_) => "park",
                            PatchProblem::Invalid(_) => "invalid",
                        },
                        stream_h = hex_short(&stream),
                        sender_h = hex_short(&device),
                        seq = seq,
                        "an applied patch this build would not apply was left out of the rebuild"
                    );
                    report.ops_skipped += 1;
                    continue;
                }
            }
            let lww = LwwStamp {
                hlc: env.hlc,
                device: env.device_id,
                seq: env.seq,
            };
            materialize_remote(tx, &inner, &lww, &env.stream_id)?;
            report.ops_replayed += 1;
        }
        Ok(())
    }
}

/// Every applied entity op in the log, by merge stamp. Only each envelope's
/// header is kept, so the log is never held in memory whole.
fn pending_ops(tx: &Transaction<'_>) -> Result<Vec<Pending>, EngineError> {
    // Each tag is a registry literal of `[a-z_]`, so quoting it is safe.
    let tags = replayable_tags()
        .iter()
        .map(|t| format!("'{t}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut stmt = tx.prepare(&format!(
        "SELECT rowid, stream_id, device_id, seq, envelope FROM ops
         WHERE target_kind IN ({tags}) AND inner_kind <> '{DIGEST_KIND}'
           AND applied_at IS NOT NULL
           AND op_id NOT IN (SELECT op_id FROM parked_ops)"
    ))?;
    let mut rows = stmt.query([])?;
    let mut pending = Vec::new();
    while let Some(r) = rows.next()? {
        let rowid: i64 = r.get(0)?;
        let stream = blob16(&r.get::<_, Vec<u8>>(1)?);
        let device = blob16(&r.get::<_, Vec<u8>>(2)?);
        let seq = u64::try_from(r.get::<_, i64>(3)?).unwrap_or(0);
        let env = decode_envelope(&r.get::<_, Vec<u8>>(4)?)
            .map_err(|_| unreadable(&stream, &device, seq, "does not decode"))?;
        pending.push(Pending {
            at: Stamp {
                hlc: env.hlc,
                device: env.device_id,
                seq: env.seq,
                stream: env.stream_id,
            },
            rowid,
        });
    }
    pending.sort_by_key(|p| p.at);
    Ok(pending)
}

/// The rebuild-identity check every convergence test runs (ADR-0065
/// §Consequences): a rebuild of the vault leaves every projection table as
/// it was, byte for byte.
#[cfg(test)]
pub(super) mod check {
    use super::{TableClass, TABLE_CLASSES};
    use crate::engine::merge::fold_rows;
    use crate::engine::Engine;
    use rusqlite::types::Value;
    use rusqlite::Connection;
    use sunrise_id::registry::{Merge, ENTITIES};
    use sunrise_storage::Db;

    /// The materialized tables of every entity whose ops a rebuild folds,
    /// read off the registry, which [`TABLE_CLASSES`] must call projections.
    pub(in crate::engine) fn projection_tables() -> Vec<&'static str> {
        let mut tables = Vec::new();
        for spec in &ENTITIES {
            if !matches!(spec.merge, Merge::Lww | Merge::AppendOnly) {
                continue;
            }
            for record in spec.records {
                if let Some(storage) = record.storage {
                    if !tables.contains(&storage.table) {
                        tables.push(storage.table);
                    }
                }
            }
        }
        tables
    }

    /// Every row of every projection table, each table's rows sorted.
    pub(in crate::engine) fn dump(conn: &Connection) -> Vec<(&'static str, Vec<String>)> {
        dump_with(conn, false)
    }

    /// [`dump`], leaving out, when `seeded`, the three things in the merge
    /// state that no projection reads and that a replay need not reproduce:
    /// `merge_entities`' create stamp, the `Task.blocks` register, and an
    /// OR-set remove whose add is gone. Each is explained where it is left out.
    ///
    /// The create stamp is what a row seed cannot know. A local command
    /// writes its row directly, and the merge first folds that row as the
    /// legacy op at the row's stamp (`sync_from_row`), so an entity whose
    /// first fold was a seed records its newest local write as its create.
    /// A rebuild folds the real create. Nothing projects from it while a
    /// legacy register holds `created_at`, which every full-state op writes,
    /// and the projected rows are still compared whole. ADR-0065 PR 3, which
    /// folds every local write, makes the two agree and retires this.
    fn dump_with(conn: &Connection, seeded: bool) -> Vec<(&'static str, Vec<String>)> {
        let mut out = Vec::new();
        for (table, class) in TABLE_CLASSES {
            if *class != TableClass::Projection {
                continue;
            }
            let sql = match *table {
                "search_idx" => {
                    "SELECT kind, id, stream_id, title, body, contexts FROM search_idx".to_owned()
                }
                "merge_entities" if seeded => {
                    let mut stmt = conn
                        .prepare("SELECT name FROM pragma_table_info('merge_entities')")
                        .expect("prepare columns");
                    let cols: Vec<String> = stmt
                        .query_map([], |r| r.get(0))
                        .expect("query columns")
                        .collect::<rusqlite::Result<Vec<String>>>()
                        .expect("read columns")
                        .into_iter()
                        .filter(|c| !c.starts_with("create_"))
                        .collect();
                    format!("SELECT {} FROM merge_entities", cols.join(", "))
                }
                // `Task.blocks` is read off `block_tasks`, so a seed records
                // the bindings made after the op it stands for. It is never
                // projected from the register.
                "merge_registers" if seeded => "SELECT r.* FROM merge_registers r
                     JOIN merge_entities m ON m.entity_id = r.entity_id
                     WHERE NOT (m.kind = 'task' AND r.field = 'blocks')"
                    .to_owned(),
                // A remove whose add the legacy floor pruned is inert: the add
                // never comes back. Whether it is still stored depends on
                // whether it arrived before the floor rose (which prunes it)
                // or after (which does not), so two replicas, or a replica and
                // its rebuild, can disagree about it. Only removes that name
                // a stored add are compared.
                "merge_orset_removes" if seeded => "SELECT r.* FROM merge_orset_removes r
                     WHERE EXISTS (SELECT 1 FROM merge_orset_adds a
                                   WHERE a.entity_id = r.entity_id AND a.field = r.field
                                     AND a.element = r.element AND a.tag_stream = r.tag_stream
                                     AND a.tag_device = r.tag_device AND a.tag_seq = r.tag_seq)"
                    .to_owned(),
                other => format!("SELECT * FROM {other}"),
            };
            let mut stmt = conn.prepare(&sql).expect("prepare dump");
            let n = stmt.column_count();
            let mut rows: Vec<String> = stmt
                .query_map([], |r| {
                    (0..n)
                        .map(|i| r.get::<_, Value>(i).map(|v| format!("{v:?}")))
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .map(|cols| cols.join(" | "))
                })
                .expect("query dump")
                .collect::<rusqlite::Result<_>>()
                .expect("read dump");
            rows.sort();
            out.push((*table, rows));
        }
        out
    }

    impl Engine {
        /// Rebuild `db`'s projection inside a transaction, assert that every
        /// projection table reads exactly as it did, and roll back.
        ///
        /// Rows a local command wrote that the merge has not folded yet are
        /// folded first, as compaction and a snapshot fold them: a command
        /// still writes its row directly (ADR-0065 PR 3 removes that), and
        /// the merge state it leaves behind is complete only once folded.
        ///
        /// # Panics
        /// When the rebuilt projection differs, naming the first table.
        pub(in crate::engine) fn assert_rebuild_identity(&self, db: &Db) {
            let tx = db.conn().unchecked_transaction().expect("open the check");
            let targets: Vec<(String, Vec<u8>)> = {
                let mut stmt = tx
                    .prepare(
                        "SELECT DISTINCT target_kind, target_id FROM ops
                         WHERE target_id IS NOT NULL ORDER BY target_kind, target_id",
                    )
                    .expect("prepare targets");
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .expect("query targets")
                    .collect::<rusqlite::Result<_>>()
                    .expect("read targets")
            };
            fold_rows(&tx, &targets).expect("fold pending rows");
            let before = dump_with(&tx, true);
            self.rebuild_in(&tx).expect("rebuild");
            let after = dump_with(&tx, true);
            tx.rollback().expect("roll the check back");
            for ((table, a), (_, b)) in before.iter().zip(&after) {
                let lost: Vec<&String> = a.iter().filter(|r| !b.contains(r)).collect();
                let gained: Vec<&String> = b.iter().filter(|r| !a.contains(r)).collect();
                assert!(
                    lost.is_empty() && gained.is_empty() && a.len() == b.len(),
                    "rebuilding the projection changed `{table}`:\n\
                     only live: {lost:#?}\nonly rebuilt: {gained:#?}"
                );
            }
        }
    }
}
