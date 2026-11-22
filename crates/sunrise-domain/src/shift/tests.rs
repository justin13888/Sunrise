use super::*;
use crate::routine::{RoutineCatchupPolicy, TaskTemplate};
use crate::rrule::RRule;
use crate::task::TaskDraft;
use crate::unknown::Unknowns;
use jiff::civil::{date, DateTime};

mod away;

fn utc(y: i16, m: i8, d: i8, h: i8, min: i8) -> Timestamp {
    date(y, m, d)
        .at(h, min, 0, 0)
        .to_zoned(TimeZone::UTC)
        .unwrap()
        .timestamp()
}

fn eref(kind: EntityKind, b: u8) -> EntityRef {
    EntityRef::new(kind, [b; 16])
}

/// A daily routine anchored at `anchor`, `zoned` in `tz` unless `floating`.
fn daily(tz: &str, anchor: DateTime, floating: bool) -> Routine {
    Routine {
        id: eref(EntityKind::Routine, 1),
        created_at: utc(2026, 1, 1, 0, 0),
        updated_at: utc(2026, 1, 1, 0, 0),
        template: TaskTemplate {
            title: "Stretch".into(),
            stream_id: None,
            contexts: Vec::new(),
            energy: None,
            priority: None,
            estimated_duration_s: None,
            body: None,
            unknown: Unknowns::new(),
        },
        rrule: RRule::parse("FREQ=DAILY").unwrap(),
        timezone: tz.into(),
        starts_at: if floating {
            SunriseTime::floating(anchor)
        } else {
            SunriseTime::zoned(anchor, tz)
        },
        ends_at: None,
        scheduling_constraints: Vec::new(),
        skip_dates: Vec::new(),
        skipped_keys: Vec::new(),
        catchup_policy: RoutineCatchupPolicy::Skip,
        streak_counter: 0,
        last_completed_at: None,
        grace_window_s: None,
        forgiveness_enabled: true,
        streak_started_at: None,
        forgivenesses_in_window: 0,
        streak_keys: Vec::new(),
        paused: false,
        paused_until: None,
        archived: false,
        deleted: false,
        split_from: None,
        shift_policy: crate::ShiftPolicy::Pause,
        unknown: Unknowns::new(),
    }
}

fn shift(name: &str, start: Timestamp, end: Option<Timestamp>, zone: Option<&str>) -> Shift {
    Shift {
        id: eref(EntityKind::Shift, 1),
        created_at: start,
        updated_at: start,
        name: name.into(),
        starts_at: start,
        ends_at: end,
        zone: zone.map(str::to_owned),
        place: None,
        primary_streams: BTreeSet::new(),
        primary_contexts: BTreeSet::new(),
        deleted: false,
        unknown: Unknowns::new(),
    }
}

const ALL: [ShiftPolicy; 4] = [
    ShiftPolicy::Pause,
    ShiftPolicy::ShiftTz,
    ShiftPolicy::Skip,
    ShiftPolicy::Unchanged,
];

fn keys(occ: &[Occurrence]) -> Vec<String> {
    occ.iter().map(|o| o.key.clone()).collect()
}

fn generated_keys(g: &ShiftGeneration) -> Vec<String> {
    g.generate
        .iter()
        .map(|o| o.occurrence.key.clone())
        .collect()
}

fn june_keys(days: std::ops::RangeInclusive<i8>, time: &str) -> Vec<String> {
    days.map(|d| format!("2026-06-{d:02}T{time}")).collect()
}

// --- the policy ------------------------------------------------------------

