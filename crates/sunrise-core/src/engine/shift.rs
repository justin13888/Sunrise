//! Context shifts (issue #1244, ADR-0073): the commands that write them, the
//! read of the row the merge projects, and the reads generation, the streak
//! and the "While you were away" summary take them through.
//!
//! # Written only by `Patch`
//!
//! `Shift` has no full-state op family (`ops: []` in the registry), so every
//! write is a `Patch` on the vault-meta stream naming only the fields it
//! changes, one register each, with `parents` set to the entity's heads as
//! this device has applied them (ADR-0065 §2), exactly as a place is written.
//! Ending a shift is a `Patch` of `ends_at` alone, so a rename made on
//! another device while it ends survives.
//!
//! # The gate
//!
//! The entity is the `shift.entity` feature. A write requires it first
//! ([`super::Engine::require_features`]), which refuses while a paired
//! device has not advertised it.
//!
//! # Nothing else is written
//!
//! Starting or ending a shift rewrites no routine, task or stream (ADR-0073
//! §4): generation and the streak read the set of shifts as an input, so
//! every replica holding the same shifts derives the same result.

use super::ids::{decode_unknowns, ms_to_ts, require_kind};
use super::write::Appended;
use super::{EngineError, META_STREAM};
use crate::commands::CommandResult;
use crate::feature::SHIFT_FEATURE;
use crate::inner_op::{InnerOp, PatchPayload};
use crate::queries::QueryResult;
use ciborium::value::Value;
use jiff::Timestamp;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::BTreeSet;
use sunrise_domain::{
    active_shift, away_summary, CborValue, Shift, ShiftDraft, ShiftPatch, ShiftPolicy, Unknowns,
};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::Db;

/// The columns [`read_row`] reads, in order.
const COLUMNS: &str = "id, name, starts_at_ms, ends_at_ms, zone, place, primary_streams, \
                       primary_contexts, deleted, created_at_ms, updated_at_ms, extra";

/// A focus set stored as canonical CBOR, NULL being empty. A blob that does
/// not decode reads as empty: the merge wrote it, so it cannot happen short
/// of corruption, and a shift then focuses nothing rather than failing every
/// read of the vault.
fn decode_set(blob: Option<Vec<u8>>) -> BTreeSet<EntityRef> {
    blob.and_then(|b| sunrise_cbor::decode_canonical(&b).ok())
        .unwrap_or_default()
}

fn read_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Shift> {
    let id: Vec<u8> = r.get(0)?;
    let place: Option<Vec<u8>> = r.get(5)?;
    Ok(Shift {
        id: EntityRef::new(EntityKind::Shift, super::ids::blob16(&id)),
        name: r.get(1)?,
        starts_at: ms_to_ts(r.get(2)?),
        ends_at: r.get::<_, Option<i64>>(3)?.map(ms_to_ts),
        zone: r.get(4)?,
        place: place.map(|b| EntityRef::new(EntityKind::Place, super::ids::blob16(&b))),
        primary_streams: decode_set(r.get(6)?),
        primary_contexts: decode_set(r.get(7)?),
        deleted: r.get::<_, i64>(8)? != 0,
        created_at: ms_to_ts(r.get(9)?),
        updated_at: ms_to_ts(r.get(10)?),
        unknown: decode_unknowns(r.get(11)?),
    })
}

/// The projected shift `id`, deleted or not.
pub(crate) fn read_shift(conn: &Connection, id: &[u8; 16]) -> Result<Option<Shift>, EngineError> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM shifts WHERE id = ?"),
            params![&id[..]],
            read_row,
        )
        .optional()?)
}

