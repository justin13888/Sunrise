//! The editors' patches: what an untouched form sends (nothing), what a
//! one-field change sends (that field), and which kind every time comes back
//! as. Ported from the Apple app's `TaskEditorSeedTests` and its block
//! editor's `kindChanged` rule, plus the kind-preservation cases of #515.

use super::*;
use crate::civil::reader_zone;
use jiff::civil::date;
use sunrise_domain::{NoteBody, Unknowns};
use sunrise_id::EntityKind;

fn toronto() -> TimeZone {
    reader_zone("America/Toronto").unwrap()
}

fn eref(kind: EntityKind, n: u8) -> EntityRef {
    EntityRef::new(kind, [n; 16])
}

fn unknown_kind() -> SunriseTime {
    SunriseTime::Unknown {
        kind: "lunar".into(),
        raw: Unknowns::default(),
    }
}

fn task(hard_due_at: Option<SunriseTime>) -> TaskFields {
    TaskFields {
        title: "Renew passport".into(),
        priority: Some(2),
        energy: Some(Energy::Low),
        estimated_duration_s: Some(1_800),
        target_at: None,
        hard_due_at,
    }
}

/// Every deadline kind, opened and saved with only the title changed, keeps
/// its deadline exactly: #515's acceptance test, for all five kinds.
#[test]
fn a_title_edit_leaves_every_kind_of_deadline_alone() {
    let tz = toronto();
    let deadlines = [
        SunriseTime::instant(jiff::Timestamp::from_millisecond(1_800_000_000_000).unwrap()),
        SunriseTime::all_day(date(2026, 3, 13)),
        SunriseTime::floating(date(2026, 3, 13).at(17, 0, 0, 0)),
        SunriseTime::zoned(date(2026, 3, 13).at(17, 0, 0, 0), "Europe/Berlin"),
        unknown_kind(),
    ];
    for due in deadlines {
        let stored = task(Some(due.clone()));
        let mut form = task_form(&stored, &tz);
        form.title = "Renew passport today".into();
        let patch = task_patch(&stored, &form, &tz).unwrap();
        assert_eq!(patch.title.as_deref(), Some("Renew passport today"));
        assert!(patch.hard_due_at.is_none(), "{due:?} was rewritten");
        assert!(patch.priority.is_none() && patch.energy.is_none());
        assert!(patch.estimated_duration_s.is_none());
    }
}

/// The Apple editor seeded its picker with `Date()` once, which moved every
/// deadline to today the moment the sheet opened. The seed is the deadline.
#[test]
fn an_untouched_form_is_an_empty_patch() {
    let tz = toronto();
    for due in [None, Some(SunriseTime::all_day(date(2026, 3, 13)))] {
        let stored = task(due);
        let form = task_form(&stored, &tz);
        let patch = task_patch(&stored, &form, &tz).unwrap();
        assert!(task_patch_is_empty(&patch), "{patch:?}");
    }
}

/// The soft deadline is its own picker (ADR-0047 §1): left alone it is not
/// written, moved it keeps its kind, switched off it clears, and switched on
/// it is floating; the hard deadline is untouched by all of it.
#[test]
fn the_target_is_edited_apart_from_the_hard_deadline() {
    let tz = toronto();
    let mut stored = task(Some(SunriseTime::all_day(date(2026, 3, 20))));
    stored.target_at = Some(SunriseTime::all_day(date(2026, 3, 13)));
    let form = task_form(&stored, &tz);
    assert!(task_patch_is_empty(
        &task_patch(&stored, &form, &tz).unwrap()
    ));
    let mut moved = form.clone();
    moved.target_at = moved.target_at.map(|p| PickedTime {
        ms: p.ms + 86_400_000,
        kind: None,
    });
    let patch = task_patch(&stored, &moved, &tz).unwrap();
    assert_eq!(
        patch.target_at,
        Some(Some(SunriseTime::all_day(date(2026, 3, 14))))
    );
    assert!(patch.hard_due_at.is_none());
    let mut off = form.clone();
    off.target_at = None;
    assert_eq!(
        task_patch(&stored, &off, &tz).unwrap().target_at,
        Some(None)
    );
    stored.target_at = None;
    let mut on = task_form(&stored, &tz);
    on.target_at = Some(PickedTime {
        ms: 1_773_435_600_000,
        kind: None,
    });
    assert_eq!(
        task_patch(&stored, &on, &tz).unwrap().target_at,
        Some(Some(SunriseTime::floating(
            date(2026, 3, 13).at(17, 0, 0, 0)
        )))
    );
}

