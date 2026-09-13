//! Time blocks (`docs/02-domain/time-blocks.md`).
//!
//! Block ops route under the Block's **own Stream**, like task ops and unlike
//! routine ops: a calendar is per-Stream in the UI, so routing there keeps a
//! Block's `seq` independent of the meta stream's.
//!
//! `Task.blocks` is never written. It is derived from the `block_tasks` join
//! every time a Task is read, which is what makes the spec's "Bound Task's
//! `blocks` field updates symmetrically" hold by construction: one writer, one
//! op, and nothing for a concurrent edit of the Task to overwrite.
//!
//! The civil-time helpers live here because a Block is the only entity whose
//! query bounds are a *day* and a *week* rather than a timestamp; `notify` is
//! their one other caller.

use super::ids::{blob16, decode_unknowns, encode_unknowns, ms_to_ts, require_kind};
use super::lww::LwwStamp;
use super::stream::ensure_stream_row;
use super::task::read_task;
use super::{Engine, EngineError};
use crate::commands::CommandResult;
use crate::inner_op::{encode_inner_op, InnerOp};
use crate::queries::{BlockRow, QueryResult};
use rusqlite::{params, OptionalExtension, Transaction};
use std::collections::BTreeSet;
use sunrise_domain::time::SunriseTime;
use sunrise_domain::Unknowns;
use sunrise_domain::{imported_block_id, Block, BlockDraft, BlockPatch};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::Db;

