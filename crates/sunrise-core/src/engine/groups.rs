//! Groups: a commit of several entity ops, applied whole or not at all
//! (ADR-0065 §6, clarifications D8 and D13).
//!
//! # What makes an op a member
//!
//! A `Patch` names its group in its own `group` key, and in a vault that
//! requires `core.field_ops` every member is one. A full-state op (every op
//! this build writes into a vault that does not, D9) has no key of its own
//! to carry one, so a member carries the top-level mark
//! [`FULL_STATE_GROUP_KEY`] beside its entity's fields:
//! `{"id": bstr .size 16, "size": uint, "op": op-ref}`. `op` is the member's
//! own op-ref. A build from before this record reads the mark as an unknown
//! field of the entity, keeps it, and writes it back in its next full-state
//! op of that entity; the copy names the op it was copied from, not the op
//! carrying it, so it marks nothing. The fold never reads the mark as a
//! field, on any op.
//!
//! # Receiving one
//!
//! A member is stored in `ops` and counts toward the sync cursor like any
//! op that arrived, and its row in `pending_groups` holds it out of the fold:
//! it is pending, not parked, since it needs no upgrade, only the rest of its
//! group ([`Engine::fold_received`]). Once at least `size` members of
//! `(id, size)` are present, every held member is folded, in stamp order, in
//! the transaction that delivered the last one, and marked applied; a member
//! that arrives after that is folded at once (D8). The result is one function
//! of the set of ops present, whatever order they came in, and a projection
//! rebuild, which refolds in stamp order through this module, lands on it.
//!
//! A local group is folded in the transaction that appends it
//! ([`Engine::append_and_fold_group`]), its rows written as applied.
//!
//! `pending_groups.op_id` is the op's deterministic id
//! ([`remote_op_id`]), which is the `ops` row's id for every op this replica
//! received, so a member is one row however this replica learned of it: from
//! its op, from a snapshot that carried its version, or from both.
//!
//! [`FULL_STATE_GROUP_KEY`]: crate::inner_op::FULL_STATE_GROUP_KEY

use super::ids::blob16;
use super::lww::{materialize_member, materialize_remote, remap_legacy_inbox, LwwStamp};
use super::merge::OpRef;
use super::oplog::remote_op_id;
use super::{Engine, EngineError};
use crate::events::DomainEvent;
use crate::inner_op::{decode_inner_op, GroupWire, InnerOp, OpEffect, FULL_STATE_GROUP_KEY};
use ciborium::value::Value;
use rusqlite::{params, Transaction};
use std::collections::BTreeMap;
use sunrise_cbor::hlc::Hlc;
use sunrise_crypto::decode_envelope;
use sunrise_crypto::op_envelope::open_envelope;
use sunrise_domain::{CborValue, Unknowns};
use sunrise_id::EntityRef;

/// One group: the commit's id and how many members it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Group {
    pub(super) id: [u8; 16],
    pub(super) size: u64,
}

impl Group {
    /// `(id, size)`, as a version records it.
    pub(super) const fn pair(self) -> ([u8; 16], u64) {
        (self.id, self.size)
    }
}

/// An entity a fold wrote, and how: what a caller reports as an event.
pub(super) type Folded = (EntityRef, OpEffect);

/// The event a client is told for one folded entity op.
pub(super) const fn event_of(target: EntityRef, effect: OpEffect) -> DomainEvent {
    match effect {
        OpEffect::Create => DomainEvent::Created(target),
        OpEffect::Delete => DomainEvent::Deleted(target),
        OpEffect::Update | OpEffect::Control => DomainEvent::Updated(target),
    }
}

/// The group the op at `at` is a member of, or `None` for an op that is not
/// one. A group smaller than two, or a full-state mark that is malformed or
/// names another op, marks nothing.
pub(super) fn group_of(inner: &InnerOp, at: OpRef) -> Option<Group> {
    match inner {
        InnerOp::Patch(p) => p.group.as_ref().filter(|g| g.size >= 2).map(|g| Group {
            id: g.id,
            size: g.size,
        }),
        other => read_mark(
            other
                .full_state_unknowns()?
                .get(FULL_STATE_GROUP_KEY)?
                .get(),
            at,
        ),
    }
}

