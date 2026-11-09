use super::*;
use crate::split_routine_id;
use jiff::civil;

const LA: &str = "America/Los_Angeles";

fn la() -> TimeZone {
    TimeZone::get(LA).unwrap()
}

fn at(y: i16, mo: i8, d: i8, h: i8, mi: i8) -> DateTime {
    civil::date(y, mo, d).at(h, mi, 0, 0)
}

/// The instant wall clock `civil` names in `tz`, by the one DST rule.
fn instant(civil: DateTime, tz: &TimeZone) -> Timestamp {
    tz.to_ambiguous_zoned(civil)
        .compatible()
        .unwrap()
        .timestamp()
}

fn series(start: SunriseTime, end: SunriseTime, rule: &str) -> BlockSeries {
    BlockSeries {
        starts_at: start,
        ends_at: end,
        rrule: RRule::parse(rule).unwrap(),
        exceptions: BTreeMap::new(),
    }
}

/// Deep work, Tuesdays 09:00–11:00 wherever the reader is.
fn deep_work() -> BlockSeries {
    series(
        SunriseTime::floating(at(2026, 3, 3, 9, 0)),
        SunriseTime::floating(at(2026, 3, 3, 11, 0)),
        "FREQ=WEEKLY;BYDAY=TU",
    )
}

/// `[from, to)` as the instants of two wall clocks in `tz`.
fn window(from: DateTime, to: DateTime, tz: &TimeZone) -> (Timestamp, Timestamp) {
    (instant(from, tz), instant(to, tz))
}

fn keys(occ: &[BlockOccurrence]) -> Vec<&str> {
    occ.iter().map(|o| o.key.as_str()).collect()
}

#[test]
fn a_weekly_block_yields_each_week_at_its_wall_clock() {
    let tz = TimeZone::UTC;
    let occ = expand_block(
        &deep_work(),
        window(at(2026, 3, 1, 0, 0), at(2026, 3, 22, 0, 0), &tz),
        &tz,
    )
    .unwrap();
    assert_eq!(
        keys(&occ),
        ["2026-03-03T09:00", "2026-03-10T09:00", "2026-03-17T09:00"]
    );
    assert_eq!(
        occ[1].starts_at,
        SunriseTime::floating(at(2026, 3, 10, 9, 0))
    );
    assert_eq!(
        occ[1].ends_at,
        SunriseTime::floating(at(2026, 3, 10, 11, 0))
    );
    assert_eq!(occ[1].exception, None);
}

/// A floating series follows the reader: another zone moves every instant
/// and keeps every key.
#[test]
fn a_floating_series_keeps_its_keys_in_every_reader_zone() {
    let utc = TimeZone::UTC;
    let tokyo = TimeZone::get("Asia/Tokyo").unwrap();
    let win = |tz: &TimeZone| window(at(2026, 3, 1, 0, 0), at(2026, 3, 15, 0, 0), tz);
    let here = expand_block(&deep_work(), win(&utc), &utc).unwrap();
    let there = expand_block(&deep_work(), win(&tokyo), &tokyo).unwrap();
    assert_eq!(keys(&here), keys(&there));
    assert_ne!(
        here[0].starts_at.resolve_in(&utc),
        there[0].starts_at.resolve_in(&tokyo)
    );
}