#[test]
fn a_moved_deadline_keeps_its_kind() {
    let tz = toronto();
    let stored = task(Some(SunriseTime::all_day(date(2026, 3, 13))));
    let mut form = task_form(&stored, &tz);
    form.hard_due_at = form.hard_due_at.map(|p| PickedTime {
        ms: p.ms + 3 * 86_400_000,
        kind: None,
    });
    let patch = task_patch(&stored, &form, &tz).unwrap();
    assert_eq!(
        patch.hard_due_at,
        Some(Some(SunriseTime::all_day(date(2026, 3, 16)))),
        "an all-day deadline stays a date"
    );

    let floating = task(Some(SunriseTime::floating(
        date(2026, 3, 13).at(17, 0, 0, 0),
    )));
    let mut form = task_form(&floating, &tz);
    form.hard_due_at = form.hard_due_at.map(|p| PickedTime {
        ms: p.ms + 3_600_000,
        kind: None,
    });
    assert_eq!(
        task_patch(&floating, &form, &tz).unwrap().hard_due_at,
        Some(Some(SunriseTime::floating(
            date(2026, 3, 13).at(18, 0, 0, 0)
        )))
    );
}

#[test]
fn switching_the_deadline_off_clears_it_and_on_writes_a_floating_one() {
    let tz = toronto();
    let stored = task(Some(SunriseTime::all_day(date(2026, 3, 13))));
    let mut form = task_form(&stored, &tz);
    form.hard_due_at = None;
    assert_eq!(
        task_patch(&stored, &form, &tz).unwrap().hard_due_at,
        Some(None)
    );

    let none = task(None);
    let mut form = task_form(&none, &tz);
    // 2026-03-13T21:00:00Z is 17:00 in Toronto.
    form.hard_due_at = Some(PickedTime {
        ms: 1_773_435_600_000,
        kind: None,
    });
    assert_eq!(
        task_patch(&none, &form, &tz).unwrap().hard_due_at,
        Some(Some(SunriseTime::floating(
            date(2026, 3, 13).at(17, 0, 0, 0)
        )))
    );
}

#[test]
fn facets_are_sent_only_when_they_change_and_zero_is_no_estimate() {
    let tz = toronto();
    let stored = task(None);
    let mut form = task_form(&stored, &tz);
    form.priority = None;
    form.energy = Some(Energy::High);
    form.estimated_duration_s = Some(0);
    let patch = task_patch(&stored, &form, &tz).unwrap();
    assert_eq!(patch.priority, Some(None));
    assert_eq!(patch.energy, Some(Some(Energy::High)));
    assert_eq!(patch.estimated_duration_s, Some(None));
    assert!(patch.title.is_none());

    form.title = "   ".into();
    assert_eq!(
        task_patch(&stored, &form, &tz).unwrap_err(),
        EditorError::EmptyTitle
    );
}

fn block(starts_at: SunriseTime, ends_at: SunriseTime) -> Block {
    Block {
        id: eref(EntityKind::Block, 1),
        created_at: jiff::Timestamp::UNIX_EPOCH,
        updated_at: jiff::Timestamp::UNIX_EPOCH,
        stream_id: None,
        starts_at,
        ends_at,
        title: Some("Deep work".into()),
        title_track_task: false,
        tasks: std::collections::BTreeSet::new(),
        deleted: false,
        unknown: Unknowns::new(),
    }
}

fn floating_block() -> Block {
    block(
        SunriseTime::floating(date(2026, 3, 7).at(9, 0, 0, 0)),
        SunriseTime::floating(date(2026, 3, 7).at(10, 0, 0, 0)),
    )
}

#[test]
fn an_untouched_block_form_is_an_empty_patch_for_every_kind() {
    let tz = toronto();
    let blocks = [
        floating_block(),
        block(
            SunriseTime::zoned(date(2026, 3, 7).at(9, 0, 0, 0), "Europe/Berlin"),
            SunriseTime::zoned(date(2026, 3, 7).at(10, 0, 0, 0), "Europe/Berlin"),
        ),
        block(
            SunriseTime::all_day(date(2026, 3, 7)),
            SunriseTime::all_day(date(2026, 3, 8)),
        ),
    ];
    for b in blocks {
        let patch = block_patch(&b, &block_form(&b, &tz), &tz).unwrap();
        assert!(
            patch.starts_at.is_none() && patch.ends_at.is_none(),
            "{b:?}"
        );
        assert!(patch.title.is_none() && patch.title_track_task.is_none());
    }
}

#[test]
fn a_moved_block_keeps_its_kind_and_its_own_zone() {
    let tz = toronto();
    let b = floating_block();
    let mut form = block_form(&b, &tz);
    form.ends_at_ms += 1_800_000;
    let patch = block_patch(&b, &form, &tz).unwrap();
    assert!(patch.starts_at.is_none(), "the start did not move");
    assert_eq!(
        patch.ends_at,
        Some(SunriseTime::floating(date(2026, 3, 7).at(10, 30, 0, 0)))
    );

    let berlin = block(
        SunriseTime::zoned(date(2026, 3, 7).at(9, 0, 0, 0), "Europe/Berlin"),
        SunriseTime::zoned(date(2026, 3, 7).at(10, 0, 0, 0), "Europe/Berlin"),
    );
    let mut form = block_form(&berlin, &tz);
    form.starts_at_ms -= 3_600_000;
    assert_eq!(
        block_patch(&berlin, &form, &tz).unwrap().starts_at,
        Some(SunriseTime::zoned(
            date(2026, 3, 7).at(8, 0, 0, 0),
            "Europe/Berlin"
        )),
        "not re-pinned to the zone the editor was opened in"
    );
}

