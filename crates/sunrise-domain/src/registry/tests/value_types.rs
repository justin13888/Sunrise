//! The wire checks for [`VALUE_TYPES`]: each described value type writes
//! exactly the names, variants, or aliased form its description gives.

use super::*;

/// The map keys serde writes for `value`, against the fields
/// [`VALUE_TYPES`] describes for `name`, less the ones `skipped` names.
fn assert_value_wire_names<T: serde::Serialize>(name: &str, value: &T, skipped: &[&str]) {
    use sunrise_id::registry::ValueShape;
    let spec = VALUE_TYPES
        .iter()
        .find(|v| v.name == name)
        .unwrap_or_else(|| panic!("{name} is not described"));
    let ValueShape::Record { fields, .. } = spec.shape else {
        panic!("{name} is not described as a record");
    };
    let mut buf = Vec::new();
    ciborium::ser::into_writer(value, &mut buf).unwrap();
    let CborValue::Map(entries) = ciborium::de::from_reader(buf.as_slice()).unwrap() else {
        panic!("{name} must encode as a map");
    };
    let written: BTreeSet<String> = entries
        .into_iter()
        .map(|(k, _)| k.into_text().expect("text key"))
        .collect();
    let described: BTreeSet<String> = fields.iter().map(|f| f.name.to_owned()).collect();
    let expected: BTreeSet<String> = described
        .iter()
        .filter(|n| !skipped.contains(&n.as_str()))
        .cloned()
        .collect();
    assert_eq!(
        written, expected,
        "{name}'s wire names drifted from VALUE_TYPES"
    );
    for s in skipped {
        assert!(described.contains(*s), "{name}: `{s}` is not described");
    }
}

fn encode<T: serde::Serialize>(value: &T) -> CborValue {
    let mut buf = Vec::new();
    ciborium::ser::into_writer(value, &mut buf).unwrap();
    ciborium::de::from_reader(buf.as_slice()).unwrap()
}

#[test]
fn value_records_write_their_described_names() {
    use jiff::civil::{date, time};
    assert_value_wire_names(
        "RRule",
        &RRule {
            freq: Frequency::Weekly,
            interval: 1,
            by_day: vec![Weekday::Mo],
            by_month_day: vec![1],
            by_month: vec![1],
            by_set_pos: vec![1],
            count: Some(3),
            until: Some(ts().into()),
            wkst: Some(Weekday::Mo),
            unknown: Unknowns::new(),
        },
        &[],
    );
    assert_value_wire_names(
        "ScheduleConstraint",
        &ScheduleConstraint {
            time_of_day: Some(TimeOfDayRange::new(time(9, 0, 0, 0), time(17, 0, 0, 0))),
            days_of_week: WeekdaySet::from_days([Weekday::Mo]),
            date_range: Some(DateRange::new(date(2024, 1, 1), None)),
            severity: ConstraintSeverity::Hard,
            unknown: Unknowns::new(),
        },
        &[],
    );
    assert_value_wire_names(
        "TimeOfDayRange",
        &TimeOfDayRange::new(time(9, 0, 0, 0), time(17, 0, 0, 0)),
        &[],
    );
    assert_value_wire_names(
        "DateRange",
        &DateRange::new(date(2024, 1, 1), Some(date(2024, 2, 1))),
        &[],
    );
    assert_value_wire_names("Chunk", &Chunk::new(1, 2), &[]);
    assert_value_wire_names("ReviewTotals", &ReviewTotals::default(), &[]);
    assert_value_wire_names(
        "ReviewSnapshotStream",
        &ReviewSnapshotStream {
            stream: eref(EntityKind::Stream, 1),
            name: "s".into(),
            completed: 0,
            deferred: 0,
            created: 0,
            unknown: Unknowns::new(),
        },
        &[],
    );
    assert_value_wire_names(
        "StreakRow",
        &StreakRow {
            routine: eref(EntityKind::Routine, 1),
            title: "r".into(),
            streak: 0,
            last_completed_at_ms: None,
            unknown: Unknowns::new(),
        },
        &[],
    );
}

