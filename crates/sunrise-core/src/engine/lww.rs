//! Remote materialization: the one dispatcher every received entity op goes
//! through, and the stamp it merges by.
//!
//! `materialize_remote` reads each entity's merge class from the entity
//! registry ([`sunrise_id::for_each_entity!`]). An entity that merges as
//! [`Merge::Lww`] is folded field by field by [`super::merge`] (ADR-0044,
//! which superseded ADR-0014's one survivor per row); an append-only record
//! is written once under its own key. Its per-op match is exhaustive, so a new
//! op variant is a build failure here until it is routed.

use super::focus::materialize_focus_remote;
use super::merge::merge_op;
use super::review::insert_review_snapshot_row;
use super::META_STREAM;
use crate::inner_op::InnerOp;
use rusqlite::{params, OptionalExtension, Transaction};
use sunrise_cbor::hlc::Hlc;
use sunrise_domain::INBOX_STREAM_BYTES;
use sunrise_id::registry::Merge;
use sunrise_id::{EntityKind, EntityRef};

/// The stamp an op writes with: the envelope's `(hlc, device, seq)`.
///
/// Every field write an op makes carries it, and the merge compares two
/// writes to one field by it ([`super::merge::Stamp`], which appends the op's
/// stream as a last term). Ordering is `(hlc, device, seq)`, in that order,
/// and every term earns its place:
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
///   counter resets to 0. In that window `seq` is what still orders them. It
///   must decide there, and the device memcmp must not: `dev > dev` is false,
///   so a device's own later op would lose to its own earlier one on every
///   remote replica while the originating replica kept it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LwwStamp {
    /// Causal timestamp from the writing device.
    pub hlc: Hlc,
    /// The writing device's 16-byte id.
    pub device: [u8; 16],
    /// The writing device's per-`(stream, device)` sequence number.
    pub seq: u64,
}

/// The stamp stored on a materialized row: the greatest stamp among the ops
/// its entity's state was merged from. `device` is `None` only for a
/// placeholder row that no real op has yet stamped (the lazily created
/// inbox stream).
#[derive(Debug, Clone)]
pub(super) struct RowLww {
    pub(super) hlc: Hlc,
    pub(super) device: Option<Vec<u8>>,
    pub(super) seq: u64,
}

