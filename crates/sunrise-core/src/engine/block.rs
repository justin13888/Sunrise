//! Time blocks (`docs/02-domain/time-blocks.md`).
//!
//! Block ops are sealed under the Block's **key domain**, like task ops and
//! unlike routine ops: its own Stream's, or the private domain's for a block
//! with no stream (ADR-0046 §2). A calendar is per-Stream in the UI, so
//! routing there keeps a Block's `seq` independent of the meta stream's.
//!
//! `Task.blocks` is never written. It is derived from the `block_tasks` join
//! every time a Task is read, which is what makes the spec's "Bound Task's
//! `blocks` field updates symmetrically" hold by construction: one writer, one
//! op, and nothing for a concurrent edit of the Task to overwrite.
//!
//! The civil-time helpers live here because a Block is the only entity whose
//! query bounds are a *day* and a *week* rather than a timestamp; `notify` is
//! their one other caller.

use super::ids::{blob16, decode_unknowns, domain_bytes, home_of, ms_to_ts, require_kind};
use super::routine::decode_blob_opt;
use super::task::read_task;
use super::write::Appended;
use super::{Engine, EngineError};
use crate::commands::CommandResult;
use crate::events::DomainEvent;
use crate::inner_op::InnerOp;
use crate::queries::{BlockRow, QueryResult};
use rusqlite::{params, OptionalExtension};
use std::collections::BTreeSet;
use sunrise_domain::time::{SunriseTime, PREFILTER_SLACK_MS};
use sunrise_domain::Unknowns;
use sunrise_domain::{
    expand_block, imported_block_id, split_block_id, Block, BlockDraft, BlockException, BlockKind,
    BlockPatch, BlockSeries, BlockSource, EditScope, NoteBody, SplitError, ValidationError,
};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::Db;

impl Engine {
    pub(super) fn create_block(
        &self,
        db: &mut Db,
        d: BlockDraft,
    ) -> Result<CommandResult, EngineError> {
        let now_ms = self.clock.now_ms();
        let block = self.new_block(db, d, now_ms)?;
        let done = self.emit_block(db, &block, now_ms, "block.create")?;
        Ok(CommandResult::new(block.id, None, done.op_id, done.seq))
    }

    /// The Block a create of `d` writes, validated and given a fresh id, and
    /// not yet logged.
    fn new_block(&self, db: &Db, d: BlockDraft, now_ms: u64) -> Result<Block, EngineError> {
        let tz = self.device_zone();
        d.validate(&tz)?;
        let home = home_of(d.stream_id)?;
        for t in &d.tasks {
            require_kind(*t, EntityKind::Task)?;
        }
        let block_id = self.fresh_id(EntityKind::Block, now_ms);
        let tasks: BTreeSet<EntityRef> = d.tasks.iter().copied().collect();
        // Shadow copy: a Block created around exactly one Task and given no
        // title of its own takes that Task's title AS IT IS NOW. Later renames
        // of the Task do not reach the Block unless `title_track_task` is set.
        let title = match d.title {
            Some(t) => Some(t.trim().to_string()),
            None => self.shadow_title(db, &tasks)?,
        };
        let block = Block {
            id: block_id,
            created_at: ms_to_ts(now_ms as i64),
            updated_at: ms_to_ts(now_ms as i64),
            stream_id: home,
            starts_at: d.starts_at,
            ends_at: d.ends_at,
            title,
            title_track_task: d.title_track_task,
            tasks,
            deleted: false,
            // A user-created block leaves `kind` and `source` absent, which
            // reads as flexible (time-blocks.md §Migration default).
            kind: None,
            rrule: None,
            exceptions: std::collections::BTreeMap::new(),
            split_from: None,
            location: None,
            notes: None,
            source: None,
            external_id: None,
            unknown: Unknowns::new(),
        };
        block.validate_invariants(&tz)?;
        Ok(block)
    }

