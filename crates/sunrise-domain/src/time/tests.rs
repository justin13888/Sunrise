//! Unit tests for [`super::SunriseTime`]: each kind resolves where it should,
//! indexes stably, and round-trips through storage and the wire.

use super::*;
use crate::unknown::CborValue;
use ciborium::value::Value;
use jiff::tz::AmbiguousOffset;
use jiff::Span;
use proptest::prelude::*;

fn ny() -> TimeZone {
    TimeZone::get("America/New_York").unwrap()
}

#[test]
fn an_instant_is_the_same_moment_in_every_zone() {
    let t = SunriseTime::instant(Timestamp::from_millisecond(1_700_000_000_000).unwrap());
    assert_eq!(
        t.to_instant(&TimeZone::UTC).unwrap(),
        t.to_instant(&ny()).unwrap()
    );
}

/// The point of `Zoned`: the zone travels with the value.
#[test]
fn a_zoned_time_ignores_the_reading_device() {
    let t = SunriseTime::zoned(civil::date(2024, 6, 1).at(9, 0, 0, 0), "America/New_York");
    let berlin = TimeZone::get("Europe/Berlin").unwrap();
    assert_eq!(
        t.to_instant(&berlin).unwrap(),
        t.to_instant(&TimeZone::UTC).unwrap()
    );
    // 09:00 EDT on 2024-06-01 is 13:00 UTC.
    assert_eq!(
        t.to_instant(&TimeZone::UTC)
            .unwrap()
            .to_zoned(TimeZone::UTC)
            .datetime()
            .hour(),
        13
    );
}

/// ...and the point of `Floating`: it follows the reader instead.
#[test]
fn a_floating_time_follows_the_device_zone() {
    let t = SunriseTime::floating(civil::date(2024, 6, 1).at(9, 0, 0, 0));
    assert_ne!(
        t.to_instant(&TimeZone::UTC).unwrap(),
        t.to_instant(&ny()).unwrap()
    );
    for zone in [TimeZone::UTC, ny()] {
        assert_eq!(
            t.to_instant(&zone)
                .unwrap()
                .to_zoned(zone.clone())
                .datetime()
                .hour(),
            9,
            "a floating 09:00 is 09:00 wherever it is read"
        );
    }
}

/// The bug the AllDay kind exists to prevent: a birthday on the 4th
/// showing as the 3rd for everyone west of UTC.
#[test]
fn an_all_day_date_is_that_date_in_the_reading_zone() {
    let t = SunriseTime::all_day(civil::date(2024, 7, 4));
    for zone in [
        TimeZone::UTC,
        ny(),
        TimeZone::get("Pacific/Auckland").unwrap(),
    ] {
        let d = t.to_instant(&zone).unwrap().to_zoned(zone.clone()).date();
        assert_eq!(d, civil::date(2024, 7, 4), "in {zone:?}");
    }
}

#[test]
fn every_kind_round_trips_through_storage() {
    let cases = [
        SunriseTime::instant(Timestamp::from_millisecond(1_700_000_000_123).unwrap()),
        SunriseTime::zoned(civil::date(2024, 6, 1).at(9, 30, 0, 0), "America/New_York"),
        SunriseTime::floating(civil::date(2024, 6, 1).at(9, 30, 0, 0)),
        SunriseTime::all_day(civil::date(2024, 7, 4)),
        lunar(Some("2024-06-01T09:30:00Z")),
        lunar(None),
        SunriseTime::Unknown {
            kind: "bare".into(),
            raw: Unknowns::new(),
        },
    ];
    for c in cases {
        let (ms, k, tz) = c.to_parts();
        let civil = c.civil_part();
        assert_eq!(
            SunriseTime::from_parts(ms, k, tz.as_deref(), civil.as_deref()),
            c,
            "round trip {c:?}"
        );
    }
}