/// A full-state op's group mark, when it names `at`.
fn read_mark(v: &Value, at: OpRef) -> Option<Group> {
    let entries = v.as_map()?;
    let get = |name: &str| {
        entries
            .iter()
            .find(|(k, _)| k.as_text() == Some(name))
            .map(|(_, v)| v)
    };
    let id = get("id")?.as_bytes()?;
    let size = u64::try_from(get("size")?.as_integer()?).ok()?;
    let op = OpRef::from_value(get("op")?)?;
    (id.len() == 16 && size >= 2 && op == at).then(|| Group {
        id: blob16(id),
        size,
    })
}

/// Mark `inner`, about to be sealed at `at`, as a member of `group`.
pub(super) fn mark(inner: &mut InnerOp, group: Group, at: OpRef) {
    match inner {
        InnerOp::Patch(p) => {
            p.group = Some(GroupWire {
                id: group.id,
                size: group.size,
                unknown: Unknowns::new(),
            });
        }
        other => {
            if let Some(unknown) = other.full_state_unknowns_mut() {
                // Deterministic order: `id`, `op`, `size`.
                let mark = Value::Map(vec![
                    (Value::Text("id".into()), Value::Bytes(group.id.to_vec())),
                    (Value::Text("op".into()), at.to_value()),
                    (
                        Value::Text("size".into()),
                        Value::Integer(group.size.into()),
                    ),
                ]);
                unknown.insert(FULL_STATE_GROUP_KEY.to_owned(), CborValue(mark));
            }
        }
    }
}

/// Record that the member at `at`, stamped `hlc`, of `group` is present;
/// `applied` when it has been folded.
fn record(
    tx: &Transaction<'_>,
    group: Group,
    at: OpRef,
    hlc: Hlc,
    applied: bool,
    received_at_ms: u64,
) -> rusqlite::Result<()> {
    tx.prepare(
        "INSERT OR IGNORE INTO pending_groups
         (group_id, group_size, op_id, hlc_ms, hlc_logical, device, seq, stream, applied,
          received_at_ms)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )?
    .execute(params![
        &group.id[..],
        group.size,
        &remote_op_id(&at.stream, &at.device, at.seq)[..],
        hlc.physical_ms,
        hlc.logical,
        &at.device[..],
        at.seq,
        &at.stream[..],
        i64::from(applied),
        received_at_ms,
    ])?;
    Ok(())
}

/// Record a member whose version a joined `doc_state` carried: it was
/// folded where the state was dumped. When was it received is the `ops`
/// row's, if this replica logged the op (a projection rebuild's base), and
/// the op's own clock reading otherwise.
pub(super) fn record_joined(
    tx: &Transaction<'_>,
    group: ([u8; 16], u64),
    at: OpRef,
    hlc: Hlc,
) -> rusqlite::Result<()> {
    use rusqlite::OptionalExtension;
    let (id, size) = group;
    let logged: Option<i64> = tx
        .query_row(
            "SELECT received_at FROM ops WHERE stream_id = ? AND device_id = ? AND seq = ?",
            params![&at.stream[..], &at.device[..], at.seq],
            |r| r.get(0),
        )
        .optional()?;
    let received_at_ms = logged.map_or(hlc.physical_ms, |ms| u64::try_from(ms).unwrap_or(0));
    record(tx, Group { id, size }, at, hlc, true, received_at_ms)
}

/// One held member, as `pending_groups` names it.
struct Held {
    at: OpRef,
}

impl Engine {
    /// Fold one received or replayed entity op: at once, or, for a member of
    /// a group not yet complete, not until it is. See the [module
    /// docs](self).
    ///
    /// Returns every entity the call folded, which is none for a member held
    /// back, and the whole group for the member that completes it.
    pub(super) fn fold_received(
        &self,
        tx: &Transaction<'_>,
        inner: &InnerOp,
        lww: &LwwStamp,
        stream: &[u8; 16],
        received_at_ms: u64,
    ) -> Result<Vec<Folded>, EngineError> {
        let at = OpRef {
            stream: *stream,
            device: lww.device,
            seq: lww.seq,
        };
        let Some(group) = group_of(inner, at) else {
            materialize_remote(tx, inner, lww, stream)?;
            return Ok(vec![(inner.target_ref(), inner.effect())]);
        };
        record(tx, group, at, lww.hlc, false, received_at_ms)?;
        self.try_complete(tx, group)
    }