/// A zoned 09:00–11:00 block stays 09:00–11:00 across spring-forward: its
/// instants move by the hour the clocks did, its keys and length do not.
#[test]
fn a_zoned_block_keeps_its_wall_clock_across_a_dst_change() {
    let s = series(
        SunriseTime::zoned(at(2025, 3, 7, 9, 0), LA),
        SunriseTime::zoned(at(2025, 3, 7, 11, 0), LA),
        "FREQ=DAILY",
    );
    let tz = la();
    let occ = expand_block(
        &s,
        window(at(2025, 3, 8, 0, 0), at(2025, 3, 10, 0, 0), &tz),
        &TimeZone::UTC,
    )
    .unwrap();
    assert_eq!(keys(&occ), ["2025-03-08T09:00", "2025-03-09T09:00"]);
    let utc_hour = |t: &SunriseTime| {
        t.resolve_in(&TimeZone::UTC)
            .unwrap()
            .to_zoned(TimeZone::UTC)
            .hour()
    };
    // PST is UTC-8, PDT is UTC-7.
    assert_eq!(utc_hour(&occ[0].starts_at), 17);
    assert_eq!(utc_hour(&occ[1].starts_at), 16);
    for o in &occ {
        let len = o
            .starts_at
            .resolve_in(&tz)
            .unwrap()
            .duration_until(o.ends_at.resolve_in(&tz).unwrap());
        assert_eq!(len, SignedDuration::from_hours(2));
    }
}

/// An occurrence that starts in a DST gap is keyed at its intended wall clock,
/// and the gap moves its start forward onto its end (time.md §3).
#[test]
fn an_occurrence_in_a_dst_gap_keeps_its_intended_key() {
    let s = series(
        SunriseTime::zoned(at(2025, 3, 8, 2, 30), LA),
        SunriseTime::zoned(at(2025, 3, 8, 3, 30), LA),
        "FREQ=DAILY",
    );
    let tz = la();
    let occ = expand_block(
        &s,
        window(at(2025, 3, 9, 0, 0), at(2025, 3, 10, 0, 0), &tz),
        &tz,
    )
    .unwrap();
    assert_eq!(keys(&occ), ["2025-03-09T02:30"]);
    assert_eq!(
        occ[0].starts_at,
        SunriseTime::zoned(at(2025, 3, 9, 2, 30), LA)
    );
    assert_eq!(
        occ[0].starts_at.resolve_in(&tz),
        Some(instant(at(2025, 3, 9, 3, 30), &tz))
    );
}

/// The query window is a day; a 23:00–01:00 block that started the evening
/// before still lands in it.
#[test]
fn an_occurrence_running_into_the_window_is_yielded() {
    let s = series(
        SunriseTime::floating(at(2026, 3, 1, 23, 0)),
        SunriseTime::floating(at(2026, 3, 2, 1, 0)),
        "FREQ=DAILY",
    );
    let tz = TimeZone::UTC;
    let occ = expand_block(
        &s,
        window(at(2026, 3, 5, 0, 0), at(2026, 3, 6, 0, 0), &tz),
        &tz,
    )
    .unwrap();
    assert_eq!(keys(&occ), ["2026-03-04T23:00", "2026-03-05T23:00"]);
    assert_eq!(occ[0].ends_at, SunriseTime::floating(at(2026, 3, 5, 1, 0)));
}

#[test]
fn count_and_until_end_the_series() {
    let tz = TimeZone::UTC;
    let win = window(at(2026, 3, 1, 0, 0), at(2026, 6, 1, 0, 0), &tz);
    let mut s = deep_work();
    s.rrule = RRule::parse("FREQ=WEEKLY;BYDAY=TU;COUNT=2").unwrap();
    assert_eq!(
        keys(&expand_block(&s, win, &tz).unwrap()),
        ["2026-03-03T09:00", "2026-03-10T09:00"]
    );
    s.rrule = RRule::parse("FREQ=WEEKLY;BYDAY=TU").unwrap();
    s.rrule.until = Some(SunriseTime::floating(at(2026, 3, 17, 9, 0)));
    assert_eq!(
        keys(&expand_block(&s, win, &tz).unwrap()),
        ["2026-03-03T09:00", "2026-03-10T09:00", "2026-03-17T09:00"]
    );
}

