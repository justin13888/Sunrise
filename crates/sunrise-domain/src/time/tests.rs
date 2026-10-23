//! Unit tests for [`super::SunriseTime`]: each kind resolves where it should,
//! indexes stably, and round-trips through storage and the wire.

use super::*;
use crate::unknown::CborValue;

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
        assert_eq!(
            SunriseTime::from_parts(ms, k, tz.as_deref()),
            c,
            "round trip {c:?}"
        );
    }
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
            SunriseTime::from_parts(1_700_000_000_000, "lunar_phase", tz),
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