    /// [`Command::MergeBlocks`](crate::commands::Command::MergeBlocks): the
    /// union's create and both tombstones, appended as one group (ADR-0065
    /// §6), each in its block's own stream.
    pub(super) fn merge_blocks(
        &self,
        db: &mut Db,
        a: EntityRef,
        b: EntityRef,
    ) -> Result<CommandResult, EngineError> {
        require_kind(a, EntityKind::Block)?;
        require_kind(b, EntityKind::Block)?;
        if a == b {
            return Err(EngineError::Invalid(format!(
                "block {a} cannot be merged with itself"
            )));
        }
        let now_ms = self.clock.now_ms();
        let live = |id: EntityRef| -> Result<Block, EngineError> {
            let block = read_block(db.conn(), id.bytes())?
                .ok_or_else(|| EngineError::NotFound(format!("block {id}")))?;
            if block.deleted {
                return Err(EngineError::Invalid(format!("block {id} is deleted")));
            }
            Ok(block)
        };
        let (first, second) = (live(a)?, live(b)?);
        let draft = sunrise_domain::merge_blocks(&first, &second, &self.device_zone())?;
        let union = self.new_block(db, draft, now_ms)?;
        let id = union.id;
        let mut members = vec![(
            InnerOp::BlockCreate(Box::new(union.clone())),
            domain_bytes(union.stream_id),
        )];
        for mut original in [first, second] {
            original.deleted = true;
            original.updated_at = ms_to_ts(now_ms as i64);
            let stream = domain_bytes(original.stream_id);
            members.push((InnerOp::BlockDelete(Box::new(original)), stream));
        }
        let done = db.with_tx(|tx| self.append_and_fold_group(tx, members, now_ms))?;
        let created = done
            .first()
            .ok_or_else(|| EngineError::Invalid("a merge wrote nothing".into()))?;
        Ok(CommandResult::new(id, None, created.op_id, created.seq))
    }

    /// Write the Block one external calendar item maps onto, at the id
    /// [`imported_block_id`] derives from `(source, uid)`.
    ///
    /// Create and update are the same code path because both are a full-state
    /// Block op folded the same way: re-importing an unchanged file projects
    /// identical column values, which is what makes the whole operation
    /// idempotent without a "have I seen this UID" table to keep in step.
    ///
    /// Two fields are read back off the existing Block rather than taken from
    /// the draft, because they are not the importing calendar's to state — see
    /// [`Command::ImportBlock`](crate::commands::Command::ImportBlock).
    pub(super) fn import_block(
        &self,
        db: &mut Db,
        source: &str,
        uid: &str,
        d: BlockDraft,
    ) -> Result<CommandResult, EngineError> {
        if uid.trim().is_empty() {
            return Err(EngineError::Invalid(
                "import: an external item needs a UID".into(),
            ));
        }
        let tz = self.device_zone();
        d.validate(&tz)?;
        let home = home_of(d.stream_id)?;
        for t in &d.tasks {
            require_kind(*t, EntityKind::Task)?;
        }
        let now_ms = self.clock.now_ms();
        let block_id = imported_block_id(source, uid);
        let existing = read_block(db.conn(), block_id.bytes())?;

        // Union, not replace: a task the user bound to an imported Block is
        // theirs, and a re-import must not quietly unbind it.
        let mut tasks: BTreeSet<EntityRef> = d.tasks.iter().copied().collect();
        if let Some(e) = &existing {
            tasks.extend(e.tasks.iter().copied());
        }
        let title = match d.title {
            Some(t) => Some(t.trim().to_string()),
            None => self.shadow_title(db, &tasks)?,
        };
        let block = Block {
            id: block_id,
            // The Block was created when it was first imported; a re-import is
            // not a second creation of it.
            created_at: existing
                .as_ref()
                .map_or_else(|| ms_to_ts(now_ms as i64), |e| e.created_at),
            updated_at: ms_to_ts(now_ms as i64),
            stream_id: home,
            starts_at: d.starts_at,
            ends_at: d.ends_at,
            title,
            title_track_task: d.title_track_task,
            tasks,
            // Re-importing an event the user deleted locally brings it back.
            // The alternative — honouring the tombstone — would make the
            // import silently incomplete, and the file is the statement of
            // what the external calendar holds.
            deleted: false,
            // The draft states none of the #1147 fields, so a re-import keeps
            // the ones the Block holds, as a build before them kept them
            // among its unknown fields. Mapping an event onto them is the
            // importer's (#1149).
            kind: existing.as_ref().and_then(|e| e.kind.clone()),
            rrule: existing.as_ref().and_then(|e| e.rrule.clone()),
            exceptions: existing
                .as_ref()
                .map(|e| e.exceptions.clone())
                .unwrap_or_default(),
            split_from: existing.as_ref().and_then(|e| e.split_from),
            location: existing.as_ref().and_then(|e| e.location.clone()),
            notes: existing.as_ref().and_then(|e| e.notes.clone()),
            source: existing.as_ref().and_then(|e| e.source.clone()),
            external_id: existing.as_ref().and_then(|e| e.external_id.clone()),
            // Forward-compat fields a newer build wrote are the Block's, not
            // the importer's, so they survive the rewrite.
            unknown: existing
                .as_ref()
                .map_or_else(Unknowns::new, |e| e.unknown.clone()),
        };
        block.validate_invariants(&tz)?;
        // The op kind is what the activity feed reads, so a first import is a
        // create and a re-import is an update. Both apply identically on a
        // remote replica — every Block op is full-state.
        let inner_kind = if existing.is_some() {
            "block.update"
        } else {
            "block.create"
        };
        let done = self.emit_block(db, &block, now_ms, inner_kind)?;
        Ok(CommandResult::new(block_id, None, done.op_id, done.seq))
    }