/// The four spellings round-trip; an unknown one is kept verbatim and reads
/// as `pause`, the default.
#[test]
fn a_policy_reads_its_four_spellings_and_keeps_an_unknown_one() {
    for (p, s) in ALL.iter().zip(["pause", "shift_tz", "skip", "unchanged"]) {
        assert_eq!(p.as_str(), s);
        assert_eq!(&ShiftPolicy::from_raw(s), p);
    }
    let newer = ShiftPolicy::from_raw("follow_place");
    assert!(newer.is_unknown());
    assert_eq!(newer.as_str(), "follow_place");
    assert_eq!(newer.effective(), ShiftPolicy::Pause);
    assert_eq!(ShiftPolicy::default(), ShiftPolicy::Pause);
    assert!(ShiftPolicy::Pause.excuses_streak() && ShiftPolicy::Skip.excuses_streak());
    assert!(newer.excuses_streak());
    assert!(!ShiftPolicy::ShiftTz.excuses_streak() && !ShiftPolicy::Unchanged.excuses_streak());
}

/// A routine at `pause` encodes as it did before the field existed; any other
/// policy, an unknown one included, is written and read back.
#[test]
fn a_routine_omits_pause_and_writes_every_other_policy() {
    let encode = |r: &Routine| {
        let mut v = Vec::new();
        ciborium::ser::into_writer(r, &mut v).unwrap();
        v
    };
    let base = daily("UTC", date(2026, 6, 1).at(9, 0, 0, 0), false);
    let bytes = encode(&base);
    let value: ciborium::Value = ciborium::de::from_reader(bytes.as_slice()).unwrap();
    let keys: Vec<String> = value
        .into_map()
        .unwrap()
        .into_iter()
        .filter_map(|(k, _)| k.into_text().ok())
        .collect();
    assert!(!keys.iter().any(|k| k == "shift_policy"));
    let back: Routine = ciborium::de::from_reader(bytes.as_slice()).unwrap();
    assert_eq!(back.shift_policy, ShiftPolicy::Pause);
    // A null, which a `Patch` writes to put the field back to its absence,
    // reads as `pause` too.
    let mut with_null: ciborium::Value = ciborium::de::from_reader(bytes.as_slice()).unwrap();
    if let ciborium::Value::Map(m) = &mut with_null {
        m.push((
            ciborium::Value::Text("shift_policy".into()),
            ciborium::Value::Null,
        ));
    }
    let back: Routine = with_null.deserialized().unwrap();
    assert_eq!(back.shift_policy, ShiftPolicy::Pause);

    for p in [
        ShiftPolicy::ShiftTz,
        ShiftPolicy::Skip,
        ShiftPolicy::Unchanged,
        ShiftPolicy::from_raw("follow_place"),
    ] {
        let r = Routine {
            shift_policy: p.clone(),
            ..base.clone()
        };
        let back: Routine = ciborium::de::from_reader(encode(&r).as_slice()).unwrap();
        assert_eq!(back.shift_policy, p);
    }
}

/// A patch writes only what it names, and an unknown policy cannot be set.
#[test]
fn a_shift_patch_applies_what_it_names() {
    let start = utc(2026, 6, 10, 0, 0);
    let s = shift("Trip", start, None, Some("Asia/Tokyo"));
    assert!(ShiftPatch::default().is_empty());
    let end = utc(2026, 6, 15, 0, 0);
    let p = ShiftPatch {
        name: Some("  Tokyo  ".into()),
        ends_at: Some(Some(end)),
        zone: Some(None),
        ..ShiftPatch::default()
    };
    assert!(!p.is_empty());
    let got = p.applied_to(&s);
    assert_eq!(got.name, "Tokyo");
    assert_eq!(got.ends_at, Some(end));
    assert_eq!(got.zone, None);
    assert_eq!(got.starts_at, start);

    let bad = crate::RoutinePatch {
        shift_policy: Some(ShiftPolicy::from_raw("follow_place")),
        ..crate::RoutinePatch::default()
    };
    assert_eq!(
        bad.validate(),
        Err(field("routine.shift_policy", "known_policy"))
    );
}

