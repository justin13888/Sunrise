//! Places (issue #1128, ADR-0051, `docs/02-domain/places.md`): the commands
//! that write them, the read of the row the merge projects, and the read of
//! every open task's place requirement.
//!
//! # Written only by `Patch`
//!
//! `Place` has no full-state op family (`ops: []` in the registry), so every
//! write is a `Patch` on the vault-meta stream naming only the fields it
//! changes, one register each, with `parents` set to the entity's heads as
//! this device has applied them (ADR-0065 §2), exactly as a saved view is
//! written.
//!
//! # The gate
//!
//! The entity is the `place.entity` feature. A write requires it first
//! ([`super::Engine::require_features`]), which refuses while a paired
//! device has not advertised it: a build without the entity cannot decode a
//! `Patch` naming `plc_`, so none is written where one would read it.
//!
//! # Presence is not here
//!
//! The engine holds no presence and writes none. Whether the user is at a
//! place is `Core`'s, in memory only (`crate::core::place`); what the engine
//! gives it is the vault's side: the live places and each open task's
//! requirement.

use super::ids::{decode_unknowns, ms_to_ts, require_kind};
use super::write::Appended;
use super::{EngineError, META_STREAM};
use crate::commands::CommandResult;
use crate::feature::PLACE_FEATURE;
use crate::inner_op::{InnerOp, PatchPayload};
use crate::queries::QueryResult;
use ciborium::value::Value;
use rusqlite::{params, Connection, OptionalExtension};
use sunrise_domain::{CborValue, Place, PlaceDraft, PlacePatch, Task, TaskPlaceReq, Unknowns};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::Db;

/// The columns [`read_row`] reads, in order.
const COLUMNS: &str = "id, name, lat_e7, lon_e7, radius_m, address_label, deleted, \
                       created_at_ms, updated_at_ms, extra";

fn read_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Place> {
    let id: Vec<u8> = r.get(0)?;
    Ok(Place {
        id: EntityRef::new(EntityKind::Place, super::ids::blob16(&id)),
        name: r.get(1)?,
        lat_e7: r.get(2)?,
        lon_e7: r.get(3)?,
        radius_m: r.get(4)?,
        address_label: r.get(5)?,
        deleted: r.get::<_, i64>(6)? != 0,
        created_at: ms_to_ts(r.get(7)?),
        updated_at: ms_to_ts(r.get(8)?),
        unknown: decode_unknowns(r.get(9)?),
    })
}

/// The projected place `id`, deleted or not.
pub(crate) fn read_place(conn: &Connection, id: &[u8; 16]) -> Result<Option<Place>, EngineError> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM places WHERE id = ?"),
            params![&id[..]],
            read_row,
        )
        .optional()?)
}