    /// [`Command::UpdateBlock`](crate::commands::Command::UpdateBlock), as
    /// far as `scope` reaches.
    pub(super) fn update_block(
        &self,
        db: &mut Db,
        id: EntityRef,
        scope: EditScope,
        patch: BlockPatch,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Block)?;
        patch.validate()?;
        match scope {
            EditScope::All => self.edit_block(db, id, patch),
            EditScope::This { key } => self.edit_block_occurrence(db, id, &key, patch),
            EditScope::ThisAndFuture { key } => self.split_block(db, id, &key, Some(patch)),
        }
    }

    /// An edit of the whole block ([`EditScope::All`]): for a recurring
    /// block, its series, whose anchor `starts_at` and `ends_at` are.
    fn edit_block(
        &self,
        db: &mut Db,
        id: EntityRef,
        patch: BlockPatch,
    ) -> Result<CommandResult, EngineError> {
        let now_ms = self.clock.now_ms();
        let mut block = read_block(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("block {id}")))?;
        apply_block_patch(&mut block, patch)?;
        block.updated_at = ms_to_ts(now_ms as i64);
        block.validate_invariants(&self.device_zone())?;
        let done = self.emit_block(db, &block, now_ms, "block.update")?;
        Ok(CommandResult::new(id, None, done.op_id, done.seq))
    }

    /// An edit of one occurrence ([`EditScope::This`]): written into that
    /// occurrence's exception, over the fields it already overrides. A
    /// cancelled occurrence is not there to edit.
    fn edit_block_occurrence(
        &self,
        db: &mut Db,
        id: EntityRef,
        key: &str,
        patch: BlockPatch,
    ) -> Result<CommandResult, EngineError> {
        if !patch.is_occurrence_edit() {
            return Err(EngineError::Invalid(
                "an edit of one occurrence sets only its start, its end and its title".into(),
            ));
        }
        let now_ms = self.clock.now_ms();
        let (mut block, mut series) = recurring_block(db.conn(), id)?;
        let mut exception = series.exceptions.get(key).cloned().unwrap_or_default();
        if exception.cancelled {
            return Err(EngineError::NotFound(format!(
                "occurrence {key} of block {id} is cancelled"
            )));
        }
        patch.apply_to_exception(&mut exception);
        series
            .except(key, exception.clone())
            .map_err(|e| occurrence_error(id, e))?;
        let (rule_start, rule_end) = series
            .occurrence_bounds(key)
            .ok_or_else(|| occurrence_error(id, SplitError::NotAnOccurrence(key.into())))?;
        let tz = self.device_zone();
        let starts_at = exception.starts_at.as_ref().unwrap_or(&rule_start);
        let ends_at = exception.ends_at.as_ref().unwrap_or(&rule_end);
        if matches!(
            (starts_at.resolve_in(&tz), ends_at.resolve_in(&tz)),
            (Some(s), Some(e)) if e <= s
        ) {
            return Err(EngineError::Validation(ValidationError::Field {
                field: "block.ends_at",
                constraint: "after_starts_at",
            }));
        }
        block.exceptions = series.exceptions;
        block.updated_at = ms_to_ts(now_ms as i64);
        block.validate_invariants(&tz)?;
        let done = self.emit_block(db, &block, now_ms, "block.update")?;
        Ok(CommandResult::new(id, None, done.op_id, done.seq))
    }

    /// Split a recurring block's series at occurrence `key`
    /// ([`EditScope::ThisAndFuture`]): the block ends before `key`, and with
    /// an edit, the block at [`split_block_id`]`(series root, key)` continues
    /// the series from it with the edit applied. Both are one group
    /// (ADR-0065 §6), so no replica shows the series ended with no
    /// continuation. Two devices splitting at the same key derive the same
    /// continuation, and its fields merge.
    ///
    /// With no edit (a delete) the series only ends. At the series' first
    /// occurrence nothing is left before `key`: an edit is then an edit of
    /// the whole block, and a delete tombstones it.
    fn split_block(
        &self,
        db: &mut Db,
        id: EntityRef,
        key: &str,
        patch: Option<BlockPatch>,
    ) -> Result<CommandResult, EngineError> {
        let now_ms = self.clock.now_ms();
        let now = ms_to_ts(now_ms as i64);
        let (block, series) = recurring_block(db.conn(), id)?;
        let root = block.split_from.unwrap_or(block.id);
        let continues = split_block_id(&root, key);
        // Splitting again where the series was already split edits the
        // block that continues it.
        if let Some(patch) = &patch {
            if read_block(db.conn(), continues.bytes())?.is_some_and(|b| !b.deleted) {
                return self.edit_block(db, continues, patch.clone());
            }
        }
        let split = series.split_at(key).map_err(|e| occurrence_error(id, e))?;
        let Some(ended) = split.ended else {
            return match patch {
                Some(patch) => self.edit_block(db, id, patch),
                None => Ok(self
                    .delete_block(db, id, EditScope::All)?
                    .with_events(vec![DomainEvent::Deleted(id)])),
            };
        };
        let tz = self.device_zone();
        let mut before = block.clone();
        before.rrule = Some(ended.rrule);
        before.exceptions = ended.exceptions;
        before.updated_at = now;
        before.validate_invariants(&tz)?;
        let Some(patch) = patch else {
            let done = self.emit_block(db, &before, now_ms, "block.update")?;
            return Ok(CommandResult::new(id, None, done.op_id, done.seq));
        };

        let existing = read_block(db.conn(), continues.bytes())?;
        // The continuation shows the title the series shows now, fixed,
        // because it takes none of the bindings a title could track.
        let shown = block_row(db.conn(), block.clone())?.title;
        let mut next = Block {
            id: continues,
            created_at: existing.as_ref().map_or(now, |e| e.created_at),
            updated_at: now,
            starts_at: split.continuation.starts_at,
            ends_at: split.continuation.ends_at,
            title: shown,
            title_track_task: false,
            // Bindings stay on the block they were made on: `tasks` merges
            // as an OR-set, so copying them would bind each task to a second
            // block no one bound it to. The import fields stay too: the
            // continuation is this device's block, not the external item a
            // re-import writes to (time-blocks.md §Recurring blocks).
            tasks: BTreeSet::new(),
            source: None,
            external_id: None,
            rrule: Some(split.continuation.rrule),
            exceptions: split.continuation.exceptions,
            split_from: Some(root),
            deleted: false,
            ..block
        };
        apply_block_patch(&mut next, patch)?;
        next.validate_invariants(&tz)?;
        let create = if existing.is_some() {
            InnerOp::BlockUpdate(Box::new(next.clone()))
        } else {
            InnerOp::BlockCreate(Box::new(next.clone()))
        };
        let members = vec![
            (create, domain_bytes(next.stream_id)),
            (
                InnerOp::BlockUpdate(Box::new(before.clone())),
                domain_bytes(before.stream_id),
            ),
        ];
        let done = db.with_tx(|tx| self.append_and_fold_group(tx, members, now_ms))?;
        let created = done
            .first()
            .ok_or_else(|| EngineError::Invalid("a split wrote nothing".into()))?;
        // A continuation tombstoned before is written back, not made.
        let continued = if existing.is_some() {
            DomainEvent::Updated(continues)
        } else {
            DomainEvent::Created(continues)
        };
        Ok(
            CommandResult::new(continues, None, created.op_id, created.seq)
                .with_events(vec![continued, DomainEvent::Updated(id)]),
        )
    }

    /// [`Command::DeleteBlock`](crate::commands::Command::DeleteBlock), as
    /// far as `scope` reaches.
    ///
    /// The whole block is tombstoned by an op carrying its **whole state**
    /// with `deleted` set, down [`Self::emit_block`] — the same path an
    /// update takes — so a delete that wins LWW replaces the row rather than
    /// flipping one column on top of whatever the receiving replica held.
    pub(super) fn delete_block(
        &self,
        db: &mut Db,
        id: EntityRef,
        scope: EditScope,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Block)?;
        let now_ms = self.clock.now_ms();
        match scope {
            EditScope::All => {
                let mut block = read_block(db.conn(), id.bytes())?
                    .ok_or_else(|| EngineError::NotFound(format!("block {id}")))?;
                block.deleted = true;
                block.updated_at = ms_to_ts(now_ms as i64);
                let done = self.emit_block(db, &block, now_ms, "block.delete")?;
                Ok(CommandResult::new(id, None, done.op_id, done.seq))
            }
            EditScope::This { key } => {
                let (mut block, mut series) = recurring_block(db.conn(), id)?;
                series
                    .except(&key, BlockException::cancelled())
                    .map_err(|e| occurrence_error(id, e))?;
                block.exceptions = series.exceptions;
                block.updated_at = ms_to_ts(now_ms as i64);
                let done = self.emit_block(db, &block, now_ms, "block.update")?;
                Ok(CommandResult::new(id, None, done.op_id, done.seq))
            }
            EditScope::ThisAndFuture { key } => self.split_block(db, id, &key, None),
        }
    }

    /// Bind a Task to a Block. Idempotent on the membership set: re-binding an
    /// already-bound Task writes the same set, so every replica converges to
    /// the same membership whichever order the ops land in.
    pub(super) fn bind_task(
        &self,
        db: &mut Db,
        block_id: EntityRef,
        task: EntityRef,
    ) -> Result<CommandResult, EngineError> {
        require_kind(block_id, EntityKind::Block)?;
        require_kind(task, EntityKind::Task)?;
        let now_ms = self.clock.now_ms();
        let mut block = read_block(db.conn(), block_id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("block {block_id}")))?;
        block.tasks.insert(task);
        // The first Task bound to an untitled Block lends it the shadow copy,
        // which is what makes drag-a-task-onto-the-grid produce a labelled
        // block instead of a blank one.
        if block.title.is_none() {
            block.title = self.shadow_title(db, &block.tasks)?;
        }
        block.updated_at = ms_to_ts(now_ms as i64);
        let done = self.emit_block(db, &block, now_ms, "block.update")?;
        Ok(CommandResult::new(block_id, None, done.op_id, done.seq))
    }

    /// Unbind a Task from a Block. The Block survives with no tasks bound: an
    /// empty Block is a legitimate calendar entry ("gym", "lunch").
    pub(super) fn unbind_task(
        &self,
        db: &mut Db,
        block_id: EntityRef,
        task: EntityRef,
    ) -> Result<CommandResult, EngineError> {
        require_kind(block_id, EntityKind::Block)?;
        require_kind(task, EntityKind::Task)?;
        let now_ms = self.clock.now_ms();
        let mut block = read_block(db.conn(), block_id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("block {block_id}")))?;
        block.tasks.remove(&task);
        block.updated_at = ms_to_ts(now_ms as i64);
        let done = self.emit_block(db, &block, now_ms, "block.update")?;
        Ok(CommandResult::new(block_id, None, done.op_id, done.seq))
    }

    /// Append a full-state Block op and fold it, which projects the row and
    /// its bindings, in one transaction.
    fn emit_block(
        &self,
        db: &mut Db,
        block: &Block,
        now_ms: u64,
        inner_kind: &str,
    ) -> Result<Appended, EngineError> {
        let inner = match inner_kind {
            "block.create" => InnerOp::BlockCreate(Box::new(block.clone())),
            "block.delete" => InnerOp::BlockDelete(Box::new(block.clone())),
            _ => InnerOp::BlockUpdate(Box::new(block.clone())),
        };
        let stream = domain_bytes(block.stream_id);
        Ok(db.with_tx(|tx| self.append_and_fold(tx, &inner, &stream, now_ms))?)
    }

    /// The title a single bound Task lends a Block. `None` when the Block binds
    /// zero or two-plus Tasks (nothing unambiguous to shadow) or when the one
    /// Task has not been materialized on this replica yet.
    fn shadow_title(
        &self,
        db: &Db,
        tasks: &BTreeSet<EntityRef>,
    ) -> Result<Option<String>, EngineError> {
        let mut it = tasks.iter();
        let (Some(only), None) = (it.next(), it.next()) else {
            return Ok(None);
        };
        Ok(read_task(db.conn(), only.bytes())?.map(|t| t.title))
    }

    /// Blocks overlapping the civil day containing `day_ms`, in the device
    /// zone.
    pub(super) fn query_day_blocks(
        &self,
        db: &Db,
        day_ms: u64,
    ) -> Result<QueryResult, EngineError> {
        let (from, to) = self.civil_span(day_ms as i64, 1)?;
        self.query_blocks_between(db, from, to)
    }

    /// Blocks overlapping the seven civil days of the week containing
    /// `week_ms`, in the device zone, the week beginning on the `week_start`
    /// preference (`docs/10-cross-cutting/time.md` §4).
    pub(super) fn query_week_blocks(
        &self,
        db: &Db,
        week_ms: u64,
    ) -> Result<QueryResult, EngineError> {
        let tz = self.device_zone();
        let anchor = ms_to_ts(week_ms as i64).to_zoned(tz).date();
        let first = sunrise_domain::first_day_of_week(anchor, &self.week_start(db)?);
        let first_ms = self.civil_day_start_ms(first)?;
        let (from, to) = self.civil_span(first_ms, 7)?;
        self.query_blocks_between(db, from, to)
    }

    /// `[start_of_day(at_ms), start_of_day(at_ms) + days)` in the device zone,
    /// as epoch-millisecond bounds.
    ///
    /// Computed over civil dates rather than by adding `86_400_000` so a day
    /// that is 23 or 25 hours long across a DST transition is still exactly one
    /// day on the grid.
    ///
    /// Only the calendar grid reads it (`DayBlocks`, `WeekBlocks`): a grid
    /// column is a calendar date, midnight to midnight, and a block that runs
    /// past midnight is drawn on both columns. Every planner day (Today,
    /// Upcoming, the briefs) reads `planner_day_at` instead
    /// (`docs/02-domain/day-schedule.md` §Planner day).
    pub(super) fn civil_span(&self, at_ms: i64, days: i64) -> Result<(i64, i64), EngineError> {
        let tz = self.device_zone();
        let date = ms_to_ts(at_ms).to_zoned(tz).date();
        let end_date = date
            .checked_add(jiff::Span::new().days(days))
            .map_err(|e| EngineError::Invalid(format!("calendar span: {e}")))?;
        Ok((
            self.civil_day_start_ms(date)?,
            self.civil_day_start_ms(end_date)?,
        ))
    }

    fn civil_day_start_ms(&self, date: jiff::civil::Date) -> Result<i64, EngineError> {
        let tz = self.device_zone();
        let zoned = tz
            .to_zoned(date.to_datetime(jiff::civil::Time::midnight()))
            .map_err(|e| EngineError::Invalid(format!("calendar day start: {e}")))?;
        Ok(zoned.timestamp().as_millisecond())
    }

    /// Every live Block whose `[starts_at, ends_at)` overlaps `[from, to)`.
    ///
    /// Overlap, not containment: a two-hour block that started before the
    /// window still belongs on the grid, which is why the `blocks_by_end`
    /// index exists.
    ///
    /// The SQL reads the storage index widened by [`PREFILTER_SLACK_MS`] on
    /// each side, and the overlap is then decided on each bound resolved in
    /// the device zone (`docs/10-cross-cutting/time.md` §2 rule 2): the index
    /// puts a floating or all-day bound up to 14 hours from where this reader
    /// sees it, so on its own it drops evening blocks west of UTC and invents
    /// morning ones east of it.
    ///
    /// A recurring block is expanded over the window ([`expand_block`],
    /// `docs/02-domain/time-blocks.md` §Recurring blocks): one row per
    /// occurrence that overlaps it, carrying its key, and nothing per
    /// occurrence is stored. Its anchor says nothing about where its
    /// occurrences fall, so every live recurring block is read. A series this
    /// build cannot expand over the window yields no rows, as a time of a
    /// kind it does not know places nothing (ADR-0045 §6).
    ///
    /// Rows come back by resolved start, then id, then occurrence key.
    pub(super) fn query_blocks_between(
        &self,
        db: &Db,
        from: i64,
        to: i64,
    ) -> Result<QueryResult, EngineError> {
        let tz = self.device_zone();
        let mut stmt = db.conn().prepare(
            "SELECT id FROM blocks
             WHERE deleted = 0 AND starts_at_ms < ? AND ends_at_ms > ?
             UNION
             SELECT id FROM blocks WHERE deleted = 0 AND rrule_cbor IS NOT NULL",
        )?;
        let ids = stmt
            .query_map(
                params![
                    to.saturating_add(PREFILTER_SLACK_MS),
                    from.saturating_sub(PREFILTER_SLACK_MS)
                ],
                |r| r.get::<_, Vec<u8>>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let window = (ms_to_ts(from), ms_to_ts(to));
        let mut blocks: Vec<(Block, Option<String>)> = Vec::with_capacity(ids.len());
        for raw in ids {
            let Some(block) = read_block(db.conn(), &blob16(&raw))? else {
                continue;
            };
            if let Some(series) = block.series() {
                for occ in expand_block(&series, window, &tz).unwrap_or_default() {
                    blocks.push((block.at_occurrence(&occ), Some(occ.key)));
                }
                continue;
            }
            let resolved = (
                block.starts_at.resolve_in(&tz),
                block.ends_at.resolve_in(&tz),
            );
            if let (Some(s), Some(e)) = resolved {
                if s.as_millisecond() < to && e.as_millisecond() > from {
                    blocks.push((block, None));
                }
            }
        }
        blocks.sort_by(|(a, ka), (b, kb)| {
            a.starts_at
                .cmp_in(&b.starts_at, &tz)
                .then_with(|| a.id.cmp(&b.id))
                .then_with(|| ka.cmp(kb))
        });
        let mut rows = Vec::with_capacity(blocks.len());
        for (block, key) in blocks {
            let mut row = block_row(db.conn(), block)?;
            row.occurrence_key = key;
            rows.push(row);
        }
        Ok(QueryResult::Blocks(rows))
    }
}

