//! Remote materialization and the entity-level LWW rule (ADR-0014).
//!
//! One decision rule — `lww_wins` over a `(ts_ms, device_id)` stamp — and the
//! one dispatcher that applies it. `materialize_remote`'s `(table, id_col)`
//! match is the single point in the engine that knows every entity's storage
//! shape, which is exactly why it earns its own module rather than being
//! smeared across seven: a new entity adds one arm here, and a reader checking
//! that the merge is uniform across entities reads one function.

use super::context::{insert_context_row, purge_context_from_tasks, update_context_row};
use super::focus::materialize_focus_remote;
use super::routine::{insert_routine_row, update_routine_row};
use super::stream::{ensure_stream_row, insert_stream_row, update_stream_row};
use super::task::{
    ftsr_delete_task, ftsr_upsert_task, insert_task_contexts, insert_task_row,
    replace_task_blockers, replace_task_contexts, update_task_row,
};
use super::{
    insert_review_snapshot_row, replace_block_tasks, upsert_attachment_row, upsert_block_row,
    META_STREAM,
};
use crate::inner_op::InnerOp;
use rusqlite::{params, OptionalExtension, Transaction};
use sunrise_cbor::hlc::Hlc;
use sunrise_domain::INBOX_STREAM_BYTES;
use sunrise_id::{EntityKind, EntityRef};

/// The stamp that decides which of two writers to a materialized row survives.
///
/// Ordering is `(hlc, device, seq)`, in that order, and every term earns its
/// place:
///
/// * `hlc` — a hybrid logical clock, not a wall clock. It is the whole point:
///   an unbounded `ts_ms` let a device with a fast clock win every conflict it
///   ever entered (issue #21), and gave no way to order two writes inside one
///   millisecond.
/// * `device` — raw 16-byte memcmp, higher wins. Breaks *cross-device* ties
///   deterministically, so every replica picks the same winner.
/// * `seq` — the writer's per-`(stream, device)` counter, already envelope
///   field 4. Reached only when two ops from the SAME device carry an equal
///   `hlc`, which the send rule makes impossible while a device's HLC state
///   lives; it becomes possible across a process restart, when the logical
///   counter resets to 0. In that window `seq` is what still orders them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LwwStamp {
    /// Causal timestamp from the writing device.
    pub hlc: Hlc,
    /// The writing device's 16-byte id.
    pub device: [u8; 16],
    /// The writing device's per-`(stream, device)` sequence number.
    pub seq: u64,
}

/// The stamp stored on a materialized row. `device` is `None` only for a
/// placeholder row that no real op has yet stamped (the lazily created
/// inbox/meta stream), which loses to everything.
#[derive(Debug, Clone)]
pub(super) struct RowLww {
    pub(super) hlc: Hlc,
    pub(super) device: Option<Vec<u8>>,
    pub(super) seq: u64,
}

/// Read the stored LWW stamp for `id` in `table`. `None` when the row is
/// absent.
fn read_row_lww(
    tx: &Transaction<'_>,
    table: &str,
    id_col: &str,
    id: &[u8; 16],
) -> rusqlite::Result<Option<RowLww>> {
    let sql = format!(
        "SELECT lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device
         FROM {table} WHERE {id_col} = ?"
    );
    tx.query_row(&sql, params![&id[..]], |r| {
        Ok(RowLww {
            hlc: Hlc {
                physical_ms: u64::try_from(r.get::<_, i64>(0)?.max(0)).unwrap_or(0),
                logical: u32::try_from(r.get::<_, i64>(1)?.max(0)).unwrap_or(u32::MAX),
            },
            device: r.get::<_, Option<Vec<u8>>>(3)?,
            seq: u64::try_from(r.get::<_, i64>(2)?.max(0)).unwrap_or(0),
        })
    })
    .optional()
}

