//! The grid's days, layout, snapping and drags, ported from the Apple app's
//! `BlockLayoutTests`, `BlockDragTests` and `CalendarModelTests`, plus the
//! DST days those could not see.

use super::*;
use crate::civil::reader_zone;
use jiff::civil::date;
use jiff::Timestamp;
use sunrise_domain::{SunriseTime, Unknowns};
use sunrise_id::EntityKind;

const HOUR: i64 = 3_600_000;
const MINUTE: i64 = 60_000;
/// 2026-03-04T00:00:00Z.
const DAY_START: i64 = 1_772_582_400_000;

fn utc() -> TimeZone {
    reader_zone("UTC").unwrap()
}

fn toronto() -> TimeZone {
    reader_zone("America/Toronto").unwrap()
}

fn utc_day() -> CalendarDay {
    CalendarDay {
        date: date(2026, 3, 4),
        start_ms: DAY_START,
        end_ms: DAY_START + 24 * HOUR,
    }
}

fn at(hour: i64, minute: i64) -> i64 {
    DAY_START + hour * HOUR + minute * MINUTE
}

fn id(n: u8) -> EntityRef {
    EntityRef::new(EntityKind::Block, [n; 16])
}

fn block_between(n: u8, starts_at: SunriseTime, ends_at: SunriseTime) -> Block {
    Block {
        id: id(n),
        created_at: Timestamp::UNIX_EPOCH,
        updated_at: Timestamp::UNIX_EPOCH,
        stream_id: None,
        starts_at,
        ends_at,
        title: Some(format!("block {n}")),
        title_track_task: false,
        tasks: std::collections::BTreeSet::new(),
        deleted: false,
        unknown: Unknowns::new(),
    }
}

fn block(n: u8, from_ms: i64, to_ms: i64) -> Block {
    let t = |ms| SunriseTime::instant(Timestamp::from_millisecond(ms).unwrap());
    block_between(n, t(from_ms), t(to_ms))
}

fn place(blocks: &[Block]) -> Vec<PlacedBlock> {
    place_blocks(blocks, &utc_day(), &utc())
}

#[test]
fn a_block_inside_the_day_keeps_its_own_times() {
    let placed = place(&[block(1, at(9, 0), at(10, 0))]);
    assert_eq!(placed.len(), 1);
    assert_eq!(
        (placed[0].start_ms, placed[0].end_ms),
        (at(9, 0), at(10, 0))
    );
    assert_eq!(placed[0].lane_count, 1);
}

/// `Query::DayBlocks` returns anything overlapping the day, so a block that
/// runs in from yesterday or out past midnight is clamped to the column.
#[test]
fn a_block_crossing_midnight_is_clamped_and_one_ending_at_it_is_not_on_the_day() {
    let placed = place(&[
        block(1, at(-3, 0), at(2, 0)),
        block(2, at(22, 0), at(26, 0)),
    ]);
    assert_eq!(
        (placed[0].start_ms, placed[0].end_ms),
        (DAY_START, at(2, 0))
    );
    assert_eq!(placed[1].end_ms, DAY_START + 24 * HOUR);
    assert!(place(&[block(1, at(-2, 0), DAY_START)]).is_empty());
}

#[test]
fn overlapping_blocks_share_the_width_and_back_to_back_ones_do_not() {
    let placed = place(&[
        block(1, at(9, 0), at(11, 0)),
        block(2, at(10, 0), at(12, 0)),
    ]);
    assert!(placed.iter().all(|p| p.lane_count == 2));
    assert_ne!(placed[0].lane, placed[1].lane);

    let placed = place(&[
        block(1, at(9, 0), at(10, 0)),
        block(2, at(10, 0), at(11, 0)),
    ]);
    assert!(placed.iter().all(|p| p.lane_count == 1));
}