/// A draft becomes a live shift starting at its own start, or at `now`.
#[test]
fn a_draft_starts_now_unless_it_names_a_start() {
    let now = utc(2026, 6, 10, 8, 0);
    let id = eref(EntityKind::Shift, 9);
    let d = ShiftDraft {
        name: " Travel ".into(),
        starts_at: None,
        ends_at: None,
        zone: None,
        place: None,
        primary_streams: BTreeSet::new(),
        primary_contexts: BTreeSet::new(),
    };
    let s = d.clone().into_shift(id, now);
    assert_eq!((s.id, s.starts_at, s.name.as_str()), (id, now, "Travel"));
    assert!(!s.deleted);
    let earlier = utc(2026, 6, 9, 0, 0);
    let s = ShiftDraft {
        starts_at: Some(earlier),
        ..d
    }
    .into_shift(id, now);
    assert_eq!(s.starts_at, earlier);
}

/// A deleted shift covers nothing, so it is never the one in force.
#[test]
fn a_deleted_shift_is_in_force_nowhere() {
    let start = utc(2026, 6, 10, 0, 0);
    let mut s = shift("Trip", start, None, None);
    assert!(s.covers(start));
    s.deleted = true;
    assert!(!s.covers(start));
    assert!(active_shift(&[(1u8, s)], start).is_none());
}

// --- the shift -------------------------------------------------------------

#[test]
fn a_shift_validates_its_name_end_zone_place_and_focus() {
    let start = utc(2026, 6, 10, 0, 0);
    let ok = shift(
        "Travel: Tokyo",
        start,
        Some(utc(2026, 6, 15, 0, 0)),
        Some("Asia/Tokyo"),
    );
    assert_eq!(ok.validate(), Ok(()));

    let mut s = ok.clone();
    s.name = "   ".into();
    assert_eq!(s.validate(), Err(ValidationError::InvalidTitle));

    let mut s = ok.clone();
    s.ends_at = Some(start);
    assert_eq!(s.validate(), Err(field("shift.ends_at", "after_starts_at")));

    let mut s = ok.clone();
    s.zone = Some("Mars/Olympus".into());
    assert_eq!(s.validate(), Err(field("shift.zone", "iana_timezone")));

    let mut s = ok.clone();
    s.place = Some(eref(EntityKind::Stream, 3));
    assert_eq!(s.validate(), Err(field("shift.place", "place_ref")));
    s.place = Some(eref(EntityKind::Place, 3));
    assert_eq!(s.validate(), Ok(()));

    let mut s = ok.clone();
    s.primary_streams.insert(eref(EntityKind::Context, 4));
    assert_eq!(
        s.validate(),
        Err(field("shift.primary_streams", "stream_ref"))
    );

    let mut s = ok;
    s.primary_contexts.insert(eref(EntityKind::Stream, 4));
    assert_eq!(
        s.validate(),
        Err(field("shift.primary_contexts", "context_ref"))
    );
}

/// The shift in force is the latest-started one that covers the instant; an
/// ended one covers nothing from its end on.
#[test]
fn the_active_shift_is_the_latest_started_one_in_force() {
    let trip = shift(
        "Trip",
        utc(2026, 6, 1, 0, 0),
        Some(utc(2026, 6, 30, 0, 0)),
        None,
    );
    let conference = shift(
        "Conference",
        utc(2026, 6, 10, 0, 0),
        Some(utc(2026, 6, 12, 0, 0)),
        None,
    );
    let shifts = [(1u8, trip), (2u8, conference)];
    assert_eq!(active_shift(&shifts, utc(2026, 5, 31, 0, 0)), None);
    assert_eq!(active_shift(&shifts, utc(2026, 6, 5, 0, 0)).unwrap().0, 1);
    assert_eq!(active_shift(&shifts, utc(2026, 6, 11, 0, 0)).unwrap().0, 2);
    assert_eq!(active_shift(&shifts, utc(2026, 6, 12, 0, 0)).unwrap().0, 1);
    assert_eq!(active_shift(&shifts, utc(2026, 6, 30, 0, 0)), None);
}

