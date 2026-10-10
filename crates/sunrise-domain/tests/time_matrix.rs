//! The test matrix of `docs/10-cross-cutting/time.md` §8, for the rules this
//! crate owns: resolution and the one DST rule (§3), the comparisons a
//! validator, lateness and block overlap make in the reader's zone (§2), the
//! day a value belongs to (§4), and the 48-hour prefilter's bound.
//!
//! Every zone is named; nothing reads the host's. Routines (expansion, keys,
//! skips, streaks), constraints and the day schedule run their rows where
//! those rules live.

#![allow(clippy::unwrap_used)]

use jiff::civil::{date, Date, DateTime};
use jiff::tz::TimeZone;
use jiff::Timestamp;
use std::collections::BTreeSet;
use sunrise_domain::time::PREFILTER_SLACK_MS;
use sunrise_domain::{is_overdue, overlaps, Block, SunriseTime, TaskDraft, ValidationError};
use sunrise_id::{EntityKind, EntityRef};

fn tz(name: &str) -> TimeZone {
    TimeZone::get(name).unwrap()
}

/// The civil time `t` reads as in `zone`.
fn civil_in(t: Timestamp, zone: &str) -> DateTime {
    t.to_zoned(tz(zone)).datetime()
}

fn at(s: &str) -> Timestamp {
    s.parse().unwrap()
}

// ---------------------------------------------------------------------------
// §3: one DST rule, `Compatible`, everywhere.
// ---------------------------------------------------------------------------

/// A gap shifts forward by its length: 02:30 on New York's spring-forward
/// night is 03:30 EDT, and Lord Howe's 30-minute gap moves 02:10 to 02:40.
#[test]
fn a_time_in_a_gap_moves_forward_by_the_gap() {
    for (zone, civil, want) in [
        (
            "America/New_York",
            date(2026, 3, 8).at(2, 30, 0, 0),
            date(2026, 3, 8).at(3, 30, 0, 0),
        ),
        (
            "Australia/Lord_Howe",
            date(2026, 10, 4).at(2, 10, 0, 0),
            date(2026, 10, 4).at(2, 40, 0, 0),
        ),
    ] {
        let got = SunriseTime::floating(civil).resolve_in(&tz(zone)).unwrap();
        assert_eq!(civil_in(got, zone), want, "{zone}");
        // The zoned kind follows the same rule in its own zone.
        let zoned = SunriseTime::zoned(civil, zone)
            .resolve_in(&TimeZone::UTC)
            .unwrap();
        assert_eq!(zoned, got, "{zone}");
    }
}

/// A fold takes the earlier instant: the one with the pre-transition offset.
#[test]
fn a_time_in_a_fold_takes_the_earlier_instant() {
    for (zone, civil, want_utc) in [
        // 01:30 EDT (UTC−4), not 01:30 EST.
        (
            "America/New_York",
            date(2026, 11, 1).at(1, 30, 0, 0),
            "2026-11-01T05:30:00Z",
        ),
        // 01:45 LHDT (UTC+11), not 01:45 LHST (UTC+10:30).
        (
            "Australia/Lord_Howe",
            date(2026, 4, 5).at(1, 45, 0, 0),
            "2026-04-04T14:45:00Z",
        ),
    ] {
        let got = SunriseTime::floating(civil).resolve_in(&tz(zone)).unwrap();
        assert_eq!(got, at(want_utc), "{zone}");
    }
}

/// Pacific/Apia skipped 2011-12-30 moving across the date line. An all-day
/// value on it resolves to the next date that exists, so its day has zero
/// length; it still names the date it was written for.
#[test]
fn a_date_the_date_line_skipped_is_a_day_of_zero_length() {
    let apia = tz("Pacific/Apia");
    let skipped = SunriseTime::all_day(date(2011, 12, 30));
    let start = skipped.resolve_in(&apia).unwrap();
    assert_eq!(
        start,
        SunriseTime::all_day(date(2011, 12, 31))
            .resolve_in(&apia)
            .unwrap()
    );
    assert_eq!(
        skipped.due_in(&apia).unwrap(),
        start,
        "it ends where it starts"
    );
    assert_eq!(skipped.day_in(&apia), Some(date(2011, 12, 30)));
    // A wall-clock time on that date lands on the same time a day later.
    let nine = SunriseTime::floating(date(2011, 12, 30).at(9, 0, 0, 0));
    assert_eq!(
        civil_in(nine.resolve_in(&apia).unwrap(), "Pacific/Apia"),
        date(2011, 12, 31).at(9, 0, 0, 0)
    );
}

