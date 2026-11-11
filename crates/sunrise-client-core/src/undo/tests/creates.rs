//! The inverses read off the result of a create after it lands
//! ([`invert_create`]), their rebinding on redo ([`rebind_creates`]), and a
//! step turned around ([`UndoEntry::flipped`]).

use super::*;
use sunrise_core::commands::FocusStartDraft;
use sunrise_domain::{
    AttachmentDraft, BlockDraft, ContextDraft, FocusKind, RRule, RoutineCatchupPolicy,
    RoutineDraft, SessionLength, StreamDraft, SunriseTime, TaskDraft, TaskTemplate,
};

/// Every create the core has that owns an entity, paired with the id the
/// core would mint for it. A table so the `match` in [`invert_create`]
/// cannot grow a seventh create without this covering it.
fn every_create() -> Vec<(Command, EntityRef)> {
    vec![
        (
            Command::CreateTask(TaskDraft {
                title: "Renew passport".into(),
                body: None,
                stream_id: None,
                contexts: Vec::new(),
                priority: None,
                energy: None,
                estimated_duration_s: None,
                planned_at: None,
                target_at: None,
                hard_due_at: None,
                scheduling_constraints: Vec::new(),
                assignee: None,
                reminder_lead_s: None,
            }),
            tid(7),
        ),
        (
            Command::CreateStream(StreamDraft {
                name: "Travel".into(),
                ..StreamDraft::default()
            }),
            sid(7),
        ),
        (
            Command::CreateContext(ContextDraft {
                name: "home".into(),
                description: None,
            }),
            cid(7),
        ),
        (
            Command::CreateRoutine(RoutineDraft {
                template: TaskTemplate {
                    title: "Water plants".into(),
                    stream_id: Some(sid(1)),
                    contexts: Vec::new(),
                    energy: None,
                    priority: None,
                    estimated_duration_s: None,
                    body: None,
                    unknown: sunrise_domain::Unknowns::new(),
                },
                rrule: RRule::parse("FREQ=DAILY").expect("valid rrule"),
                timezone: "UTC".into(),
                starts_at: jiff::Timestamp::UNIX_EPOCH.into(),
                ends_at: None,
                scheduling_constraints: Vec::new(),
                catchup_policy: RoutineCatchupPolicy::Skip,
            }),
            rid(7),
        ),
        (
            Command::CreateBlock(BlockDraft {
                stream_id: Some(sid(1)),
                starts_at: SunriseTime::Instant {
                    at: jiff::Timestamp::UNIX_EPOCH,
                },
                ends_at: SunriseTime::Instant {
                    at: jiff::Timestamp::UNIX_EPOCH,
                },
                title: Some("Deep work".into()),
                title_track_task: false,
                tasks: Vec::new(),
            }),
            EntityRef::new(EntityKind::Block, [7; 16]),
        ),
        (
            Command::AttachFile(AttachmentDraft {
                parent: tid(1),
                filename: "scan.pdf".into(),
                mime_type: "application/pdf".into(),
                size_bytes: 1,
                blob_key: [0; 32],
                blob_id: [0; 16],
                chunk_count: 1,
                content_hash: [0; 32],
                ciphertext_hash: [0; 32],
                width: None,
                height: None,
                thumbnail: None,
            }),
            EntityRef::new(EntityKind::Attachment, [7; 16]),
        ),
    ]
}

/// The id a delete names, whichever kind of delete it is.
pub(super) fn deletes(cmd: &Command) -> Option<EntityRef> {
    match cmd {
        Command::DeleteTask(id)
        | Command::DeleteStream { id, .. }
        | Command::DeleteContext(id)
        | Command::DeleteRoutine(id)
        | Command::DeleteBlock { id, .. }
        | Command::DetachFile(id) => Some(*id),
        _ => None,
    }
}

#[test]
fn undoing_a_create_deletes_the_entity_it_minted() {
    for (cmd, minted) in every_create() {
        assert!(is_create(&cmd), "{cmd:?} is a create");
        let back = invert_create(&cmd, minted).expect("a create is invertible once applied");
        assert_eq!(
            deletes(&back),
            Some(minted),
            "{cmd:?} must invert to the delete of the id the core minted, not another"
        );
    }
}