/// Every live place, by name and then id.
pub(crate) fn read_places(conn: &Connection) -> Result<Vec<Place>, EngineError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM places WHERE deleted = 0 ORDER BY name COLLATE NOCASE, id"
    ))?;
    let rows = stmt.query_map([], read_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Every open task ([`Task::is_open`]) whose scheduling constraints name a
/// place, with that requirement ([`TaskPlaceReq::of`]), by task id.
pub(crate) fn read_place_requirements(
    conn: &Connection,
) -> Result<Vec<(Task, TaskPlaceReq)>, EngineError> {
    let mut stmt = conn.prepare(
        "SELECT id FROM tasks WHERE deleted = 0 AND scheduling_constraints IS NOT NULL
         ORDER BY id ASC",
    )?;
    let ids = stmt
        .query_map([], |r| r.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::new();
    for raw in ids {
        let Some(task) = super::task::read_task(conn, &super::ids::blob16(&raw))? else {
            continue;
        };
        if !task.is_open() {
            continue;
        }
        if let Some(req) = TaskPlaceReq::of(&task.scheduling_constraints) {
            out.push((task, req));
        }
    }
    Ok(out)
}

/// One field op: `{"set": value}`.
fn set(value: &impl serde::Serialize) -> Result<CborValue, EngineError> {
    let v = Value::serialized(value).map_err(|e| EngineError::Cbor(e.to_string()))?;
    Ok(CborValue(Value::Map(vec![(Value::Text("set".into()), v)])))
}

impl super::Engine {
    /// Emit one `Patch` of `target` writing `fields`, naming its heads as
    /// parents, and fold it here.
    fn patch_place(
        &self,
        db: &mut Db,
        target: EntityRef,
        create: bool,
        fields: Unknowns,
    ) -> Result<CommandResult, EngineError> {
        self.require_features(db, &[PLACE_FEATURE], false)?;
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

    /// [`Command::CreatePlace`](crate::Command::CreatePlace): a new place.
    pub(super) fn create_place(
        &self,
        db: &mut Db,
        d: &PlaceDraft,
    ) -> Result<CommandResult, EngineError> {
        d.validate()?;
        let now_ms = self.clock.now_ms();
        let id = self.fresh_id(EntityKind::Place, now_ms);
        let mut fields = Unknowns::new();
        fields.insert("name".into(), set(&d.name.trim())?);
        fields.insert("lat_e7".into(), set(&d.lat_e7)?);
        fields.insert("lon_e7".into(), set(&d.lon_e7)?);
        fields.insert("radius_m".into(), set(&d.radius_m)?);
        if let Some(label) = &d.address_label {
            fields.insert("address_label".into(), set(label)?);
        }
        fields.insert("deleted".into(), set(&false)?);
        self.patch_place(db, id, true, fields)
    }

    /// The live place `id`, or `NotFound`.
    fn live_place(db: &Db, id: EntityRef) -> Result<Place, EngineError> {
        require_kind(id, EntityKind::Place)?;
        match read_place(db.conn(), id.bytes())? {
            Some(p) if !p.deleted => Ok(p),
            _ => Err(EngineError::NotFound(format!("place {id}"))),
        }
    }

    /// [`Command::UpdatePlace`](crate::Command::UpdatePlace): write the
    /// fields the patch names, and no other.
    pub(super) fn update_place(
        &self,
        db: &mut Db,
        id: EntityRef,
        patch: &PlacePatch,
    ) -> Result<CommandResult, EngineError> {
        patch.validate()?;
        Self::live_place(db, id)?;
        if patch.is_empty() {
            return Err(EngineError::Invalid("an edit that changes nothing".into()));
        }
        let mut fields = Unknowns::new();
        if let Some(n) = &patch.name {
            fields.insert("name".into(), set(&n.trim())?);
        }
        if let Some(v) = patch.lat_e7 {
            fields.insert("lat_e7".into(), set(&v)?);
        }
        if let Some(v) = patch.lon_e7 {
            fields.insert("lon_e7".into(), set(&v)?);
        }
        if let Some(v) = patch.radius_m {
            fields.insert("radius_m".into(), set(&v)?);
        }
        if let Some(label) = &patch.address_label {
            fields.insert("address_label".into(), set(label)?);
        }
        self.patch_place(db, id, false, fields)
    }

    /// [`Command::DeletePlace`](crate::Command::DeletePlace): a tombstone and
    /// nothing else. A task that names it keeps the ref, which evaluates
    /// `Unknown` (`docs/02-domain/places.md` §Deletion); `Command::Restore`
    /// brings it back.
    pub(super) fn delete_place(
        &self,
        db: &mut Db,
        id: EntityRef,
    ) -> Result<CommandResult, EngineError> {
        Self::live_place(db, id)?;
        let mut fields = Unknowns::new();
        fields.insert("deleted".into(), set(&true)?);
        self.patch_place(db, id, false, fields)
    }

    /// `Command::Restore` and `Command::ResolveConflict` of a place: the one
    /// field, as a `Patch` descending from every head, which settles the
    /// field's conflict wherever it is applied (ADR-0065 §4).
    pub(super) fn settle_place(
        &self,
        db: &mut Db,
        id: EntityRef,
        field: &str,
        value: Value,
        restore: bool,
    ) -> Result<CommandResult, EngineError> {
        let place = read_place(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("place {id}")))?;
        if restore && !place.deleted {
            return Err(EngineError::Invalid(format!("{id} is not deleted")));
        }
        let mut fields = Unknowns::new();
        fields.insert(
            field.to_owned(),
            CborValue(Value::Map(vec![(Value::Text("set".into()), value)])),
        );
        self.patch_place(db, id, false, fields)
    }

    /// `Query::Places`.
    pub(super) fn query_places(db: &Db) -> Result<QueryResult, EngineError> {
        Ok(QueryResult::Places(read_places(db.conn())?))
    }

    /// Every live place, for `Core`'s region selection.
    #[allow(
        clippy::unused_self,
        reason = "`Core` reads it through the engine it holds (`core/place.rs`); the receiver \
                  keeps that call shape"
    )]
    pub(crate) fn live_places(&self, db: &Db) -> Result<Vec<Place>, EngineError> {
        read_places(db.conn())
    }

    /// Every open task that names a place, with its requirement, for `Core`'s
    /// "here now" and "not actionable here".
    #[allow(
        clippy::unused_self,
        reason = "`Core` reads it through the engine it holds (`core/place.rs`); the receiver \
                  keeps that call shape"
    )]
    pub(crate) fn place_requirements(
        &self,
        db: &Db,
    ) -> Result<Vec<(Task, TaskPlaceReq)>, EngineError> {
        read_place_requirements(db.conn())
    }
}
