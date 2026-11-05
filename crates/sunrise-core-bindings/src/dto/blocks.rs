//! Time blocks: a stretch of the calendar reserved for work.

use sunrise_core::queries::BlockRow;
use sunrise_domain::{Block, SunriseTime};
use sunrise_id::EntityRef;

use super::{task::patch_field, TimeValue};

/// See [`sunrise_domain::Block`]: a scheduled range that may bind 0..N tasks.
#[derive(Debug, Clone, uniffi::Record)]
pub struct BlockItem {
    /// Block id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: jiff::Timestamp,
    /// Last-update time.
    pub updated_at: jiff::Timestamp,
    /// Owning stream.
    pub stream_id: EntityRef,
    /// Start.
    pub starts_at: TimeValue,
    /// End; resolves after `starts_at`.
    pub ends_at: TimeValue,
    /// Stored title — the shadow copy taken at bind time, or a user edit.
    /// [`BlockGridRow::title`] is what a grid should render.
    pub title: Option<String>,
    /// Recompute the title from the single bound task's current title.
    pub title_track_task: bool,
    /// Bound tasks, sorted.
    pub tasks: Vec<EntityRef>,
    /// Tombstoned.
    pub deleted: bool,
}

impl From<&Block> for BlockItem {
    fn from(b: &Block) -> Self {
        let Block {
            id,
            created_at,
            updated_at,
            stream_id,
            starts_at,
            ends_at,
            title,
            title_track_task,
            tasks,
            deleted,
            // Deliberately not exported: see the module docs.
            unknown: _,
        } = b;
        Self {
            id: *id,
            created_at: *created_at,
            updated_at: *updated_at,
            stream_id: sunrise_domain::stream_key(*stream_id),
            starts_at: TimeValue::from(starts_at),
            ends_at: TimeValue::from(ends_at),
            title: title.clone(),
            title_track_task: *title_track_task,
            tasks: tasks.iter().copied().collect(),
            deleted: *deleted,
        }
    }
}

impl BlockItem {
    /// Back to the domain record this row was projected from.
    ///
    /// Infallible — every field is already the domain's own type — and
    /// `unknown` is empty, because forward-compat fields are preserved in
    /// storage and deliberately never exported. Nothing round-trips a Block
    /// *back into the vault* through here; the callers are the read-only
    /// conflict and merge helpers in [`crate::vocab`], which need a `Block` to
    /// hand the domain and never write one.
    pub(crate) fn to_domain(&self) -> Block {
        Block {
            id: self.id,
            created_at: self.created_at,
            updated_at: self.updated_at,
            stream_id: sunrise_domain::home_stream(Some(self.stream_id)),
            starts_at: SunriseTime::from(self.starts_at.clone()),
            ends_at: SunriseTime::from(self.ends_at.clone()),
            title: self.title.clone(),
            title_track_task: self.title_track_task,
            tasks: self.tasks.iter().copied().collect(),
            deleted: self.deleted,
            unknown: sunrise_domain::Unknowns::new(),
        }
    }
}

/// One row of the calendar grid. See [`sunrise_core::queries::BlockRow`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct BlockGridRow {
    /// The block.
    pub block: BlockItem,
    /// The title to render, after the shadow-copy / tracking rules.
    pub title: Option<String>,
    /// Titles of the bound live tasks this replica knows about. Shorter than
    /// `block.tasks` when a binding names a task whose op has not arrived.
    pub task_titles: Vec<String>,
}

impl From<&BlockRow> for BlockGridRow {
    fn from(r: &BlockRow) -> Self {
        let BlockRow {
            block,
            title,
            task_titles,
        } = r;
        Self {
            block: BlockItem::from(block),
            title: title.clone(),
            task_titles: task_titles.clone(),
        }
    }
}

/// See [`sunrise_domain::BlockDraft`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct BlockDraftIn {
    /// Owning stream.
    pub stream_id: EntityRef,
    /// Start.
    pub starts_at: TimeValue,
    /// End.
    pub ends_at: TimeValue,
    /// Title. Absent with exactly one task bound, the core shadow-copies that
    /// task's title; absent with two or more, the create is rejected.
    pub title: Option<String>,
    /// Track the single bound task's title instead of shadow-copying it.
    pub title_track_task: bool,
    /// Tasks to bind on creation.
    pub tasks: Vec<EntityRef>,
}

