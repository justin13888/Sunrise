//! What a series split and a chain's ownership promise for every rule shape,
//! not only the daily rule in one zone the examples in `edit.rs` use: a split
//! partitions the occurrences, and exactly one routine of a chain owns a key
//! (`docs/02-domain/routines-and-recurrence.md` §Edit scope).

use super::tests::{now, routine};
use super::*;
use crate::time::SunriseTime;
use proptest::prelude::*;

/// The zones the properties range over: an hour's and a half hour's DST
/// shift, in both hemispheres, and none at all.
const ZONES: &[&str] = &[
    "America/New_York",
    "Europe/London",
    "Australia/Lord_Howe",
    "UTC",
];

const RULES: &[&str] = &[
    "FREQ=DAILY",
    "FREQ=DAILY;INTERVAL=3",
    "FREQ=WEEKLY",
    "FREQ=WEEKLY;BYDAY=MO,WE,SU",
    "FREQ=WEEKLY;INTERVAL=2;BYDAY=TU,SA",
    "FREQ=MONTHLY",
    "FREQ=MONTHLY;BYMONTHDAY=31",
    "FREQ=MONTHLY;BYDAY=SU;BYSETPOS=2",
    "FREQ=MONTHLY;INTERVAL=2;BYDAY=FR;BYSETPOS=-1",
];

/// Wall clocks that fall in or next to the transitions of [`ZONES`].
const TIMES: &[&str] = &["00:00", "01:30", "01:45", "02:30", "09:00", "23:59"];

fn routine_in(rule: &str, anchor: &str, zone: &str) -> Routine {
    Routine {
        timezone: zone.into(),
        starts_at: SunriseTime::zoned(anchor.parse().expect("civil"), zone),
        ..routine(rule, anchor)
    }
}

/// The keys `r` generates in its own zone, over a span that holds every anchor
/// these tests use and the series for months after it.
fn generated_keys(r: &Routine) -> Vec<String> {
    if r.archived {
        return Vec::new();
    }
    let r = r.clone().normalized();
    // A rule this build cannot read generates nothing, which would leave the
    // properties below nothing to check.
    assert!(r.rrule.is_understood(), "{:?}", r.rrule);
    let window = (
        "2026-01-01T00:00:00Z".parse().expect("ts"),
        "2027-07-01T00:00:00Z".parse().expect("ts"),
    );
    let tz = TimeZone::get(&r.timezone).expect("zone");
    expand(&r.rrule, r.anchor_civil().expect("anchor"), &tz, window)
        .expect("expands")
        .into_iter()
        .map(|o| o.key)
        .collect()
}

/// The ended routine generates exactly the keys before `key`, the continuation
/// exactly the rest, and a `COUNT` is shared out between them.
fn check_split_partitions(r: &Routine, key: &str) -> Result<(), TestCaseError> {
    let keys = generated_keys(r);
    let at = keys
        .iter()
        .position(|k| k == key)
        .unwrap_or_else(|| panic!("{key} is not in {:?}: {keys:?}", r.rrule));
    let s = r.split_at(key, now()).expect("splits");
    prop_assert_eq!(generated_keys(&s.ended), keys[..at].to_vec());
    prop_assert_eq!(s.ended.archived, at == 0);
    prop_assert_eq!(generated_keys(&s.continuation), keys[at..].to_vec());
    let kept = u32::try_from(at).expect("few keys");
    prop_assert_eq!(s.continuation.rrule.count, r.rrule.count.map(|c| c - kept));
    Ok(())
}

/// Keys on a DST gap and a DST fold, which name a wall clock that happens never
/// or twice that day, split like any other.
#[test]
fn a_split_on_a_dst_transition_still_partitions() {
    for (rule, zone, anchor, key) in [
        (
            "FREQ=DAILY;COUNT=20",
            "America/New_York",
            "2026-03-01T02:30",
            "2026-03-08T02:30",
        ),
        (
            "FREQ=WEEKLY;BYDAY=SU",
            "America/New_York",
            "2026-10-11T01:30",
            "2026-11-01T01:30",
        ),
        (
            "FREQ=DAILY",
            "Europe/London",
            "2026-03-20T01:30",
            "2026-03-29T01:30",
        ),
        (
            "FREQ=MONTHLY;BYDAY=SU;BYSETPOS=-1",
            "Europe/London",
            "2026-08-30T01:30",
            "2026-10-25T01:30",
        ),
        (
            "FREQ=WEEKLY;BYDAY=SU",
            "Australia/Lord_Howe",
            "2026-03-22T01:45",
            "2026-04-05T01:45",
        ),
    ] {
        check_split_partitions(&routine_in(rule, anchor, zone), key).unwrap();
    }
}