/// Write an edit of the whole block onto `block`: the times, the title, title
/// tracking and the stream.
fn apply_block_patch(block: &mut Block, patch: BlockPatch) -> Result<(), EngineError> {
    if let Some(stream) = patch.stream_id {
        block.stream_id = home_of(stream)?;
    }
    if let Some(s) = patch.starts_at {
        block.starts_at = s;
    }
    if let Some(e) = patch.ends_at {
        block.ends_at = e;
    }
    if let Some(t) = patch.title {
        block.title = t.map(|s| s.trim().to_string());
    }
    if let Some(track) = patch.title_track_task {
        block.title_track_task = track;
    }
    Ok(())
}

/// The live recurring Block `id` and its series, for an edit that names one
/// of its occurrences.
fn recurring_block(
    conn: &rusqlite::Connection,
    id: EntityRef,
) -> Result<(Block, BlockSeries), EngineError> {
    let block = read_block(conn, id.bytes())?
        .ok_or_else(|| EngineError::NotFound(format!("block {id}")))?;
    if block.deleted {
        return Err(EngineError::Invalid(format!("block {id} is deleted")));
    }
    let series = block
        .series()
        .ok_or_else(|| EngineError::Invalid(format!("block {id} does not recur")))?;
    Ok((block, series))
}