/// A stored wall-clock time is stored, not recomputed (ADR-0066 §2): with its
/// civil column, a zoned value reads back as written whatever its index key
/// says, so a tzdata change that moved the instant cannot move 09:00, and a
/// zone this build does not know keeps its civil time and name.
#[test]
fn a_stored_civil_time_is_read_back_without_its_zone() {
    let civil = civil::date(2026, 3, 8).at(2, 30, 0, 0);
    let in_gap = SunriseTime::zoned(civil, "America/New_York");
    let (ms, kind, tz) = in_gap.to_parts();
    let text = in_gap.civil_part();
    assert_eq!(text.as_deref(), Some("2026-03-08T02:30:00"));
    // The index key resolves the gap to 03:30 EDT; the civil column keeps
    // 02:30, which re-deriving from the key would lose.
    assert_eq!(
        SunriseTime::from_parts(ms, kind, tz.as_deref(), text.as_deref()),
        in_gap
    );
    assert_ne!(
        SunriseTime::from_parts(ms, kind, tz.as_deref(), None),
        in_gap
    );

    // A key a tzdata change has left an hour stale.
    let stale = ms + 3_600_000;
    assert_eq!(
        SunriseTime::from_parts(stale, kind, tz.as_deref(), text.as_deref()),
        in_gap
    );

    // A zone this build cannot resolve: civil time and name, byte for byte.
    let unknown = SunriseTime::zoned(civil::date(2026, 6, 1).at(9, 0, 0, 0), "Mars/Olympus_Mons");
    let (ms, kind, tz) = unknown.to_parts();
    assert_eq!(
        SunriseTime::from_parts(ms, kind, tz.as_deref(), unknown.civil_part().as_deref()),
        unknown
    );

    // A civil value at the calendar's edge keeps its date.
    let edge = SunriseTime::all_day(civil::Date::MAX);
    let (ms, kind, tz) = edge.to_parts();
    assert_eq!(
        SunriseTime::from_parts(ms, kind, tz.as_deref(), edge.civil_part().as_deref()),
        edge
    );
}

/// A kind a newer build added, with an optional `at` anchor.
fn lunar(at: Option<&str>) -> SunriseTime {
    let mut raw = Unknowns::new();
    raw.insert(
        "phase".into(),
        CborValue(ciborium::value::Value::Text("waxing".into())),
    );
    if let Some(at) = at {
        raw.insert(
            "at".into(),
            CborValue(ciborium::value::Value::Text(at.into())),
        );
    }
    SunriseTime::Unknown {
        kind: "lunar".into(),
        raw,
    }
}

#[test]
fn the_unanchored_key_is_the_start_of_9999() {
    assert_eq!(
        Timestamp::from_millisecond(UNANCHORED_MS).unwrap(),
        "9999-01-01T00:00:00Z".parse::<Timestamp>().unwrap()
    );
}

/// A kind this build does not know decodes, keeps every field, and writes
/// back the bytes it arrived as.
#[test]
fn an_unknown_kind_round_trips_byte_exact() {
    for t in [lunar(Some("2024-06-01T09:30:00Z")), lunar(None)] {
        let bytes = sunrise_cbor::encode_canonical(&t).unwrap();
        let back: SunriseTime = sunrise_cbor::decode_canonical(&bytes).unwrap();
        assert_eq!(back, t);
        assert!(back.is_unknown());
        assert_eq!(back.kind_str(), "lunar");
        assert_eq!(sunrise_cbor::encode_canonical(&back).unwrap(), bytes);
    }
    let j = serde_json::json!({ "kind": "lunar", "phase": "waxing" });
    let t: SunriseTime = serde_json::from_value(j.clone()).unwrap();
    assert_eq!(t, lunar(None));
    assert_eq!(serde_json::to_value(&t).unwrap(), j);
}

/// A known kind whose fields do not parse is still an error: it is not
/// "a kind this build does not know".
#[test]
fn a_malformed_known_kind_is_refused_not_kept() {
    for j in [
        serde_json::json!({ "kind": "instant", "at": "not a time" }),
        serde_json::json!({ "kind": "zoned", "civil": "2024-06-01T09:00:00" }),
    ] {
        assert!(serde_json::from_value::<SunriseTime>(j).is_err());
    }
}

/// Placed by its `at` when it has one; after everything else when not.
#[test]
fn an_unknown_kind_orders_by_its_anchor_or_last() {
    let anchored = lunar(Some("2024-06-01T09:30:00Z"));
    assert_eq!(
        anchored.index_ms(),
        "2024-06-01T09:30:00Z"
            .parse::<Timestamp>()
            .unwrap()
            .as_millisecond()
    );
    assert_eq!(anchored.index_key(), Some(anchored.index_ms()));

    let unanchored = lunar(None);
    assert_eq!(unanchored.index_ms(), UNANCHORED_MS);
    assert_eq!(unanchored.index_key(), None);

    let mut v = [
        unanchored.clone(),
        SunriseTime::all_day(civil::date(2024, 7, 4)),
        anchored.clone(),
        SunriseTime::instant(Timestamp::from_millisecond(0).unwrap()),
    ];
    v.sort();
    assert_eq!(v[1], anchored);
    assert_eq!(v[3], unanchored);

    // Two unknown values that differ only in a field are not Equal.
    let mut other = lunar(None);
    if let SunriseTime::Unknown { raw, .. } = &mut other {
        raw.insert(
            "phase".into(),
            CborValue(ciborium::value::Value::Text("waning".into())),
        );
    }
    assert_ne!(unanchored.cmp(&other), std::cmp::Ordering::Equal);
}

