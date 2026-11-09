use super::*;

fn id(n: u8) -> PlaceId {
    let mut bytes = [0u8; 16];
    bytes[15] = n;
    PlaceId::from_ulid(Ulid::from_bytes(bytes))
}

fn place(n: u8, lat_e7: i32, lon_e7: i32) -> Place {
    Place {
        id: id(n),
        name: format!("place {n}"),
        lat_e7,
        lon_e7,
        radius_m: DEFAULT_RADIUS_M,
        address_label: None,
        deleted: false,
    }
}

fn req(places: &[u8], hard: bool) -> TaskPlaceReq {
    TaskPlaceReq {
        places: places.iter().map(|&n| id(n)).collect(),
        hard,
    }
}

#[test]
fn a_place_id_round_trips_through_its_string_form() {
    let s = "plc_01HZX4T7Q0000000000000000Z";
    let parsed = PlaceId::parse(s).unwrap();
    assert_eq!(parsed.to_str(), s);
    assert_eq!(parsed.to_string(), s);
    assert_eq!(s.parse::<PlaceId>().unwrap(), parsed);
}

#[test]
fn a_place_id_refuses_another_prefix_and_a_bad_ulid() {
    assert_eq!(
        PlaceId::parse("tsk_01HZX4T7Q0000000000000000Z"),
        Err(PlaceIdError::Prefix)
    );
    assert!(matches!(
        PlaceId::parse("plc_short"),
        Err(PlaceIdError::Ulid(_))
    ));
}

#[test]
fn a_valid_place_validates() {
    let mut p = place(1, 437_000_000, -794_000_000);
    p.address_label = Some("100 Queen St W".into());
    assert_eq!(p.validate(), Ok(()));
    // Every bound is inclusive.
    for (lat, lon) in [(MAX_LAT_E7, MAX_LON_E7), (-MAX_LAT_E7, -MAX_LON_E7), (0, 0)] {
        assert_eq!(validate_coordinates(lat, lon), Ok(()));
    }
    for r in [MIN_RADIUS_M, DEFAULT_RADIUS_M, MAX_RADIUS_M] {
        assert_eq!(validate_radius(r), Ok(()));
    }
}

#[test]
fn validation_refuses_each_rule_it_names() {
    let field = |field, constraint| Err(ValidationError::Field { field, constraint });

    let mut p = place(1, 0, 0);
    p.name = "   ".into();
    assert_eq!(p.validate(), Err(ValidationError::InvalidTitle));
    p.name = "x".repeat(MAX_PLACE_NAME_LEN + 1);
    assert_eq!(p.validate(), field("place.name", "max_length"));

    assert_eq!(
        validate_coordinates(MAX_LAT_E7 + 1, 0),
        field("place.lat_e7", "range")
    );
    assert_eq!(
        validate_coordinates(-MAX_LAT_E7 - 1, 0),
        field("place.lat_e7", "range")
    );
    assert_eq!(
        validate_coordinates(0, MAX_LON_E7 + 1),
        field("place.lon_e7", "range")
    );
    assert_eq!(
        validate_coordinates(0, -MAX_LON_E7 - 1),
        field("place.lon_e7", "range")
    );

    assert_eq!(
        validate_radius(MIN_RADIUS_M - 1),
        field("place.radius_m", "range_50_5000")
    );
    assert_eq!(
        validate_radius(MAX_RADIUS_M + 1),
        field("place.radius_m", "range_50_5000")
    );

    let mut p = place(1, 0, 0);
    p.address_label = Some("a".repeat(MAX_ADDRESS_LABEL_LEN + 1));
    assert_eq!(p.validate(), field("place.address_label", "max_length"));
    p.address_label = Some("é".repeat(MAX_ADDRESS_LABEL_LEN));
    assert_eq!(p.validate(), Ok(()), "the label limit counts chars");
}

#[test]
fn evaluation_is_satisfied_by_any_present_place() {
    let mut presence = Presence::new();
    presence.set(id(1), false);
    presence.set(id(2), true);
    assert_eq!(
        evaluate_place(&[id(1), id(2)], &presence),
        PlaceEval::Satisfied
    );
    assert_eq!(
        evaluate_place(&[id(3), id(2)], &presence),
        PlaceEval::Satisfied,
        "an unknown place does not hide a present one"
    );
}

#[test]
fn evaluation_is_violated_only_when_every_place_is_known_absent() {
    let mut presence = Presence::new();
    presence.set(id(1), false);
    presence.set(id(2), false);
    assert_eq!(
        evaluate_place(&[id(1), id(2)], &presence),
        PlaceEval::Violated
    );
    assert_eq!(
        evaluate_place(&[id(1), id(3)], &presence),
        PlaceEval::Unknown,
        "one unknown place keeps the requirement unknown"
    );
}