/// iCal `EXDATE`: a cancelled occurrence is gone and its neighbours are not.
#[test]
fn a_cancelled_occurrence_is_not_yielded() {
    let tz = TimeZone::UTC;
    let mut s = deep_work();
    s.except("2026-03-10T09:00", BlockException::cancelled())
        .unwrap();
    let occ = expand_block(
        &s,
        window(at(2026, 3, 1, 0, 0), at(2026, 3, 22, 0, 0), &tz),
        &tz,
    )
    .unwrap();
    assert_eq!(keys(&occ), ["2026-03-03T09:00", "2026-03-17T09:00"]);
}

#[test]
fn a_field_override_keeps_the_rules_time() {
    let tz = TimeZone::UTC;
    let mut s = deep_work();
    let x = BlockException {
        title: Some("Deep work: taxes".into()),
        location: Some("Library".into()),
        ..BlockException::default()
    };
    s.except("2026-03-10T09:00", x.clone()).unwrap();
    let occ = expand_block(
        &s,
        window(at(2026, 3, 10, 0, 0), at(2026, 3, 11, 0, 0), &tz),
        &tz,
    )
    .unwrap();
    assert_eq!(occ.len(), 1);
    assert_eq!(
        occ[0].starts_at,
        SunriseTime::floating(at(2026, 3, 10, 9, 0))
    );
    assert_eq!(occ[0].exception, Some(x));
}

/// A moved occurrence is where it moved to: out of its own day, and into a
/// day the rule never touches.
#[test]
fn a_moved_occurrence_is_yielded_where_it_moved() {
    let tz = TimeZone::UTC;
    let mut s = deep_work();
    s.except(
        "2026-03-10T09:00",
        BlockException {
            starts_at: Some(SunriseTime::floating(at(2026, 3, 12, 14, 0))),
            ends_at: Some(SunriseTime::floating(at(2026, 3, 12, 16, 0))),
            ..BlockException::default()
        },
    )
    .unwrap();
    let tuesday = window(at(2026, 3, 10, 0, 0), at(2026, 3, 11, 0, 0), &tz);
    assert!(expand_block(&s, tuesday, &tz).unwrap().is_empty());
    let thursday = window(at(2026, 3, 12, 0, 0), at(2026, 3, 13, 0, 0), &tz);
    let occ = expand_block(&s, thursday, &tz).unwrap();
    assert_eq!(keys(&occ), ["2026-03-10T09:00"]);
    assert_eq!(
        occ[0].starts_at,
        SunriseTime::floating(at(2026, 3, 12, 14, 0))
    );

    // Moving only the start keeps the rule's end.
    s.except(
        "2026-03-17T09:00",
        BlockException {
            starts_at: Some(SunriseTime::floating(at(2026, 3, 17, 10, 0))),
            ..BlockException::default()
        },
    )
    .unwrap();
    let occ = expand_block(
        &s,
        window(at(2026, 3, 17, 0, 0), at(2026, 3, 18, 0, 0), &tz),
        &tz,
    )
    .unwrap();
    assert_eq!(
        occ[0].ends_at,
        SunriseTime::floating(at(2026, 3, 17, 11, 0))
    );
}

#[test]
fn an_exception_off_the_rule_is_refused_and_applies_to_nothing() {
    let tz = TimeZone::UTC;
    let mut s = deep_work();
    assert_eq!(
        s.except("2026-03-11T09:00", BlockException::cancelled()),
        Err(SplitError::NotAnOccurrence("2026-03-11T09:00".into()))
    );
    assert_eq!(
        s.except("Tuesday", BlockException::cancelled()),
        Err(SplitError::BadKey("Tuesday".into()))
    );
    // One that arrived anyway (a merge, an import) moves nothing.
    s.exceptions.insert(
        "2026-03-11T09:00".into(),
        BlockException {
            starts_at: Some(SunriseTime::floating(at(2026, 3, 11, 9, 0))),
            ..BlockException::default()
        },
    );
    let occ = expand_block(
        &s,
        window(at(2026, 3, 11, 0, 0), at(2026, 3, 12, 0, 0), &tz),
        &tz,
    )
    .unwrap();
    assert!(occ.is_empty());
}