/// A row with an unknown kind and no sidecar this build wrote has nothing
/// to rebuild from; it degrades to an instant, as before.
#[test]
fn an_unknown_kind_without_its_sidecar_degrades_to_an_instant() {
    for tz in [None, Some("Europe/Berlin"), Some("cbor:zz"), Some("cbor:0")] {
        assert_eq!(
            SunriseTime::from_parts(1_700_000_000_000, "lunar_phase", tz, None),
            SunriseTime::instant(Timestamp::from_millisecond(1_700_000_000_000).unwrap()),
            "sidecar {tz:?}"
        );
    }
}

/// The index key must not move when the device's zone does — an index
/// built on one device has to stay valid on another.
#[test]
fn the_index_key_is_independent_of_the_device_zone() {
    let t = SunriseTime::floating(civil::date(2024, 6, 1).at(9, 30, 0, 0));
    let before = t.index_ms();
    // `index_ms` takes no zone argument at all; this asserts the contract
    // rather than the implementation, by checking the value equals the UTC
    // resolution and nothing else.
    assert_eq!(
        before,
        t.to_instant(&TimeZone::UTC).unwrap().as_millisecond()
    );
}

#[test]
fn ordering_matches_the_storage_index() {
    let mut v = [
        SunriseTime::all_day(civil::date(2024, 7, 4)),
        SunriseTime::instant(Timestamp::from_millisecond(0).unwrap()),
        SunriseTime::floating(civil::date(2024, 6, 1).at(9, 30, 0, 0)),
    ];
    v.sort();
    let keys: Vec<i64> = v.iter().map(SunriseTime::index_ms).collect();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    assert_eq!(keys, sorted);
}

#[test]
fn display_shows_the_kind() {
    assert_eq!(
        SunriseTime::zoned(civil::date(2024, 6, 1).at(9, 0, 0, 0), "America/New_York").to_string(),
        "2024-06-01T09:00:00[America/New_York]"
    );
    assert_eq!(
        SunriseTime::floating(civil::date(2024, 6, 1).at(9, 0, 0, 0)).to_string(),
        "2024-06-01T09:00:00"
    );
    assert_eq!(
        SunriseTime::all_day(civil::date(2024, 7, 4)).to_string(),
        "2024-07-04"
    );
    assert_eq!(
        lunar(Some("2024-06-01T09:30:00Z")).to_string(),
        "2024-06-01T09:30:00Z[unknown kind lunar]"
    );
    assert_eq!(lunar(None).to_string(), "[unknown kind lunar]");
}

/// A payload written at `DOC_SCHEMA_V = 1`, where the field was a bare
/// instant with no kind, still reads.
#[test]
fn a_pre_schema_bare_instant_still_deserializes() {
    let legacy =
        sunrise_cbor::encode_canonical(&Timestamp::from_millisecond(1_762_680_600_789).unwrap())
            .expect("encode legacy");
    let t: SunriseTime = ciborium::de::from_reader(&legacy[..]).expect("legacy decodes");
    assert_eq!(
        t,
        SunriseTime::instant(Timestamp::from_millisecond(1_762_680_600_789).unwrap())
    );
}

#[test]
fn cbor_round_trips_every_kind() {
    for c in [
        SunriseTime::instant(Timestamp::from_millisecond(1).unwrap()),
        SunriseTime::zoned(civil::date(2024, 6, 1).at(9, 0, 0, 0), "Europe/Berlin"),
        SunriseTime::floating(civil::date(2024, 6, 1).at(9, 0, 0, 0)),
        SunriseTime::all_day(civil::date(2024, 7, 4)),
    ] {
        let bytes = sunrise_cbor::encode_canonical(&c).expect("encode");
        let back: SunriseTime = sunrise_cbor::decode_canonical(&bytes).expect("decode");
        assert_eq!(back, c);
    }
}

/// Any date of the calendar, its two ends included.
fn any_date() -> impl Strategy<Value = civil::Date> {
    let general = (-9999i16..=9999, 1i8..=12, 1i8..=31)
        .prop_map(|(y, m, d)| civil::date(y, m, d.min(civil::date(y, m, 1).days_in_month())));
    prop_oneof![
        8 => general,
        1 => Just(civil::Date::MIN),
        1 => Just(civil::Date::MAX),
    ]
}