/// A overlaps B and B overlaps C, but A and C do not touch: all three share
/// the width, and C reuses A's column.
#[test]
fn transitively_overlapping_blocks_all_share_the_width() {
    let placed = place(&[
        block(1, at(9, 0), at(11, 0)),
        block(2, at(10, 0), at(13, 0)),
        block(3, at(12, 0), at(14, 0)),
    ]);
    assert!(placed.iter().all(|p| p.lane_count == 2));
    let lane = |n| placed.iter().find(|p| p.id == id(n)).unwrap().lane;
    assert_ne!(lane(1), lane(2));
    assert_eq!(lane(3), lane(1));
}

#[test]
fn placement_is_by_start_then_id_and_skips_what_cannot_be_placed() {
    let unknown = SunriseTime::Unknown {
        kind: "lunar".into(),
        raw: Unknowns::default(),
    };
    let lost = block_between(9, unknown.clone(), unknown);
    let placed = place(&[
        block(2, at(9, 0), at(10, 0)),
        lost,
        block(1, at(9, 0), at(10, 0)),
    ]);
    assert_eq!(
        placed.iter().map(|p| p.id).collect::<Vec<_>>(),
        [id(1), id(2)]
    );
}

/// A floating block is drawn where the reader is: 09:00 floating sits at
/// 09:00 on a Toronto column.
#[test]
fn a_floating_block_is_placed_in_the_readers_zone() {
    let tz = toronto();
    let days = calendar_days(DAY_START + 17 * HOUR, CalendarSpan::Day, &Weekday::Su, &tz).unwrap();
    let nine = block_between(
        1,
        SunriseTime::floating(date(2026, 3, 4).at(9, 0, 0, 0)),
        SunriseTime::floating(date(2026, 3, 4).at(10, 0, 0, 0)),
    );
    let placed = place_blocks(&[nine], &days[0], &tz);
    assert_eq!(placed[0].start_ms - days[0].start_ms, 9 * HOUR);
}

#[test]
fn snapping_rounds_to_the_nearest_slot_of_the_day() {
    assert_eq!(snap(at(9, 7), 15, DAY_START), at(9, 0));
    assert_eq!(snap(at(9, 8), 15, DAY_START), at(9, 15));
    assert_eq!(snap(at(9, 20), 30, DAY_START), at(9, 30));
    assert_eq!(snap(at(9, 4), 5, DAY_START), at(9, 5));
    assert_eq!(
        snap(at(9, 4), 0, DAY_START),
        at(9, 4),
        "a step under a minute is one"
    );
    // India is UTC+5:30; snapping from the epoch would offer :00 and :45.
    let half_hour_zone = DAY_START - 30 * MINUTE;
    assert_eq!(
        snap(half_hour_zone + 9 * HOUR + MINUTE, 15, half_hour_zone),
        half_hour_zone + 9 * HOUR
    );
}

#[test]
fn a_drawn_range_is_ordered_snapped_and_at_least_one_step() {
    assert_eq!(
        drawn_range(at(11, 2), at(9, 58), 15, DAY_START),
        GridRange {
            start_ms: at(10, 0),
            end_ms: at(11, 0)
        }
    );
    assert_eq!(
        drawn_range(at(9, 1), at(9, 3), 15, DAY_START),
        GridRange {
            start_ms: at(9, 0),
            end_ms: at(9, 15)
        }
    );
    assert_eq!(
        dropped_range(at(9, 6), 15, DAY_START),
        GridRange {
            start_ms: at(9, 0),
            end_ms: at(9, 30)
        }
    );
    assert_eq!(dropped_range(at(9, 0), 60, DAY_START).end_ms, at(10, 0));
}

fn drag(mode: DragMode, start: i64, end: i64, delta: i64, minutes: i64) -> GridRange {
    dragged_range(
        mode,
        GridRange {
            start_ms: start,
            end_ms: end,
        },
        delta,
        minutes,
        &utc_day(),
    )
}