// ---------------------------------------------------------------------------
// §2: the index is a key, the reader's zone decides.
// ---------------------------------------------------------------------------

/// The identity case: in UTC the storage index IS the resolved instant, for
/// every kind. Everywhere else it is not, which is the whole defect.
#[test]
fn in_utc_the_index_is_the_resolved_instant_for_every_kind() {
    let civil = date(2026, 6, 1).at(9, 0, 0, 0);
    for t in [
        SunriseTime::instant(at("2026-06-01T09:00:00Z")),
        SunriseTime::zoned(civil, "Asia/Kolkata"),
        SunriseTime::floating(civil),
        SunriseTime::all_day(date(2026, 6, 1)),
    ] {
        assert_eq!(
            t.index_key(),
            t.resolve_in(&TimeZone::UTC).map(Timestamp::as_millisecond)
        );
    }
}

/// A +05:30 zone with no DST: a floating 09:00 is 09:00 IST read in Kolkata
/// and 09:00 UTC read in UTC, five and a half hours apart.
#[test]
fn a_floating_value_resolves_where_it_is_read() {
    let nine = SunriseTime::floating(date(2026, 6, 1).at(9, 0, 0, 0));
    let kolkata = nine.resolve_in(&tz("Asia/Kolkata")).unwrap();
    let utc = nine.resolve_in(&TimeZone::UTC).unwrap();
    assert_eq!(kolkata, at("2026-06-01T03:30:00Z"));
    assert_eq!(
        utc.as_millisecond() - kolkata.as_millisecond(),
        5 * 3_600_000 + 1_800_000
    );
}

