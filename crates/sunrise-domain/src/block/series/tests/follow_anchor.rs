//! `BlockSeries::follow_anchor`: an edit that moves the anchor re-keys each
//! exception onto the occurrence it applied to, or is refused (#1200).

use super::*;

/// `deep_work` with week 10 cancelled, week 17 retitled, and an exception
/// at a key the rule does not produce.
fn deep_work_with_exceptions() -> BlockSeries {
    let mut s = deep_work();
    s.exceptions = BTreeMap::from([
        ("2026-03-10T09:00".into(), BlockException::cancelled()),
        (
            "2026-03-17T09:00".into(),
            BlockException {
                title: Some("Taxes".into()),
                ..BlockException::default()
            },
        ),
        ("2026-03-18T09:00".into(), BlockException::cancelled()),
    ]);
    s
}

/// Moving the anchor an hour later moves every exception's key an hour
/// later, so the cancelled week stays cancelled and the retitled week keeps
/// its title; an exception that applied to nothing moves too.
#[test]
fn moving_the_anchor_moves_each_exception_with_its_occurrence() {
    let was = deep_work_with_exceptions();
    let mut moved = was.clone();
    moved.starts_at = SunriseTime::floating(at(2026, 3, 3, 10, 0));
    moved.ends_at = SunriseTime::floating(at(2026, 3, 3, 12, 0));
    moved.follow_anchor(&was).unwrap();
    assert_eq!(
        moved
            .exceptions
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["2026-03-10T10:00", "2026-03-17T10:00", "2026-03-18T10:00"]
    );
    assert_eq!(
        moved.exceptions["2026-03-17T10:00"].title.as_deref(),
        Some("Taxes")
    );

    let tz = TimeZone::UTC;
    let occ = expand_block(
        &moved,
        window(at(2026, 3, 1, 0, 0), at(2026, 3, 22, 0, 0), &tz),
        &tz,
    )
    .unwrap();
    assert_eq!(keys(&occ), ["2026-03-03T10:00", "2026-03-17T10:00"]);
    assert_eq!(
        occ[1].exception.as_ref().and_then(|x| x.title.as_deref()),
        Some("Taxes")
    );
}

/// An edit that leaves the anchor where it was, or moves an instant
/// anchor, re-keys nothing.
#[test]
fn an_unmoved_anchor_keeps_the_exceptions() {
    let was = deep_work_with_exceptions();
    let mut same = was.clone();
    same.ends_at = SunriseTime::floating(at(2026, 3, 3, 12, 0));
    same.follow_anchor(&was).unwrap();
    assert_eq!(same.exceptions, was.exceptions);

    let mut instant = was.clone();
    instant.starts_at = SunriseTime::instant(Timestamp::UNIX_EPOCH);
    instant.follow_anchor(&was).unwrap();
    assert_eq!(instant.exceptions, was.exceptions);
}

/// A rule whose `BYDAY` holds the weekday does not follow an anchor moved
/// to another day, so the exceptions it applied cannot follow either: the
/// move is refused and the exceptions are left as they were.
#[test]
fn a_move_that_strands_an_exception_is_refused() {
    let was = deep_work_with_exceptions();
    let mut moved = was.clone();
    moved.starts_at = SunriseTime::floating(at(2026, 3, 4, 9, 0));
    moved.ends_at = SunriseTime::floating(at(2026, 3, 4, 11, 0));
    assert_eq!(
        moved.follow_anchor(&was),
        Err(StrandedException {
            key: "2026-03-10T09:00".into()
        })
    );
    assert_eq!(moved.exceptions, was.exceptions);
}