#[test]
fn a_move_keeps_the_length_and_snaps() {
    let moved = drag(DragMode::Move, at(9, 0), at(10, 0), HOUR, 15);
    assert_eq!((moved.start_ms, moved.end_ms), (at(10, 0), at(11, 0)));
    assert_eq!(
        drag(DragMode::Move, at(9, 0), at(10, 0), 8 * MINUTE, 15).start_ms,
        at(9, 15)
    );
    assert_eq!(
        drag(DragMode::Move, at(9, 0), at(10, 0), 8 * MINUTE, 5).start_ms,
        at(9, 10)
    );
    let still = drag(DragMode::Move, at(9, 0), at(10, 0), 0, 15);
    assert_eq!((still.start_ms, still.end_ms), (at(9, 0), at(10, 0)));
}

#[test]
fn a_move_stops_at_either_midnight_without_shrinking() {
    let down = drag(DragMode::Move, at(23, 0), at(24, 0), 5 * HOUR, 15);
    assert_eq!(down.end_ms, DAY_START + 24 * HOUR);
    assert_eq!(down.end_ms - down.start_ms, HOUR);
    let up = drag(DragMode::Move, at(0, 30), at(1, 30), -5 * HOUR, 15);
    assert_eq!((up.start_ms, up.end_ms - up.start_ms), (DAY_START, HOUR));
}

#[test]
fn a_resize_holds_the_start_and_keeps_one_step_and_the_day() {
    let stretched = drag(DragMode::ResizeEnd, at(9, 0), at(10, 0), 30 * MINUTE, 15);
    assert_eq!(
        (stretched.start_ms, stretched.end_ms),
        (at(9, 0), at(10, 30))
    );
    let squashed = drag(DragMode::ResizeEnd, at(9, 0), at(10, 0), -5 * HOUR, 15);
    assert_eq!(squashed.end_ms, at(9, 15));
    let late = drag(DragMode::ResizeEnd, at(23, 0), at(23, 30), 5 * HOUR, 15);
    assert_eq!(late.end_ms, DAY_START + 24 * HOUR);
}

/// The spring-forward day is 23 hours long, and a drag stops at its real
/// end: the Apple grid's `start + 24 h` let a block run an hour into the
/// next day.
#[test]
fn the_dst_days_are_23_and_25_hours_and_a_drag_stops_at_the_real_midnight() {
    let tz = toronto();
    // Noon on Sunday 2026-03-08 and Sunday 2026-11-01 in Toronto.
    let spring = calendar_days(1_772_985_600_000, CalendarSpan::Day, &Weekday::Su, &tz).unwrap();
    let fall = calendar_days(1_793_552_400_000, CalendarSpan::Day, &Weekday::Su, &tz).unwrap();
    assert_eq!(spring[0].date, date(2026, 3, 8));
    assert_eq!(spring[0].end_ms - spring[0].start_ms, 23 * HOUR);
    assert_eq!(fall[0].date, date(2026, 11, 1));
    assert_eq!(fall[0].end_ms - fall[0].start_ms, 25 * HOUR);

    let late = GridRange {
        start_ms: spring[0].end_ms - HOUR,
        end_ms: spring[0].end_ms,
    };
    let moved = dragged_range(DragMode::Move, late, 2 * HOUR, 15, &spring[0]);
    assert_eq!(moved.end_ms, spring[0].end_ms);
}