/// Any wall-clock time, sub-second included, on any date.
fn any_civil() -> impl Strategy<Value = civil::DateTime> {
    let nanos = prop_oneof![Just(0i32), 0i32..1_000_000_000];
    let general =
        (any_date(), 0i8..24, 0i8..60, 0i8..60, nanos).prop_map(|(d, h, m, s, n)| d.at(h, m, s, n));
    prop_oneof![
        8 => general,
        1 => Just(civil::DateTime::MIN),
        1 => Just(civil::DateTime::MAX),
    ]
}

/// A zone name from the bundled tzdb.
fn any_zone() -> impl Strategy<Value = String> {
    let names: Vec<String> = jiff::tz::db()
        .available()
        .map(|name| name.to_string())
        .collect();
    assert!(!names.is_empty(), "the tzdb is bundled");
    proptest::sample::select(names)
}

/// A zone name as a value can carry one: mostly real, sometimes not a zone.
fn any_zone_name() -> impl Strategy<Value = String> {
    prop_oneof![8 => any_zone(), 1 => Just("Mars/Olympus_Mons".to_owned()), 1 => any::<String>()]
}

/// An instant at millisecond resolution, which is what the index column holds:
/// negative ones and both ends of the range `from_millisecond` admits included.
/// That range stops at `Timestamp::MAX`'s whole second, short of its fraction.
fn any_ms_instant() -> impl Strategy<Value = Timestamp> {
    let (min, max) = (
        Timestamp::MIN.as_millisecond(),
        Timestamp::MAX.as_second() * 1000,
    );
    prop_oneof![
        6 => min..=max,
        2 => -86_400_000i64..=86_400_000,
        1 => Just(min),
        1 => Just(max),
    ]
    .prop_map(|ms| Timestamp::from_millisecond(ms).expect("in range"))
}

/// An instant at the full nanosecond resolution the wire carries, short of
/// `Timestamp::MAX`'s second: storage cannot rebuild that one's key (#898), so
/// only the wire-only property below draws from it.
fn any_nano_instant() -> impl Strategy<Value = Timestamp> {
    let seconds = (Timestamp::MIN.as_second() + 1)..Timestamp::MAX.as_second();
    prop_oneof![seconds, -86_400i64..=86_400]
        .prop_flat_map(|s| (Just(s), 0i32..1_000_000_000))
        .prop_map(|(s, n)| Timestamp::new(s, n).expect("in range"))
}

/// A kind a newer build added, with whatever fields it chose.
fn any_unknown() -> impl Strategy<Value = SunriseTime> {
    let name = prop_oneof![
        Just("at".to_owned()),
        Just("phase".to_owned()),
        any::<String>()
    ]
    .prop_filter("the kind has its own key", |k| k != "kind");
    let value = prop_oneof![
        any::<String>().prop_map(Value::Text),
        any::<i64>().prop_map(|i| Value::Integer(i.into())),
        any::<bool>().prop_map(Value::Bool),
        proptest::collection::vec(any::<u8>(), 0..8).prop_map(Value::Bytes),
        any_nano_instant().prop_map(|at| Value::Text(at.to_string())),
    ];
    let kind = any::<String>().prop_filter("a kind this build does not know", |k| {
        !kind::KNOWN.contains(&k.as_str())
    });
    let raw = proptest::collection::btree_map(name, value.prop_map(CborValue), 0..4);
    (kind, raw).prop_map(|(kind, raw)| SunriseTime::Unknown { kind, raw })
}

/// A value storage can hold exactly: an instant is at millisecond resolution.
fn any_storable_time() -> impl Strategy<Value = SunriseTime> {
    prop_oneof![
        any_ms_instant().prop_map(SunriseTime::instant),
        (any_civil(), any_zone_name()).prop_map(|(c, tz)| SunriseTime::zoned(c, tz)),
        any_civil().prop_map(SunriseTime::floating),
        any_date().prop_map(SunriseTime::all_day),
        any_unknown(),
    ]
}