/// Entity-level LWW decision: does the incoming stamp beat the row's stored
/// one? Compares `(hlc, device, seq)` in that order.
pub(super) fn lww_wins(incoming: &LwwStamp, row: &RowLww) -> bool {
    if incoming.hlc != row.hlc {
        return incoming.hlc > row.hlc;
    }
    let Some(row_dev) = row.device.as_deref() else {
        // A placeholder row no op has stamped loses to any real op.
        return true;
    };
    if row_dev != incoming.device.as_slice() {
        return incoming.device.as_slice() > row_dev;
    }
    // Same device, same HLC. This is NOT a conflict, and the device-id memcmp
    // must NOT be applied here: `dev > dev` is false, so a device's own later
    // op would lose to its own earlier one — silently discarded on every remote
    // replica while the originating replica kept it. Permanent, undetected
    // divergence, and easy to hit before HLCs, when creating a task and
    // immediately patching it landed both ops in one millisecond.
    //
    // Ops from one device are causally ordered by their per-(stream, device)
    // `seq`, so the higher `seq` is the newer state and wins. Equal `seq` on the
    // same device is the same op re-delivered; the caller's idempotence gate has
    // already handled that, and `true` keeps a replay a harmless no-op rewrite.
    //
    // See `docs/05-sync/conflict-resolution.md` and ADR-0016.
    incoming.seq >= row.seq
}

/// Apply one decoded remote inner op to the materialized tables under LWW.
///
/// Compares the envelope's `(hlc, device, seq)` stamp against the target row's
/// stored one. If the envelope wins (or the row is absent for a
/// create/update), it performs the same insert/update the local path does and
/// stamps the LWW columns from the envelope. A losing op is a no-op here (it is
/// still recorded in the op log by the caller).
///
/// `*Delete` ops are full-state like every other op: each carries its entity
/// with `deleted` set, so a winning delete replaces the whole row and a delete
/// that overtakes its own create still materializes the tombstone (see
/// [`crate::inner_op`] docs and ADR-0014).
//
// Two arms below have empty bodies and are deliberately not merged: the
// append-only families and the control families are handled by different
// guards earlier in the function, and spelling both out is what makes adding a
// fifth family a compile-time decision rather than a silent fall-through.
/// Re-point a pre-0017 Inbox reference — the sixteen zero bytes the Inbox once
/// shared with the vault-meta stream — at [`INBOX_STREAM_BYTES`].
///
/// Migration 0017 does exactly this to the local tables of a vault that
/// upgrades. It cannot do it to the already-signed envelopes in `ops`, which
/// still name the old id and cannot be rewritten without breaking their
/// signatures. That is fine on the device that migrated, and not fine on a
/// device paired *after* the upgrade: that device receives the legacy
/// `([0u8; 16], 1)` key in its pairing payload, replays the whole history, and
/// materializes those tasks under the id in the payload — which is the
/// vault-meta stream id. `materialize_remote` would then `ensure_stream_row`
/// the one stream that must never have a `streams` row, and the two replicas of
/// one account would disagree about where the user's oldest tasks live.
///
/// Remapping here rather than refusing the ops is deliberate. Refusing would
/// drop the user's pre-0017 Inbox on the new device while the migrated device
/// kept it — permanent, silent divergence, and a data loss the user did not ask
/// for. Remapping reproduces the migration's own rewrite at the only other
/// place the same rows can be born, so both replicas land on the same state.
///
/// Only *entity* payloads are touched. A `key_envelope` naming the vault-meta
/// stream at `[0u8; 16]` is naming it correctly and is left alone.
///
/// Delete at 1.0, with the rest of the 0017 legacy path.
pub(super) fn remap_legacy_inbox(inner: &mut InnerOp) {
    fn fix(r: &mut EntityRef) {
        if r.bytes() == &META_STREAM {
            *r = EntityRef::new(EntityKind::Stream, INBOX_STREAM_BYTES);
        }
    }
    match inner {
        InnerOp::TaskCreate(t) | InnerOp::TaskUpdate(t) | InnerOp::TaskDelete(t) => {
            fix(&mut t.stream_id);
        }
        InnerOp::RoutineCreate(r) | InnerOp::RoutineUpdate(r) | InnerOp::RoutineDelete(r) => {
            fix(&mut r.template.stream_id);
        }
        InnerOp::BlockCreate(b) | InnerOp::BlockUpdate(b) | InnerOp::BlockDelete(b) => {
            fix(&mut b.stream_id);
        }
        InnerOp::FocusStart(f) => fix(&mut f.stream_id),
        _ => {}
    }
}

