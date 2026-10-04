//! Block conflicts per `docs/02-domain/time-blocks.md` §Conflicts: which live
//! Blocks overlap, and the draft the Resolve menu's **Merge** action creates.

use super::{Block, BlockDraft};
use crate::time::{SunriseTime, UNANCHORED_MS};
use crate::validation::ValidationError;
use jiff::tz::TimeZone;
use sunrise_id::EntityRef;

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
/// Tombstoned blocks are skipped, and so is a block with a bound this build
/// cannot place: an unknown kind with no `at` instant, or the
/// [`UNANCHORED_MS`] instant a client mirror shows that kind as. Quadratic in
/// the number of blocks on screen, which is a day or a week of them.
#[must_use]
pub fn overlaps(blocks: &[Block]) -> Vec<BlockOverlap> {
    let mut live: Vec<(&Block, i64, i64)> = blocks
        .iter()
        .filter(|b| !b.deleted)
        .filter_map(|b| Some((b, placed_ms(&b.starts_at)?, placed_ms(&b.ends_at)?)))
        .collect();
    live.sort_by_key(|(b, starts, _)| (*starts, b.id));

    let mut out = Vec::new();
    for (i, (a, a_start, a_end)) in live.iter().enumerate() {
        for (b, b_start, b_end) in &live[i + 1..] {
            let from_ms = *a_start.max(b_start);
            let to_ms = *a_end.min(b_end);
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

/// A bound's position for comparing two blocks, or `None` where this build
/// cannot place it: an unknown kind with no `at` anchor, or the
/// [`UNANCHORED_MS`] instant a client mirror shows that kind as. Such a bound
/// is compared with nothing, as in [`super::validate_range`], because its
/// stand-in key would invent a conflict with every later block.
fn placed_ms(t: &SunriseTime) -> Option<i64> {
    t.index_key().filter(|ms| *ms < UNANCHORED_MS)
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

/// The wider of two bounds, keeping the kind when both agree on it. A bound
/// this build cannot place ([`placed_ms`]) loses to one it can.
fn union_bound(x: &SunriseTime, y: &SunriseTime, zone: &TimeZone, bound: Bound) -> SunriseTime {
    let take_x = match (placed_ms(x), placed_ms(y), bound) {
        (Some(xm), Some(ym), Bound::Start) => xm <= ym,
        (Some(xm), Some(ym), Bound::End) => xm >= ym,
        (None, Some(_), _) => false,
        (_, None, _) => true,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unknown::Unknowns;
    use jiff::civil;
    use jiff::Timestamp;
    use std::collections::BTreeSet;
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

    /// An unplaceable bound, and the far-future instant a client mirror shows
    /// it as, conflicts with nothing and loses a merge's union to a real bound.
    #[test]
    fn an_unplaceable_bound_invents_no_conflict() {
        let lunar = SunriseTime::Unknown {
            kind: "lunar".into(),
            raw: Unknowns::new(),
        };
        let far = SunriseTime::instant(Timestamp::from_millisecond(UNANCHORED_MS).unwrap());
        for end in [lunar, far] {
            let (a, b) = (
                block(1, floating(9), end.clone()),
                block(2, floating(10), floating(11)),
            );
            assert!(overlaps(&[a.clone(), b.clone(), block(3, end, floating(12))]).is_empty());
            let merged = merge_blocks(&a, &b, &utc()).expect("merge");
            assert_eq!(merged.ends_at.index_ms(), floating(11).index_ms());
        }
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
