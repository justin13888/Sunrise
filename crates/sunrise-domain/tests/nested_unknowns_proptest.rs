//! Unknown fields survive at every nesting level of an entity (ADR-0045 §6,
//! issue #322).
//!
//! Each property takes an entity whose nested values are all populated, lifts
//! its canonical CBOR to a value tree, injects random keys this build does not
//! know into every map in it, and requires the result to decode and re-encode
//! to the very same bytes. A struct that drops a key it does not know fails
//! here at the depth it sits at, which a top-level-only check cannot see.
//!
//! One kind of map is left alone: a `SunriseTime` of a KNOWN kind. Its four
//! shapes are closed, and a newer build that needs another adds a kind, which
//! is what `unknown_time_kinds_survive_in_every_time_field` covers.

use ciborium::value::Value;
use jiff::civil::{date, time};
use jiff::Timestamp;
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use sunrise_cbor::{decode_canonical, encode_canonical};
use sunrise_domain::constraint::{DateRange, ScheduleConstraint, TimeOfDayRange, WeekdaySet};
use sunrise_domain::{
    Block, ConstraintSeverity, RRule, Routine, SunriseTime, Task, TaskTemplate, Unknowns, Weekday,
};
use sunrise_id::{EntityKind, EntityRef};

/// Keys no entity or nested struct knows: every known field name is
/// lowercase `snake_case` without this prefix.
const PREFIX: &str = "zq";

/// A CBOR value with no float, which is what `CborValue` holds.
fn value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        any::<i64>().prop_map(|i| Value::Integer(i.into())),
        ".{0,8}".prop_map(Value::Text),
        proptest::collection::vec(any::<u8>(), 0..8).prop_map(Value::Bytes),
        any::<bool>().prop_map(Value::Bool),
        Just(Value::Null),
    ];
    leaf.prop_recursive(3, 16, 4, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            proptest::collection::btree_map("[a-z]{1,4}", inner, 0..4).prop_map(|m| {
                Value::Map(m.into_iter().map(|(k, v)| (Value::Text(k), v)).collect())
            }),
        ]
    })
}

/// Unknown keys for one map: none to three, each with a random value.
fn injection() -> impl Strategy<Value = BTreeMap<String, Value>> {
    proptest::collection::btree_map("[a-z0-9_]{1,6}", value(), 0..4).prop_map(|m| {
        m.into_iter()
            .map(|(k, v)| (format!("{PREFIX}{k}"), v))
            .collect()
    })
}

/// Whether `m` is a `SunriseTime` of a kind this build knows.
fn is_known_time(m: &[(Value, Value)]) -> bool {
    m.iter().any(|(k, v)| {
        k.as_text() == Some("kind")
            && v.as_text()
                .is_some_and(|s| sunrise_domain::time::kind::KNOWN.contains(&s))
    })
}

/// Inject `per_map[i]` into the `i`-th map met depth first, children before
/// their parent; `next` counts the maps met so far.
fn inject(v: &mut Value, per_map: &[BTreeMap<String, Value>], next: &mut usize) {
    match v {
        Value::Map(m) => {
            // Children first: an injected value is not itself walked.
            for (_, child) in m.iter_mut() {
                inject(child, per_map, next);
            }
            if !is_known_time(m) {
                if let Some(add) = per_map.get(*next) {
                    for (k, val) in add {
                        // A map carries a key once; an unknown time kind's own
                        // fields are drawn from the same key space.
                        if !m.iter().any(|(have, _)| have.as_text() == Some(k)) {
                            m.push((Value::Text(k.clone()), val.clone()));
                        }
                    }
                }
                *next += 1;
            }
        }
        Value::Array(a) => {
            for child in a {
                inject(child, per_map, next);
            }
        }
        _ => {}
    }
}

/// The property: `entity` with `per_map` injected decodes, and re-encodes to
/// the same bytes.
fn survives<T>(entity: &T, per_map: &[BTreeMap<String, Value>]) -> Result<(), TestCaseError>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let mut tree = Value::serialized(entity).expect("an entity lifts to a value tree");
    let mut maps = 0;
    inject(&mut tree, per_map, &mut maps);
    let bytes = encode_canonical(&tree).expect("an injected tree encodes");
    // `decode_canonical` itself refuses a value whose re-encoding differs.
    let back: T = decode_canonical(&bytes)
        .map_err(|e| TestCaseError::fail(format!("decode or re-encode differs: {e}")))?;
    prop_assert_eq!(encode_canonical(&back).unwrap(), bytes);
    Ok(())
}

fn eref(kind: EntityKind, b: u8) -> EntityRef {
    EntityRef::new(kind, [b; 16])
}

fn ts(ms: i64) -> Timestamp {
    Timestamp::from_millisecond(ms).unwrap()
}