impl Engine {
    pub(super) fn create_block(
        &self,
        db: &mut Db,
        d: BlockDraft,
    ) -> Result<CommandResult, EngineError> {
        d.validate()?;
        require_kind(d.stream_id, EntityKind::Stream)?;
        for t in &d.tasks {
            require_kind(*t, EntityKind::Task)?;
        }
        let now_ms = self.clock.now_ms();
        let block_id = self.fresh_id(EntityKind::Block, now_ms);
        let op_id = self.fresh_op_id(now_ms);
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
            stream_id: d.stream_id,
            starts_at: d.starts_at,
            ends_at: d.ends_at,
            title,
            title_track_task: d.title_track_task,
            tasks,
            deleted: false,
            unknown: Unknowns::new(),
        };
        block.validate_invariants()?;
        let seq = self.emit_block(db, &block, now_ms, &op_id, "block.create")?;
        Ok(CommandResult::new(block_id, None, op_id, seq))
    }

    /// Write the Block one external calendar item maps onto, at the id
    /// [`imported_block_id`] derives from `(source, uid)`.
    ///
    /// Create and update are the same code path because the row write already
    /// is one (`upsert_block_row`): re-importing an unchanged file rewrites
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
        d.validate()?;
        require_kind(d.stream_id, EntityKind::Stream)?;
        for t in &d.tasks {
            require_kind(*t, EntityKind::Task)?;
        }
        let now_ms = self.clock.now_ms();
        let block_id = imported_block_id(source, uid);
        let existing = read_block(db.conn(), block_id.bytes())?;
        let op_id = self.fresh_op_id(now_ms);

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
            stream_id: d.stream_id,
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
            // Forward-compat fields a newer build wrote are the Block's, not
            // the importer's, so they survive the rewrite.
            unknown: existing
                .as_ref()
                .map_or_else(Unknowns::new, |e| e.unknown.clone()),
        };
        block.validate_invariants()?;
        // The op kind is what the activity feed reads, so a first import is a
        // create and a re-import is an update. Both apply identically on a
        // remote replica — every Block op is full-state.
        let inner_kind = if existing.is_some() {
            "block.update"
        } else {
            "block.create"
        };
        let seq = self.emit_block(db, &block, now_ms, &op_id, inner_kind)?;
        Ok(CommandResult::new(block_id, None, op_id, seq))
    }

    pub(super) fn update_block(
        &self,
        db: &mut Db,
        id: EntityRef,
        patch: BlockPatch,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Block)?;
        patch.validate()?;
        let now_ms = self.clock.now_ms();
        let mut block = read_block(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("block {id}")))?;
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
        if let Some(s) = patch.stream_id {
            require_kind(s, EntityKind::Stream)?;
            block.stream_id = s;
        }
        block.updated_at = ms_to_ts(now_ms as i64);
        block.validate_invariants()?;
        let op_id = self.fresh_op_id(now_ms);
        let seq = self.emit_block(db, &block, now_ms, &op_id, "block.update")?;
        Ok(CommandResult::new(id, None, op_id, seq))
    }

    /// Tombstone a Block. The op carries the Block's **whole state** with
    /// `deleted` set and goes down [`Self::emit_block`] — the same path an
    /// update takes — so a delete that wins LWW replaces the row rather than
    /// flipping one column on top of whatever the receiving replica held.
    pub(super) fn delete_block(
        &self,
        db: &mut Db,
        id: EntityRef,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Block)?;
        let now_ms = self.clock.now_ms();
        let mut block = read_block(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("block {id}")))?;
        block.deleted = true;
        block.updated_at = ms_to_ts(now_ms as i64);
        let op_id = self.fresh_op_id(now_ms);
        let seq = self.emit_block(db, &block, now_ms, &op_id, "block.delete")?;
        Ok(CommandResult::new(id, None, op_id, seq))
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
        let op_id = self.fresh_op_id(now_ms);
        let seq = self.emit_block(db, &block, now_ms, &op_id, "block.update")?;
        Ok(CommandResult::new(block_id, None, op_id, seq))
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
        let op_id = self.fresh_op_id(now_ms);
        let seq = self.emit_block(db, &block, now_ms, &op_id, "block.update")?;
        Ok(CommandResult::new(block_id, None, op_id, seq))
    }

    /// Seal a full-state Block op, materialize the row and its bindings, and
    /// append the op — all in one transaction. Returns the op's `seq`.
    fn emit_block(
        &self,
        db: &mut Db,
        block: &Block,
        now_ms: u64,
        op_id: &[u8; 16],
        inner_kind: &str,
    ) -> Result<u64, EngineError> {
        let inner = match inner_kind {
            "block.create" => InnerOp::BlockCreate(Box::new(block.clone())),
            "block.delete" => InnerOp::BlockDelete(Box::new(block.clone())),
            _ => InnerOp::BlockUpdate(Box::new(block.clone())),
        };
        let inner_op = encode_inner_op(&inner)?;
        let seq = self.next_seq(db, block.stream_id.bytes())?;
        let lww = self.lww_stamp(seq);
        let stream_bytes = *block.stream_id.bytes();
        let block = block.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            ensure_stream_row(tx, &block.stream_id, now_ms)?;
            upsert_block_row(tx, &block, &lww)?;
            replace_block_tasks(tx, &block)?;
            self.ops_insert(
                tx,
                op_id,
                &stream_bytes,
                seq,
                lww.hlc,
                &inner_op,
                inner_kind,
                "block",
                Some(block.id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;
        Ok(seq)
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

    /// Blocks overlapping the seven civil days beginning at the Monday of the
    /// week containing `week_ms`, in the device zone.
    pub(super) fn query_week_blocks(
        &self,
        db: &Db,
        week_ms: u64,
    ) -> Result<QueryResult, EngineError> {
        let tz = self.device_zone();
        let anchor = ms_to_ts(week_ms as i64).to_zoned(tz).date();
        // Monday-first, matching `WeekGrid` and every other weekly fold here.
        let back = i64::from(anchor.weekday().to_monday_zero_offset());
        let monday = anchor
            .checked_sub(jiff::Span::new().days(back))
            .unwrap_or(anchor);
        let monday_ms = self.civil_day_start_ms(monday)?;
        let (from, to) = self.civil_span(monday_ms, 7)?;
        self.query_blocks_between(db, from, to)
    }

    /// `[start_of_day(at_ms), start_of_day(at_ms) + days)` in the device zone,
    /// as epoch-millisecond bounds.
    ///
    /// Computed over civil dates rather than by adding `86_400_000` so a day
    /// that is 23 or 25 hours long across a DST transition is still exactly one
    /// day on the grid.
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

    /// The device-local zone, from the injected clock. An unknown IANA name
    /// degrades to UTC rather than failing a read.
    pub(super) fn device_zone(&self) -> jiff::tz::TimeZone {
        jiff::tz::TimeZone::get(&self.clock.timezone()).unwrap_or(jiff::tz::TimeZone::UTC)
    }

    /// Every live Block whose `[starts_at, ends_at)` overlaps `[from, to)`.
    ///
    /// Overlap, not containment: a two-hour block that started before the
    /// window still belongs on the grid, which is why the `blocks_by_end`
    /// index exists.
    pub(super) fn query_blocks_between(
        &self,
        db: &Db,
        from: i64,
        to: i64,
    ) -> Result<QueryResult, EngineError> {
        let mut stmt = db.conn().prepare(
            "SELECT id FROM blocks
             WHERE deleted = 0 AND starts_at_ms < ? AND ends_at_ms > ?
             ORDER BY starts_at_ms ASC, id ASC",
        )?;
        let ids = stmt
            .query_map(params![to, from], |r| r.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut rows = Vec::with_capacity(ids.len());
        for raw in ids {
            let Some(block) = read_block(db.conn(), &blob16(&raw))? else {
                continue;
            };
            rows.push(block_row(db.conn(), block)?);
        }
        Ok(QueryResult::Blocks(rows))
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
    })
}

/// INSERT-or-REPLACE a Block row, stamping the LWW columns.
pub(super) fn upsert_block_row(
    tx: &Transaction<'_>,
    b: &Block,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    let (start_ms, start_kind, start_tz) = b.starts_at.to_parts();
    let (end_ms, end_kind, end_tz) = b.ends_at.to_parts();
    tx.execute(
        "INSERT INTO blocks
         (id, stream_id, starts_at_ms, starts_at_kind, starts_at_tz,
          ends_at_ms, ends_at_kind, ends_at_tz, title, title_track_task,
          deleted, extra, created_at_ms, updated_at_ms,
          lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (id) DO UPDATE SET
            stream_id = excluded.stream_id,
            starts_at_ms = excluded.starts_at_ms,
            starts_at_kind = excluded.starts_at_kind,
            starts_at_tz = excluded.starts_at_tz,
            ends_at_ms = excluded.ends_at_ms,
            ends_at_kind = excluded.ends_at_kind,
            ends_at_tz = excluded.ends_at_tz,
            title = excluded.title,
            title_track_task = excluded.title_track_task,
            deleted = excluded.deleted,
            extra = excluded.extra,
            updated_at_ms = excluded.updated_at_ms,
            lww_hlc_ms = excluded.lww_hlc_ms,
            lww_hlc_logical = excluded.lww_hlc_logical,
            lww_seq = excluded.lww_seq,
            lww_device = excluded.lww_device",
        params![
            &b.id.bytes()[..],
            &b.stream_id.bytes()[..],
            start_ms,
            start_kind,
            start_tz,
            end_ms,
            end_kind,
            end_tz,
            b.title.as_deref(),
            b.title_track_task as i64,
            b.deleted as i64,
            encode_unknowns(&b.unknown)?,
            b.created_at.as_millisecond(),
            b.updated_at.as_millisecond(),
            lww.hlc.physical_ms as i64,
            lww.hlc.logical,
            lww.seq as i64,
            &lww.device[..],
        ],
    )?;
    Ok(())
}

/// Replace a Block's bindings with the op's set. Full-state, like every other
/// v1 op: the set on the winning op is the set.
pub(super) fn replace_block_tasks(tx: &Transaction<'_>, b: &Block) -> rusqlite::Result<()> {
    tx.execute(
        "DELETE FROM block_tasks WHERE block_id = ?",
        params![&b.id.bytes()[..]],
    )?;
    for t in &b.tasks {
        tx.execute(
            "INSERT OR IGNORE INTO block_tasks (block_id, task_id) VALUES (?, ?)",
            params![&b.id.bytes()[..], &t.bytes()[..]],
        )?;
    }
    Ok(())
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
                    deleted, extra, created_at_ms, updated_at_ms
             FROM blocks WHERE id = ?",
            params![&id[..]],
            |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
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
                ))
            },
        )
        .optional()?;
    let Some(b) = row else {
        return Ok(None);
    };
    Ok(Some(Block {
        id: EntityRef::new(EntityKind::Block, *id),
        created_at: ms_to_ts(b.11.max(0)),
        updated_at: ms_to_ts(b.12.max(0)),
        stream_id: EntityRef::new(EntityKind::Stream, blob16(&b.0)),
        starts_at: SunriseTime::from_parts(b.1, &b.2, b.3.as_deref()),
        ends_at: SunriseTime::from_parts(b.4, &b.5, b.6.as_deref()),
        title: b.7,
        title_track_task: b.8 != 0,
        tasks: read_block_tasks(conn, id)?,
        deleted: b.9 != 0,
        unknown: decode_unknowns(b.10),
    }))
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