/// The prefilter's bound holds at the extremes: every row the exact filter
/// keeps is within `PREFILTER_SLACK_MS` of its index key, for a start and for
/// a deadline's end of day, at UTC+14 and UTC−10 and in between.
#[test]
fn the_prefilter_admits_every_row_the_exact_filter_keeps() {
    let values = [
        SunriseTime::floating(date(2026, 3, 8).at(0, 0, 0, 0)),
        SunriseTime::floating(date(2026, 3, 8).at(23, 59, 0, 0)),
        SunriseTime::all_day(date(2026, 3, 8)),
        SunriseTime::all_day(date(2026, 12, 31)),
        SunriseTime::zoned(date(2026, 3, 8).at(2, 30, 0, 0), "America/New_York"),
    ];
    for zone in [
        "Pacific/Kiritimati",
        "Pacific/Honolulu",
        "Pacific/Apia",
        "Pacific/Pago_Pago",
        "Asia/Kolkata",
        "Australia/Lord_Howe",
        "America/New_York",
        "UTC",
    ] {
        let z = tz(zone);
        for v in &values {
            let key = v.index_key().unwrap();
            for resolved in [v.resolve_in(&z), v.due_in(&z)] {
                let ms = resolved.unwrap().as_millisecond();
                assert!(
                    (ms - key).abs() <= PREFILTER_SLACK_MS,
                    "{v} in {zone} is {}h from its key",
                    (ms - key) / 3_600_000
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tasks: validation and lateness.
// ---------------------------------------------------------------------------

fn draft(scheduled_at: SunriseTime, due_at: SunriseTime) -> TaskDraft {
    TaskDraft {
        title: "t".into(),
        scheduled_at: Some(scheduled_at),
        due_at: Some(due_at),
        ..Default::default()
    }
}

/// A plan at 01:30 and a deadline at 01:40 on New York's fall-back night are
/// both the first pass through the fold, so the plan is before the deadline.
/// On a spring-forward night a floating 02:30 plan (03:30 EDT) is after a
/// zoned 03:15 deadline, and refused.
#[test]
fn due_after_scheduled_holds_across_a_fold_and_a_gap() {
    let ny = tz("America/New_York");
    let fold = date(2026, 11, 1);
    draft(
        SunriseTime::floating(fold.at(1, 30, 0, 0)),
        SunriseTime::floating(fold.at(1, 40, 0, 0)),
    )
    .validate(&ny)
    .unwrap();
    let gap = date(2026, 3, 8);
    assert_eq!(
        draft(
            SunriseTime::floating(gap.at(2, 30, 0, 0)),
            SunriseTime::zoned(gap.at(3, 15, 0, 0), "America/New_York"),
        )
        .validate(&ny),
        Err(ValidationError::DueBeforeScheduled)
    );
}

/// `Pacific/Apia` and `Pacific/Pago_Pago` share a longitude and sit 24 hours
/// apart. At one instant an all-day deadline on 2 June is late in Apia,
/// where it is already the 3rd, and on track in Pago Pago, where it is the
/// 2nd. Both answers are the reader's.
#[test]
fn an_all_day_deadline_is_late_in_one_zone_and_on_track_in_the_other() {
    let now = u64::try_from(at("2026-06-02T12:00:00Z").as_millisecond()).unwrap();
    let due = SunriseTime::all_day(date(2026, 6, 2));
    assert!(is_overdue(&due, now, &tz("Pacific/Apia")));
    assert!(!is_overdue(&due, now, &tz("Pacific/Pago_Pago")));
}

/// New York to Kolkata mid-day: a floating 09:00 task moves to 09:00 IST, a
/// zoned 09:00 New York one stays at its instant, and nothing stored changes.
#[test]
fn travelling_moves_the_floating_task_and_not_the_zoned_one() {
    let day = date(2026, 6, 1);
    let floating = SunriseTime::floating(day.at(9, 0, 0, 0));
    let zoned = SunriseTime::zoned(day.at(9, 0, 0, 0), "America/New_York");
    let (ny, kolkata) = (tz("America/New_York"), tz("Asia/Kolkata"));
    assert_eq!(
        civil_in(floating.resolve_in(&kolkata).unwrap(), "Asia/Kolkata"),
        day.at(9, 0, 0, 0)
    );
    assert_ne!(floating.resolve_in(&ny), floating.resolve_in(&kolkata));
    assert_eq!(zoned.resolve_in(&ny), zoned.resolve_in(&kolkata));
    // Which day each belongs to is the reader's too: 09:00 New York is the
    // evening of the same date in Kolkata.
    assert_eq!(zoned.day_in(&kolkata), Some(day));
    assert_eq!(floating.day_in(&kolkata), Some(day));
}

// ---------------------------------------------------------------------------
// Blocks: overlap in the reader's zone.
// ---------------------------------------------------------------------------

fn block(n: u8, starts_at: SunriseTime, ends_at: SunriseTime) -> Block {
    Block {
        id: EntityRef::new(EntityKind::Block, [n; 16]),
        created_at: Timestamp::UNIX_EPOCH,
        updated_at: Timestamp::UNIX_EPOCH,
        stream_id: EntityRef::new(EntityKind::Stream, [1u8; 16]),
        starts_at,
        ends_at,
        title: None,
        title_track_task: false,
        tasks: BTreeSet::new(),
        deleted: false,
        unknown: sunrise_domain::Unknowns::new(),
    }
}

fn floating(d: Date, h: i8, m: i8) -> SunriseTime {
    SunriseTime::floating(d.at(h, m, 0, 0))
}

/// On New York's spring-forward night a floating 01:30–02:30 block ends at
/// 03:30 EDT, so it collides with a 03:00–04:00 block there. Read in UTC the
/// two are half an hour apart.
#[test]
fn a_block_ending_in_a_gap_overlaps_what_follows_the_gap() {
    let d = date(2026, 3, 8);
    let blocks = [
        block(1, floating(d, 1, 30), floating(d, 2, 30)),
        block(2, floating(d, 3, 0), floating(d, 4, 0)),
    ];
    let found = overlaps(&blocks, &tz("America/New_York"));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].to_ms - found[0].from_ms, 30 * 60_000);
    assert!(overlaps(&blocks, &TimeZone::UTC).is_empty());
}

/// Across the fold, 01:00–01:59 floating and a zoned 01:30 New York block
/// resolve to the same first pass, so they overlap; nothing is counted twice.
#[test]
fn blocks_inside_a_fold_overlap_once() {
    let d = date(2026, 11, 1);
    let ny = "America/New_York";
    let blocks = [
        block(1, floating(d, 1, 0), floating(d, 1, 59)),
        block(
            2,
            SunriseTime::zoned(d.at(1, 30, 0, 0), ny),
            SunriseTime::zoned(d.at(1, 45, 0, 0), ny),
        ),
    ];
    let found = overlaps(&blocks, &tz(ny));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].to_ms - found[0].from_ms, 15 * 60_000);
}
