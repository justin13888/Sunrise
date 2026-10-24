//! Rebuilding the projection from the local op log (issue #441,
//! `docs/04-storage/migrations.md` §Migration rigor rule 4).
//!
//! # What a rebuild re-derives
//!
//! The entity rows, their side tables (`task_contexts`, `task_blockers`,
//! `block_tasks`), the rows of both full-text indexes ([ADR-0052]), the
//! per-field merge state they are projected from ([ADR-0044]), and the
//! version DAG beside it
//! ([ADR-0065] §1–§2), are all a function of the entity ops this replica
//! applied. [`Engine::rebuild_projection`] clears them and folds those ops
//! again, in one transaction:
//!
//! 1. **The compacted base.** An op below a `compaction_floor` has been folded
//!    and deleted ([ADR-0059]); a replica that joined a snapshot never held
//!    it. What those ops wrote is still in the merge state, attributed to
//!    them by stamp, so it is dumped first ([`dump_compacted_base`]).
//! 2. **Clear** every table [`TABLE_CLASSES`] classifies as a projection,
//!    keeping the rows named below.
//! 3. **Join the base back** with [`join_stream_state`], the snapshot join,
//!    which brings the compacted versions and their frontiers back first.
//! 4. **Refold the log.** Every applied entity op still in `ops` is read,
//!    sorted by its merge stamp `(hlc, device, seq, stream)`, opened, and
//!    folded through [`Engine::fold_received`], the step the receive path
//!    takes, which holds a group member until its group is complete.
//!    Parked ops are not entity ops here: this build cannot read them, so the
//!    live projection never held them either, and their replay after an
//!    upgrade is [`Engine::replay_parked_ops`]'s.
//! 5. Put back the local columns named below.
//!
//! [`Engine::rebuild_projection`] then records the fold version it ran at,
//! [`FOLD_V`], in `projection_rebuild`.
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
//! local-only tables such as `device_preferences` and `blob_cache`, and
//! `search_meta`, the record of how the full-text indexes were built, whose
//! tokenizer the rebuild keeps. A background re-index of the word index for a
//! new content language (`engine::search`) is abandoned, since the docs it
//! copied are rewritten, and starts again the next time it is asked for.
//! Inside the cleared tables three things survive, because no op still in
//! the log derives them:
//!
//! - a placeholder `streams` row no op has stamped (`lww_device IS NULL`),
//!   which `ensure_stream_row` wrote when an op named a stream before its
//!   create; the lazily seeded Inbox is one;
//! - each routine's `materialized_until_ms`, how far this device has
//!   generated its occurrences, which is never on the wire;
//! - every `pending_groups` row whose op is no longer in the log. Compaction
//!   deletes a group member only once its group has folded, and the row is
//!   what tells the refold that the member is present, so the members still
//!   in the log complete the group again.
//!
//! The versions and the register losers of compacted ops are not kept here:
//! compaction keeps them (ADR-0065 D10), and the base, a `doc_state` v2,
//! carries them with their frontiers beside the merge state (§8), so they
//! come back with the join.
//!
//! # When a rebuild runs on its own
//!
//! [`Engine::rebuild_if_stale`], which `Core::open` calls before anything
//! reads or folds into the projection, rebuilds a vault whose
//! `projection_rebuild` row is missing or names another [`FOLD_V`]: the first
//! open after migration 0039, and the first open of a build whose fold
//! differs. It is the same one-transaction rebuild, so a vault shows either
//! the projection it had or the rebuilt one, never a mix, and one that fails
//! is rolled back and tried again at the next open. Its progress is logged
//! every [`PROGRESS_EVERY`] ops.
//!
//! [`TABLE_CLASSES`] classifies every table in the schema. A test fails when
//! a migration adds a table it does not classify, or when the registry
//! projects an entity to a table it does not call a projection.
//!
//! [ADR-0044]: ../../../../docs/11-adr/0044-per-field-ops.md
//! [ADR-0052]: ../../../../docs/11-adr/0052-search-v2.md
//! [ADR-0059]: ../../../../docs/11-adr/0059-client-op-log-compaction.md
//! [ADR-0065]: ../../../../docs/11-adr/0065-copy-on-write-entity-versions.md

use super::ids::{blob16, hex_short};
use super::lww::{remap_legacy_inbox, LwwStamp};
use super::merge::{
    check_patch, dump_compacted_base, join_stream_state, PatchProblem, Stamp, FOLD_V,
};
use super::snapshot::in_tx;
use super::{Engine, EngineError};
use crate::inner_op::{decode_inner_op, InnerOp};
use rusqlite::{params, OptionalExtension, Transaction};
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
        // The full-text rows of every indexed kind, and the side tables of
        // entity rows.
        ("search_docs", Projection),
        ("search_words", Projection),
        ("search_tri", Projection),
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
        // The version DAG (ADR-0065).
        ("entity_versions", Projection),
        ("entity_frontier", Projection),
        ("register_losers", Projection),
        ("pending_groups", Projection),
        // Entity rows.
        ("tasks", Projection),
        ("contexts", Projection),
        ("routines", Projection),
        ("blocks", Projection),
        ("attachments", Projection),
        ("preferences", Projection),
        ("saved_views", Projection),
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
        // How the full-text indexes were built: their tokenizer, which a
        // rebuild keeps, and any re-index under way, which it abandons.
        ("search_meta", Source),
        // The record of the last rebuild, which the rebuild writes.
        ("projection_rebuild", Source),
    ]
};

