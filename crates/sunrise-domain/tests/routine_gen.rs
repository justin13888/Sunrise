//! Golden DST cases + property tests for the RRULE expansion engine.

use jiff::civil::{DateTime, Time};
use jiff::tz::TimeZone;
use jiff::Timestamp;
use sunrise_domain::routine_gen::{expand, occurrence_task_id, Occurrence};
use sunrise_domain::rrule::{Frequency, RRule, Weekday};
use sunrise_id::{EntityKind, EntityRef};

/// Build an absolute instant from a wall-clock civil datetime in `tz`,
/// resolving DST via the same Compatible policy the engine uses.
fn at(tz: &TimeZone, y: i16, mo: i8, d: i8, h: i8, mi: i8) -> Timestamp {
    let dt = DateTime::from_parts(
        jiff::civil::Date::new(y, mo, d).unwrap(),
        Time::new(h, mi, 0, 0).unwrap(),
    );
    tz.to_ambiguous_zoned(dt).compatible().unwrap().timestamp()
}

fn rule(body: &str) -> RRule {
    RRule::parse(body).unwrap()
}

fn keys(occ: &[Occurrence]) -> Vec<String> {
    occ.iter().map(|o| o.key.clone()).collect()
}

/// Convert an occurrence instant back to a local civil time in `tz`.
fn local(tz: &TimeZone, ts: Timestamp) -> DateTime {
    ts.to_zoned(tz.clone()).datetime()
}

#[test]
fn golden_a_spring_forward_shifts_to_0330() {
    // America/Los_Angeles, daily 02:30. 2025-03-09 02:30 is in the DST gap
    // (02:00 -> 03:00); Compatible shifts forward to 03:30 local.
    let tz = TimeZone::get("America/Los_Angeles").unwrap();
    let r = rule("FREQ=DAILY");
    let anchor = at(&tz, 2025, 3, 1, 2, 30);
    let window = (at(&tz, 2025, 3, 1, 0, 0), at(&tz, 2025, 3, 15, 0, 0));
    let occ = expand(&r, anchor, &tz, window).unwrap();

    let march9 = occ
        .iter()
        .find(|o| o.key == "2025-03-09T02:30")
        .expect("2025-03-09 occurrence exists with the intended wall-clock key");
    let l = local(&tz, march9.at);
    assert_eq!(l.hour(), 3, "gap shifts 02:30 forward to 03:30 local");
    assert_eq!(l.minute(), 30);
    // Key is the *intended* wall clock, immune to the DST shift.
    assert_eq!(march9.key, "2025-03-09T02:30");
}

#[test]
fn golden_b_fall_back_takes_earlier_offset() {
    // America/Los_Angeles, daily 01:30. 2025-11-02 01:30 happens twice;
    // Compatible (fold) takes the earlier offset (PDT, -07:00).
    let tz = TimeZone::get("America/Los_Angeles").unwrap();
    let r = rule("FREQ=DAILY");
    let anchor = at(&tz, 2025, 11, 1, 1, 30);
    let window = (at(&tz, 2025, 11, 1, 0, 0), at(&tz, 2025, 11, 5, 0, 0));
    let occ = expand(&r, anchor, &tz, window).unwrap();

    let nov2 = occ.iter().find(|o| o.key == "2025-11-02T01:30").unwrap();
    let z = nov2.at.to_zoned(tz.clone());
    // Earlier offset is PDT = -7h.
    assert_eq!(
        z.offset().seconds(),
        -7 * 3600,
        "fold resolves to the earlier (pre-fall-back) offset"
    );
}

#[test]
fn golden_c_lord_howe_30min_dst_gap() {
    // Australia/Lord_Howe has a 30-minute DST shift. Spring forward on the
    // first Sunday of October (2025-10-05) jumps 02:00 -> 02:30, so a 02:15
    // routine lands in the gap and shifts forward to 02:45.
    let tz = TimeZone::get("Australia/Lord_Howe").unwrap();
    let r = rule("FREQ=DAILY");
    let anchor = at(&tz, 2025, 10, 1, 2, 15);
    let window = (at(&tz, 2025, 10, 1, 0, 0), at(&tz, 2025, 10, 10, 0, 0));
    let occ = expand(&r, anchor, &tz, window).unwrap();

    let oct5 = occ.iter().find(|o| o.key == "2025-10-05T02:15").unwrap();
    let l = local(&tz, oct5.at);
    assert_eq!(l.hour(), 2);
    assert_eq!(l.minute(), 45, "02:15 in the 30-min gap shifts to 02:45");
}

