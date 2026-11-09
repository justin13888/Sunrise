//! Time blocks: a stretch of the calendar reserved for work.

use sunrise_core::queries::BlockRow;
use sunrise_domain::{Block, BlockException, BlockKind, BlockSource, NoteBody, RRule, SunriseTime};
use sunrise_id::EntityRef;

use super::{task::patch_field, Recurrence, TimeValue};

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
    /// Whether the planner may move the block: `fixed` or `flexible`, as
    /// [`Block::effective_kind`] reads it, so never a spelling a client does
    /// not know.
    #[uniffi(default = "flexible")]
    pub kind: String,
    /// The rule of a recurring block, whose `starts_at` and `ends_at` are
    /// then the series anchor. Absent for a one-off block.
    #[uniffi(default = None)]
    pub rrule: Option<Recurrence>,
    /// A recurring block's per-occurrence changes, sorted by key.
    #[uniffi(default = [])]
    pub exceptions: Vec<BlockExceptionItem>,
    /// The series root a block split off by an edit *this and future*
    /// continues.
    #[uniffi(default = None)]
    pub split_from: Option<EntityRef>,
    /// Free-text location.
    #[uniffi(default = None)]
    pub location: Option<String>,
    /// Notes.
    #[uniffi(default = None)]
    pub notes: Option<NoteBody>,
    /// Where the block came from (`sunrise`, `import:ics`, or a spelling a
    /// newer build wrote), or absent for a block that does not say.
    #[uniffi(default = None)]
    pub source: Option<String>,
    /// The foreign id an `.ics` import read.
    #[uniffi(default = None)]
    pub external_id: Option<String>,
}

/// One occurrence's change on a recurring block. See
/// [`sunrise_domain::BlockException`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct BlockExceptionItem {
    /// The occurrence key, `YYYY-MM-DDTHH:MM`.
    pub key: String,
    /// The occurrence is removed; the other fields are then ignored.
    pub cancelled: bool,
    /// The occurrence's new start, over the rule's.
    pub starts_at: Option<TimeValue>,
    /// The occurrence's new end, over the rule's.
    pub ends_at: Option<TimeValue>,
    /// The occurrence's title, over the series'.
    pub title: Option<String>,
    /// The occurrence's location, over the series'.
    pub location: Option<String>,
    /// The occurrence's notes, over the series'.
    pub notes: Option<NoteBody>,
}

impl BlockExceptionItem {
    fn new(key: &str, e: &BlockException) -> Self {
        let BlockException {
            cancelled,
            starts_at,
            ends_at,
            title,
            location,
            notes,
            // Deliberately not exported: see the module docs.
            unknown: _,
        } = e;
        Self {
            key: key.to_owned(),
            cancelled: *cancelled,
            starts_at: starts_at.as_ref().map(TimeValue::from),
            ends_at: ends_at.as_ref().map(TimeValue::from),
            title: title.clone(),
            location: location.clone(),
            notes: notes.clone(),
        }
    }

    fn to_domain(&self) -> BlockException {
        BlockException {
            cancelled: self.cancelled,
            starts_at: self.starts_at.clone().map(SunriseTime::from),
            ends_at: self.ends_at.clone().map(SunriseTime::from),
            title: self.title.clone(),
            location: self.location.clone(),
            notes: self.notes.clone(),
            unknown: sunrise_domain::Unknowns::new(),
        }
    }
}

