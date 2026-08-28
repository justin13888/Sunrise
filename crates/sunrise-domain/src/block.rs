//! Block (time-block) entity per `docs/02-domain/time-blocks.md`.
//!
//! A Block is a scheduled time range that may bind to 0..N Tasks. Tasks may
//! reference 0..N Blocks via `Task.blocks` (OR-Set).
//!
//! # Title
//!
//! A Block's title is a **shadow copy** of the bound Task's title taken at
//! bind time, not a live binding — so renaming the Task later does not rewrite
//! the calendar. [`Block::title_track_task`] opts one Block back into live
//! tracking, and [`Block::resolve_title`] is the single place that decides
//! which of the two a reader gets. Per the spec a Block bound to two or more
//! Tasks ignores the flag: there is no single task title to shadow.

use crate::time::SunriseTime;
use crate::unknown::Unknowns;
use crate::validation::{validate_title, ValidationError, MAX_BLOCK_TITLE_LEN};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use sunrise_id::EntityRef;

/// Persisted Block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    /// Block id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: Timestamp,
    /// Last update.
    pub updated_at: Timestamp,
    /// Owning Stream.
    pub stream_id: EntityRef,
    /// Start time. See [`SunriseTime`]: a block at "09:00" is a different
    /// commitment from a block at a fixed instant, and a device that changes
    /// zone must move one and not the other.
    pub starts_at: SunriseTime,
    /// End time. MUST resolve after `starts_at`.
    pub ends_at: SunriseTime,
    /// Optional title (free text; UI may compose from bound tasks).
    #[serde(default)]
    pub title: Option<String>,
    /// Recompute the title from the single bound Task's *current* title on
    /// read, instead of keeping the shadow copy taken at bind time.
    ///
    /// Defaults to `false` so a user-edited Block title is never silently
    /// overwritten, per `docs/02-domain/time-blocks.md` §Block title. Ignored
    /// when the Block binds two or more Tasks.
    #[serde(default)]
    pub title_track_task: bool,
    /// Tasks bound to this Block (OR-Set).
    #[serde(default)]
    pub tasks: BTreeSet<EntityRef>,
    /// Tombstone.
    #[serde(default)]
    pub deleted: bool,
    /// Fields written by a newer `DOC_SCHEMA_V` that this build does not
    /// model, preserved verbatim and re-emitted. See [`crate::unknown`] and
    /// `docs/10-cross-cutting/protocol-versioning.md` §7.
    #[serde(flatten)]
    pub unknown: Unknowns,
}

impl Block {
    /// Re-check the Block's cross-field invariants.
    ///
    /// `ends_at` must resolve strictly after `starts_at`. The comparison is on
    /// [`SunriseTime::index_ms`] — the same key storage indexes and orders on —
    /// so "the block ends before it starts" means the same thing here as it
    /// does to `ORDER BY starts_at_ms`, whatever kinds the two values are.
    pub fn validate_invariants(&self) -> Result<(), ValidationError> {
        validate_range(&self.starts_at, &self.ends_at)?;
        if let Some(t) = &self.title {
            let _ = validate_title(t, "block.title", MAX_BLOCK_TITLE_LEN)?;
        }
        Ok(())
    }

    /// The title a reader should show, given the current titles of the bound
    /// Tasks in `bound_titles` (in `tasks` order).
    ///
    /// Three cases, in the order the spec states them:
    ///
    /// 1. Two or more bound Tasks — `title_track_task` is ignored and the
    ///    stored title stands, because there is no single task to shadow.
    /// 2. Exactly one bound Task and `title_track_task` — the Task's current
    ///    title wins over the stored shadow copy.
    /// 3. Otherwise — the stored title, which for a freshly bound Block *is*
    ///    the shadow copy taken at bind time.
    #[must_use]
    pub fn resolve_title<'a>(&'a self, bound_titles: &'a [String]) -> Option<&'a str> {
        if self.title_track_task && bound_titles.len() == 1 {
            return Some(bound_titles[0].as_str());
        }
        self.title.as_deref()
    }
}

/// Draft for creating a Block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockDraft {
    /// Owning Stream.
    pub stream_id: EntityRef,
    /// Start.
    pub starts_at: SunriseTime,
    /// End.
    pub ends_at: SunriseTime,
    /// Optional title. Left `None` with exactly one task bound, the core takes
    /// the shadow copy from that task's title.
    pub title: Option<String>,
    /// Track the single bound Task's title instead of shadow-copying it.
    #[serde(default)]
    pub title_track_task: bool,
    /// Optional tasks to bind on creation.
    pub tasks: Vec<EntityRef>,
}

impl BlockDraft {
    /// Validate the draft against domain rules.
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_range(&self.starts_at, &self.ends_at)?;
        if let Some(t) = &self.title {
            let _ = validate_title(t, "block.title", MAX_BLOCK_TITLE_LEN)?;
        }
        // A Block over two or more Tasks has no single title to shadow, so an
        // explicit one is required rather than silently produced.
        if self.title.is_none() && self.tasks.len() > 1 {
            return Err(ValidationError::Field {
                field: "block.title",
                constraint: "required_for_multi_task",
            });
        }
        Ok(())
    }
}

/// Patch applied via `Command::UpdateBlock`. `Some(None)` clears an optional
/// field; `None` leaves it unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlockPatch {
    /// New start.
    pub starts_at: Option<SunriseTime>,
    /// New end.
    pub ends_at: Option<SunriseTime>,
    /// New title; `Some(None)` clears it.
    pub title: Option<Option<String>>,
    /// Turn live title tracking on or off.
    pub title_track_task: Option<bool>,
    /// Move the Block to another Stream.
    pub stream_id: Option<EntityRef>,
}