/// Two devices start a shift each at the same instant. Once both have
/// synced, each holds both, in the order it learned of them, and both pick
/// the same one; with distinct starts the later one wins on both.
#[test]
fn concurrent_shifts_on_two_devices_converge_on_one_active_shift() {
    let start = utc(2026, 6, 10, 8, 0);
    let a = (
        eref(EntityKind::Task, 0xa0),
        shift("Travel: Tokyo", start, None, Some("Asia/Tokyo")),
    );
    let b = (
        eref(EntityKind::Task, 0xb0),
        shift("Travel: Osaka", start, None, Some("Asia/Tokyo")),
    );
    let now = utc(2026, 6, 11, 0, 0);
    let device_a = [a.clone(), b.clone()];
    let device_b = [b.clone(), a.clone()];
    let on_a = active_shift(&device_a, now).unwrap();
    let on_b = active_shift(&device_b, now).unwrap();
    assert_eq!(on_a, on_b);
    assert_eq!(on_a.0, b.0, "the tie goes to the greater key");

    let mut later = a.clone();
    later.1.starts_at = utc(2026, 6, 10, 8, 1);
    let device_a = [later.clone(), b.clone()];
    let device_b = [b, later.clone()];
    assert_eq!(active_shift(&device_a, now), Some(&later));
    assert_eq!(active_shift(&device_b, now), Some(&later));
}

/// A shift with focus keeps its streams and the tasks carrying its contexts
/// primary and demotes the rest; one with none demotes nothing.
#[test]
fn a_shift_keeps_its_streams_and_contexts_primary() {
    let trip_stream = eref(EntityKind::Stream, 1);
    let work_stream = eref(EntityKind::Stream, 2);
    let travel_ctx = eref(EntityKind::Context, 3);
    let mut s = shift("Travel: Tokyo", utc(2026, 6, 10, 0, 0), None, None);
    let task = |stream: Option<EntityRef>, ctx: Option<EntityRef>| {
        Task::from_draft(
            eref(EntityKind::Task, 9),
            utc(2026, 6, 1, 0, 0),
            TaskDraft {
                stream_id: stream,
                contexts: ctx.into_iter().collect(),
                ..TaskDraft::default()
            },
        )
    };
    assert!(!s.has_focus());
    assert!(s.keeps_stream_primary(Some(&work_stream)) && s.keeps_stream_primary(None));
    assert!(s.keeps_task_primary(&task(Some(work_stream), None)));

    s.primary_streams.insert(trip_stream);
    s.primary_contexts.insert(travel_ctx);
    assert!(s.keeps_stream_primary(Some(&trip_stream)));
    assert!(!s.keeps_stream_primary(Some(&work_stream)) && !s.keeps_stream_primary(None));
    assert!(s.keeps_task_primary(&task(Some(trip_stream), None)));
    assert!(s.keeps_task_primary(&task(Some(work_stream), Some(travel_ctx))));
    assert!(s.keeps_task_primary(&task(None, Some(travel_ctx))));
    assert!(!s.keeps_task_primary(&task(Some(work_stream), None)));
    assert!(!s.keeps_task_primary(&task(None, None)));
}

// --- generation across a zone change ---------------------------------------

/// "09:00 Los Angeles" daily, June 2026 (UTC-7: 16:00Z), and a trip to Tokyo
/// (UTC+9) from June 10 00:00Z to June 15 00:00Z. The shift covers the
/// occurrences of the 10th to the 14th where they would fire.
fn tokyo_trip() -> (Routine, [(u8, Shift); 1], (Timestamp, Timestamp)) {
    let r = daily(
        "America/Los_Angeles",
        date(2026, 6, 1).at(9, 0, 0, 0),
        false,
    );
    let trip = shift(
        "Travel: Tokyo",
        utc(2026, 6, 10, 0, 0),
        Some(utc(2026, 6, 15, 0, 0)),
        Some("Asia/Tokyo"),
    );
    (
        r,
        [(1, trip)],
        (utc(2026, 6, 1, 0, 0), utc(2026, 6, 20, 0, 0)),
    )
}

