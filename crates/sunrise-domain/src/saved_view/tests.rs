//! The filter's predicates, one table per kind, and the reader-zone window.

use super::*;
use crate::{TaskState, Unknowns};
use std::collections::BTreeSet;
use sunrise_id::{EntityKind, EntityRef};

fn stream(n: u8) -> EntityRef {
    EntityRef::new(EntityKind::Stream, [n; 16])
}

fn ctx(n: u8) -> EntityRef {
    EntityRef::new(EntityKind::Context, [n; 16])
}

fn task(n: u8) -> Task {
    Task {
        id: EntityRef::new(EntityKind::Task, [n; 16]),
        created_at: Timestamp::from_second(i64::from(n)).unwrap(),
        updated_at: Timestamp::from_second(100 - i64::from(n)).unwrap(),
        title: format!("Task {n}"),
        body: None,
        stream_id: Some(stream(1)),
        contexts: BTreeSet::new(),
        state: TaskState::Todo,
        priority: None,
        energy: None,
        estimated_duration_s: None,
        planned_at: None,
        target_at: None,
        hard_due_at: None,
        scheduling_constraints: Vec::new(),
        completed_at: None,
        deferred_count: 0,
        blocks: BTreeSet::new(),
        blocked_by: BTreeSet::new(),
        assignee: None,
        routine_id: None,
        routine_occurrence: None,
        reminder_lead_s: None,
        sort_order: None,
        triaged_at: None,
        cancel_reason: None,
        late_acknowledged_at: None,
        archived: false,
        deleted: false,
        unknown: Unknowns::new(),
    }
}

fn reader(tz: &str, today: Date) -> FilterReader {
    let zone = TimeZone::get(tz).unwrap();
    FilterReader {
        now: zone.to_zoned(today.at(12, 0, 0, 0)).unwrap().timestamp(),
        tz: zone,
        today,
        schedule: crate::DaySchedule::default(),
        stale_after_days: crate::DEFAULT_STALE_AFTER_DAYS,
    }
}

fn utc() -> FilterReader {
    reader("UTC", jiff::civil::date(2026, 6, 10))
}

/// `(filter, task, expected)` rows, each checked.
fn table(rows: &[(ViewFilter, Task, bool)], r: &FilterReader) {
    for (i, (f, t, want)) in rows.iter().enumerate() {
        assert_eq!(f.matches(t, r, true), *want, "row {i}: {f:?}");
    }
}

#[test]
fn an_empty_filter_keeps_everything() {
    table(&[(ViewFilter::default(), task(1), true)], &utc());
}

#[test]
fn streams_are_any_of() {
    let f = ViewFilter {
        streams: vec![stream(1), stream(2)],
        ..ViewFilter::default()
    };
    let mut other = task(2);
    other.stream_id = Some(stream(3));
    table(&[(f.clone(), task(1), true), (f, other, false)], &utc());
}

/// "Work or no stream" (ADR-0046): `no_stream` ORs with `streams`, and the
/// legacy Inbox reference in `streams` means it too.
#[test]
fn no_stream_is_one_more_choice_beside_the_streams() {
    let mut loose = task(3);
    loose.stream_id = None;
    let mut other = task(2);
    other.stream_id = Some(stream(3));
    let either = ViewFilter {
        streams: vec![stream(1)],
        no_stream: true,
        ..ViewFilter::default()
    };
    let only = ViewFilter {
        no_stream: true,
        ..ViewFilter::default()
    };
    let legacy = ViewFilter {
        streams: vec![crate::inbox_stream_ref()],
        ..ViewFilter::default()
    };
    table(
        &[
            (either.clone(), task(1), true),
            (either.clone(), loose.clone(), true),
            (either, other.clone(), false),
            (only.clone(), loose.clone(), true),
            (only, task(1), false),
            (legacy.clone(), loose, true),
            (legacy, other, false),
        ],
        &utc(),
    );
}