#[test]
fn a_week_starts_on_the_preference_and_steps_by_civil_days() {
    let tz = toronto();
    // Wednesday 2026-03-04, 12:00 in Toronto.
    let wed_noon = DAY_START + 17 * HOUR;
    let sunday_first = calendar_days(wed_noon, CalendarSpan::Week, &Weekday::Su, &tz).unwrap();
    assert_eq!(sunday_first.len(), 7);
    assert_eq!(sunday_first[0].date, date(2026, 3, 1));
    assert_eq!(sunday_first[6].date, date(2026, 3, 7));
    let monday_first = calendar_days(wed_noon, CalendarSpan::Week, &Weekday::Mo, &tz).unwrap();
    assert_eq!(monday_first[0].date, date(2026, 3, 2));
    // The week holding spring-forward is an hour short of 168.
    let span = monday_first[6].end_ms - monday_first[0].start_ms;
    assert_eq!(span, 7 * 24 * HOUR - HOUR);

    let next = step_anchor(wed_noon, CalendarSpan::Week, 1, &tz).unwrap();
    assert_eq!(
        next - wed_noon,
        7 * 24 * HOUR - HOUR,
        "noon a week on, not 168 hours"
    );
    let back = step_anchor(next, CalendarSpan::Day, -1, &tz).unwrap();
    let back_day = Timestamp::from_millisecond(back)
        .unwrap()
        .to_zoned(tz)
        .datetime();
    assert_eq!(back_day, date(2026, 3, 10).at(12, 0, 0, 0));
}

#[test]
fn a_new_block_takes_the_kind_asked_for_at_both_ends() {
    let tz = toronto();
    let draft = new_block_draft(
        DAY_START + 14 * HOUR,
        DAY_START + 15 * HOUR,
        NEW_BLOCK_KIND,
        None,
        Some("  ".into()),
        vec![],
        &tz,
    )
    .unwrap();
    assert_eq!(
        draft.starts_at,
        SunriseTime::zoned(date(2026, 3, 4).at(9, 0, 0, 0), "America/Toronto")
    );
    assert_eq!(
        draft.ends_at,
        SunriseTime::zoned(date(2026, 3, 4).at(10, 0, 0, 0), "America/Toronto")
    );
    assert_eq!(draft.title, None, "a blank title lets the core shadow-copy");
}

/// Dragging a block keeps its kind; the Apple model's `sameKind` rule, now
/// sending only the bound that moved.
#[test]
fn a_dragged_block_keeps_its_kind_and_sends_only_what_moved() {
    let tz = toronto();
    let floating = block_between(
        1,
        SunriseTime::floating(date(2026, 3, 4).at(9, 0, 0, 0)),
        SunriseTime::floating(date(2026, 3, 4).at(10, 0, 0, 0)),
    );
    let nine = DAY_START + 14 * HOUR;
    let patch = moved_block_patch(&floating, nine, nine + 2 * HOUR, &tz).unwrap();
    assert!(patch.starts_at.is_none());
    assert_eq!(
        patch.ends_at,
        Some(SunriseTime::floating(date(2026, 3, 4).at(11, 0, 0, 0)))
    );
    assert!(patch.title.is_none() && patch.title_track_task.is_none());

    let moved = moved_block_patch(&floating, nine + HOUR, nine + 2 * HOUR, &tz).unwrap();
    assert_eq!(
        moved.starts_at,
        Some(SunriseTime::floating(date(2026, 3, 4).at(10, 0, 0, 0)))
    );
    assert!(moved_block_patch(&floating, nine, nine, &tz).is_err());
}

#[test]
fn a_merge_is_one_command() {
    let a = block(1, at(9, 0), at(11, 0));
    let b = block(2, at(10, 0), at(13, 0));
    assert!(matches!(
        merge_command(&a, &b, &utc()).unwrap(),
        Command::MergeBlocks { a: x, b: y } if x == a.id && y == b.id
    ));
}

/// Two untitled blocks over two tasks each have no single title to shadow:
/// the union is refused before anything is written.
#[test]
fn a_merge_that_would_not_validate_writes_nothing() {
    let mut a = block(1, at(9, 0), at(11, 0));
    let mut b = block(2, at(10, 0), at(13, 0));
    a.title = None;
    b.title = None;
    a.tasks.insert(EntityRef::new(EntityKind::Task, [5; 16]));
    b.tasks.insert(EntityRef::new(EntityKind::Task, [6; 16]));
    assert!(matches!(
        merge_command(&a, &b, &utc()),
        Err(MergeError::Invalid(_))
    ));
}