#[test]
fn each_policy_across_a_zone_change() {
    let (r, shifts, window) = tokyo_trip();
    let utc_tz = TimeZone::UTC;
    let plain = r.occurrences_in(window, &utc_tz).unwrap();
    assert_eq!(keys(&plain), june_keys(1..=19, "09:00"));

    let covered = june_keys(10..=14, "09:00");
    let outside: Vec<String> = june_keys(1..=19, "09:00")
        .into_iter()
        .filter(|k| !covered.contains(k))
        .collect();

    // pause: the horizon stops for the shift and resumes after it, with no
    // backfill and nothing skipped.
    let g = r
        .occurrences_under_shifts(window, &utc_tz, &shifts, &ShiftPolicy::Pause)
        .unwrap();
    assert_eq!(generated_keys(&g), outside);
    assert!(g.skipped.is_empty());

    // skip: the covered occurrences are skipped, the rest generated.
    let g = r
        .occurrences_under_shifts(window, &utc_tz, &shifts, &ShiftPolicy::Skip)
        .unwrap();
    assert_eq!(generated_keys(&g), outside);
    assert_eq!(keys(&g.skipped), covered);

    // unchanged: exactly what the routine generates with no shift.
    let g = r
        .occurrences_under_shifts(window, &utc_tz, &shifts, &ShiftPolicy::Unchanged)
        .unwrap();
    assert!(g.skipped.is_empty());
    assert_eq!(
        g.generate
            .iter()
            .map(|o| o.occurrence.clone())
            .collect::<Vec<_>>(),
        plain
    );
    assert!(g.generate.iter().all(|o| !o.reanchored));

    // shift_tz: 09:00 Tokyo (00:00Z) during the trip, 09:00 Los Angeles
    // around it, every key unchanged.
    let g = r
        .occurrences_under_shifts(window, &utc_tz, &shifts, &ShiftPolicy::ShiftTz)
        .unwrap();
    assert!(g.skipped.is_empty());
    assert_eq!(generated_keys(&g), june_keys(1..=19, "09:00"));
    for o in &g.generate {
        let day = o.occurrence.civil.day();
        if (10..=14).contains(&day) {
            assert!(o.reanchored, "{}", o.occurrence.key);
            assert_eq!(o.occurrence.at, utc(2026, 6, day, 0, 0));
            assert_eq!(
                o.planned_at,
                SunriseTime::zoned(date(2026, 6, day).at(9, 0, 0, 0), "Asia/Tokyo")
            );
        } else {
            assert!(!o.reanchored, "{}", o.occurrence.key);
            assert_eq!(o.occurrence.at, utc(2026, 6, day, 16, 0));
            assert_eq!(o.planned_at, r.occurrence_time(o.occurrence.civil));
        }
    }
}

/// An unknown policy generates as `pause` does.
#[test]
fn an_unknown_policy_generates_as_pause() {
    let (r, shifts, window) = tokyo_trip();
    let utc_tz = TimeZone::UTC;
    let pause = r
        .occurrences_under_shifts(window, &utc_tz, &shifts, &ShiftPolicy::Pause)
        .unwrap();
    let newer = r
        .occurrences_under_shifts(window, &utc_tz, &shifts, &ShiftPolicy::from_raw("x"))
        .unwrap();
    assert_eq!(pause, newer);
}

/// A floating routine already follows the reader, so `shift_tz` leaves it
/// where it is; so does a shift that names no zone.
#[test]
fn shift_tz_leaves_a_floating_routine_and_a_zoneless_shift_alone() {
    let (_, shifts, window) = tokyo_trip();
    let tokyo = TimeZone::get("Asia/Tokyo").unwrap();
    let floating = daily("America/Los_Angeles", date(2026, 6, 1).at(9, 0, 0, 0), true);
    let g = floating
        .occurrences_under_shifts(window, &tokyo, &shifts, &ShiftPolicy::ShiftTz)
        .unwrap();
    assert!(g.generate.iter().all(|o| !o.reanchored));
    assert_eq!(
        g.generate
            .iter()
            .map(|o| o.occurrence.clone())
            .collect::<Vec<_>>(),
        floating.occurrences_in(window, &tokyo).unwrap()
    );

    let (r, mut shifts, window) = tokyo_trip();
    shifts[0].1.zone = None;
    let g = r
        .occurrences_under_shifts(window, &TimeZone::UTC, &shifts, &ShiftPolicy::ShiftTz)
        .unwrap();
    assert!(g.generate.iter().all(|o| !o.reanchored));
    assert_eq!(g.generate.len(), 19);
}