fn series() -> impl Strategy<Value = Routine> {
    (
        prop::sample::select(RULES),
        proptest::option::of(1u32..40),
        prop::sample::select(ZONES),
        0i32..300,
        prop::sample::select(TIMES),
    )
        .prop_map(|(rule, count, zone, day, time)| {
            let rule = match count {
                Some(n) => format!("{rule};COUNT={n}"),
                None => rule.to_owned(),
            };
            let date = jiff::civil::date(2026, 2, 1)
                .checked_add(jiff::Span::new().days(day))
                .expect("date");
            routine_in(&rule, &format!("{date}T{time}"), zone)
        })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        ..ProptestConfig::default()
    })]

    /// A split at any occurrence of any rule shape partitions the series.
    #[test]
    fn a_split_partitions_the_occurrences(
        r in series(),
        pick in any::<prop::sample::Index>(),
    ) {
        let keys = generated_keys(&r);
        check_split_partitions(&r, &keys[pick.index(keys.len())])?;
    }

    /// However many splits a chain holds, in whatever order it lists them, each
    /// key has exactly one owner: the routine with the latest anchor at or
    /// before it, ties going to the greater id. A key before every anchor has
    /// none.
    #[test]
    fn exactly_one_routine_of_a_chain_owns_each_key(
        r in series(),
        picks in proptest::collection::vec(any::<prop::sample::Index>(), 1..4),
        twin_id in any::<[u8; 16]>(),
    ) {
        let keys = generated_keys(&r);
        let mut chain = vec![r.clone()];
        for pick in &picks {
            let key = &keys[pick.index(keys.len())];
            let next = r.split_at(key, now()).expect("splits").continuation;
            if chain.iter().all(|m| m.id != next.id) {
                chain.push(next);
            }
        }
        // A second routine anchored with the last one, to make a tie.
        let mut twin = chain.last().expect("the root at least").clone();
        twin.id = EntityRef::new(EntityKind::Routine, twin_id);
        twin.split_from = Some(r.id);
        chain.push(twin);

        let anchor_key = |m: &Routine| {
            let a = m.anchor_civil().expect("anchor");
            crate::routine_gen::format_key(a.date(), a.time())
        };
        let owners = |key: &str, chain: &[Routine]| {
            chain
                .iter()
                .filter(|m| m.owns_key(key, chain))
                .map(|m| m.id)
                .collect::<Vec<_>>()
        };
        let mut reversed = chain.clone();
        reversed.reverse();
        for key in &keys {
            let expected = chain
                .iter()
                .filter(|m| anchor_key(m).as_str() <= key.as_str())
                .max_by_key(|m| (anchor_key(m), m.id))
                .map(|m| m.id);
            prop_assert_eq!(owners(key, &chain), Vec::from_iter(expected));
            prop_assert_eq!(owners(key, &reversed), Vec::from_iter(expected));
        }
        prop_assert_eq!(owners("2026-01-01T00:00", &chain), Vec::new());
    }
}

/// Two routines anchored in the same minute tie, whatever their seconds: the
/// greater id owns the key, and the order of the chain does not matter.
#[test]
fn a_tie_on_the_anchor_minute_goes_to_the_greater_id() {
    let r = routine("FREQ=DAILY", "2026-03-01T09:00");
    let continuation = r.split_at("2026-03-04T09:00", now()).unwrap().continuation;
    let with = |id: u8, civil: &str| Routine {
        id: EntityRef::new(EntityKind::Routine, [id; 16]),
        starts_at: SunriseTime::zoned(civil.parse().expect("civil"), "America/New_York"),
        ..continuation.clone()
    };
    let lesser = with(1, "2026-03-04T09:00:40");
    let greater = with(2, "2026-03-04T09:00:00");
    for chain in [
        vec![r.clone(), lesser.clone(), greater.clone()],
        vec![greater.clone(), lesser.clone(), r.clone()],
    ] {
        assert!(greater.owns_key("2026-03-04T09:00", &chain));
        assert!(!lesser.owns_key("2026-03-04T09:00", &chain));
        assert!(!r.owns_key("2026-03-04T09:00", &chain));
    }
}