/// A key the series cannot edit, as the engine reports it: a key that is
/// not one is invalid, and an occurrence the rule does not produce is not
/// found.
fn occurrence_error(id: EntityRef, e: SplitError) -> EngineError {
    match e {
        SplitError::NotAnOccurrence(key) => {
            EngineError::NotFound(format!("occurrence {key} of block {id}"))
        }
        other => EngineError::Invalid(format!("block {id}: {other}")),
    }
}

// ---- table operations ----

/// Assemble one [`BlockRow`]: the Block, the titles of the bound Tasks this
/// replica knows about, and the title the spec's shadow-copy rules resolve to.
pub(super) fn block_row(
    conn: &rusqlite::Connection,
    block: Block,
) -> Result<BlockRow, EngineError> {
    let mut task_titles = Vec::with_capacity(block.tasks.len());
    for t in &block.tasks {
        if let Some(task) = read_task(conn, t.bytes())? {
            if !task.deleted {
                task_titles.push(task.title);
            }
        }
    }
    let title = block.resolve_title(&task_titles).map(ToOwned::to_owned);
    Ok(BlockRow {
        block,
        title,
        task_titles,
        occurrence_key: None,
    })
}

/// Read one Block, with its bindings.
pub(super) fn read_block(
    conn: &rusqlite::Connection,
    id: &[u8; 16],
) -> Result<Option<Block>, EngineError> {
    let row = conn
        .query_row(
            "SELECT stream_id, starts_at_ms, starts_at_kind, starts_at_tz,
                    ends_at_ms, ends_at_kind, ends_at_tz, title, title_track_task,
                    deleted, extra, created_at_ms, updated_at_ms,
                    starts_at_civil, ends_at_civil,
                    kind, rrule_cbor, exceptions, split_from, location, notes,
                    source, external_id
             FROM blocks WHERE id = ?",
            params![&id[..]],
            |r| {
                let series = BlockSeriesColumns {
                    kind: r.get(15)?,
                    rrule: r.get(16)?,
                    exceptions: r.get(17)?,
                    split_from: r.get(18)?,
                    location: r.get(19)?,
                    notes: r.get(20)?,
                    source: r.get(21)?,
                    external_id: r.get(22)?,
                };
                Ok((
                    r.get::<_, Option<Vec<u8>>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, i64>(9)?,
                    r.get::<_, Option<Vec<u8>>>(10)?,
                    r.get::<_, i64>(11)?,
                    r.get::<_, i64>(12)?,
                    r.get::<_, Option<String>>(13)?,
                    r.get::<_, Option<String>>(14)?,
                    series,
                ))
            },
        )
        .optional()?;
    let Some(b) = row else {
        return Ok(None);
    };
    let s = b.15;
    Ok(Some(Block {
        id: EntityRef::new(EntityKind::Block, *id),
        created_at: ms_to_ts(b.11.max(0)),
        updated_at: ms_to_ts(b.12.max(0)),
        stream_id: b.0.map(|s| EntityRef::new(EntityKind::Stream, blob16(&s))),
        starts_at: SunriseTime::from_parts(b.1, &b.2, b.3.as_deref(), b.13.as_deref()),
        ends_at: SunriseTime::from_parts(b.4, &b.5, b.6.as_deref(), b.14.as_deref()),
        title: b.7,
        title_track_task: b.8 != 0,
        tasks: read_block_tasks(conn, id)?,
        deleted: b.9 != 0,
        kind: s.kind.as_deref().map(BlockKind::from_raw),
        rrule: s
            .rrule
            .map(|bytes| sunrise_cbor::decode_canonical(&bytes))
            .transpose()
            .map_err(|e| EngineError::Cbor(e.to_string()))?,
        exceptions: decode_blob_opt(s.exceptions)?,
        split_from: s
            .split_from
            .map(|raw| EntityRef::new(EntityKind::Block, blob16(&raw))),
        location: s.location,
        notes: s.notes.map(NoteBody),
        source: s.source.as_deref().map(BlockSource::from_raw),
        external_id: s.external_id,
        unknown: decode_unknowns(b.10),
    }))
}