#[test]
fn a_create_cannot_be_inverted_before_it_is_applied() {
    // Not "unsupported for ever" — unsupported *from here*. `invert` reads
    // rows that already exist, and the entity a create makes does not.
    let rows = Rows::default();
    for (cmd, _) in every_create() {
        assert_eq!(
            invert(&rows, &[cmd]).unwrap_err(),
            NotUndoable::Unsupported,
            "the id to delete does not exist yet"
        );
    }
}

#[test]
fn a_create_with_no_delete_to_invert_into_is_refused() {
    // `StartFocus` mints an append-only record and the core has no command
    // that removes one. Refused rather than mapped onto some other delete.
    let start = Command::StartFocus(FocusStartDraft {
        task_id: tid(1),
        kind: FocusKind::Work,
        length: SessionLength::UntilDone,
        energy: None,
    });
    assert!(!is_create(&start));
    assert_eq!(
        invert_create(&start, EntityRef::new(EntityKind::FocusSession, [1; 16])).unwrap_err(),
        NotUndoable::Unsupported
    );
}

#[test]
fn redoing_a_create_rebinds_the_undo_to_the_entity_the_replay_minted() {
    // The whole point: a replayed create is a *different* entity. Without
    // rebinding, the next undo would delete the already-tombstoned first
    // id and leave the second live for ever.
    let (create, first) = every_create().remove(0);
    let entry = UndoEntry {
        label: "new task".into(),
        forward: vec![create.clone()],
        backward: vec![invert_create(&create, first).expect("invertible")],
    };
    assert_eq!(deletes(&entry.backward[0]), Some(first));

    // Undo stores the step flipped, and redo applies that step's
    // `backward` — the create — then binds its `forward` to what came
    // back.
    let mut flipped = entry.clone().flipped();
    let second = tid(9);
    rebind_creates(&flipped.backward, &mut flipped.forward, &[second]);
    let after_redo = flipped.flipped();
    assert_eq!(
        deletes(&after_redo.backward[0]),
        Some(second),
        "the next undo deletes what the redo made, not what the first create made"
    );
}

#[test]
fn a_rebind_leaves_the_inverses_of_everything_that_is_not_a_create_alone() {
    let rows = rows_with(task(1));
    let forward = vec![
        Command::CompleteTask(tid(1)),
        Command::CreateContext(ContextDraft {
            name: "home".into(),
            description: None,
        }),
    ];
    // `invert` refuses a batch containing a create, so the inverse is
    // assembled the way a caller assembles it: the pre-read half, and the
    // minted half on top. Reversed order, like `invert`'s own output.
    let mut backward = vec![
        invert_create(&forward[1], cid(3)).expect("invertible"),
        invert(&rows, &forward[..1]).expect("invertible").remove(0),
    ];
    rebind_creates(&forward, &mut backward, &[tid(1), cid(4)]);
    assert!(
        matches!(&backward[1], Command::UpdateTask { id, .. } if *id == tid(1)),
        "the completion's inverse is untouched"
    );
    assert_eq!(
        deletes(&backward[0]),
        Some(cid(4)),
        "only the create is re-bound, and to the id its own replay returned"
    );
}

#[test]
fn a_create_round_trip_converges() {
    // Two flips is the identity: create → undo → redo → undo settles on
    // "the entity is gone" rather than oscillating into a shape the two
    // sides disagree about.
    let (create, minted) = every_create().remove(0);
    let entry = UndoEntry {
        label: "new task".into(),
        forward: vec![create],
        backward: vec![Command::DeleteTask(minted)],
    };
    let round_trip = entry.clone().flipped().flipped();
    assert!(matches!(round_trip.forward[..], [Command::CreateTask(_)]));
    assert_eq!(deletes(&round_trip.backward[0]), Some(minted));
    assert_eq!(round_trip.label, entry.label);
}

#[test]
fn a_step_flipped_is_the_same_step_facing_the_other_way() {
    let entry = UndoEntry {
        label: "completed 3 tasks".into(),
        forward: vec![Command::CompleteTask(tid(1))],
        backward: vec![Command::DeleteTask(tid(1))],
    };
    let flipped = entry.clone().flipped();
    assert_eq!(flipped.label, entry.label);
    assert!(matches!(flipped.forward[..], [Command::DeleteTask(_)]));
    assert!(matches!(flipped.backward[..], [Command::CompleteTask(_)]));
}