/// Nothing outside the window comes back, re-anchored or not, and an
/// occurrence a re-anchor would move out of the shift keeps its own time.
#[test]
fn shift_tz_keeps_to_the_window_and_the_shift() {
    let (r, mut shifts, _) = tokyo_trip();
    // The window ends June 12 00:00Z: the re-anchored 12th is at its end.
    let window = (utc(2026, 6, 9, 0, 0), utc(2026, 6, 12, 0, 0));
    let g = r
        .occurrences_under_shifts(window, &TimeZone::UTC, &shifts, &ShiftPolicy::ShiftTz)
        .unwrap();
    assert_eq!(
        generated_keys(&g),
        ["2026-06-09T09:00", "2026-06-10T09:00", "2026-06-11T09:00"]
    );

    // A shift starting June 10 10:00Z: the 10th re-anchored (00:00Z) would
    // be before it, so it fires where it always would, at 16:00Z.
    shifts[0].1.starts_at = utc(2026, 6, 10, 10, 0);
    let g = r
        .occurrences_under_shifts(window, &TimeZone::UTC, &shifts, &ShiftPolicy::ShiftTz)
        .unwrap();
    let tenth = g
        .generate
        .iter()
        .find(|o| o.occurrence.key == "2026-06-10T09:00")
        .unwrap();
    assert!(!tenth.reanchored);
    assert_eq!(tenth.occurrence.at, utc(2026, 6, 10, 16, 0));
}

/// A conference from June 11 00:00Z to June 13 00:00Z inside the Tokyo
/// trip. Naming Kolkata (UTC+5:30), it is in force for its days, so the 11th
/// and 12th fire at 09:00 Kolkata (03:30Z) and the trip's other days at 09:00
/// Tokyo. Naming no zone, it leaves the trip's zone in force: every day of
/// the trip fires at 09:00 Tokyo.
#[test]
fn shift_tz_under_a_shift_inside_a_trip() {
    let (r, [trip], window) = tokyo_trip();
    let conference = |zone| {
        (
            2,
            shift(
                "Conference",
                utc(2026, 6, 11, 0, 0),
                Some(utc(2026, 6, 13, 0, 0)),
                zone,
            ),
        )
    };
    let fired = |shifts: &[(u8, Shift)]| {
        let g = r
            .occurrences_under_shifts(window, &TimeZone::UTC, shifts, &ShiftPolicy::ShiftTz)
            .unwrap();
        assert_eq!(generated_keys(&g), june_keys(1..=19, "09:00"));
        g.generate
            .into_iter()
            .filter(|o| (10..=14).contains(&o.occurrence.civil.day()))
            .map(|o| {
                assert!(o.reanchored, "{}", o.occurrence.key);
                (o.occurrence.at, o.planned_at)
            })
            .collect::<Vec<_>>()
    };
    let tokyo = |d| {
        (
            utc(2026, 6, d, 0, 0),
            SunriseTime::zoned(date(2026, 6, d).at(9, 0, 0, 0), "Asia/Tokyo"),
        )
    };
    let kolkata = |d| {
        (
            utc(2026, 6, d, 3, 30),
            SunriseTime::zoned(date(2026, 6, d).at(9, 0, 0, 0), "Asia/Kolkata"),
        )
    };

    let nested = [trip.clone(), conference(Some("Asia/Kolkata"))];
    assert_eq!(
        fired(&nested),
        [tokyo(10), kolkata(11), kolkata(12), tokyo(13), tokyo(14)]
    );

    let zoneless = [trip, conference(None)];
    assert_eq!(fired(&zoneless), (10..=14).map(tokyo).collect::<Vec<_>>());
}

// --- generation across a DST change inside the shift ------------------------