/// Every live shift, by start and then id.
pub(crate) fn read_shifts(conn: &Connection) -> Result<Vec<Shift>, EngineError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM shifts WHERE deleted = 0 ORDER BY starts_at_ms, id"
    ))?;
    let rows = stmt.query_map([], read_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Every live shift keyed by its id: the set generation, the streak and the
/// in-force rule take ([`active_shift`]). The id breaks a tie between two
/// shifts started at the same instant, so every replica picks the same one.
pub(crate) fn keyed_shifts(conn: &Connection) -> Result<Vec<(EntityRef, Shift)>, EngineError> {
    Ok(read_shifts(conn)?.into_iter().map(|s| (s.id, s)).collect())
}

/// One field op: `{"set": value}`.
fn set(value: &impl serde::Serialize) -> Result<CborValue, EngineError> {
    let v = Value::serialized(value).map_err(|e| EngineError::Cbor(e.to_string()))?;
    Ok(CborValue(Value::Map(vec![(Value::Text("set".into()), v)])))
}

impl super::Engine {
    /// Emit one `Patch` of `target` writing `fields`, naming its heads as
    /// parents, and fold it here.
    fn patch_shift(
        &self,
        db: &mut Db,
        target: EntityRef,
        create: bool,
        fields: Unknowns,
    ) -> Result<CommandResult, EngineError> {
        self.require_features(db, &[SHIFT_FEATURE], false)?;
        let now_ms = self.clock.now_ms();
        let done = db.with_tx(|tx| -> rusqlite::Result<Appended> {
            let parents = super::merge::parents_of(tx, target.bytes())?;
            let op = InnerOp::Patch(Box::new(PatchPayload {
                target,
                create,
                origin: None,
                parents: Some(parents),
                group: None,
                fields,
                unknown: Unknowns::new(),
            }));
            self.append_and_fold(tx, &op, &META_STREAM, now_ms)
        })?;
        Ok(CommandResult::new(target, None, done.op_id, done.seq))
    }

    /// [`Command::CreateShift`](crate::Command::CreateShift): a new shift,
    /// starting now unless the draft names a start.
    pub(super) fn create_shift(
        &self,
        db: &mut Db,
        d: &ShiftDraft,
    ) -> Result<CommandResult, EngineError> {
        let now_ms = self.clock.now_ms();
        let id = self.fresh_id(EntityKind::Shift, now_ms);
        let shift = d.clone().into_shift(id, ms_to_ts(now_ms.cast_signed()));
        shift.validate()?;
        let mut fields = Unknowns::new();
        fields.insert("name".into(), set(&shift.name)?);
        fields.insert("starts_at".into(), set(&shift.starts_at)?);
        if let Some(end) = shift.ends_at {
            fields.insert("ends_at".into(), set(&end)?);
        }
        if let Some(zone) = &shift.zone {
            fields.insert("zone".into(), set(zone)?);
        }
        if let Some(place) = shift.place {
            fields.insert("place".into(), set(&place)?);
        }
        fields.insert("primary_streams".into(), set(&shift.primary_streams)?);
        fields.insert("primary_contexts".into(), set(&shift.primary_contexts)?);
        fields.insert("deleted".into(), set(&false)?);
        self.patch_shift(db, id, true, fields)
    }

    /// The live shift `id`, or `NotFound`.
    fn live_shift(db: &Db, id: EntityRef) -> Result<Shift, EngineError> {
        require_kind(id, EntityKind::Shift)?;
        match read_shift(db.conn(), id.bytes())? {
            Some(s) if !s.deleted => Ok(s),
            _ => Err(EngineError::NotFound(format!("shift {id}"))),
        }
    }

    /// [`Command::UpdateShift`](crate::Command::UpdateShift): write the
    /// fields the patch names, and no other. The shift the edit leaves must
    /// validate, since an end is only valid against the start it lands on.
    pub(super) fn update_shift(
        &self,
        db: &mut Db,
        id: EntityRef,
        patch: &ShiftPatch,
    ) -> Result<CommandResult, EngineError> {
        let current = Self::live_shift(db, id)?;
        if patch.is_empty() {
            return Err(EngineError::Invalid("an edit that changes nothing".into()));
        }
        let next = patch.applied_to(&current);
        next.validate()?;
        let mut fields = Unknowns::new();
        if patch.name.is_some() {
            fields.insert("name".into(), set(&next.name)?);
        }
        if patch.starts_at.is_some() {
            fields.insert("starts_at".into(), set(&next.starts_at)?);
        }
        if patch.ends_at.is_some() {
            fields.insert("ends_at".into(), set(&next.ends_at)?);
        }
        if patch.zone.is_some() {
            fields.insert("zone".into(), set(&next.zone)?);
        }
        if patch.place.is_some() {
            fields.insert("place".into(), set(&next.place)?);
        }
        if patch.primary_streams.is_some() {
            fields.insert("primary_streams".into(), set(&next.primary_streams)?);
        }
        if patch.primary_contexts.is_some() {
            fields.insert("primary_contexts".into(), set(&next.primary_contexts)?);
        }
        self.patch_shift(db, id, false, fields)
    }

    /// [`Command::EndShift`](crate::Command::EndShift): a `Patch` of
    /// `ends_at`, now unless `at` names another moment. Refused with
    /// `Invalid` when that is not after the shift's start.
    pub(super) fn end_shift(
        &self,
        db: &mut Db,
        id: EntityRef,
        at: Option<Timestamp>,
    ) -> Result<CommandResult, EngineError> {
        let current = Self::live_shift(db, id)?;
        let end = at.unwrap_or_else(|| ms_to_ts(self.clock.now_ms().cast_signed()));
        let next = Shift {
            ends_at: Some(end),
            ..current
        };
        next.validate()?;
        let mut fields = Unknowns::new();
        fields.insert("ends_at".into(), set(&next.ends_at)?);
        self.patch_shift(db, id, false, fields)
    }

    /// [`Command::DeleteShift`](crate::Command::DeleteShift): a tombstone and
    /// nothing else; `Command::Restore` brings it back.
    pub(super) fn delete_shift(
        &self,
        db: &mut Db,
        id: EntityRef,
    ) -> Result<CommandResult, EngineError> {
        Self::live_shift(db, id)?;
        let mut fields = Unknowns::new();
        fields.insert("deleted".into(), set(&true)?);
        self.patch_shift(db, id, false, fields)
    }

    /// `Command::Restore` and `Command::ResolveConflict` of a shift: the one
    /// field, as a `Patch` descending from every head, which settles the
    /// field's conflict wherever it is applied (ADR-0065 §4).
    pub(super) fn settle_shift(
        &self,
        db: &mut Db,
        id: EntityRef,
        field: &str,
        value: Value,
        restore: bool,
    ) -> Result<CommandResult, EngineError> {
        let shift = read_shift(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("shift {id}")))?;
        if restore && !shift.deleted {
            return Err(EngineError::Invalid(format!("{id} is not deleted")));
        }
        let mut fields = Unknowns::new();
        fields.insert(
            field.to_owned(),
            CborValue(Value::Map(vec![(Value::Text("set".into()), value)])),
        );
        self.patch_shift(db, id, false, fields)
    }

    /// `Query::Shifts`.
    pub(super) fn query_shifts(db: &Db) -> Result<QueryResult, EngineError> {
        Ok(QueryResult::Shifts(read_shifts(db.conn())?))
    }

    /// `Query::ActiveShift`: the shift in force at the clock's now.
    pub(super) fn query_active_shift(&self, db: &Db) -> Result<QueryResult, EngineError> {
        let now = ms_to_ts(self.clock.now_ms().cast_signed());
        let shifts = keyed_shifts(db.conn())?;
        Ok(QueryResult::ActiveShift(
            active_shift(&shifts, now).map(|(_, s)| s.clone()),
        ))
    }

    /// `Query::AwaySummary`: what accrued during shift `id`, over its window
    /// to now, for a reader in this device's zone (ADR-0073 §6). A deleted
    /// shift is still summarised over its own window, which is what the
    /// user asked about.
    pub(super) fn query_away_summary(
        &self,
        db: &Db,
        id: EntityRef,
    ) -> Result<QueryResult, EngineError> {
        require_kind(id, EntityKind::Shift)?;
        let shift = read_shift(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("shift {id}")))?;
        let shift = Shift {
            deleted: false,
            ..shift
        };
        let now = ms_to_ts(self.clock.now_ms().cast_signed());
        let routines: Vec<_> = super::routine::read_routines(db.conn())?
            .into_iter()
            .filter(|r| !r.archived)
            .map(|r| {
                let policy: ShiftPolicy = r.shift_policy.clone();
                (r, policy)
            })
            .collect();
        let tasks = super::review::read_live_tasks(db.conn())?;
        let summary = away_summary(&shift, now, &routines, &tasks, &self.device_zone())
            .map_err(|e| EngineError::Invalid(e.to_string()))?;
        Ok(QueryResult::AwaySummary(summary))
    }
}