#[test]
fn the_anchor_rules_are_enforced() {
    let tz = TimeZone::UTC;
    let win = window(at(2026, 3, 1, 0, 0), at(2026, 3, 8, 0, 0), &tz);
    let instant_anchor = series(
        SunriseTime::instant(instant(at(2026, 3, 3, 9, 0), &tz)),
        SunriseTime::instant(instant(at(2026, 3, 3, 11, 0), &tz)),
        "FREQ=DAILY",
    );
    assert_eq!(
        expand_block(&instant_anchor, win, &tz),
        Err(BlockSeriesError::Anchor)
    );
    let mixed = series(
        SunriseTime::floating(at(2026, 3, 3, 9, 0)),
        SunriseTime::zoned(at(2026, 3, 3, 11, 0), LA),
        "FREQ=DAILY",
    );
    assert_eq!(mixed.validate(), Err(BlockSeriesError::End));
    let two_zones = series(
        SunriseTime::zoned(at(2026, 3, 3, 9, 0), LA),
        SunriseTime::zoned(at(2026, 3, 3, 11, 0), "America/New_York"),
        "FREQ=DAILY",
    );
    assert_eq!(two_zones.validate(), Err(BlockSeriesError::End));
    let inverted = series(
        SunriseTime::floating(at(2026, 3, 3, 11, 0)),
        SunriseTime::floating(at(2026, 3, 3, 9, 0)),
        "FREQ=DAILY",
    );
    assert_eq!(inverted.validate(), Err(BlockSeriesError::End));
    deep_work().validate().unwrap();
}

/// A kind a newer build wrote is never guessed at (ADR-0045 §6).
#[test]
fn an_unknown_time_kind_yields_nothing() {
    let tz = TimeZone::UTC;
    let s = series(
        SunriseTime::Unknown {
            kind: "lunar".into(),
            raw: crate::Unknowns::new(),
        },
        SunriseTime::floating(at(2026, 3, 3, 11, 0)),
        "FREQ=DAILY",
    );
    let win = window(at(2026, 3, 1, 0, 0), at(2026, 3, 8, 0, 0), &tz);
    assert_eq!(expand_block(&s, win, &tz), Ok(Vec::new()));
}

/// The block expander and the routine expander are one function: the same
/// rule and anchor give the same keys.
#[test]
fn block_keys_are_routine_keys() {
    let tz = TimeZone::UTC;
    let s = deep_work();
    let win = window(at(2026, 3, 1, 0, 0), at(2026, 4, 1, 0, 0), &tz);
    let routine: Vec<String> = expand(&s.rrule, at(2026, 3, 3, 9, 0), &tz, win)
        .unwrap()
        .into_iter()
        .map(|o| o.key)
        .collect();
    let block: Vec<String> = expand_block(&s, win, &tz)
        .unwrap()
        .into_iter()
        .map(|o| o.key)
        .collect();
    assert_eq!(routine, block);
}

/// Edit scope "this and future": the two halves hold exactly the series'
/// occurrences, split at the key, with their exceptions.
#[test]
fn a_split_partitions_the_series_at_the_key() {
    let tz = TimeZone::UTC;
    let mut s = deep_work();
    s.rrule = RRule::parse("FREQ=WEEKLY;BYDAY=TU;COUNT=5").unwrap();
    s.except("2026-03-10T09:00", BlockException::cancelled())
        .unwrap();
    s.except("2026-03-24T09:00", BlockException::cancelled())
        .unwrap();
    let win = window(at(2026, 3, 1, 0, 0), at(2026, 6, 1, 0, 0), &tz);
    let whole = expand_block(&s, win, &tz).unwrap();

    let split = s.split_at("2026-03-17T09:00").unwrap();
    let ended = split.ended.unwrap();
    assert_eq!(
        ended.rrule.until,
        Some(SunriseTime::floating(at(2026, 3, 10, 9, 0)))
    );
    assert_eq!(
        ended.exceptions.keys().collect::<Vec<_>>(),
        ["2026-03-10T09:00"]
    );
    let next = split.continuation;
    assert_eq!(next.starts_at, SunriseTime::floating(at(2026, 3, 17, 9, 0)));
    assert_eq!(next.ends_at, SunriseTime::floating(at(2026, 3, 17, 11, 0)));
    assert_eq!(next.rrule.count, Some(3));
    assert_eq!(
        next.exceptions.keys().collect::<Vec<_>>(),
        ["2026-03-24T09:00"]
    );

    let mut halves = expand_block(&ended, win, &tz).unwrap();
    halves.extend(expand_block(&next, win, &tz).unwrap());
    assert_eq!(keys(&halves), keys(&whole));
    assert_eq!(
        keys(&whole),
        ["2026-03-03T09:00", "2026-03-17T09:00", "2026-03-31T09:00"]
    );
}