#[test]
fn nothing_known_is_unknown_never_violated() {
    let presence = Presence::new();
    assert_eq!(evaluate_place(&[id(1)], &presence), PlaceEval::Unknown);
    assert_eq!(evaluate_place(&[], &presence), PlaceEval::Unknown);

    // A forgotten place (deleted, or its region dropped) and a cleared
    // presence (permission revoked) are unknown again.
    let mut presence = Presence::new();
    presence.set(id(1), false);
    presence.set(id(2), true);
    presence.forget(id(1));
    assert_eq!(evaluate_place(&[id(1)], &presence), PlaceEval::Unknown);
    presence.clear();
    assert!(presence.is_empty());
    assert_eq!(evaluate_place(&[id(2)], &presence), PlaceEval::Unknown);
}

#[test]
fn presence_debug_never_names_a_place_or_a_state() {
    let mut presence = Presence::new();
    presence.set(id(7), true);
    let shown = format!("{presence:?}");
    assert_eq!(presence.len(), 1);
    assert!(!shown.contains("plc_"), "{shown}");
    assert!(!shown.contains("true"), "{shown}");
}

/// The fixed table ADR-0051 §5 asks for. Places 1..=6 around Toronto, the
/// position at place 6's pin; cap 5 leaves room for four.
#[test]
fn region_selection_ranks_hard_then_demand_then_distance_then_id() {
    let places = vec![
        place(1, 437_000_000, -794_000_000), // 1 soft task, near
        place(2, 437_100_000, -794_100_000), // 2 soft tasks
        place(3, 450_000_000, -760_000_000), // 1 hard task, far
        place(4, 437_000_000, -794_000_000), // 1 soft task, same pin as 1
        place(5, 400_000_000, -740_000_000), // 1 soft task, farthest
        place(6, 436_500_000, -793_500_000), // no task requires it
    ];
    let tasks = vec![
        req(&[1], false),
        req(&[2], false),
        req(&[2, 2], false), // a place named twice counts once
        req(&[3], true),
        req(&[4], false),
        req(&[5], false),
    ];
    let at = Some((436_500_000, -793_500_000));
    assert_eq!(
        select_regions(&places, &tasks, at, 5),
        vec![id(3), id(2), id(1), id(4)],
        "hard first; then demand; then nearest; a tie in distance by id"
    );
    assert_eq!(
        select_regions(&places, &tasks, None, 5),
        vec![id(3), id(2), id(1), id(4)],
        "with no position, ties fall to the id"
    );
    assert_eq!(
        select_regions(&places, &tasks, Some((400_000_000, -740_000_000)), 4),
        vec![id(3), id(2), id(5)],
        "nearest first from another position, under a smaller cap"
    );
}

#[test]
fn region_selection_leaves_a_slot_and_skips_what_it_cannot_monitor() {
    let mut deleted = place(2, 0, 0);
    deleted.deleted = true;
    let places = vec![place(1, 0, 0), deleted, place(1, 10, 10)];
    let tasks = vec![req(&[1], false), req(&[2], true), req(&[9], true)];
    assert!(select_regions(&places, &tasks, None, 0).is_empty());
    assert!(
        select_regions(&places, &tasks, None, 1).is_empty(),
        "the only slot is the client's re-selection trigger"
    );
    assert_eq!(
        select_regions(&places, &tasks, None, 20),
        vec![id(1)],
        "a deleted place, an unknown ref and a duplicate are not registered"
    );
    assert!(select_regions(&places, &[], None, 20).is_empty());
}

#[test]
fn distance_wraps_the_antimeridian_and_shrinks_toward_the_poles() {
    let east = (0, 1_799_000_000);
    let west = (0, -1_799_000_000);
    assert_eq!(
        distance_key(east, west).to_bits(),
        distance_key((0, 0), (0, 2_000_000)).to_bits(),
        "0.2° across the antimeridian is 0.2°, not 359.8°"
    );

    let equator = distance_key((0, 0), (0, 10_000_000));
    let sixty = distance_key((600_000_000, 0), (600_000_000, 10_000_000));
    let ratio = (sixty / equator).sqrt();
    assert!(
        (ratio - 0.5).abs() < 1e-6,
        "a degree of longitude at 60° is half one at the equator: {ratio}"
    );
}

#[test]
fn the_cosine_polynomial_tracks_the_platform_cosine() {
    for lat_e7 in (-MAX_LAT_E7..=MAX_LAT_E7).step_by(50_000_000) {
        let x = f64::from(lat_e7) * 1e-7 * std::f64::consts::PI / 180.0;
        let err = (cos_lat(f64::from(lat_e7)) - x.cos()).abs();
        assert!(err < 3e-7, "lat_e7 {lat_e7}: off by {err}");
    }
}