impl BlockPatch {
    /// Validate the patch's individual fields. Cross-field rules are re-checked
    /// on the patched Block by [`Block::validate_invariants`].
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(Some(t)) = &self.title {
            let _ = validate_title(t, "block.title", MAX_BLOCK_TITLE_LEN)?;
        }
        Ok(())
    }
}

/// `ends_at` must resolve strictly after `starts_at`.
fn validate_range(starts_at: &SunriseTime, ends_at: &SunriseTime) -> Result<(), ValidationError> {
    if ends_at.index_ms() <= starts_at.index_ms() {
        return Err(ValidationError::Field {
            field: "block.ends_at",
            constraint: "after_starts_at",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil;
    use sunrise_id::EntityKind;

    fn stream() -> EntityRef {
        EntityRef::new(EntityKind::Stream, [7u8; 16])
    }

    fn task(n: u8) -> EntityRef {
        EntityRef::new(EntityKind::Task, [n; 16])
    }

    fn at(hour: i8) -> SunriseTime {
        SunriseTime::floating(civil::date(2026, 3, 4).at(hour, 0, 0, 0))
    }

    fn draft() -> BlockDraft {
        BlockDraft {
            stream_id: stream(),
            starts_at: at(9),
            ends_at: at(10),
            title: Some("Deep work".into()),
            title_track_task: false,
            tasks: Vec::new(),
        }
    }

    fn block() -> Block {
        Block {
            id: EntityRef::new(EntityKind::Block, [1u8; 16]),
            created_at: Timestamp::UNIX_EPOCH,
            updated_at: Timestamp::UNIX_EPOCH,
            stream_id: stream(),
            starts_at: at(9),
            ends_at: at(10),
            title: Some("Deep work".into()),
            title_track_task: false,
            tasks: BTreeSet::new(),
            deleted: false,
            unknown: Unknowns::new(),
        }
    }

    #[test]
    fn draft_accepts_a_forward_range() {
        draft().validate().unwrap();
    }

    #[test]
    fn draft_rejects_an_inverted_range() {
        let d = BlockDraft {
            starts_at: at(10),
            ends_at: at(9),
            ..draft()
        };
        assert_eq!(
            d.validate(),
            Err(ValidationError::Field {
                field: "block.ends_at",
                constraint: "after_starts_at"
            })
        );
    }

    #[test]
    fn draft_rejects_a_zero_length_range() {
        let d = BlockDraft {
            starts_at: at(9),
            ends_at: at(9),
            ..draft()
        };
        assert!(d.validate().is_err());
    }

    /// A range is compared on the storage index key, so mixed kinds order the
    /// same way here as they do in `ORDER BY starts_at_ms`.
    #[test]
    fn range_compares_across_kinds() {
        let d = BlockDraft {
            starts_at: SunriseTime::instant(
                Timestamp::from_millisecond(1_772_000_000_000).unwrap(),
            ),
            ends_at: SunriseTime::floating(civil::date(2000, 1, 1).at(0, 0, 0, 0)),
            ..draft()
        };
        assert!(d.validate().is_err());
    }

    #[test]
    fn draft_requires_a_title_for_a_multi_task_block() {
        let d = BlockDraft {
            title: None,
            tasks: vec![task(1), task(2)],
            ..draft()
        };
        assert_eq!(
            d.validate(),
            Err(ValidationError::Field {
                field: "block.title",
                constraint: "required_for_multi_task"
            })
        );
        // One task is fine: the core shadow-copies that task's title.
        let one = BlockDraft {
            title: None,
            tasks: vec![task(1)],
            ..draft()
        };
        one.validate().unwrap();
    }

    #[test]
    fn draft_rejects_an_over_long_title() {
        let d = BlockDraft {
            title: Some("x".repeat(MAX_BLOCK_TITLE_LEN + 1)),
            ..draft()
        };
        assert!(d.validate().is_err());
    }

    #[test]
    fn shadow_copy_is_the_default() {
        let mut b = block();
        b.tasks.insert(task(1));
        assert_eq!(
            b.resolve_title(&["Renamed later".to_string()]),
            Some("Deep work")
        );
    }

    #[test]
    fn tracking_follows_the_single_bound_task() {
        let mut b = block();
        b.title_track_task = true;
        b.tasks.insert(task(1));
        assert_eq!(
            b.resolve_title(&["Renamed later".to_string()]),
            Some("Renamed later")
        );
    }

    #[test]
    fn tracking_is_ignored_once_a_second_task_is_bound() {
        let mut b = block();
        b.title_track_task = true;
        b.tasks.insert(task(1));
        b.tasks.insert(task(2));
        assert_eq!(
            b.resolve_title(&["One".to_string(), "Two".to_string()]),
            Some("Deep work")
        );
    }

    #[test]
    fn tracking_with_no_bound_task_falls_back_to_the_stored_title() {
        let mut b = block();
        b.title_track_task = true;
        assert_eq!(b.resolve_title(&[]), Some("Deep work"));
    }

    #[test]
    fn invariants_reject_an_inverted_range() {
        let b = Block {
            starts_at: at(11),
            ends_at: at(10),
            ..block()
        };
        assert!(b.validate_invariants().is_err());
    }

    #[test]
    fn patch_validates_its_title() {
        let p = BlockPatch {
            title: Some(Some("  ".into())),
            ..Default::default()
        };
        assert_eq!(p.validate(), Err(ValidationError::InvalidTitle));
        let clear = BlockPatch {
            title: Some(None),
            ..Default::default()
        };
        clear.validate().unwrap();
    }
}