/// East of UTC, the ended series' `UNTIL` is a zoned wall clock that fires
/// before the same wall clock read in UTC: its last occurrence is still one
/// of its own, so it can be edited, split again and moved.
#[test]
fn an_ended_series_east_of_utc_keeps_its_last_occurrence() {
    const TOKYO: &str = "Asia/Tokyo";
    let tokyo = TimeZone::get(TOKYO).unwrap();
    let s = series(
        SunriseTime::zoned(at(2026, 3, 3, 9, 0), TOKYO),
        SunriseTime::zoned(at(2026, 3, 3, 11, 0), TOKYO),
        "FREQ=WEEKLY;BYDAY=TU",
    );
    let mut ended = s.split_at("2026-03-17T09:00").unwrap().ended.unwrap();
    assert_eq!(
        ended.rrule.until,
        Some(SunriseTime::zoned(at(2026, 3, 10, 9, 0), TOKYO))
    );
    let last = "2026-03-10T09:00";
    assert!(ended.produces_key(last));
    assert!(!ended.produces_key("2026-03-17T09:00"));
    assert!(ended.split_at(last).is_ok());
    let moved = BlockException {
        starts_at: Some(SunriseTime::zoned(at(2026, 3, 11, 9, 0), TOKYO)),
        ends_at: Some(SunriseTime::zoned(at(2026, 3, 11, 11, 0), TOKYO)),
        ..BlockException::default()
    };
    ended.except(last, moved).unwrap();
    let occ = expand_block(
        &ended,
        window(at(2026, 3, 1, 0, 0), at(2026, 4, 1, 0, 0), &tokyo),
        &tokyo,
    )
    .unwrap();
    assert_eq!(keys(&occ), ["2026-03-03T09:00", last]);
    assert_eq!(
        occ[1].starts_at,
        SunriseTime::zoned(at(2026, 3, 11, 9, 0), TOKYO)
    );
}

#[test]
fn a_split_at_the_first_occurrence_supersedes_the_series() {
    let split = deep_work().split_at("2026-03-03T09:00").unwrap();
    assert_eq!(split.ended, None);
    assert_eq!(split.continuation, deep_work());
}

#[test]
fn a_split_off_the_rule_is_refused() {
    assert_eq!(
        deep_work().split_at("2026-03-04T09:00"),
        Err(SplitError::NotAnOccurrence("2026-03-04T09:00".into()))
    );
}

/// Two devices splitting at the same occurrence name the same Block, and
/// never the routine the same bytes would name.
#[test]
fn the_split_id_is_a_function_of_root_and_key() {
    let root = EntityRef::new(EntityKind::Block, [3u8; 16]);
    let a = split_block_id(&root, "2026-03-17T09:00");
    assert_eq!(a, split_block_id(&root, "2026-03-17T09:00"));
    assert_eq!(a.kind(), EntityKind::Block);
    assert_ne!(a, split_block_id(&root, "2026-03-24T09:00"));
    assert_ne!(
        a.bytes(),
        split_routine_id(&root, "2026-03-17T09:00").bytes()
    );
}