#[test]
fn contexts_are_any_of_by_id() {
    let f = ViewFilter {
        contexts: vec![ctx(1), ctx(2)],
        ..ViewFilter::default()
    };
    let mut tagged = task(1);
    tagged.contexts.insert(ctx(2));
    let mut wrong = task(2);
    wrong.contexts.insert(ctx(3));
    table(
        &[
            (f.clone(), tagged, true),
            (f.clone(), wrong, false),
            (f, task(3), false),
        ],
        &utc(),
    );
}

/// A name the import could not resolve matches nothing and filters nothing
/// away: it is reported by the read, not applied.
#[test]
fn an_unresolved_context_name_is_not_a_predicate() {
    let f = ViewFilter {
        context_names: vec!["ghost".into()],
        ..ViewFilter::default()
    };
    table(&[(f, task(1), true)], &utc());
}

#[test]
fn priority_is_an_inclusive_range_and_unset_is_outside() {
    let f = ViewFilter {
        priority: Some(PriorityRange {
            min: 1,
            max: 2,
            unknown: Unknowns::new(),
        }),
        ..ViewFilter::default()
    };
    let with = |p: Option<u8>| {
        let mut t = task(1);
        t.priority = p;
        t
    };
    table(
        &[
            (f.clone(), with(Some(1)), true),
            (f.clone(), with(Some(2)), true),
            (f.clone(), with(Some(3)), false),
            (f, with(None), false),
        ],
        &utc(),
    );
}

#[test]
fn energy_is_any_of() {
    let f = ViewFilter {
        energy: vec![Energy::Low],
        ..ViewFilter::default()
    };
    let with = |e: Option<Energy>| {
        let mut t = task(1);
        t.energy = e;
        t
    };
    table(
        &[
            (f.clone(), with(Some(Energy::Low)), true),
            (f.clone(), with(Some(Energy::High)), false),
            (f, with(None), false),
        ],
        &utc(),
    );
}

#[test]
fn text_matches_the_title_without_case_and_can_be_skipped() {
    let f = ViewFilter {
        text: "  PASSport ".into(),
        ..ViewFilter::default()
    };
    let mut t = task(1);
    t.title = "Renew passport".into();
    table(&[(f.clone(), t, true), (f.clone(), task(2), false)], &utc());
    assert!(f.matches(&task(2), &utc(), false), "a search applied it");
}

fn window(field: WindowField, from: Option<i32>, to: Option<i32>) -> ViewFilter {
    ViewFilter {
        window: Some(DateWindow {
            field,
            from_days: from,
            to_days: to,
            unknown: Unknowns::new(),
        }),
        ..ViewFilter::default()
    }
}

#[test]
fn a_window_is_inclusive_relative_days_and_a_missing_time_is_outside() {
    let r = utc();
    let on = |field: WindowField, day: Date| {
        let mut t = task(1);
        let v = Some(SunriseTime::all_day(day));
        match field {
            WindowField::Scheduled => t.planned_at = v,
            WindowField::Target => t.target_at = v,
            _ => t.hard_due_at = v,
        }
        t
    };
    let today = r.today;
    let tomorrow = today.tomorrow().unwrap();
    let yesterday = today.yesterday().unwrap();
    let next_week = jiff::civil::date(2026, 6, 17);
    table(
        &[
            (
                window(WindowField::Due, None, Some(0)),
                on(WindowField::Due, yesterday),
                true,
            ),
            (
                window(WindowField::Due, None, Some(0)),
                on(WindowField::Due, today),
                true,
            ),
            (
                window(WindowField::Due, None, Some(0)),
                on(WindowField::Due, tomorrow),
                false,
            ),
            (
                window(WindowField::Scheduled, Some(1), Some(7)),
                on(WindowField::Scheduled, tomorrow),
                true,
            ),
            (
                window(WindowField::Scheduled, Some(1), Some(7)),
                on(WindowField::Scheduled, next_week),
                true,
            ),
            (
                window(WindowField::Scheduled, Some(1), Some(7)),
                on(WindowField::Scheduled, today),
                false,
            ),
            (
                window(WindowField::Scheduled, Some(0), None),
                on(WindowField::Due, today),
                false,
            ),
            (window(WindowField::Due, None, None), task(1), false),
            (
                window(WindowField::Target, Some(0), Some(0)),
                on(WindowField::Target, today),
                true,
            ),
            (
                window(WindowField::Target, Some(0), Some(0)),
                on(WindowField::Due, today),
                false,
            ),
        ],
        &r,
    );
}