/// The op-log `target_kind`s a rebuild folds: the tag of every entity that
/// merges as [`Merge::Lww`] or [`Merge::AppendOnly`] and is projected to a
/// table, read off the registry.
///
/// Selected by target and not by `inner_kind`, because the log's
/// `inner_kind` is not always the registry's: a vault keeps the defers its
/// own commands logged as `task.defer` before ADR-0065 PR 3, each a
/// `TaskUpdate` on the wire.
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
    /// When this replica logged it, which a held group member records.
    received_at_ms: u64,
}

/// How many ops a rebuild folds between two `core.rebuild.progress` events.
const PROGRESS_EVERY: u64 = 5_000;

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
/// docs name: placeholder stream rows, and the group members whose ops
/// compaction deleted. A background re-index of the word index is abandoned.
fn clear(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    // Rows reference each other (a task its stream, a stream its parent).
    // Every reference is whole again once the refold has run, which is
    // before the transaction commits.
    tx.execute_batch(
        "PRAGMA defer_foreign_keys = ON;
         DROP TABLE IF EXISTS search_words_next;
         UPDATE search_meta SET reindex_stemmed = NULL, reindex_cursor = 0;",
    )?;
    for (table, class) in TABLE_CLASSES {
        if *class != TableClass::Projection {
            continue;
        }
        let sql = match *table {
            "streams" => "DELETE FROM streams WHERE lww_device IS NOT NULL".to_owned(),
            "pending_groups" => "DELETE FROM pending_groups WHERE EXISTS (
                     SELECT 1 FROM ops o WHERE o.stream_id = pending_groups.stream
                       AND o.device_id = pending_groups.device AND o.seq = pending_groups.seq)"
                .to_owned(),
            other => format!("DELETE FROM {other}"),
        };
        tx.execute(&sql, [])?;
    }
    Ok(())
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
    /// The `engine::rebuild` module docs give the steps and what is kept.
    ///
    /// Nothing is sent, and the projection is a function of the vault's op
    /// log, its compacted merge state and the local rows it keeps. The clock
    /// is read once, for when `projection_rebuild` records the rebuild
    /// finished. No [`DomainEvent`](crate::events::DomainEvent) is produced; a
    /// client re-reads what it shows.
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
        let now_ms = self.clock.now_ms();
        let result = in_tx(db, |tx| {
            let report = self.rebuild_in(tx)?;
            tx.execute(
                "INSERT INTO projection_rebuild (id, fold_v, done_at_ms) VALUES (1, ?1, ?2)
                 ON CONFLICT (id) DO UPDATE SET
                    fold_v = excluded.fold_v, done_at_ms = excluded.done_at_ms",
                params![FOLD_V, now_ms],
            )?;
            Ok(report)
        });
        db.conn()
            .execute_batch(&format!("PRAGMA cache_size = {cache}"))?;
        let report = result?;
        tracing::info!(
            ev = "core.rebuild.done",
            n_ops = report.ops_replayed,
            n_dropped = report.ops_skipped,
            "the projection was rebuilt from the op log"
        );
        Ok(report)
    }

    /// Rebuild the projection when the vault was last rebuilt at another
    /// [`FOLD_V`] than this build's, or never: the first open after
    /// migration 0039, or the first open of a build whose fold differs. See
    /// the [module docs](self#when-a-rebuild-runs-on-its-own).
    ///
    /// The vault is copied first ([`Db::back_up_for_rebuild`]): the open's
    /// own pre-migration copy when it took one, `<vault>.pre-rebuild.bak`
    /// otherwise, and nothing for a vault the open has just created.
    ///
    /// Returns what the rebuild did, or `None` when the projection was
    /// current and nothing ran.
    ///
    /// # Errors
    /// [`sunrise_storage::DbError::RebuildBackup`] when the copy cannot be
    /// written, and nothing is rebuilt. As [`Self::rebuild_projection`]
    /// otherwise, which leaves the projection as it was and
    /// `projection_rebuild` unwritten, so the next call tries again.
    pub(crate) fn rebuild_if_stale(
        &self,
        db: &mut Db,
    ) -> Result<Option<RebuildReport>, EngineError> {
        let at: Option<i64> = db
            .conn()
            .query_row(
                "SELECT fold_v FROM projection_rebuild WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if at == Some(i64::from(FOLD_V)) {
            return Ok(None);
        }
        tracing::info!(
            ev = "core.rebuild.stale",
            from_v = at,
            to_v = FOLD_V,
            "the projection was built by another fold, and is rebuilt from the op log"
        );
        // Under a copy of the vault (rule 4), or the one this open took
        // before migrating it. A vault this open created has nothing yet a
        // copy would keep, and one in memory has no file to copy.
        if !db.created_by_this_open() {
            db.back_up_for_rebuild()?;
        }
        self.rebuild_projection(db).map(Some)
    }

    /// [`Self::rebuild_projection`] inside the caller's transaction.
    pub(super) fn rebuild_in(&self, tx: &Transaction<'_>) -> Result<RebuildReport, EngineError> {
        let base = dump_compacted_base(tx)?;
        let kept = Kept::save(tx)?;
        let pending = pending_ops(tx)?;
        clear(tx)?;
        let compacted = join_stream_state(tx, &base)?;
        let mut report = RebuildReport {
            compacted_entities: compacted.len() as u64,
            ..RebuildReport::default()
        };
        self.refold(tx, pending, &mut report)?;
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
        let total = pending.len() as u64;
        for (done, op) in (0u64..).zip(pending) {
            if done > 0 && done % PROGRESS_EVERY == 0 {
                tracing::info!(
                    ev = "core.rebuild.progress",
                    n_ops = done,
                    n_total = total,
                    "the projection rebuild is part way through the op log"
                );
            }
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
            self.fold_received(tx, &inner, &lww, &env.stream_id, op.received_at_ms)?;
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
        "SELECT rowid, stream_id, device_id, seq, envelope, received_at FROM ops
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
            received_at_ms: u64::try_from(r.get::<_, i64>(5)?).unwrap_or(0),
        });
    }
    pending.sort_by_key(|p| p.at);
    Ok(pending)
}

