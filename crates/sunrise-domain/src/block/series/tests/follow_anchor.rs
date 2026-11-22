//! `BlockSeries::follow_anchor`: an edit that moves the anchor re-keys each
//! exception onto the occurrence it applied to, or is refused (#1200), and
//! `Block::follow_bindings` re-keys each occurrence binding the same way
//! (#1275).

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

/// `deep_work` as a block, its week 10 bound to one task and a key the rule
/// does not produce bound to another.
fn deep_work_with_bindings() -> (crate::Block, BlockSeries) {
    use crate::OccurrenceBinding;
    let s = deep_work();
    let task = |n: u8| EntityRef::new(EntityKind::Task, [n; 16]);
    let block = crate::Block {
        id: EntityRef::new(EntityKind::Block, [1u8; 16]),
        created_at: Timestamp::UNIX_EPOCH,
        updated_at: Timestamp::UNIX_EPOCH,
        stream_id: None,
        starts_at: s.starts_at.clone(),
        ends_at: s.ends_at.clone(),
        title: Some("Deep work".into()),
        title_track_task: false,
        tasks: std::collections::BTreeSet::new(),
        occurrence_tasks: std::collections::BTreeSet::from([
            OccurrenceBinding::new("2026-03-10T09:00", task(2)),
            OccurrenceBinding::new("2026-03-18T09:00", task(3)),
        ]),
        deleted: false,
        kind: None,
        rrule: Some(s.rrule.clone()),
        exceptions: BTreeMap::new(),
        split_from: None,
        location: None,
        notes: None,
        source: None,
        external_id: None,
        unknown: Unknowns::new(),
    };
    (block, s)
}

/// The keys of a block's occurrence bindings, in order.
fn binding_keys(block: &crate::Block) -> Vec<&str> {
    block
        .occurrence_tasks
        .iter()
        .map(crate::OccurrenceBinding::key)
        .collect()
}

/// Moving the anchor an hour later moves each binding with its occurrence,
/// and one that named no occurrence moves too (#1275).
#[test]
fn moving_the_anchor_moves_each_binding_with_its_occurrence() {
    let (mut block, was) = deep_work_with_bindings();
    block.starts_at = SunriseTime::floating(at(2026, 3, 3, 10, 0));
    block.ends_at = SunriseTime::floating(at(2026, 3, 3, 12, 0));
    block.follow_bindings(&was).unwrap();
    assert_eq!(
        binding_keys(&block),
        ["2026-03-10T10:00", "2026-03-18T10:00"]
    );
}

/// A rule whose `BYDAY` holds the weekday does not follow an anchor moved
/// to another day, so a binding it produced cannot follow either: the move
/// is refused and the bindings are left as they were (#1275).
#[test]
fn a_move_that_strands_a_binding_is_refused() {
    let (mut block, was) = deep_work_with_bindings();
    let bound = block.occurrence_tasks.clone();
    block.starts_at = SunriseTime::floating(at(2026, 3, 4, 9, 0));
    block.ends_at = SunriseTime::floating(at(2026, 3, 4, 11, 0));
    assert_eq!(
        block.follow_bindings(&was),
        Err(crate::StrandedBinding {
            key: "2026-03-10T09:00".into(),
            task: EntityRef::new(EntityKind::Task, [2; 16]),
        })
    );
    assert_eq!(block.occurrence_tasks, bound);

    // With the rule dropped there is no occurrence to check against, so the
    // bindings are only re-keyed.
    block.rrule = None;
    block.follow_bindings(&was).unwrap();
    assert_eq!(
        binding_keys(&block),
        ["2026-03-11T09:00", "2026-03-19T09:00"]
    );
}