/// The lateness part keeps the tasks in any of its states for this reader,
/// and a state a newer build added is kept verbatim, matches nothing, and
/// marks the view incomplete.
#[test]
fn the_lateness_part_reads_each_tasks_lateness_for_the_reader() {
    let r = utc();
    let yesterday = SunriseTime::all_day(r.today.yesterday().unwrap());
    let tomorrow = SunriseTime::all_day(r.today.tomorrow().unwrap());
    let mut hard = task(1);
    hard.hard_due_at = Some(yesterday.clone());
    let mut soft = task(2);
    soft.target_at = Some(yesterday);
    soft.hard_due_at = Some(tomorrow.clone());
    let mut on_track = task(3);
    on_track.hard_due_at = Some(tomorrow);
    // `task` was last touched in 1970: stale for any reader in 2026.
    let stale = task(4);
    let only = |parts: Vec<LatenessPart>| ViewFilter {
        lateness: parts,
        ..ViewFilter::default()
    };
    let late = only(vec![LatenessPart::LateHard, LatenessPart::LateSoft]);
    table(
        &[
            (late.clone(), hard.clone(), true),
            (late.clone(), soft.clone(), true),
            (late.clone(), on_track.clone(), false),
            (late, stale.clone(), false),
            (only(vec![LatenessPart::Stale]), stale.clone(), true),
            (only(vec![LatenessPart::OnTrack]), on_track, true),
            (only(vec![LatenessPart::OnTrack]), hard.clone(), false),
        ],
        &r,
    );
    let newer = only(vec![
        LatenessPart::from_raw("overdue_forever"),
        LatenessPart::LateHard,
    ]);
    assert!(!newer.is_complete());
    assert!(newer.matches(&hard, &r, true));
    assert!(!newer.matches(&soft, &r, true));
    let alone = only(vec![LatenessPart::from_raw("overdue_forever")]);
    assert!(
        alone.matches(&stale, &r, true),
        "an unknown part alone passes"
    );
    // The wire spelling is the state's.
    let json = serde_json::to_string(&only(vec![LatenessPart::LateSoft])).unwrap();
    assert_eq!(json, r#"{"lateness":["late_soft"]}"#);
    for l in crate::Lateness::ALL {
        assert_eq!(LatenessPart::from(l).state(), Some(l));
        assert_eq!(LatenessPart::from(l).as_str(), l.as_str());
    }
}

/// The day a time falls on is the reader's: 23:30 UTC is today in London in
/// winter and tomorrow in Tokyo, and a floating 09:00 is the same day
/// everywhere.
#[test]
fn a_window_reads_the_day_in_the_readers_zone() {
    let today = jiff::civil::date(2026, 1, 10);
    let mut instant = task(1);
    instant.planned_at = Some(SunriseTime::instant(
        jiff::civil::date(2026, 1, 10)
            .at(23, 30, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap()
            .timestamp(),
    ));
    let mut floating = task(2);
    floating.planned_at = Some(SunriseTime::floating(today.at(9, 0, 0, 0)));
    let today_only = window(WindowField::Scheduled, Some(0), Some(0));
    for (zone, instant_today) in [
        ("Europe/London", true),
        ("Asia/Tokyo", false),
        ("America/New_York", true),
    ] {
        let r = reader(zone, today);
        assert_eq!(
            today_only.matches(&instant, &r, true),
            instant_today,
            "{zone}"
        );
        assert!(today_only.matches(&floating, &r, true), "{zone}");
    }
}

/// A part a newer build added rides along in the filter and makes it
/// incomplete here; it is written back byte for byte.
#[test]
fn an_unknown_filter_part_round_trips_and_marks_the_filter_incomplete() {
    let mut f = window(WindowField::Due, None, Some(0));
    assert!(f.is_complete());
    f.unknown.insert(
        "place".into(),
        sunrise_cbor::CborValue(ciborium::value::Value::Text("plc_x".into())),
    );
    assert!(!f.is_complete());
    let bytes = sunrise_cbor::encode_canonical(&f).unwrap();
    let back: ViewFilter = sunrise_cbor::decode_canonical(&bytes).unwrap();
    assert_eq!(back, f);
    let newer_field = window(WindowField::from_raw("finish_by"), None, Some(0));
    assert!(!newer_field.is_complete());
    assert!(
        newer_field.matches(&task(1), &utc(), true),
        "an unknown time does not filter"
    );
    assert_eq!(
        sunrise_cbor::decode_canonical::<ViewFilter>(
            &sunrise_cbor::encode_canonical(&newer_field).unwrap()
        )
        .unwrap(),
        newer_field
    );
}

#[test]
fn every_sort_ends_on_id_and_default_keeps_the_order_given() {
    let tz = TimeZone::UTC;
    let mut a = task(3);
    a.priority = Some(2);
    a.title = "b".into();
    let mut b = task(1);
    b.priority = None;
    b.title = "C".into();
    let mut c = task(2);
    c.priority = Some(1);
    c.title = "a".into();
    let ids = |ts: &[Task]| ts.iter().map(|t| t.id).collect::<Vec<_>>();
    let order = |sort: ViewSort| {
        let mut ts = vec![a.clone(), b.clone(), c.clone()];
        sort_tasks(&mut ts, &sort, &tz);
        ids(&ts)
    };
    assert_eq!(
        order(ViewSort::Default),
        ids(&[a.clone(), b.clone(), c.clone()])
    );
    assert_eq!(
        order(ViewSort::Priority),
        ids(&[c.clone(), a.clone(), b.clone()])
    );
    assert_eq!(
        order(ViewSort::Title),
        ids(&[c.clone(), a.clone(), b.clone()])
    );
    assert_eq!(
        order(ViewSort::Created),
        ids(&[b.clone(), c.clone(), a.clone()])
    );
    assert_eq!(
        order(ViewSort::Updated),
        ids(&[b.clone(), c.clone(), a.clone()])
    );
    assert_eq!(order(ViewSort::from_raw("newer")), order(ViewSort::Default));
}

#[test]
fn the_view_and_sort_spellings_are_lossless() {
    for v in ViewKind::KNOWN {
        assert_eq!(ViewKind::from_raw(v.as_str()), *v);
    }
    assert_eq!(ViewKind::from_raw("map").as_str(), "map");
    assert!(!ViewKind::from_raw("map").lists_tasks());
    assert!(ViewKind::Search.lists_tasks() && !ViewKind::Calendar.lists_tasks());
    for s in ViewSort::KNOWN {
        assert_eq!(ViewSort::from_raw(s.as_str()), *s);
    }
}

#[test]
fn drafts_and_patches_validate_what_they_write() {
    let draft = SavedViewDraft {
        name: "  Errands ".into(),
        view: ViewKind::Today,
        filter: ViewFilter::default(),
        sort: ViewSort::Default,
    };
    draft.validate().unwrap();
    assert!(SavedViewDraft {
        name: " ".into(),
        ..draft.clone()
    }
    .validate()
    .is_err());
    let bad_range = ViewFilter {
        priority: Some(PriorityRange {
            min: 3,
            max: 1,
            unknown: Unknowns::new(),
        }),
        ..ViewFilter::default()
    };
    assert!(SavedViewDraft {
        filter: bad_range.clone(),
        ..draft
    }
    .validate()
    .is_err());
    assert!(SavedViewPatch {
        filter: Some(bad_range),
        ..SavedViewPatch::default()
    }
    .validate()
    .is_err());
    assert!(SavedViewPatch {
        sort_order: Some(Some("NA".into())),
        ..SavedViewPatch::default()
    }
    .validate()
    .is_err());
    assert!(SavedViewPatch::default().is_empty());
}