#[test]
fn a_changed_kind_rewrites_both_bounds() {
    let tz = toronto();
    let b = floating_block();
    let mut form = block_form(&b, &tz);
    form.kind = TimeKind::Zoned;
    let patch = block_patch(&b, &form, &tz).unwrap();
    assert_eq!(
        patch.starts_at,
        Some(SunriseTime::zoned(
            date(2026, 3, 7).at(9, 0, 0, 0),
            "America/Toronto"
        ))
    );
    assert_eq!(
        patch.ends_at,
        Some(SunriseTime::zoned(
            date(2026, 3, 7).at(10, 0, 0, 0),
            "America/Toronto"
        ))
    );
}

/// An all-day block reads as zoned in the picker; given a time, both bounds
/// become zoned together rather than leaving one a date.
#[test]
fn an_all_day_block_given_a_time_becomes_zoned_at_both_ends() {
    let tz = toronto();
    let b = block(
        SunriseTime::all_day(date(2026, 3, 7)),
        SunriseTime::all_day(date(2026, 3, 8)),
    );
    let mut form = block_form(&b, &tz);
    assert_eq!(form.kind, TimeKind::Zoned);
    form.starts_at_ms += 9 * 3_600_000;
    let patch = block_patch(&b, &form, &tz).unwrap();
    assert_eq!(
        patch.starts_at,
        Some(SunriseTime::zoned(
            date(2026, 3, 7).at(9, 0, 0, 0),
            "America/Toronto"
        ))
    );
    assert_eq!(
        patch.ends_at,
        Some(SunriseTime::zoned(
            date(2026, 3, 8).at(0, 0, 0, 0),
            "America/Toronto"
        ))
    );
}

#[test]
fn a_block_title_clears_when_blank_and_the_range_must_be_forward() {
    let tz = toronto();
    let b = floating_block();
    let mut form = block_form(&b, &tz);
    form.title = "  ".into();
    form.title_track_task = true;
    let patch = block_patch(&b, &form, &tz).unwrap();
    assert_eq!(patch.title, Some(None));
    assert_eq!(patch.title_track_task, Some(true));

    form.ends_at_ms = form.starts_at_ms;
    assert_eq!(
        block_patch(&b, &form, &tz).unwrap_err(),
        EditorError::EndNotAfterStart
    );
}

fn routine() -> RoutineFields {
    RoutineFields {
        template: TaskTemplate {
            title: "Water the plants".into(),
            stream_id: None,
            contexts: vec![eref(EntityKind::Context, 4)],
            energy: None,
            priority: Some(3),
            estimated_duration_s: None,
            body: Some(NoteBody(vec![1, 2, 3])),
            unknown: Unknowns::new(),
        },
        rrule: RRule::parse("FREQ=DAILY").unwrap(),
        timezone: "America/Toronto".into(),
        catchup_policy: RoutineCatchupPolicy::Skip,
        ends_at: None,
    }
}

#[test]
fn an_untouched_routine_form_is_an_empty_patch() {
    let stored = routine();
    let patch = routine_patch(&stored, &routine_form(&stored)).unwrap();
    assert!(patch.template.is_none() && patch.rrule.is_none());
    assert!(patch.timezone.is_none() && patch.catchup_policy.is_none());
    assert!(patch.ends_at.is_none());
}

/// A title change replaces the whole template, so the template it sends is
/// the stored one with the title changed: contexts and body survive.
#[test]
fn a_routine_title_edit_keeps_the_rest_of_the_template() {
    let stored = routine();
    let mut form = routine_form(&stored);
    form.title = " Water the ferns ".into();
    let patch = routine_patch(&stored, &form).unwrap();
    let template = patch.template.expect("the template changed");
    assert_eq!(template.title, "Water the ferns");
    assert_eq!(template.contexts, stored.template.contexts);
    assert_eq!(template.body, stored.template.body);
    assert!(patch.rrule.is_none() && patch.timezone.is_none());
}

#[test]
fn a_routine_sends_the_rule_zone_and_end_only_when_they_change() {
    let stored = routine();
    let mut form = routine_form(&stored);
    form.rrule = RRule::parse("FREQ=WEEKLY;BYDAY=MO").unwrap();
    form.ends_at = Some(SunriseTime::all_day(jiff::civil::date(2027, 1, 15)));
    let patch = routine_patch(&stored, &form).unwrap();
    assert!(patch.template.is_none());
    assert_eq!(patch.rrule, Some(form.rrule.clone()));
    assert_eq!(patch.ends_at, Some(form.ends_at.clone()));
    assert!(patch.timezone.is_none() && patch.catchup_policy.is_none());

    form.title.clear();
    assert_eq!(
        routine_patch(&stored, &form).unwrap_err(),
        EditorError::EmptyTitle
    );
}
