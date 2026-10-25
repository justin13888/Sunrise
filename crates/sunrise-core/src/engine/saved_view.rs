//! Saved views (issue #341, `docs/08-features/planning-views.md` §Saved
//! views): the commands that write them, the read of the row the merge
//! projects, and the evaluation that turns a spec into the tasks it matches.
//!
//! # Written only by `Patch`
//!
//! `SavedView` has no full-state op family (`ops: []` in the registry), so
//! every write is a `Patch` on the vault-meta stream that names only the
//! fields it changes, one register each, with `parents` set to the entity's
//! heads as this device has applied them (ADR-0065 §2). An edit of the name
//! on one device and of the filter on another are two writes to two
//! registers, and both survive; two concurrent writes of one field are a
//! conflict the registers keep (§3, §4). Like Preferences, it is outside
//! ADR-0065 D9's full-state rule: no build has a full-state op that could
//! overwrite it whole.
//!
//! # The gate
//!
//! The entity is the `saved_view.entity` feature. A write requires it first
//! ([`super::Engine::require_features`]), which refuses while a paired
//! device has not advertised it: a build without the entity cannot decode a
//! `Patch` naming `svw_`, so none is written where one would read it.
//!
//! # The result is never stored
//!
//! `Query::SavedView` evaluates the spec on every read, in the reader's
//! zone, over the vault as it is now ([`super::Engine::evaluate_saved_view`]).

use super::ids::{decode_unknowns, ms_to_ts, require_kind};
use super::write::Appended;
use super::{EngineError, META_STREAM};
use crate::commands::CommandResult;
use crate::inner_op::{InnerOp, PatchPayload};
use crate::queries::{QueryResult, SavedViewResult};
use ciborium::value::Value;
use rusqlite::{params, Connection, OptionalExtension};
use sunrise_domain::saved_view::{sort_tasks, FilterReader};
use sunrise_domain::{
    sort_order, CborValue, SavedView, SavedViewDraft, SavedViewPatch, Task, Unknowns, ViewKind,
    ViewSort,
};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::Db;

/// The feature id the entity is gated on.
pub(crate) const SAVED_VIEW_FEATURE: &str = "saved_view.entity";

/// The columns [`read_row`] reads, in order.
const COLUMNS: &str = "id, name, view, filter, sort, sort_order, deleted, \
                       created_at_ms, updated_at_ms, extra";

fn read_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SavedView> {
    let id: Vec<u8> = r.get(0)?;
    let filter: Vec<u8> = r.get(3)?;
    let sort: String = r.get(4)?;
    Ok(SavedView {
        id: EntityRef::new(EntityKind::SavedView, super::ids::blob16(&id)),
        name: r.get(1)?,
        view: ViewKind::from_raw(&r.get::<_, String>(2)?),
        // A filter that does not decode reads as no filter, never as an
        // error: the row is the merge's, and the register still holds it.
        filter: sunrise_cbor::decode_lenient(&filter).unwrap_or_default(),
        sort: ViewSort::from_raw(&sort),
        sort_order: r.get(5)?,
        deleted: r.get::<_, i64>(6)? != 0,
        created_at: ms_to_ts(r.get(7)?),
        updated_at: ms_to_ts(r.get(8)?),
        unknown: decode_unknowns(r.get(9)?),
    })
}

/// The projected view `id`, deleted or not.
pub(crate) fn read_saved_view(
    conn: &Connection,
    id: &[u8; 16],
) -> Result<Option<SavedView>, EngineError> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM saved_views WHERE id = ?"),
            params![&id[..]],
            read_row,
        )
        .optional()?)
}