/// The rebuild-identity check every convergence test runs (ADR-0065
/// §Consequences): a rebuild of the vault leaves every projection table as
/// it was, byte for byte.
///
/// [`Engine::assert_rebuild_identity`] runs it on demand. It also runs after
/// every [`Engine::apply`] and every [`Engine::apply_remote_all`] on an
/// engine built by the tests' `engine_seeded`, which turns it on for its
/// thread ([`enable`]), as does the tests' plain `engine`. A test that builds
/// rows no op wrote, as a vault older than the merge holds them, turns it off
/// ([`disable`]).
#[cfg(test)]
pub(super) mod check {
    use super::{TableClass, TABLE_CLASSES};
    use crate::engine::Engine;
    use rusqlite::types::Value;
    use rusqlite::Connection;
    use std::cell::Cell;
    use sunrise_id::registry::{Merge, ENTITIES};
    use sunrise_storage::Db;

    thread_local! {
        /// Whether a command or a delivery checks the rebuild after it.
        static AFTER_EVERY_WRITE: Cell<bool> = const { Cell::new(false) };
    }

    /// Check the rebuild after every command and delivery on this thread.
    pub(crate) fn enable() {
        AFTER_EVERY_WRITE.with(|c| c.set(true));
    }

    /// Stop the after-every-write check on this thread for the rest of the
    /// test: it built rows no op wrote, as a vault older than the merge holds
    /// them, and no rebuild of the log can reproduce those.
    pub(crate) fn disable() {
        AFTER_EVERY_WRITE.with(|c| c.set(false));
    }

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
        let mut out = Vec::new();
        for (table, class) in TABLE_CLASSES {
            if *class != TableClass::Projection {
                continue;
            }
            // A full-text row is named by its entity, not its rowid, which
            // is the order the docs were first written in.
            let sql = match *table {
                "search_docs" => "SELECT kind, id FROM search_docs".to_owned(),
                "search_words" => "SELECT d.kind, d.id, w.title, w.aux, w.body
                     FROM search_words w LEFT JOIN search_docs d ON d.doc = w.rowid"
                    .to_owned(),
                "search_tri" => "SELECT d.kind, d.id, t.title, t.body
                     FROM search_tri t LEFT JOIN search_docs d ON d.doc = t.rowid"
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
        /// projection table, merge state included, reads exactly as it did,
        /// and roll back.
        ///
        /// # Panics
        /// When the rebuilt projection differs, naming the first table.
        pub(in crate::engine) fn assert_rebuild_identity(&self, db: &Db) {
            let tx = db.conn().unchecked_transaction().expect("open the check");
            let before = dump(&tx);
            self.rebuild_in(&tx).expect("rebuild");
            let after = dump(&tx);
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

        /// [`Self::assert_rebuild_identity`], when this thread checks after
        /// every write ([`enable`]) and the write left a transaction-free
        /// connection to check on.
        pub(in crate::engine) fn check_rebuild_after_write(&self, db: &Db) {
            if AFTER_EVERY_WRITE.with(Cell::get) && db.conn().is_autocommit() {
                self.assert_rebuild_identity(db);
            }
        }
    }
}