impl From<&Block> for BlockItem {
    fn from(b: &Block) -> Self {
        let kind = b.effective_kind();
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
            kind: _,
            rrule,
            exceptions,
            split_from,
            location,
            notes,
            source,
            external_id,
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
            kind: kind.as_str().to_owned(),
            rrule: rrule.as_ref().map(Recurrence::from),
            exceptions: exceptions
                .iter()
                .map(|(k, e)| BlockExceptionItem::new(k, e))
                .collect(),
            split_from: *split_from,
            location: location.clone(),
            notes: notes.clone(),
            source: source.as_ref().map(|s| s.as_str().to_owned()),
            external_id: external_id.clone(),
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
            kind: Some(BlockKind::from_raw(&self.kind)),
            rrule: self.rrule.clone().map(RRule::from),
            exceptions: self
                .exceptions
                .iter()
                .map(|e| (e.key.clone(), e.to_domain()))
                .collect(),
            split_from: self.split_from,
            location: self.location.clone(),
            notes: self.notes.clone(),
            source: self.source.as_deref().map(BlockSource::from_raw),
            external_id: self.external_id.clone(),
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
    use std::collections::{BTreeMap, BTreeSet};

    use sunrise_domain::{
        Block, BlockDraft, BlockException, BlockKind, BlockSource, NoteBody, SunriseTime, Unknowns,
    };
    use sunrise_id::{EntityKind, EntityRef};

    use super::{BlockDraftIn, BlockItem};
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

    fn block(kind: Option<BlockKind>, source: Option<BlockSource>) -> Block {
        let at = |ms| SunriseTime::instant(jiff::Timestamp::from_millisecond(ms).unwrap());
        Block {
            id: EntityRef::new(EntityKind::Block, [1; 16]),
            created_at: jiff::Timestamp::UNIX_EPOCH,
            updated_at: jiff::Timestamp::UNIX_EPOCH,
            stream_id: None,
            starts_at: at(1_000),
            ends_at: at(2_000),
            title: None,
            title_track_task: false,
            tasks: BTreeSet::new(),
            deleted: false,
            kind,
            rrule: None,
            exceptions: BTreeMap::new(),
            split_from: None,
            location: None,
            notes: None,
            source,
            external_id: None,
            unknown: Unknowns::new(),
        }
    }

    /// `kind` crosses as the effective spelling, never absent or unknown:
    /// an `import:ics` block with no kind reads `fixed`, a `sunrise` or
    /// sourceless one `flexible`, an unknown kind `fixed`. `source` crosses
    /// raw, an unknown spelling verbatim.
    #[test]
    fn a_blocks_kind_crosses_effective_and_its_source_raw() {
        let cases = [
            (
                None,
                Some(BlockSource::ImportIcs),
                "fixed",
                Some("import:ics"),
            ),
            (
                None,
                Some(BlockSource::Sunrise),
                "flexible",
                Some("sunrise"),
            ),
            (None, None, "flexible", None),
            (
                None,
                Some(BlockSource::from_raw("caldav")),
                "fixed",
                Some("caldav"),
            ),
            (
                Some(BlockKind::Flexible),
                Some(BlockSource::ImportIcs),
                "flexible",
                Some("import:ics"),
            ),
            (Some(BlockKind::from_raw("tentative")), None, "fixed", None),
        ];
        for (kind, source, want_kind, want_source) in cases {
            let item = BlockItem::from(&block(kind.clone(), source.clone()));
            assert_eq!(item.kind, want_kind, "kind {kind:?}, source {source:?}");
            assert_eq!(item.source.as_deref(), want_source);
        }
    }

    /// The exceptions map flattens into items sorted by key, each field
    /// carried, and the series, import and note fields cross as they are.
    #[test]
    fn a_blocks_series_and_import_fields_cross() {
        let at = |ms| SunriseTime::instant(jiff::Timestamp::from_millisecond(ms).unwrap());
        let root = EntityRef::new(EntityKind::Block, [9; 16]);
        let mut b = block(None, Some(BlockSource::ImportIcs));
        b.exceptions.insert(
            "2026-03-10T09:00".into(),
            BlockException {
                starts_at: Some(at(5_000)),
                ends_at: Some(at(6_000)),
                title: Some("Moved".into()),
                location: Some("Room 2".into()),
                notes: Some(NoteBody(vec![7, 8])),
                ..BlockException::default()
            },
        );
        b.exceptions.insert(
            "2026-03-03T09:00".into(),
            BlockException {
                cancelled: true,
                ..BlockException::default()
            },
        );
        b.split_from = Some(root);
        b.location = Some("Office".into());
        b.notes = Some(NoteBody(vec![1, 2, 3]));
        b.external_id = Some("uid-1@example.com".into());

        let item = BlockItem::from(&b);
        let keys: Vec<&str> = item.exceptions.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, ["2026-03-03T09:00", "2026-03-10T09:00"]);
        let cancelled = &item.exceptions[0];
        assert!(cancelled.cancelled);
        assert!(cancelled.starts_at.is_none() && cancelled.title.is_none());
        let moved = &item.exceptions[1];
        assert!(!moved.cancelled);
        assert_eq!(
            format!("{:?}", moved.starts_at),
            format!("{:?}", Some(TimeValue::from(&at(5_000))))
        );
        assert_eq!(
            format!("{:?}", moved.ends_at),
            format!("{:?}", Some(TimeValue::from(&at(6_000))))
        );
        assert_eq!(moved.title.as_deref(), Some("Moved"));
        assert_eq!(moved.location.as_deref(), Some("Room 2"));
        assert_eq!(moved.notes, Some(NoteBody(vec![7, 8])));
        assert_eq!(item.split_from, Some(root));
        assert_eq!(item.location.as_deref(), Some("Office"));
        assert_eq!(item.notes, Some(NoteBody(vec![1, 2, 3])));
        assert_eq!(item.external_id.as_deref(), Some("uid-1@example.com"));
        assert!(item.rrule.is_none());

        // Back to the domain, the effective kind is now stored explicitly.
        let back = item.to_domain();
        assert_eq!(back.kind, Some(BlockKind::Fixed));
        assert_eq!(back.exceptions, b.exceptions);
        assert_eq!(back.source, b.source);
    }
}