/// A wall-clock time inside a real gap or fold of a tzdb zone: between where
/// the clock stood and where it landed at the first offset change after a
/// random instant. Values that turn out unambiguous are rejected, so every
/// case that runs is a gap or a fold, and proptest fails the run if too few
/// are.
fn any_gap_or_fold() -> impl Strategy<Value = (String, civil::DateTime, AmbiguousOffset)> {
    // 1900 to 2037: most zones change offset at least once from here.
    let from = -2_208_988_800i64..2_114_380_800;
    (any_zone(), from, 0i64..1000).prop_filter_map(
        "no offset change to land in",
        |(name, from, thousandths)| {
            let zone = TimeZone::get(&name).ok()?;
            let shift = zone.following(Timestamp::from_second(from).ok()?).next()?;
            let at = shift.timestamp();
            let before = zone.to_offset(at.checked_sub(Span::new().seconds(1)).ok()?);
            let after = shift.offset();
            let width = i64::from(after.seconds() - before.seconds());
            let start = before.to_datetime(at).min(after.to_datetime(at));
            let civil = start
                .checked_add(Span::new().seconds(width.abs() * thousandths / 1000))
                .ok()?;
            let offset = zone.to_ambiguous_timestamp(civil).offset();
            (!matches!(offset, AmbiguousOffset::Unambiguous { .. }))
                .then_some((name, civil, offset))
        },
    )
}

#[track_caller]
fn assert_storage_round_trip(t: &SunriseTime) {
    let (ms, kind, tz) = t.to_parts();
    let civil = t.civil_part();
    let back = SunriseTime::from_parts(ms, kind, tz.as_deref(), civil.as_deref());
    assert_eq!(&back, t);
    assert_eq!(back.to_parts(), (ms, kind, tz), "the columns are stable");
}

#[track_caller]
fn assert_wire_round_trip(t: &SunriseTime) {
    let bytes = sunrise_cbor::encode_canonical(t).expect("encode");
    let back: SunriseTime = sunrise_cbor::decode_canonical(&bytes).expect("decode");
    assert_eq!(&back, t);
    assert_eq!(
        sunrise_cbor::encode_canonical(&back).expect("re-encode"),
        bytes
    );
}

proptest! {
    #![proptest_config(ProptestConfig {
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        ..ProptestConfig::default()
    })]

    /// Every kind, over the whole input space: the calendar's ends, any zone
    /// name, any fields of an unknown kind.
    #[test]
    fn any_time_round_trips_through_storage_and_the_wire(t in any_storable_time()) {
        assert_storage_round_trip(&t);
        assert_wire_round_trip(&t);
    }

    /// The wire carries an instant to the nanosecond, negative ones included.
    /// The index column holds milliseconds, so storage keeps the key stable
    /// and nothing finer.
    #[test]
    fn any_instant_round_trips_at_the_resolution_of_its_carrier(at in any_nano_instant()) {
        let t = SunriseTime::instant(at);
        assert_wire_round_trip(&t);
        let (ms, kind, tz) = t.to_parts();
        let stored = SunriseTime::from_parts(ms, kind, tz.as_deref(), None);
        prop_assert_eq!(stored.index_ms(), ms);
    }

    /// The last second of the range, `Timestamp::MAX` itself included, arrives
    /// by decoding a peer's value, so the wire holds it to the nanosecond.
    #[test]
    fn an_instant_in_the_last_second_round_trips_over_the_wire(
        nanos in prop_oneof![
            8 => 0i32..1_000_000_000,
            2 => Just(Timestamp::MAX.subsec_nanosecond()),
        ],
    ) {
        let at = Timestamp::new(Timestamp::MAX.as_second(), nanos).expect("in range");
        assert_wire_round_trip(&SunriseTime::instant(at));
    }

    /// A wall clock a zone skipped or repeated round-trips as written, and
    /// resolves as `to_instant` defines: forward by the gap's length, to the
    /// earlier of a fold's two instants. Reading the civil time at the offset
    /// in force before the change gives both.
    #[test]
    fn a_gap_or_fold_round_trips_and_resolves_as_defined(
        (name, civil, offset) in any_gap_or_fold(),
    ) {
        let (AmbiguousOffset::Gap { before, .. } | AmbiguousOffset::Fold { before, .. }) = offset
        else {
            unreachable!("the strategy rejects an unambiguous value");
        };
        let expected = before.to_timestamp(civil).expect("in range");
        let zone = TimeZone::get(&name).expect("from the tzdb");

        let zoned = SunriseTime::zoned(civil, name);
        assert_storage_round_trip(&zoned);
        assert_wire_round_trip(&zoned);
        prop_assert_eq!(zoned.to_instant(&TimeZone::UTC), Ok(expected));
        prop_assert_eq!(zoned.index_ms(), expected.as_millisecond());

        let floating = SunriseTime::floating(civil);
        assert_storage_round_trip(&floating);
        assert_wire_round_trip(&floating);
        prop_assert_eq!(floating.to_instant(&zone), Ok(expected));
    }
}
