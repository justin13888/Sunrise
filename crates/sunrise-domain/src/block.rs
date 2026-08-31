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
use jiff::tz::TimeZone;
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

/// Two live Blocks whose ranges intersect, and the region they share.
///
/// `docs/02-domain/time-blocks.md` §Conflicts is explicit that both Blocks
/// survive: nothing auto-merges and nothing auto-deletes. The UI shades the
/// shared region and offers "keep both / merge / adjust times", so what a
/// client needs is *where the shading goes*, and that is what this carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockOverlap {
    /// The earlier-starting Block.
    pub a: EntityRef,
    /// The later-starting Block.
    pub b: EntityRef,
    /// Start of the shared region (epoch ms).
    pub from_ms: i64,
    /// End of the shared region (epoch ms), exclusive.
    pub to_ms: i64,
}

/// Every pair of `blocks` whose `[starts_at, ends_at)` intersect.
///
/// Compared on [`SunriseTime::index_ms`] — the key storage indexes and orders
/// on — so a floating 09:00 and a zoned 09:00 New York are compared the way
/// `ORDER BY starts_at_ms` compares them, and the answer does not depend on
/// which of the four kinds each bound happens to be.
///
/// Touching is not overlapping: a block ending at 10:00 and one starting at
/// 10:00 share no time, and shading a zero-width region would put a conflict
/// marker on every back-to-back pair in a well-planned day.
///
/// Tombstoned blocks are skipped. Quadratic in the number of blocks on screen,
/// which is a day or a week of them.
#[must_use]
pub fn overlaps(blocks: &[Block]) -> Vec<BlockOverlap> {
    let mut live: Vec<&Block> = blocks.iter().filter(|b| !b.deleted).collect();
    live.sort_by_key(|b| (b.starts_at.index_ms(), b.id));

    let mut out = Vec::new();
    for (i, a) in live.iter().enumerate() {
        for b in &live[i + 1..] {
            let from_ms = a.starts_at.index_ms().max(b.starts_at.index_ms());
            let to_ms = a.ends_at.index_ms().min(b.ends_at.index_ms());
            if from_ms < to_ms {
                out.push(BlockOverlap {
                    a: a.id,
                    b: b.id,
                    from_ms,
                    to_ms,
                });
            }
        }
    }
    out
}

/// The draft the Resolve menu's **Merge** action creates: the union time
/// range and the concatenated tasks of two Blocks.
///
/// Per `docs/02-domain/time-blocks.md` §Conflicts, merging is "tombstone the
/// two original Blocks and create a new one" — so this produces the create
/// half, and the caller submits all three.
///
/// # Which time kind survives
///
/// A union of two ranges has to pick a kind, and the four are not
/// interchangeable: a 09:00 floating block and a block at a fixed instant are
/// different commitments. Two bounds of the *same* kind — and, for zoned, the
/// same zone — merge into that kind, keeping what the user meant. Two
/// different kinds cannot both be honoured, so the union resolves to
/// [`SunriseTime::Instant`]: an instant is what every kind already resolves to
/// on this device, and pretending the result is still floating would silently
/// re-anchor the other block's meaning on the next flight.
///
/// All-day is included in that rule, which means merging an all-day block with
/// a timed one gives a timed block. That is the honest answer — the merged
/// commitment does have a time of day, because half of it did.
///
/// # Errors
///
/// [`ValidationError`] when the union does not validate — in practice only
/// when it binds two or more tasks and neither Block carried a title, since a
/// multi-task Block has no single task title to shadow.
pub fn merge_blocks(a: &Block, b: &Block, zone: &TimeZone) -> Result<BlockDraft, ValidationError> {
    let (early, late) = if a.starts_at.index_ms() <= b.starts_at.index_ms() {
        (a, b)
    } else {
        (b, a)
    };
    let starts_at = union_bound(&early.starts_at, &late.starts_at, zone, Bound::Start);
    let ends_at = union_bound(&early.ends_at, &late.ends_at, zone, Bound::End);

    let mut tasks: Vec<EntityRef> = early.tasks.iter().copied().collect();
    for t in &late.tasks {
        if !tasks.contains(t) {
            tasks.push(*t);
        }
    }

    let draft = BlockDraft {
        stream_id: early.stream_id,
        starts_at,
        ends_at,
        title: merged_title(early.title.as_deref(), late.title.as_deref()),
        // The merged Block's title is now its own: it was composed from two,
        // and letting it snap back to a single task's title would discard the
        // other half of what it is about.
        title_track_task: false,
        tasks,
    };
    draft.validate()?;
    Ok(draft)
}

/// Which end of the range a bound is, and therefore which way the union goes.
#[derive(Clone, Copy)]
enum Bound {
    Start,
    End,
}

