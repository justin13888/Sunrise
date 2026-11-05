//! The grid exports: days, drags and layout cross the seam unchanged, and a
//! moved block keeps its kind.

use super::*;
use jiff::civil::date;
use jiff::Timestamp;
use sunrise_domain::{Block, SunriseTime, Unknowns};
use sunrise_id::EntityKind;

const TZ: &str = "America/Toronto";
const HOUR: i64 = 3_600_000;

fn row(n: u8, starts_at: SunriseTime, ends_at: SunriseTime) -> BlockGridRow {
    BlockGridRow {
        block: BlockItem::from(&Block {
            id: EntityRef::new(EntityKind::Block, [n; 16]),
            created_at: Timestamp::UNIX_EPOCH,
            updated_at: Timestamp::UNIX_EPOCH,
            stream_id: None,
            starts_at,
            ends_at,
            title: Some("Deep work".into()),
            title_track_task: false,
            tasks: std::collections::BTreeSet::new(),
            deleted: false,
            unknown: Unknowns::new(),
        }),
        title: Some("Deep work".into()),
        task_titles: Vec::new(),
    }
}

fn floating(day: i8, hour: i8) -> SunriseTime {
    SunriseTime::floating(date(2026, 3, day).at(hour, 0, 0, 0))
}

#[test]
fn the_spring_forward_day_is_23_hours_and_a_drag_stops_at_its_end() {
    // Noon on Sunday 2026-03-08 in Toronto.
    let days = calendar_days(
        1_772_985_600_000,
        CalendarGridSpan::Day,
        Weekday::Su,
        TZ.into(),
    )
    .unwrap();
    let day = days[0];
    assert_eq!(day.end_ms - day.start_ms, 23 * HOUR);
    let late = BlockBounds {
        start_ms: day.end_ms - HOUR,
        end_ms: day.end_ms,
    };
    let moved = drag_block_bounds(BlockDragGesture::Move, late, 3 * HOUR, 15, day);
    assert_eq!(moved, late);
    assert_eq!(
        calendar_step(day.start_ms, CalendarGridSpan::Day, 1, TZ.into()).unwrap(),
        day.end_ms
    );
}

#[test]
fn layout_and_a_moved_block_keep_to_the_readers_zone_and_the_blocks_kind() {
    let days = calendar_days(
        1_772_985_600_000,
        CalendarGridSpan::Day,
        Weekday::Su,
        TZ.into(),
    )
    .unwrap();
    let r = row(1, floating(8, 9), floating(8, 10));
    let placed = lay_out_blocks(vec![r.clone()], days[0], TZ.into()).unwrap();
    assert_eq!(placed.len(), 1);
    // 09:00 on the 23-hour day is eight real hours after midnight.
    assert_eq!(placed[0].start_ms - days[0].start_ms, 8 * HOUR);

    let edit = moved_block_edit(
        r.block,
        placed[0].start_ms + HOUR,
        placed[0].end_ms + HOUR,
        TZ.into(),
    )
    .unwrap();
    assert!(matches!(
        edit.starts_at,
        Some(TimeValue::Floating { civil }) if civil == date(2026, 3, 8).at(10, 0, 0, 0)
    ));
    assert!(edit.set_title.is_none() && !edit.clear_title);
}

#[test]
fn a_drawn_block_is_zoned_by_default_and_snapped() {
    let draft = grid_block_draft(
        1_772_985_600_000,
        1_772_985_600_000 + HOUR,
        TimeKind::Zoned,
        sunrise_domain::inbox_stream_ref(),
        None,
        Vec::new(),
        TZ.into(),
    )
    .unwrap();
    assert!(matches!(
        draft.starts_at,
        TimeValue::Zoned { ref tz, civil } if tz == TZ && civil == date(2026, 3, 8).at(12, 0, 0, 0)
    ));
    let day_start = 1_772_946_000_000;
    let drawn = drawn_block_bounds(day_start + 61_000, day_start, 15, day_start);
    assert_eq!(drawn.end_ms - drawn.start_ms, 15 * 60_000);
    assert_eq!(
        dropped_task_bounds(day_start, 15, day_start).end_ms,
        day_start + HOUR / 2
    );
    assert_eq!(
        snap_to_grid(day_start + 8 * 60_000, 15, day_start),
        day_start + 15 * 60_000
    );
}