    /// Fold every held member of `group` once at least its `size` members are
    /// present, in stamp order, and mark them applied.
    fn try_complete(&self, tx: &Transaction<'_>, group: Group) -> Result<Vec<Folded>, EngineError> {
        let present: i64 = tx.query_row(
            "SELECT COUNT(*) FROM pending_groups WHERE group_id = ? AND group_size = ?",
            params![&group.id[..], group.size],
            |r| r.get(0),
        )?;
        if u64::try_from(present).unwrap_or(0) < group.size {
            return Ok(Vec::new());
        }
        let held: Vec<Held> = {
            let mut stmt = tx.prepare(
                "SELECT stream, device, seq FROM pending_groups
                 WHERE group_id = ? AND group_size = ? AND applied = 0
                 ORDER BY hlc_ms, hlc_logical, device, seq, stream",
            )?;
            let rows = stmt.query_map(params![&group.id[..], group.size], |r| {
                Ok(Held {
                    at: OpRef {
                        stream: blob16(&r.get::<_, Vec<u8>>(0)?),
                        device: blob16(&r.get::<_, Vec<u8>>(1)?),
                        seq: u64::try_from(r.get::<_, i64>(2)?).unwrap_or(0),
                    },
                })
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        if held.is_empty() {
            return Ok(Vec::new());
        }
        let signers = self.device_signing_keys(tx)?;
        let mut folded = Vec::with_capacity(held.len());
        for member in held {
            let (inner, lww) = self.open_logged(tx, &signers, member.at)?;
            materialize_member(tx, &inner, &lww, &member.at.stream, Some(group.pair()))?;
            tx.execute(
                "UPDATE pending_groups SET applied = 1
                 WHERE group_id = ? AND group_size = ? AND op_id = ?",
                params![
                    &group.id[..],
                    group.size,
                    &remote_op_id(&member.at.stream, &member.at.device, member.at.seq)[..]
                ],
            )?;
            folded.push((inner.target_ref(), inner.effect()));
        }
        Ok(folded)
    }

    /// Fold every group whose members are all present and some of them
    /// still held: after a snapshot join, which can bring the members a
    /// compacted log no longer carries.
    pub(super) fn complete_ready_groups(
        &self,
        tx: &Transaction<'_>,
    ) -> Result<Vec<Folded>, EngineError> {
        let ready: Vec<Group> = {
            let mut stmt = tx.prepare(
                "SELECT group_id, group_size FROM pending_groups
                 GROUP BY group_id, group_size
                 HAVING COUNT(*) >= group_size AND MIN(applied) = 0
                 ORDER BY group_id, group_size",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(Group {
                    id: blob16(&r.get::<_, Vec<u8>>(0)?),
                    size: u64::try_from(r.get::<_, i64>(1)?).unwrap_or(0),
                })
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut folded = Vec::new();
        for group in ready {
            folded.extend(self.try_complete(tx, group)?);
        }
        Ok(folded)
    }

    /// The op this replica logged at `at`, opened and decoded, with the stamp
    /// its envelope carries.
    fn open_logged(
        &self,
        tx: &Transaction<'_>,
        signers: &BTreeMap<[u8; 16], [u8; 32]>,
        at: OpRef,
    ) -> Result<(InnerOp, LwwStamp), EngineError> {
        let damaged = |why: &str| {
            EngineError::Invalid(format!("a held group member at seq {} {why}", at.seq))
        };
        let bytes: Vec<u8> = tx.query_row(
            "SELECT envelope FROM ops WHERE stream_id = ? AND device_id = ? AND seq = ?",
            params![&at.stream[..], &at.device[..], at.seq],
            |r| r.get(0),
        )?;
        let env = decode_envelope(&bytes).map_err(|_| damaged("does not decode"))?;
        let signer = signers
            .get(&env.device_id)
            .ok_or_else(|| damaged("has no known signer"))?;
        let keys = self.keychain.stream_keys_at(&env.stream_id, env.epoch);
        let inner_cbor = keys
            .iter()
            .map(Some)
            .chain([None])
            .find_map(|k| open_envelope(&env, signer, k).ok())
            .ok_or_else(|| damaged("does not open"))?;
        let mut inner = decode_inner_op(&inner_cbor).map_err(|_| damaged("does not decode"))?;
        remap_legacy_inbox(&mut inner);
        Ok((
            inner,
            LwwStamp {
                hlc: env.hlc,
                device: env.device_id,
                seq: env.seq,
            },
        ))
    }
}

/// Record a local member, folded in the transaction that appends it.
pub(super) fn record_local(
    tx: &Transaction<'_>,
    group: Group,
    at: OpRef,
    hlc: Hlc,
    now_ms: u64,
) -> rusqlite::Result<()> {
    record(tx, group, at, hlc, true, now_ms)
}