/// The `blocks` columns of the #1147 fields (migration 0049), as stored.
struct BlockSeriesColumns {
    kind: Option<String>,
    rrule: Option<Vec<u8>>,
    exceptions: Option<Vec<u8>>,
    split_from: Option<Vec<u8>>,
    location: Option<String>,
    notes: Option<Vec<u8>>,
    source: Option<String>,
    external_id: Option<String>,
}

/// The Tasks bound to one Block.
fn read_block_tasks(
    conn: &rusqlite::Connection,
    block: &[u8; 16],
) -> Result<BTreeSet<EntityRef>, EngineError> {
    let mut stmt = conn.prepare("SELECT task_id FROM block_tasks WHERE block_id = ?")?;
    let out = stmt
        .query_map(params![&block[..]], |r| r.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|raw| EntityRef::new(EntityKind::Task, blob16(&raw)))
        .collect();
    Ok(out)
}

/// The live Blocks scheduling one Task — the derived other half of the
/// binding, and the only source `Task.blocks` is ever read from.
///
/// Deleted Blocks are filtered here rather than at bind time: a Block
/// tombstoned on another device stops appearing on its Tasks the moment the
/// tombstone merges, with no repair pass over `block_tasks`.
pub(super) fn read_task_blocks(
    conn: &rusqlite::Connection,
    task: &[u8; 16],
) -> Result<BTreeSet<EntityRef>, EngineError> {
    let mut stmt = conn.prepare(
        "SELECT bt.block_id FROM block_tasks bt
         JOIN blocks b ON b.id = bt.block_id
         WHERE bt.task_id = ? AND b.deleted = 0",
    )?;
    let out = stmt
        .query_map(params![&task[..]], |r| r.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|raw| EntityRef::new(EntityKind::Block, blob16(&raw)))
        .collect();
    Ok(out)
}