/// "09:00 New York" daily from March 20 2026 (EDT, 13:00Z), and a stay in
/// London from March 25 to April 2 00:00Z, across the UK's spring-forward on
/// March 29 (GMT to BST at 01:00Z).
fn london_stay(at: (i8, i8)) -> (Routine, [(u8, Shift); 1], (Timestamp, Timestamp)) {
    let r = daily(
        "America/New_York",
        date(2026, 3, 20).at(at.0, at.1, 0, 0),
        false,
    );
    let stay = shift(
        "London",
        utc(2026, 3, 25, 0, 0),
        Some(utc(2026, 4, 2, 0, 0)),
        Some("Europe/London"),
    );
    (
        r,
        [(1, stay)],
        (utc(2026, 3, 20, 0, 0), utc(2026, 4, 5, 0, 0)),
    )
}

fn march_april_key(d: (i8, i8), time: &str) -> String {
    format!("2026-{:02}-{:02}T{time}", d.0, d.1)
}

#[test]
fn each_policy_across_a_dst_change_inside_the_shift() {
    let (r, shifts, window) = london_stay((9, 0));
    let utc_tz = TimeZone::UTC;
    let all: Vec<(i8, i8)> = (20..=31)
        .map(|d| (3, d))
        .chain((1..=4).map(|d| (4, d)))
        .collect();
    let covered: Vec<(i8, i8)> = all
        .iter()
        .copied()
        .filter(|&(m, d)| (m == 3 && d >= 25) || (m == 4 && d == 1))
        .collect();
    let key = |d: &(i8, i8)| march_april_key(*d, "09:00");
    let outside: Vec<String> = all
        .iter()
        .filter(|d| !covered.contains(d))
        .map(key)
        .collect();

    for policy in ALL {
        let g = r
            .occurrences_under_shifts(window, &utc_tz, &shifts, &policy)
            .unwrap();
        match policy {
            ShiftPolicy::Pause => {
                assert_eq!(generated_keys(&g), outside);
                assert!(g.skipped.is_empty());
            }
            ShiftPolicy::Skip => {
                assert_eq!(generated_keys(&g), outside);
                assert_eq!(
                    keys(&g.skipped),
                    covered.iter().map(key).collect::<Vec<_>>()
                );
            }
            ShiftPolicy::Unchanged => {
                assert_eq!(generated_keys(&g), all.iter().map(key).collect::<Vec<_>>());
                // New York's 09:00 stays 13:00Z throughout: its own DST
                // change was on March 8.
                assert!(g.generate.iter().all(|o| o.occurrence.at.as_second()
                    == utc(
                        2026,
                        o.occurrence.civil.month(),
                        o.occurrence.civil.day(),
                        13,
                        0
                    )
                    .as_second()));
            }
            ShiftPolicy::ShiftTz => {
                assert_eq!(generated_keys(&g), all.iter().map(key).collect::<Vec<_>>());
                for o in &g.generate {
                    let (m, d) = (o.occurrence.civil.month(), o.occurrence.civil.day());
                    let want = if !covered.contains(&(m, d)) {
                        utc(2026, m, d, 13, 0)
                    } else if m == 3 && d < 29 {
                        // 09:00 GMT.
                        utc(2026, m, d, 9, 0)
                    } else {
                        // 09:00 BST: the wall clock holds across the change.
                        utc(2026, m, d, 8, 0)
                    };
                    assert_eq!(o.occurrence.at, want, "{}", o.occurrence.key);
                    assert_eq!(o.reanchored, covered.contains(&(m, d)));
                }
            }
            ShiftPolicy::Unknown(_) => unreachable!(),
        }
    }
}

/// A re-anchored 01:30 on London's spring-forward night falls in the gap:
/// the one DST rule fires it at 02:30 BST (01:30Z), and its key stays the
/// wall clock the rule asked for.
#[test]
fn shift_tz_resolves_a_key_in_the_destinations_gap_by_the_one_dst_rule() {
    let (r, shifts, window) = london_stay((1, 30));
    let g = r
        .occurrences_under_shifts(window, &TimeZone::UTC, &shifts, &ShiftPolicy::ShiftTz)
        .unwrap();
    let find = |key: &str| {
        g.generate
            .iter()
            .find(|o| o.occurrence.key == key)
            .unwrap()
            .occurrence
            .at
    };
    assert_eq!(find("2026-03-28T01:30"), utc(2026, 3, 28, 1, 30));
    assert_eq!(find("2026-03-29T01:30"), utc(2026, 3, 29, 1, 30));
    assert_eq!(find("2026-03-30T01:30"), utc(2026, 3, 30, 0, 30));
}