/// A constraint with every dimension populated, so each nested map exists.
fn full_constraint() -> ScheduleConstraint {
    ScheduleConstraint {
        time_of_day: Some(TimeOfDayRange::new(time(9, 0, 0, 0), time(17, 0, 0, 0))),
        days_of_week: WeekdaySet::from_days([Weekday::Mo, Weekday::Fr]),
        date_range: Some(DateRange::new(date(2026, 1, 1), Some(date(2026, 12, 31)))),
        severity: ConstraintSeverity::Hard,
        unknown: Unknowns::new(),
    }
}

fn task() -> Task {
    let mut t: Task = serde_json::from_value(serde_json::json!({
        "id": eref(EntityKind::Task, 1),
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
        "stream_id": eref(EntityKind::Stream, 2),
        "title": "t",
        "state": "todo",
    }))
    .expect("a minimal task");
    t.scheduled_at = Some(SunriseTime::zoned(
        date(2026, 3, 1).at(9, 0, 0, 0),
        "Europe/Berlin",
    ));
    t.due_at = Some(SunriseTime::all_day(date(2026, 3, 2)));
    t.scheduling_constraints = vec![full_constraint(), full_constraint()];
    t
}

fn routine() -> Routine {
    let template = TaskTemplate {
        title: "r".into(),
        stream_id: eref(EntityKind::Stream, 2),
        contexts: vec![eref(EntityKind::Context, 3)],
        energy: None,
        priority: Some(2),
        estimated_duration_s: Some(60),
        body: None,
        unknown: Unknowns::new(),
    };
    let mut r: Routine = serde_json::from_value(serde_json::json!({
        "id": eref(EntityKind::Routine, 4),
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
        "template": template,
        "rrule": RRule::parse("FREQ=WEEKLY;BYDAY=MO,WE;WKST=SU").unwrap(),
        "timezone": "UTC",
        "starts_at": "2026-01-01T00:00:00Z",
        "catchup_policy": "skip",
    }))
    .expect("a minimal routine");
    r.scheduling_constraints = vec![full_constraint()];
    r
}

fn block() -> Block {
    Block {
        id: eref(EntityKind::Block, 5),
        created_at: ts(0),
        updated_at: ts(0),
        stream_id: eref(EntityKind::Stream, 2),
        starts_at: SunriseTime::floating(date(2026, 3, 1).at(9, 0, 0, 0)),
        ends_at: SunriseTime::floating(date(2026, 3, 1).at(10, 0, 0, 0)),
        title: Some("b".into()),
        title_track_task: false,
        tasks: BTreeSet::from([eref(EntityKind::Task, 1)]),
        deleted: false,
        unknown: Unknowns::new(),
    }
}

/// A `SunriseTime` of a kind no build ships: random fields, maybe an `at`.
fn unknown_time() -> impl Strategy<Value = SunriseTime> {
    (
        "[a-z]{1,8}_x",
        injection(),
        proptest::option::of(0i64..4_102_444_800_000),
    )
        .prop_map(|(kind, fields, at)| {
            let mut raw: Unknowns = fields
                .into_iter()
                .map(|(k, v)| (k, sunrise_domain::CborValue(v)))
                .collect();
            if let Some(ms) = at {
                raw.insert(
                    "at".into(),
                    sunrise_domain::CborValue(Value::Text(ts(ms).to_string())),
                );
            }
            SunriseTime::Unknown { kind, raw }
        })
}

proptest! {
    #![proptest_config(ProptestConfig {
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/nested_unknowns_proptest.txt",
            ),
        )),
        ..ProptestConfig::default()
    })]

    /// A Task: its own map, each constraint, and each constraint's
    /// time-of-day and date-range maps.
    #[test]
    fn task_keeps_unknown_keys_at_every_level(
        per_map in proptest::collection::vec(injection(), 0..12),
    ) {
        survives(&task(), &per_map)?;
    }

    /// A Routine: its own map, the template, the rule, and the constraints.
    #[test]
    fn routine_keeps_unknown_keys_at_every_level(
        per_map in proptest::collection::vec(injection(), 0..12),
    ) {
        survives(&routine(), &per_map)?;
    }

    /// A Block: its own map (its two times are known kinds).
    #[test]
    fn block_keeps_unknown_keys_at_every_level(
        per_map in proptest::collection::vec(injection(), 0..4),
    ) {
        survives(&block(), &per_map)?;
    }

    /// A time kind this build does not know, in every place a Task and a Block
    /// hold one, with unknown keys around it too.
    #[test]
    fn unknown_time_kinds_survive_in_every_time_field(
        scheduled in unknown_time(),
        due in unknown_time(),
        starts in unknown_time(),
        per_map in proptest::collection::vec(injection(), 0..12),
    ) {
        let mut t = task();
        t.scheduled_at = Some(scheduled);
        t.due_at = Some(due);
        survives(&t, &per_map)?;

        let mut b = block();
        b.starts_at = starts;
        survives(&b, &per_map)?;
    }
}