/// Every live view, in the saved-views list's order: placed ones by key and
/// then id, then the rest by name and id.
pub(crate) fn read_saved_views(conn: &Connection) -> Result<Vec<SavedView>, EngineError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM saved_views WHERE deleted = 0
         ORDER BY sort_order IS NULL, sort_order, name COLLATE NOCASE, id"
    ))?;
    let rows = stmt.query_map([], read_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// One field op: `{"set": value}`.
fn set(value: &impl serde::Serialize) -> Result<CborValue, EngineError> {
    let v = Value::serialized(value).map_err(|e| EngineError::Cbor(e.to_string()))?;
    Ok(CborValue(Value::Map(vec![(Value::Text("set".into()), v)])))
}

impl super::Engine {
    /// Emit one `Patch` of `target` writing `fields`, naming its heads as
    /// parents, and fold it here.
    fn patch_saved_view(
        &self,
        db: &mut Db,
        target: EntityRef,
        create: bool,
        fields: Unknowns,
    ) -> Result<CommandResult, EngineError> {
        self.require_features(db, &[SAVED_VIEW_FEATURE], false)?;
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

    /// [`Command::CreateSavedView`](crate::Command::CreateSavedView): a new
    /// view, last in the list.
    pub(super) fn create_saved_view(
        &self,
        db: &mut Db,
        d: SavedViewDraft,
    ) -> Result<CommandResult, EngineError> {
        d.validate()?;
        let now_ms = self.clock.now_ms();
        let id = self.fresh_id(EntityKind::SavedView, now_ms);
        let last: Option<String> = db.conn().query_row(
            "SELECT MAX(sort_order) FROM saved_views WHERE deleted = 0
               AND sort_order GLOB '[A-Z]*' AND sort_order NOT GLOB '*[^A-Z]*'",
            [],
            |r| r.get(0),
        )?;
        let mut fields = Unknowns::new();
        fields.insert("name".into(), set(&d.name.trim())?);
        fields.insert("view".into(), set(&d.view)?);
        fields.insert("filter".into(), set(&d.filter)?);
        fields.insert("sort".into(), set(&d.sort)?);
        fields.insert(
            "sort_order".into(),
            set(&sort_order::append_after(last.as_deref()))?,
        );
        fields.insert("deleted".into(), set(&false)?);
        self.patch_saved_view(db, id, true, fields)
    }

    /// The live view `id`, or `NotFound`.
    fn live_saved_view(&self, db: &Db, id: EntityRef) -> Result<SavedView, EngineError> {
        require_kind(id, EntityKind::SavedView)?;
        match read_saved_view(db.conn(), id.bytes())? {
            Some(v) if !v.deleted => Ok(v),
            _ => Err(EngineError::NotFound(format!("saved view {id}"))),
        }
    }

    /// [`Command::UpdateSavedView`](crate::Command::UpdateSavedView): write
    /// the fields the patch names, and no other.
    pub(super) fn update_saved_view(
        &self,
        db: &mut Db,
        id: EntityRef,
        patch: SavedViewPatch,
    ) -> Result<CommandResult, EngineError> {
        patch.validate()?;
        self.live_saved_view(db, id)?;
        if patch.is_empty() {
            return Err(EngineError::Invalid("an edit that changes nothing".into()));
        }
        let mut fields = Unknowns::new();
        if let Some(n) = &patch.name {
            fields.insert("name".into(), set(&n.trim())?);
        }
        if let Some(v) = &patch.view {
            fields.insert("view".into(), set(v)?);
        }
        if let Some(f) = &patch.filter {
            fields.insert("filter".into(), set(f)?);
        }
        if let Some(s) = &patch.sort {
            fields.insert("sort".into(), set(s)?);
        }
        if let Some(k) = &patch.sort_order {
            fields.insert("sort_order".into(), set(k)?);
        }
        self.patch_saved_view(db, id, false, fields)
    }

    /// [`Command::DeleteSavedView`](crate::Command::DeleteSavedView): a
    /// tombstone; `Command::Restore` brings it back.
    pub(super) fn delete_saved_view(
        &self,
        db: &mut Db,
        id: EntityRef,
    ) -> Result<CommandResult, EngineError> {
        self.live_saved_view(db, id)?;
        let mut fields = Unknowns::new();
        fields.insert("deleted".into(), set(&true)?);
        self.patch_saved_view(db, id, false, fields)
    }

    /// `Command::Restore` and `Command::ResolveConflict` of a saved view:
    /// the one field, as a `Patch` descending from every head, which settles
    /// the field's conflict wherever it is applied (ADR-0065 §4).
    pub(super) fn settle_saved_view(
        &self,
        db: &mut Db,
        id: EntityRef,
        field: &str,
        value: Value,
        restore: bool,
    ) -> Result<CommandResult, EngineError> {
        let view = read_saved_view(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("saved view {id}")))?;
        if restore && !view.deleted {
            return Err(EngineError::Invalid(format!("{id} is not deleted")));
        }
        let mut fields = Unknowns::new();
        fields.insert(
            field.to_owned(),
            CborValue(Value::Map(vec![(Value::Text("set".into()), value)])),
        );
        self.patch_saved_view(db, id, false, fields)
    }

    /// `Query::SavedViews`.
    pub(super) fn query_saved_views(&self, db: &Db) -> Result<QueryResult, EngineError> {
        Ok(QueryResult::SavedViews(read_saved_views(db.conn())?))
    }

    /// `Query::SavedView`: evaluate the view `id` now, in the reader's zone.
    pub(super) fn evaluate_saved_view(
        &self,
        db: &Db,
        id: EntityRef,
    ) -> Result<QueryResult, EngineError> {
        let view = self.live_saved_view(db, id)?;
        let tz = self.device_zone();
        let now_ms = self.clock.now_ms();
        let schedule = self.day_schedule(db)?;
        let (today, _, _) = self.planner_day_at(&schedule, now_ms)?;
        let reader = FilterReader {
            tz: tz.clone(),
            today,
        };
        let filter = &view.filter;
        let tasks_of = |r: QueryResult| match r {
            QueryResult::Tasks(t) | QueryResult::StreamTasks(t) => t,
            _ => Vec::new(),
        };
        let (base, text_applied): (Vec<Task>, bool) = match &view.view {
            ViewKind::Today => (tasks_of(self.query_today(db, now_ms, &[])?), false),
            ViewKind::Inbox => (tasks_of(self.query_inbox(db)?), false),
            ViewKind::Stream if !filter.streams.is_empty() || filter.no_stream => {
                let mut all = Vec::new();
                for s in &filter.streams {
                    all.extend(tasks_of(self.query_stream_tasks(db, s)?));
                }
                // The tasks of no stream, once: the legacy Inbox reference in
                // `streams` already named them.
                if filter.no_stream && !filter.streams.iter().any(sunrise_domain::names_no_stream) {
                    let none = sunrise_domain::inbox_stream_ref();
                    all.extend(tasks_of(self.query_stream_tasks(db, &none)?));
                }
                (all, false)
            }
            ViewKind::Stream => (super::review::read_live_tasks(db.conn())?, false),
            ViewKind::Search if filter.text.trim().is_empty() => (Vec::new(), true),
            ViewKind::Search => (tasks_of(self.query_search(db, &filter.text, 500)?), true),
            _ => (Vec::new(), false),
        };
        let mut tasks: Vec<Task> = base
            .into_iter()
            .filter(|t| !t.deleted && filter.matches(t, &reader, !text_applied))
            .collect();
        sort_tasks(&mut tasks, &view.sort, &tz);
        let mut missing_contexts = Vec::new();
        for c in &filter.contexts {
            let live: Option<i64> = db
                .conn()
                .query_row(
                    "SELECT deleted FROM contexts WHERE id = ?",
                    params![&c.bytes()[..]],
                    |r| r.get(0),
                )
                .optional()?;
            if live != Some(0) {
                missing_contexts.push(*c);
            }
        }
        Ok(QueryResult::SavedView(Box::new(SavedViewResult {
            complete: filter.is_complete() && !view.view.is_unknown(),
            unresolved_names: filter.context_names.clone(),
            missing_contexts,
            view,
            tasks,
        })))
    }
}