/// An occurrence's rule bounds are the anchor's length at its wall clock,
/// and a key the rule does not produce has none.
#[test]
fn an_occurrence_has_the_rules_bounds() {
    let s = deep_work();
    assert_eq!(
        s.occurrence_bounds("2026-03-17T09:00"),
        Some((
            SunriseTime::floating(at(2026, 3, 17, 9, 0)),
            SunriseTime::floating(at(2026, 3, 17, 11, 0)),
        ))
    );
    assert_eq!(s.occurrence_bounds("2026-03-18T09:00"), None);
}

/// A Block read as one occurrence takes its times and the exception's
/// fields, and keeps the rest of the series.
#[test]
fn a_block_at_an_occurrence_takes_its_times_and_overrides() {
    let s = deep_work();
    let block = crate::Block {
        id: EntityRef::new(EntityKind::Block, [1u8; 16]),
        created_at: Timestamp::UNIX_EPOCH,
        updated_at: Timestamp::UNIX_EPOCH,
        stream_id: None,
        starts_at: s.starts_at.clone(),
        ends_at: s.ends_at.clone(),
        title: Some("Deep work".into()),
        title_track_task: false,
        tasks: std::collections::BTreeSet::new(),
        deleted: false,
        kind: None,
        rrule: Some(s.rrule.clone()),
        exceptions: BTreeMap::new(),
        split_from: None,
        location: Some("Desk".into()),
        notes: None,
        source: None,
        external_id: None,
        unknown: Unknowns::new(),
    };
    let (starts_at, ends_at) = s.occurrence_bounds("2026-03-17T09:00").unwrap();
    let plain = BlockOccurrence {
        key: "2026-03-17T09:00".into(),
        starts_at: starts_at.clone(),
        ends_at: ends_at.clone(),
        exception: None,
    };
    let read = block.at_occurrence(&plain);
    assert_eq!(
        (read.starts_at, read.title),
        (starts_at.clone(), block.title.clone())
    );
    assert_eq!(read.rrule, block.rrule);

    let renamed = BlockOccurrence {
        exception: Some(BlockException {
            title: Some("Taxes".into()),
            notes: Some(NoteBody(b"receipts".to_vec())),
            ..BlockException::default()
        }),
        ..plain
    };
    let read = block.at_occurrence(&renamed);
    assert_eq!(read.title.as_deref(), Some("Taxes"));
    assert_eq!(read.location.as_deref(), Some("Desk"));
    assert_eq!(read.notes, Some(NoteBody(b"receipts".to_vec())));
}

/// One occurrence takes the times and a title, over what its exception
/// already holds; the series' fields are refused.
#[test]
fn an_occurrence_edit_carries_only_what_an_exception_can() {
    let moved = SunriseTime::floating(at(2026, 3, 17, 10, 0));
    let patch = crate::BlockPatch {
        starts_at: Some(moved.clone()),
        title: Some(Some(" Taxes ".into())),
        ..crate::BlockPatch::default()
    };
    assert!(patch.is_occurrence_edit());
    let mut x = BlockException {
        location: Some("Library".into()),
        ..BlockException::default()
    };
    patch.apply_to_exception(&mut x);
    assert_eq!(x.starts_at, Some(moved));
    assert_eq!(x.title.as_deref(), Some("Taxes"));
    assert_eq!(x.location.as_deref(), Some("Library"));
    assert_eq!(x.ends_at, None);

    for series_only in [
        crate::BlockPatch {
            title: Some(None),
            ..crate::BlockPatch::default()
        },
        crate::BlockPatch {
            title_track_task: Some(true),
            ..crate::BlockPatch::default()
        },
        crate::BlockPatch {
            stream_id: Some(None),
            ..crate::BlockPatch::default()
        },
    ] {
        assert!(!series_only.is_occurrence_edit());
    }
}