/// Read the stored LWW stamp for `id` in `table`. `None` when the row is
/// absent.
pub(super) fn read_row_lww(
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

/// The register rule: does a write stamped `incoming` replace the one a
/// register holds, stamped `row`? Compares `(hlc, device, seq)` in that
/// order. Every field register and map key decides by it
/// ([`super::merge`]), where it once decided whole rows.
pub(super) fn lww_wins(incoming: &LwwStamp, row: &RowLww) -> bool {
    if incoming.hlc != row.hlc {
        return incoming.hlc > row.hlc;
    }
    let Some(row_dev) = row.device.as_deref() else {
        // A placeholder no op has stamped loses to any real op.
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
    // same device is the same op folded again, and `true` keeps that a
    // harmless rewrite of the same value.
    //
    // See `docs/05-sync/conflict-resolution.md` and ADR-0016.
    incoming.seq >= row.seq
}

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

/// Apply one decoded remote entity op to the materialized tables.
///
/// `lww` is the sender's stamp and `stream` the stream the envelope was sealed
/// under. An op on an entity that merges as [`Merge::Lww`], full-state or
/// `Patch`, is folded into that entity's field state and the entity is
/// re-projected ([`merge_op`]); a losing write is kept in the state and
/// changes nothing it shows. An append-only record is written once.
//
// The control arm below has an empty body and is spelled out rather than
// caught by `_ =>`: control ops are turned away by the guard at the top of the
// function, and naming each family is what makes adding a fifth a
// compile-time decision rather than a silent fall-through.
pub(super) fn materialize_remote(
    tx: &Transaction<'_>,
    inner: &InnerOp,
    lww: &LwwStamp,
    stream: &[u8; 16],
) -> rusqlite::Result<()> {
    // A control op has no entity and must never reach the registry lookup
    // below: its `entity_kind` names what it is *about* — a KeyEnvelope says
    // Stream — so it would be read as a write to that kind's row.
    // `apply_remote` routes them away before this is called; this is the belt
    // to that braces, and it is a `debug_assert` rather than a silent return
    // so a routing mistake surfaces in a test run instead of as a mysterious
    // row in production.
    if inner.is_control() {
        debug_assert!(false, "a control op reached the entity materializer");
        return Ok(());
    }
    // The entity's registry entry decides how the op merges. There is no
    // default: an entity the registry does not project cannot reach here,
    // because its kind has no op variant.
    let spec = inner.entity_kind().spec();
    if spec.storage().is_none() || !matches!(spec.merge, Merge::Lww | Merge::AppendOnly) {
        debug_assert!(false, "{:?} has no materialized row", spec.kind);
        return Ok(());
    }
    match inner {
        // Every entity write that merges field by field: the full-state ops,
        // read as writes to every field they carry (ADR-0044 §7), and `Patch`.
        //
        // What used to be special here is now the merge's: a delete is a
        // `deleted` register like any other, so a delete that overtakes its
        // create still lands as a tombstone; a context delete still purges the
        // context from every task, keyed on its id alone; dependency edges and
        // block bindings are OR-sets, written even when the entity they name
        // has not arrived yet; and an attachment's parent task need not exist.
        InnerOp::TaskCreate(_)
        | InnerOp::TaskUpdate(_)
        | InnerOp::TaskDelete(_)
        | InnerOp::StreamCreate(_)
        | InnerOp::StreamUpdate(_)
        | InnerOp::StreamDelete(_)
        | InnerOp::ContextCreate(_)
        | InnerOp::ContextUpdate(_)
        | InnerOp::ContextDelete(_)
        | InnerOp::RoutineCreate(_)
        | InnerOp::RoutineUpdate(_)
        | InnerOp::RoutineDelete(_)
        | InnerOp::BlockCreate(_)
        | InnerOp::BlockUpdate(_)
        | InnerOp::BlockDelete(_)
        | InnerOp::AttachmentCreate(_)
        | InnerOp::AttachmentDelete(_)
        | InnerOp::Patch(_) => {
            return merge_op(tx, inner, lww, *stream);
        }
        // Focus ops never enter the LWW contest. Each writes one immutable record
        // keyed by the session's own id (start, end, and interruption in three
        // distinct tables), so there is nothing for a later op to overwrite and
        // nothing for an earlier one to lose — including when an `end` overtakes
        // its own `start`. Running an LWW comparison here would be actively wrong:
        // a `start` stamped later than its `end` would suppress the `end`.
        InnerOp::FocusStart(_) | InnerOp::FocusEnd(_) | InnerOp::FocusInterrupt(_) => {
            return materialize_focus_remote(tx, inner, lww);
        }
        // A review snapshot is append-only for the same reason: it is keyed by its
        // own `rvw_` id, written once, and never edited. Running LWW here would let
        // one device's review of a week suppress another device's, which is exactly
        // the loss the representation exists to prevent. It also deliberately does
        // NOT `ensure_stream_row` for the Streams it names — the counts live inside
        // an opaque blob, so a snapshot that overtakes a `stream.create` still
        // lands intact.
        InnerOp::ReviewSnapshotCreate(snapshot) => {
            return insert_review_snapshot_row(tx, snapshot, lww);
        }
        // Unreachable: the guard at the top of this function returns before
        // the LWW read. Spelled out rather than caught by a `_ =>` arm so a
        // sixth control family cannot be added without being considered here.
        InnerOp::KeyEnvelope(_)
        | InnerOp::DeviceRevoke(_)
        | InnerOp::DeviceCertPublish(_)
        | InnerOp::IdentityTransition(_)
        | InnerOp::StreamDigest(_) => {}
    }
    Ok(())
}