// --- the streak --------------------------------------------------------------

/// "09:00 UTC" daily from August 1 2026, completed on the 1st to 5th and the
/// 11th to 15th, with a shift covering the 6th to the 10th.
fn streak_across_a_trip() -> (Routine, [(u8, Shift); 1]) {
    let mut r = daily("UTC", date(2026, 8, 1).at(9, 0, 0, 0), false);
    r.streak_keys = (1..=5)
        .chain(11..=15)
        .map(|d| format!("2026-08-{d:02}T09:00"))
        .collect();
    let trip = shift(
        "Trip",
        utc(2026, 8, 6, 0, 0),
        Some(utc(2026, 8, 11, 0, 0)),
        None,
    );
    (r, [(1, trip)])
}

/// Five missed days break the streak with no shift. A `skip` or `pause`
/// shift over them excuses them, spending no forgiveness; `shift_tz` and
/// `unchanged` do not.
#[test]
fn a_streak_holds_across_a_skip_window() {
    let (r, shifts) = streak_across_a_trip();
    assert_eq!(r.derived_streak().streak_counter, 5);

    let skip = r.derived_streak_across(&shifts, &ShiftPolicy::Skip);
    assert_eq!(skip.streak_counter, 10);
    assert_eq!(skip.forgivenesses_in_window, 0);
    assert_eq!(skip.streak_started_at, Some(utc(2026, 8, 1, 9, 0)));
    assert_eq!(r.derived_streak_across(&shifts, &ShiftPolicy::Pause), skip);

    for policy in [ShiftPolicy::ShiftTz, ShiftPolicy::Unchanged] {
        assert_eq!(
            r.derived_streak_across(&shifts, &policy),
            r.derived_streak()
        );
    }
    let none: [(u8, Shift); 0] = [];
    assert_eq!(
        r.derived_streak_across(&none, &ShiftPolicy::Skip),
        r.derived_streak()
    );
}

/// A miss the shift does not cover still spends the allowance as before.
#[test]
fn a_miss_outside_the_shift_still_counts() {
    let (mut r, shifts) = streak_across_a_trip();
    r.streak_keys.retain(|k| k != "2026-08-12T09:00");
    let s = r.derived_streak_across(&shifts, &ShiftPolicy::Skip);
    assert_eq!(s.streak_counter, 9);
    assert_eq!(s.forgivenesses_in_window, 1);
}

/// A shift still running at the read: the days it covers do not lapse the
/// streak under `skip`, and do under `unchanged`.
#[test]
fn a_running_skip_shift_keeps_the_streak_from_lapsing() {
    let mut r = daily("UTC", date(2026, 8, 1).at(9, 0, 0, 0), false);
    r.streak_keys = (1..=5).map(|d| format!("2026-08-{d:02}T09:00")).collect();
    r.derived_streak().apply_to(&mut r);
    assert_eq!(r.streak_counter, 5);
    let shifts = [(1u8, shift("Trip", utc(2026, 8, 6, 0, 0), None, None))];
    let now = utc(2026, 8, 10, 12, 0);
    let utc_tz = TimeZone::UTC;
    assert_eq!(r.streak_as_of(now, &utc_tz).streak_counter, 0);
    let held = r.streak_as_of_across(now, &utc_tz, &shifts, &ShiftPolicy::Skip);
    assert_eq!(held.streak_counter, 5);
    assert_eq!(held.forgivenesses_in_window, 0);
    assert_eq!(
        r.streak_as_of_across(now, &utc_tz, &shifts, &ShiftPolicy::Unchanged)
            .streak_counter,
        0
    );
}