impl From<BlockDraftIn> for sunrise_domain::BlockDraft {
    fn from(d: BlockDraftIn) -> Self {
        Self {
            // The legacy Inbox id means no stream (ADR-0046 §5).
            stream_id: sunrise_domain::home_stream(Some(d.stream_id)),
            starts_at: SunriseTime::from(d.starts_at),
            ends_at: SunriseTime::from(d.ends_at),
            title: d.title,
            title_track_task: d.title_track_task,
            tasks: d.tasks,
        }
    }
}

impl From<&sunrise_domain::BlockDraft> for BlockDraftIn {
    fn from(d: &sunrise_domain::BlockDraft) -> Self {
        let sunrise_domain::BlockDraft {
            stream_id,
            starts_at,
            ends_at,
            title,
            title_track_task,
            tasks,
        } = d;
        Self {
            stream_id: sunrise_domain::stream_key(*stream_id),
            starts_at: TimeValue::from(starts_at),
            ends_at: TimeValue::from(ends_at),
            title: title.clone(),
            title_track_task: *title_track_task,
            tasks: tasks.clone(),
        }
    }
}

/// An edit to a block. Split-optional, like [`TaskEdit`](super::TaskEdit).
#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct BlockEdit {
    /// New start.
    #[uniffi(default = None)]
    pub starts_at: Option<TimeValue>,
    /// New end.
    #[uniffi(default = None)]
    pub ends_at: Option<TimeValue>,
    /// New title.
    #[uniffi(default = None)]
    pub set_title: Option<String>,
    /// Clear the title.
    #[uniffi(default = false)]
    pub clear_title: bool,
    /// Turn live title tracking on or off.
    #[uniffi(default = None)]
    pub title_track_task: Option<bool>,
    /// Move the block to another stream.
    #[uniffi(default = None)]
    pub stream_id: Option<EntityRef>,
}

impl From<BlockEdit> for sunrise_domain::BlockPatch {
    fn from(e: BlockEdit) -> Self {
        Self {
            starts_at: e.starts_at.map(SunriseTime::from),
            ends_at: e.ends_at.map(SunriseTime::from),
            title: patch_field(e.set_title, e.clear_title),
            title_track_task: e.title_track_task,
            // The legacy Inbox id moves the block to no stream.
            stream_id: e.stream_id.map(|s| sunrise_domain::home_stream(Some(s))),
        }
    }
}

#[cfg(test)]
mod tests {
    use sunrise_domain::{BlockDraft, SunriseTime};
    use sunrise_id::{EntityKind, EntityRef};

    use super::BlockDraftIn;
    use crate::dto::TimeValue;

    /// Every field crosses, and no stream reads as the legacy Inbox key, the
    /// inverse of `From<BlockDraftIn> for BlockDraft`.
    #[test]
    fn a_block_draft_crosses_field_for_field() {
        let stream = EntityRef::new(EntityKind::Stream, [2; 16]);
        let task = EntityRef::new(EntityKind::Task, [3; 16]);
        let starts_at = SunriseTime::instant(jiff::Timestamp::from_millisecond(1_000).unwrap());
        let ends_at = SunriseTime::instant(jiff::Timestamp::from_millisecond(2_000).unwrap());
        let d = BlockDraft {
            stream_id: Some(stream),
            starts_at: starts_at.clone(),
            ends_at: ends_at.clone(),
            title: Some("Deep work".into()),
            title_track_task: true,
            tasks: vec![task],
        };
        let into = BlockDraftIn::from(&d);
        assert_eq!(into.stream_id, stream);
        assert_eq!(
            format!("{:?}", into.starts_at),
            format!("{:?}", TimeValue::from(&starts_at))
        );
        assert_eq!(
            format!("{:?}", into.ends_at),
            format!("{:?}", TimeValue::from(&ends_at))
        );
        assert_eq!(into.title.as_deref(), Some("Deep work"));
        assert!(into.title_track_task);
        assert_eq!(into.tasks, vec![task]);
        assert_eq!(format!("{:?}", BlockDraft::from(into)), format!("{d:?}"));

        let homeless = BlockDraftIn::from(&BlockDraft {
            stream_id: None,
            ..d
        });
        assert_eq!(homeless.stream_id, sunrise_domain::stream_key(None));
    }
}