/// `SunriseTime` is internally tagged by `kind`: each described variant
/// writes its described name there and exactly its described fields, and
/// the described names are every kind this build knows.
#[test]
fn sunrise_time_writes_its_described_variants() {
    use jiff::civil::{date, datetime};
    use sunrise_id::registry::{Tagging, ValueShape, VariantShape};
    let spec = VALUE_TYPES
        .iter()
        .find(|v| v.name == "SunriseTime")
        .expect("SunriseTime is described");
    let ValueShape::Variants {
        tagging: Tagging::Internal(tag),
        variants,
        keeps_unknowns: true,
    } = spec.shape
    else {
        panic!("SunriseTime is internally tagged and keeps unknown kinds");
    };
    let civil = datetime(2024, 6, 1, 9, 0, 0, 0);
    let values = [
        SunriseTime::Instant { at: ts() },
        SunriseTime::Zoned {
            civil,
            tz: "UTC".into(),
        },
        SunriseTime::Floating { civil },
        SunriseTime::AllDay {
            date: date(2024, 6, 1),
        },
    ];
    assert_eq!(values.len(), variants.len());
    for (value, variant) in values.iter().zip(variants) {
        let CborValue::Map(entries) = encode(value) else {
            panic!("{} must encode as a map", variant.rust);
        };
        let mut keys = BTreeSet::new();
        for (k, v) in entries {
            let k = k.into_text().expect("text key");
            if k == tag {
                assert_eq!(v.into_text().expect("text kind"), variant.name);
            } else {
                keys.insert(k);
            }
        }
        let VariantShape::Fields(fields) = variant.shape else {
            panic!("{} carries fields", variant.rust);
        };
        let described: BTreeSet<String> = fields.iter().map(|f| f.name.to_owned()).collect();
        assert_eq!(keys, described, "{}'s fields drifted", variant.rust);
    }
    let names: Vec<&str> = variants.iter().map(|v| v.name).collect();
    assert_eq!(names, crate::time::kind::KNOWN);
}

/// The saved view's filter and its two parts (#341). Every part is skipped
/// when empty, so the full one writes every name and the default none.
#[test]
fn the_saved_view_filter_writes_its_described_names() {
    use crate::{DateWindow, PriorityRange, ViewFilter, WindowField};
    let id = EntityRef::new(sunrise_id::EntityKind::Context, [1; 16]);
    assert_value_wire_names(
        "ViewFilter",
        &ViewFilter {
            streams: vec![EntityRef::new(sunrise_id::EntityKind::Stream, [2; 16])],
            no_stream: true,
            contexts: vec![id],
            context_names: vec!["ghost".into()],
            priority: Some(PriorityRange {
                min: 1,
                max: 2,
                unknown: Unknowns::new(),
            }),
            energy: vec![crate::Energy::Low],
            window: Some(DateWindow {
                field: WindowField::Due,
                from_days: Some(-1),
                to_days: Some(1),
                unknown: Unknowns::new(),
            }),
            lateness: vec![crate::LatenessPart::LateSoft],
            text: "milk".into(),
            unknown: Unknowns::new(),
        },
        &[],
    );
    assert_value_wire_names(
        "ViewFilter",
        &ViewFilter::default(),
        &[
            "streams",
            "no_stream",
            "contexts",
            "context_names",
            "priority",
            "energy",
            "window",
            "lateness",
            "text",
        ],
    );
    assert_value_wire_names(
        "PriorityRange",
        &PriorityRange {
            min: 1,
            max: 5,
            unknown: Unknowns::new(),
        },
        &[],
    );
    assert_value_wire_names(
        "DateWindow",
        &DateWindow {
            field: WindowField::Scheduled,
            from_days: None,
            to_days: None,
            unknown: Unknowns::new(),
        },
        &["from_days", "to_days"],
    );
}

/// The two value types whose wire form is another type: `NoteBody` is
/// its bytes, and `WeekdaySet`, whose serde is hand-written, is a list of
/// weekday spellings.
#[test]
fn aliased_value_types_write_the_type_they_name() {
    use sunrise_id::registry::ValueShape;
    let alias = |name: &str| {
        let spec = VALUE_TYPES.iter().find(|v| v.name == name).expect(name);
        let ValueShape::Alias(ty) = spec.shape else {
            panic!("{name} is described as an alias");
        };
        ty.replace(' ', "")
    };
    assert_eq!(alias("NoteBody"), "Vec<u8>");
    assert_eq!(encode(&NoteBody(vec![1, 2])), CborValue::Bytes(vec![1, 2]));
    assert_eq!(alias("WeekdaySet"), "Vec<Weekday>");
    assert_eq!(
        encode(&WeekdaySet::from_days([Weekday::Mo, Weekday::Fr])),
        CborValue::Array(vec![
            CborValue::Text("MO".into()),
            CborValue::Text("FR".into())
        ])
    );
}

/// Every described record is reached by a wire test above.
#[test]
fn every_described_value_record_is_checked_on_the_wire() {
    use sunrise_id::registry::ValueShape;
    let checked = [
        "RRule",
        "ScheduleConstraint",
        "TimeOfDayRange",
        "DateRange",
        "Chunk",
        "ReviewTotals",
        "ReviewSnapshotStream",
        "StreakRow",
        "ViewFilter",
        "PriorityRange",
        "DateWindow",
    ];
    for v in VALUE_TYPES {
        if matches!(v.shape, ValueShape::Record { .. }) {
            assert!(
                checked.contains(&v.name),
                "{} has no wire-name check here",
                v.name
            );
        }
    }
}