#[test]
fn golden_d_monthly_last_friday_via_setpos() {
    let tz = TimeZone::get("UTC").unwrap();
    let r = rule("FREQ=MONTHLY;BYDAY=FR;BYSETPOS=-1");
    let anchor = at(&tz, 2025, 1, 1, 9, 0);
    let window = (at(&tz, 2025, 1, 1, 0, 0), at(&tz, 2025, 7, 1, 0, 0));
    let occ = expand(&r, anchor, &tz, window).unwrap();
    assert_eq!(
        keys(&occ),
        vec![
            "2025-01-31T09:00",
            "2025-02-28T09:00",
            "2025-03-28T09:00",
            "2025-04-25T09:00",
            "2025-05-30T09:00",
            "2025-06-27T09:00",
        ]
    );
}

#[test]
fn golden_e_weekly_interval2_wkst_sensitivity() {
    let tz = TimeZone::get("UTC").unwrap();
    // Anchor on a Sunday; BYDAY spans the Sun/Mon week boundary so WKST
    // changes which fortnight each day lands in.
    let anchor = at(&tz, 2025, 1, 5, 8, 0); // 2025-01-05 is a Sunday
    let window = (at(&tz, 2025, 1, 1, 0, 0), at(&tz, 2025, 2, 1, 0, 0));

    let wkst_mo = expand(
        &rule("FREQ=WEEKLY;INTERVAL=2;BYDAY=SU,TU;WKST=MO"),
        anchor,
        &tz,
        window,
    )
    .unwrap();
    let wkst_su = expand(
        &rule("FREQ=WEEKLY;INTERVAL=2;BYDAY=SU,TU;WKST=SU"),
        anchor,
        &tz,
        window,
    )
    .unwrap();

    let mo = keys(&wkst_mo);
    let su = keys(&wkst_su);
    // Both share the anchor and the second on-week's Sunday.
    assert!(mo.contains(&"2025-01-05T08:00".to_string()));
    assert!(su.contains(&"2025-01-05T08:00".to_string()));
    assert!(mo.contains(&"2025-01-19T08:00".to_string()));
    assert!(su.contains(&"2025-01-19T08:00".to_string()));
    // WKST=SU keeps Tue 2025-01-07 in the first on-week; WKST=MO pushes the
    // next Tue out to 2025-01-14.
    assert!(su.contains(&"2025-01-07T08:00".to_string()));
    assert!(!mo.contains(&"2025-01-07T08:00".to_string()));
    assert!(mo.contains(&"2025-01-14T08:00".to_string()));
    assert!(!su.contains(&"2025-01-14T08:00".to_string()));
    assert_ne!(mo, su, "WKST materially changes the expansion");
}

#[test]
fn golden_f_count_and_until_respected() {
    let tz = TimeZone::get("UTC").unwrap();
    let anchor = at(&tz, 2025, 1, 1, 9, 0);
    let wide = (at(&tz, 2025, 1, 1, 0, 0), at(&tz, 2026, 1, 1, 0, 0));

    let counted = expand(&rule("FREQ=DAILY;COUNT=3"), anchor, &tz, wide).unwrap();
    assert_eq!(
        keys(&counted).len(),
        3,
        "COUNT bounds the series from anchor"
    );
    assert_eq!(
        keys(&counted),
        vec!["2025-01-01T09:00", "2025-01-02T09:00", "2025-01-03T09:00"]
    );

    let until = expand(
        &rule("FREQ=DAILY;UNTIL=2025-01-03T09:00:00Z"),
        anchor,
        &tz,
        wide,
    )
    .unwrap();
    // 01-01, 01-02, 01-03 all <= UNTIL (inclusive).
    assert_eq!(keys(&until).len(), 3);
    assert_eq!(until.last().unwrap().key, "2025-01-03T09:00");
}