#[allow(clippy::match_same_arms)]
pub(super) fn materialize_remote(
    tx: &Transaction<'_>,
    inner: &InnerOp,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    // A control op has no entity and must never reach the kind table below,
    // whose `_ =>` arm would file anything it does not recognise under
    // `tasks`. `apply_remote` routes them away before this is called; this is
    // the belt to that braces, and it is a `debug_assert` rather than a silent
    // return so a routing mistake surfaces in a test run instead of as a
    // mysterious task row in production.
    if inner.is_control() {
        debug_assert!(false, "a control op reached the entity materializer");
        return Ok(());
    }
    let ts_ms = lww.hlc.physical_ms;
    // Focus ops never enter the LWW contest. Each writes one immutable record
    // keyed by the session's own id (start, end, and interruption in three
    // distinct tables), so there is nothing for a later op to overwrite and
    // nothing for an earlier one to lose — including when an `end` overtakes
    // its own `start`. Running an LWW comparison here would be actively wrong:
    // a `start` stamped later than its `end` would suppress the `end`.
    if matches!(
        inner,
        InnerOp::FocusStart(_) | InnerOp::FocusEnd(_) | InnerOp::FocusInterrupt(_)
    ) {
        return materialize_focus_remote(tx, inner, lww);
    }
    // A review snapshot is append-only for the same reason: it is keyed by its
    // own `rvw_` id, written once, and never edited. Running LWW here would let
    // one device's review of a week suppress another device's, which is exactly
    // the loss the representation exists to prevent. It also deliberately does
    // NOT `ensure_stream_row` for the Streams it names — the counts live inside
    // an opaque blob, so a snapshot that overtakes a `stream.create` still
    // lands intact.
    if let InnerOp::ReviewSnapshotCreate(snapshot) = inner {
        return insert_review_snapshot_row(tx, snapshot, lww);
    }
    let (table, id_col) = match inner.entity_kind() {
        EntityKind::Stream => ("streams", "stream_id"),
        EntityKind::Context => ("contexts", "id"),
        EntityKind::Routine => ("routines", "id"),
        EntityKind::Block => ("blocks", "id"),
        EntityKind::Attachment => ("attachments", "id"),
        // Task (and any future kind) key on `id`.
        _ => ("tasks", "id"),
    };
    let target = inner.target_ref();
    let existing = read_row_lww(tx, table, id_col, target.bytes())?;
    let wins = existing.as_ref().is_none_or(|row| lww_wins(lww, row));
    if !wins {
        return Ok(());
    }
    let present = existing.is_some();
    match inner {
        InnerOp::TaskCreate(t) | InnerOp::TaskUpdate(t) => {
            // The owning stream must exist before the task (FK).
            ensure_stream_row(tx, &t.stream_id, ts_ms)?;
            if present {
                update_task_row(tx, t, lww)?;
                replace_task_contexts(tx, t)?;
            } else {
                insert_task_row(tx, t, lww)?;
                insert_task_contexts(tx, t)?;
            }
            // Dependency edges ride along with the full-state task op. They are
            // written even when the blockers they name have not arrived on this
            // replica yet — an unknown blocker reads as still-open, so the
            // dependent shows blocked until its blocker turns up, whichever
            // order the two ops land in.
            replace_task_blockers(tx, t)?;
            ftsr_upsert_task(tx, t)?;
        }
        InnerOp::TaskDelete(t) => {
            // Applied exactly like TaskUpdate, because that is what it now is:
            // a full-state op whose state happens to have `deleted` set. The
            // whole row is replaced, so two replicas that had diverged on
            // `title` before the delete converge on the deleting replica's
            // state rather than each keeping its own.
            ensure_stream_row(tx, &t.stream_id, ts_ms)?;
            if present {
                update_task_row(tx, t, lww)?;
                replace_task_contexts(tx, t)?;
            } else {
                insert_task_row(tx, t, lww)?;
                insert_task_contexts(tx, t)?;
            }
            replace_task_blockers(tx, t)?;
            // Not `ftsr_upsert_task`: a tombstoned task must leave the search
            // index, and `update_task_row` does not touch it.
            ftsr_delete_task(tx, &t.id)?;
        }
        InnerOp::StreamCreate(s) | InnerOp::StreamUpdate(s) => {
            if present {
                update_stream_row(tx, s, lww)?;
            } else {
                insert_stream_row(tx, s, lww)?;
            }
        }
        InnerOp::StreamDelete(s) => {
            // Applied as the full-state op it now is, so the whole row is
            // replaced rather than only its tombstone flag.
            if present {
                update_stream_row(tx, s, lww)?;
            } else {
                insert_stream_row(tx, s, lww)?;
            }
        }
        InnerOp::ContextCreate(c) | InnerOp::ContextUpdate(c) => {
            if present {
                update_context_row(tx, c, lww)?;
            } else {
                insert_context_row(tx, c, lww)?;
            }
        }
        InnerOp::ContextDelete(c) => {
            // The membership purge is keyed on the context id alone and is
            // idempotent, so it runs even when this replica has not yet
            // materialized the Context row itself (a delete that overtook its
            // create). That keeps "deleting a Context removes it from all
            // Tasks" true on every replica that sees the delete.
            purge_context_from_tasks(tx, target.bytes())?;
            // Full-state, like every other delete: insert when this replica has
            // not materialized the Context yet. Without the `else` the delete
            // was dropped on arrival, the older create then landed live, and
            // the replica sat permanently out of step with the one that
            // deleted — the purge above ran, so the memberships were stripped
            // while the Context itself stayed alive, which is worse than
            // either outcome alone.
            if present {
                update_context_row(tx, c, lww)?;
            } else {
                insert_context_row(tx, c, lww)?;
            }
        }
        InnerOp::RoutineCreate(r) | InnerOp::RoutineUpdate(r) => {
            ensure_stream_row(tx, &r.template.stream_id, ts_ms)?;
            if present {
                update_routine_row(tx, r, lww)?;
            } else {
                insert_routine_row(tx, r, ts_ms, lww)?;
            }
        }
        InnerOp::RoutineDelete(rt) => {
            if present {
                update_routine_row(tx, rt, lww)?;
            } else {
                insert_routine_row(tx, rt, ts_ms, lww)?;
            }
        }
        // The delete rides the same arm as create and update: it is a
        // full-state op now, so the whole row is replaced rather than only its
        // tombstone flag. That also makes a delete that overtakes its create
        // land as a tombstoned row instead of vanishing.
        InnerOp::BlockCreate(b) | InnerOp::BlockUpdate(b) | InnerOp::BlockDelete(b) => {
            ensure_stream_row(tx, &b.stream_id, ts_ms)?;
            upsert_block_row(tx, b, lww)?;
            // Bindings ride along with the full-state Block op, and are written
            // even for Tasks this replica has not materialized yet: a binding
            // to an unknown Task is a fact, and `Task.blocks` picks it up the
            // moment that Task's own op lands.
            replace_block_tasks(tx, b)?;
        }
        // Create and delete share an arm for the same reason Block's do: the
        // delete carries the attachment's full state, so it replaces the row.
        // Deliberately no `ensure` of the parent task: an attachment op that
        // overtook its task's create still lands, and the two join up when the
        // task turns up. Nothing about the row depends on the parent existing.
        InnerOp::AttachmentCreate(a) | InnerOp::AttachmentDelete(a) => {
            upsert_attachment_row(tx, a, lww)?;
        }
        // Handled by the append-only branch at the top of this function; the
        // arm exists so a new append-only op cannot be added without deciding
        // here.
        InnerOp::FocusStart(_)
        | InnerOp::FocusEnd(_)
        | InnerOp::FocusInterrupt(_)
        | InnerOp::ReviewSnapshotCreate(_) => {}
        // Unreachable: the guard at the top of this function returns before
        // the LWW read. Spelled out rather than caught by a `_ =>` arm so a
        // fourth control family cannot be added without being considered here.
        InnerOp::KeyEnvelope(_) | InnerOp::DeviceRevoke(_) | InnerOp::DeviceCertPublish(_) => {}
    }
    Ok(())
}