/// The wider of two bounds, keeping the kind when both agree on it.
fn union_bound(x: &SunriseTime, y: &SunriseTime, zone: &TimeZone, bound: Bound) -> SunriseTime {
    let take_x = match bound {
        Bound::Start => x.index_ms() <= y.index_ms(),
        Bound::End => x.index_ms() >= y.index_ms(),
    };
    let winner = if take_x { x } else { y };
    if same_kind(x, y) {
        winner.clone()
    } else {
        SunriseTime::instant(winner.to_instant(zone))
    }
}

/// Whether two values are the same kind — and, for zoned, the same zone.
fn same_kind(x: &SunriseTime, y: &SunriseTime) -> bool {
    match (x, y) {
        (SunriseTime::Zoned { tz: a, .. }, SunriseTime::Zoned { tz: b, .. }) => a == b,
        _ => x.kind_str() == y.kind_str(),
    }
}

/// The merged Block's title.
///
/// Two titles become "A + B" because a merged Block really is about both, and
/// silently keeping only the first would lose what the user was looking at on
/// the other one. Identical titles are not doubled.
fn merged_title(x: Option<&str>, y: Option<&str>) -> Option<String> {
    match (x, y) {
        (Some(a), Some(b)) if a == b => Some(a.to_string()),
        (Some(a), Some(b)) => Some(format!("{a} + {b}")),
        (Some(t), None) | (None, Some(t)) => Some(t.to_string()),
        (None, None) => None,
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

#[cfg(test)]
mod conflict_tests {
    use super::*;
    use jiff::civil;
    use sunrise_id::EntityKind;

    fn block(n: u8, starts_at: SunriseTime, ends_at: SunriseTime) -> Block {
        Block {
            id: EntityRef::new(EntityKind::Block, [n; 16]),
            created_at: Timestamp::UNIX_EPOCH,
            updated_at: Timestamp::UNIX_EPOCH,
            stream_id: EntityRef::new(EntityKind::Stream, [1u8; 16]),
            starts_at,
            ends_at,
            title: Some(format!("Block {n}")),
            title_track_task: false,
            tasks: BTreeSet::new(),
            deleted: false,
            unknown: Unknowns::new(),
        }
    }

    fn floating(hour: i8) -> SunriseTime {
        SunriseTime::floating(civil::date(2026, 3, 4).at(hour, 0, 0, 0))
    }

    fn utc() -> TimeZone {
        TimeZone::UTC
    }

    #[test]
    fn intersecting_blocks_report_the_region_they_share() {
        let a = block(1, floating(9), floating(11));
        let b = block(2, floating(10), floating(12));
        let found = overlaps(&[a.clone(), b.clone()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].a, a.id);
        assert_eq!(found[0].b, b.id);
        assert_eq!(found[0].from_ms, floating(10).index_ms());
        assert_eq!(found[0].to_ms, floating(11).index_ms());
    }

    /// Back-to-back is not a conflict. Shading a zero-width region would put a
    /// marker on every well-planned day.
    #[test]
    fn touching_blocks_do_not_overlap() {
        let a = block(1, floating(9), floating(10));
        let b = block(2, floating(10), floating(11));
        assert!(overlaps(&[a, b]).is_empty());
    }

    #[test]
    fn a_containing_block_overlaps_the_one_inside_it() {
        let outer = block(1, floating(8), floating(18));
        let inner = block(2, floating(10), floating(11));
        let found = overlaps(&[inner.clone(), outer.clone()]);
        assert_eq!(found.len(), 1);
        // Ordered by start, so the container is `a` however they arrive.
        assert_eq!(found[0].a, outer.id);
        assert_eq!(found[0].b, inner.id);
        assert_eq!(found[0].from_ms, floating(10).index_ms());
        assert_eq!(found[0].to_ms, floating(11).index_ms());
    }

    #[test]
    fn a_tombstoned_block_conflicts_with_nothing() {
        let a = block(1, floating(9), floating(11));
        let mut b = block(2, floating(10), floating(12));
        b.deleted = true;
        assert!(overlaps(&[a, b]).is_empty());
    }

    #[test]
    fn three_mutually_overlapping_blocks_report_all_three_pairs() {
        let blocks = vec![
            block(1, floating(9), floating(12)),
            block(2, floating(10), floating(13)),
            block(3, floating(11), floating(14)),
        ];
        assert_eq!(overlaps(&blocks).len(), 3);
    }

    #[test]
    fn merging_takes_the_union_range_and_both_task_sets() {
        let mut a = block(1, floating(9), floating(11));
        a.tasks.insert(EntityRef::new(EntityKind::Task, [1u8; 16]));
        let mut b = block(2, floating(10), floating(13));
        b.tasks.insert(EntityRef::new(EntityKind::Task, [2u8; 16]));

        let merged = merge_blocks(&a, &b, &utc()).expect("merge");
        assert_eq!(merged.starts_at, floating(9));
        assert_eq!(merged.ends_at, floating(13));
        assert_eq!(merged.tasks.len(), 2);
        assert_eq!(merged.title.as_deref(), Some("Block 1 + Block 2"));
        assert!(!merged.title_track_task);
    }

    /// Argument order must not change the answer: the earlier block is decided
    /// by its start, not by which side of the call it is on.
    #[test]
    fn merging_is_order_independent() {
        let a = block(1, floating(9), floating(11));
        let b = block(2, floating(10), floating(13));
        let one = merge_blocks(&a, &b, &utc()).expect("merge");
        let other = merge_blocks(&b, &a, &utc()).expect("merge");
        assert_eq!(one.starts_at, other.starts_at);
        assert_eq!(one.ends_at, other.ends_at);
        assert_eq!(one.title, other.title);
    }

    /// The rule that keeps the four kinds meaningful: two agreeing kinds stay
    /// themselves, so a merge of two 09:00 floating blocks still moves when
    /// the device does.
    #[test]
    fn two_floating_blocks_merge_into_a_floating_block() {
        let merged = merge_blocks(
            &block(1, floating(9), floating(11)),
            &block(2, floating(10), floating(13)),
            &utc(),
        )
        .expect("merge");
        assert!(matches!(merged.starts_at, SunriseTime::Floating { .. }));
    }

    /// And the rule that stops one silently re-anchoring the other: a floating
    /// bound and a fixed instant cannot both survive, so the union is an
    /// instant rather than a floating value that has quietly changed meaning.
    #[test]
    fn mixing_kinds_resolves_the_union_to_an_instant() {
        let fixed = Timestamp::from_millisecond(floating(10).index_ms()).expect("instant");
        let merged = merge_blocks(
            &block(1, floating(9), floating(11)),
            &block(2, SunriseTime::instant(fixed), floating(13)),
            &utc(),
        )
        .expect("merge");
        assert!(matches!(merged.starts_at, SunriseTime::Instant { .. }));
        // The end bounds agree on their kind, so that half keeps it.
        assert!(matches!(merged.ends_at, SunriseTime::Floating { .. }));
    }

    /// Two zoned bounds in *different* zones are not the same kind of
    /// commitment either, whatever their wall clocks say.
    #[test]
    fn zoned_bounds_in_different_zones_resolve_to_an_instant() {
        let civil_at = civil::date(2026, 3, 4).at(9, 0, 0, 0);
        let merged = merge_blocks(
            &block(
                1,
                SunriseTime::zoned(civil_at, "America/New_York"),
                SunriseTime::zoned(civil::date(2026, 3, 4).at(11, 0, 0, 0), "America/New_York"),
            ),
            &block(
                2,
                SunriseTime::zoned(civil_at, "Europe/London"),
                SunriseTime::zoned(civil::date(2026, 3, 4).at(11, 0, 0, 0), "Europe/London"),
            ),
            &utc(),
        )
        .expect("merge");
        assert!(matches!(merged.starts_at, SunriseTime::Instant { .. }));
    }

    #[test]
    fn identical_titles_are_not_doubled_and_a_missing_one_is_not_invented() {
        let mut a = block(1, floating(9), floating(11));
        let mut b = block(2, floating(10), floating(13));
        a.title = Some("Deep work".into());
        b.title = Some("Deep work".into());
        assert_eq!(
            merge_blocks(&a, &b, &utc())
                .expect("merge")
                .title
                .as_deref(),
            Some("Deep work")
        );

        b.title = None;
        assert_eq!(
            merge_blocks(&a, &b, &utc())
                .expect("merge")
                .title
                .as_deref(),
            Some("Deep work")
        );

        a.title = None;
        assert_eq!(merge_blocks(&a, &b, &utc()).expect("merge").title, None);
    }

    /// The one way a merge can fail: a multi-task Block has no single task
    /// title to shadow, so it needs an explicit one and neither side had it.
    #[test]
    fn a_titleless_multi_task_merge_is_refused_rather_than_written() {
        let mut a = block(1, floating(9), floating(11));
        a.title = None;
        a.tasks.insert(EntityRef::new(EntityKind::Task, [1u8; 16]));
        let mut b = block(2, floating(10), floating(13));
        b.title = None;
        b.tasks.insert(EntityRef::new(EntityKind::Task, [2u8; 16]));

        assert!(merge_blocks(&a, &b, &utc()).is_err());
    }
}