#[test]
fn golden_g_invalid_monthday_skipped() {
    let tz = TimeZone::get("UTC").unwrap();
    let r = rule("FREQ=MONTHLY;BYMONTHDAY=31");
    let anchor = at(&tz, 2025, 1, 31, 8, 0);
    let window = (at(&tz, 2025, 1, 1, 0, 0), at(&tz, 2025, 9, 1, 0, 0));
    let occ = keys(&expand(&r, anchor, &tz, window).unwrap());
    // Months with a 31st only: Jan, Mar, May, Jul, Aug.
    assert_eq!(
        occ,
        vec![
            "2025-01-31T08:00",
            "2025-03-31T08:00",
            "2025-05-31T08:00",
            "2025-07-31T08:00",
            "2025-08-31T08:00",
        ]
    );
    assert!(!occ.iter().any(|k| k.starts_with("2025-02")));
    assert!(!occ.iter().any(|k| k.starts_with("2025-04")));
}

#[test]
fn task_id_derivation_is_deterministic_and_key_sensitive() {
    let rid = EntityRef::new(EntityKind::Routine, [7u8; 16]);
    let a = occurrence_task_id(&rid, "2025-01-31T08:00");
    let b = occurrence_task_id(&rid, "2025-01-31T08:00");
    let c = occurrence_task_id(&rid, "2025-02-28T08:00");
    assert_eq!(a, b, "same inputs -> same id");
    assert_ne!(a, c, "different key -> different id");
    assert_eq!(a.kind(), EntityKind::Task);
}

mod prop {
    use super::*;
    use proptest::prelude::*;

    const TZS: &[&str] = &[
        "America/Los_Angeles",
        "Europe/London",
        "America/Sao_Paulo",
        "Australia/Lord_Howe",
        "UTC",
    ];

    fn weekday_strat() -> impl Strategy<Value = Weekday> {
        prop_oneof![
            Just(Weekday::Su),
            Just(Weekday::Mo),
            Just(Weekday::Tu),
            Just(Weekday::We),
            Just(Weekday::Th),
            Just(Weekday::Fr),
            Just(Weekday::Sa),
        ]
    }

    fn freq_strat() -> impl Strategy<Value = Frequency> {
        prop_oneof![
            Just(Frequency::Daily),
            Just(Frequency::Weekly),
            Just(Frequency::Monthly),
            Just(Frequency::Yearly),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(400))]

        #[test]
        fn expansion_invariants(
            tz_idx in 0usize..TZS.len(),
            freq in freq_strat(),
            interval in 1u32..=3,
            by_day in proptest::collection::vec(weekday_strat(), 0..3),
            by_set_pos in proptest::collection::vec(-2i32..=2, 0..2),
            count in proptest::option::of(1u32..=6),
            // Anchor within a bounded epoch-day range so we never approach the
            // iteration cap.
            anchor_day in 16_000i64..17_500,
            tod_min in 0i64..1440,
            win_start_off in -30i64..365,
            win_len in 1i64..120,
        ) {
            let tz = TimeZone::get(TZS[tz_idx]).unwrap();
            let anchor = Timestamp::from_millisecond(
                (anchor_day * 86_400 + tod_min * 60) * 1000,
            ).unwrap();
            let rrule = RRule {
                freq,
                interval,
                by_day,
                by_month_day: Vec::new(),
                by_month: Vec::new(),
                by_set_pos,
                count,
                until: None,
                wkst: None,
            };
            let ws = Timestamp::from_millisecond(
                ((anchor_day + win_start_off) * 86_400) * 1000,
            ).unwrap();
            let we = Timestamp::from_millisecond(
                ((anchor_day + win_start_off + win_len) * 86_400) * 1000,
            ).unwrap();
            let window = (ws, we);

            let Ok(occ) = expand(&rrule, anchor, &tz, window) else {
                return Ok(());
            };

            // Determinism: same inputs, same outputs.
            let occ2 = expand(&rrule, anchor, &tz, window).unwrap();
            prop_assert_eq!(&occ, &occ2);

            // All within window, strictly increasing instants, unique keys.
            let mut seen = std::collections::BTreeSet::new();
            for w in occ.windows(2) {
                prop_assert!(w[0].at < w[1].at, "strictly increasing instants");
            }
            for o in &occ {
                prop_assert!(o.at >= ws && o.at < we, "within window");
                prop_assert!(seen.insert(o.key.clone()), "unique keys");
            }
            // COUNT bounds the in-window slice too.
            if let Some(c) = count {
                prop_assert!(occ.len() <= c as usize, "COUNT respected");
            }
        }
    }
}
