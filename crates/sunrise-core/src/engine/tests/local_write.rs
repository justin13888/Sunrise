//! One merge path for every local write (ADR-0065 §5, PR 3 of its plan).
//!
//! A command appends its op and folds it through the step a received op
//! takes, so the rows a writer's commands leave are the rows any replica
//! that receives those ops derives, and the rows a rebuild of the writer's
//! own log derives. The after-every-write rebuild check (`rebuild::check`)
//! holds the second for every command the suite runs; this module holds the
//! first, and the merge-state rules that had to change for either to hold.

use super::field_merge::{
    add, assert_converged, emit_patch, eref, inbox, op, patch, remove, set, shared_task, task_at,
    text,
};
use super::support::*;
use super::*;
use crate::engine::rebuild::check::dump;
use ciborium::value::Value;
use sunrise_domain::{
    InterruptionReason, PrefTarget, PrefValue, ReviewSnapshotDraft, ReviewTotals, RoutinePatch,
    Weekday,
};

/// Every projection table, merge state included, with each routine's
/// `materialized_until_ms` set aside: it is how far this device generated
/// occurrences, a local watermark no op carries.
fn projected(db: &Db) -> Vec<(&'static str, Vec<String>)> {
    let tx = db.conn().unchecked_transaction().unwrap();
    tx.execute("UPDATE routines SET materialized_until_ms = 0", [])
        .unwrap();
    let rows = dump(&tx);
    tx.rollback().unwrap();
    rows
}

fn apply(e: &Engine, db: &mut Db, cmd: Command) -> EntityRef {
    e.apply(db, cmd).unwrap().entity
}

/// Every command that writes an entity, run on one device, and its whole log
/// replayed on a second in the order it was written: the two hold the same
/// projection, byte for byte, merge state and row stamps included.
///
/// Before every command folded its own op the writer's rows were its own
/// copy of what it meant to write, and they parted from the replica's
/// wherever the fold reads an op differently from the command that built it.
#[test]
fn a_local_write_and_its_replay_project_identical_rows() {
    let clock = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let w = engine_seeded(ROOT, [1u8; 32], clock.clone());
    let r = engine_seeded(ROOT, [2u8; 32], clock.clone());
    let mut dbw = db_root(ROOT);
    let mut dbr = db_root(ROOT);
    trust(&r, &mut dbr, &w);

    let stream = apply(
        &w,
        &mut dbw,
        Command::CreateStream(StreamDraft {
            name: "Work".into(),
            ..Default::default()
        }),
    );
    let home = apply(
        &w,
        &mut dbw,
        Command::CreateContext(ContextDraft {
            name: "home".into(),
            ..Default::default()
        }),
    );
    let phone = apply(
        &w,
        &mut dbw,
        Command::CreateContext(ContextDraft {
            name: "phone".into(),
            ..Default::default()
        }),
    );
    let a = apply(
        &w,
        &mut dbw,
        Command::CreateTask(TaskDraft {
            title: "a".into(),
            stream_id: Some(stream),
            contexts: vec![home, phone],
            ..Default::default()
        }),
    );
    let b = new_task(&w, &mut dbw, "b");
    apply(
        &w,
        &mut dbw,
        Command::UpdateTask {
            id: a,
            patch: TaskPatch {
                title: Some("a, renamed".into()),
                priority: Some(Some(2)),
                blocked_by: Some(vec![b]),
                ..Default::default()
            },
        },
    );
    apply(
        &w,
        &mut dbw,
        Command::DeferTask {
            id: a,
            to_ms: (NOW + DAY_MS) as u64,
        },
    );
    apply(
        &w,
        &mut dbw,
        Command::UpdateStream {
            id: stream,
            patch: StreamPatch {
                name: Some("Work, renamed".into()),
                ..Default::default()
            },
        },
    );

    // A routine, its occurrences, a completion that advances its streak, a
    // skip, and a structural edit that regenerates what is still untouched.
    let routine = apply(
        &w,
        &mut dbw,
        Command::CreateRoutine(routine_draft(
            stream,
            "FREQ=DAILY",
            NOW + 3_600_000,
            RoutineCatchupPolicy::Skip,
            Vec::new(),
        )),
    );
    let occ = occurrence_tasks(&routine_of(&w, &dbw, routine), 3);
    apply(&w, &mut dbw, Command::CompleteTask(occ[0].0));
    apply(
        &w,
        &mut dbw,
        Command::SkipRoutineOccurrence {
            id: routine,
            occurrence_key: occurrence_key_at("UTC", ms_to_ts(occ[1].1)).unwrap(),
        },
    );
    apply(
        &w,
        &mut dbw,
        Command::UpdateRoutine {
            scope: sunrise_domain::EditScope::All,
            id: routine,
            patch: RoutinePatch {
                rrule: Some(RRule::parse("FREQ=WEEKLY").unwrap()),
                ..Default::default()
            },
        },
    );

    // Blocks, created, bound, unbound and imported twice.
    let block = apply(&w, &mut dbw, Command::CreateBlock(block_draft(9, 1, None)));
    apply(&w, &mut dbw, Command::BindTask { block, task: a });
    apply(&w, &mut dbw, Command::BindTask { block, task: b });
    apply(&w, &mut dbw, Command::UnbindTask { block, task: b });
    for _ in 0..2 {
        apply(
            &w,
            &mut dbw,
            import("ics", "uid-1", block_draft(13, 1, Some("Lunch"))),
        );
    }

    // An attachment, focus records with a completion, a review and a
    // preference.
    let file = apply(&w, &mut dbw, Command::AttachFile(attachment_draft(a)));
    apply(&w, &mut dbw, Command::DetachFile(file));
    let session = open_session(&w, &mut dbw, b);
    apply(
        &w,
        &mut dbw,
        Command::LogInterruption {
            session,
            reason: InterruptionReason::Meeting,
        },
    );
    end_session(&w, &mut dbw, session, true);
    apply(
        &w,
        &mut dbw,
        Command::SaveReviewSnapshot(ReviewSnapshotDraft {
            window_start_ms: NOW as u64 - 7 * DAY_MS as u64,
            window_end_ms: NOW as u64,
            totals: ReviewTotals::default(),
            streams: Vec::new(),
            streaks: Vec::new(),
            note: None,
        }),
    );
    apply(
        &w,
        &mut dbw,
        Command::SetPreference {
            key: "week_start".into(),
            value: PrefValue::Weekday(Weekday::Mo),
            target: PrefTarget::Vault,
        },
    );

    // Deletes, one of which reaches into every task carrying the context.
    apply(&w, &mut dbw, Command::DeleteContext(home));
    apply(&w, &mut dbw, Command::DeleteBlock(block));
    apply(&w, &mut dbw, Command::DeleteTask(a));
    apply(&w, &mut dbw, Command::DeleteRoutine(routine));

    assert_eq!(
        task_of(&w, &dbw, b).state,
        TaskState::Done,
        "the focus session completed its task"
    );
    let log: Vec<Vec<u8>> = {
        let mut stmt = dbw
            .conn()
            .prepare("SELECT envelope FROM ops ORDER BY rowid")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    for env in &log {
        r.apply_remote_all(&mut dbr, env).unwrap();
    }
    let (writer, replica) = (projected(&dbw), projected(&dbr));
    for ((table, w_rows), (_, r_rows)) in writer.iter().zip(&replica) {
        assert_eq!(
            w_rows, r_rows,
            "`{table}` differs between writer and replica"
        );
    }
    w.assert_rebuild_identity(&dbw);
}

/// A defer is logged as the `task.update` it is, the kind its receivers log
/// it under, so one op has one kind on every replica.
#[test]
fn a_defer_is_logged_as_the_update_it_is() {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let e = engine_seeded(ROOT, [1u8; 32], clock);
    let mut db = db_root(ROOT);
    legacy_peer(&db);
    let task = new_task(&e, &mut db, "later");
    let res = e
        .apply(
            &mut db,
            Command::DeferTask {
                id: task,
                to_ms: T0 + 86_400_000,
            },
        )
        .unwrap();
    let kind: String = db
        .conn()
        .query_row(
            "SELECT inner_kind FROM ops WHERE op_id = ?",
            params![&res.op_id[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kind, "task.update");
}

/// Bug (a) of ADR-0065 PR 2: a `Patch` writes an unknown map field key by
/// key, and a later full-state op from the command path carries the whole map
/// as one unknown value. The map a full-state op carries is the field's value
/// as of its stamp, as it is for a map this build registers, so it is kept on
/// the writer and on every peer, and a later `Patch` still edits it.
#[test]
fn a_full_state_write_after_an_unknown_map_patch_keeps_the_map_on_every_replica() {
    let mut p = pair();
    let task = shared_task(
        &mut p,
        TaskDraft {
            title: "labelled".into(),
            ..Default::default()
        },
    );
    let label = |k: &str, v: i64| -> Value {
        op(
            "map",
            Value::Map(vec![(text(k), set(Value::Integer(v.into())))]),
        )
    };
    set_clock(&p.ca, T0 + 1_000);
    let (labelled, _) = emit_patch(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &patch(task, false, vec![("x_labels", label("k0", 1))]),
    );
    p.eb.apply_remote(&mut p.dbb, &labelled).unwrap();

    set_clock(&p.ca, T0 + 2_000);
    let renamed =
        p.ea.apply(
            &mut p.dba,
            Command::UpdateTask {
                id: task,
                patch: TaskPatch {
                    title: Some("renamed".into()),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    p.eb.apply_remote(&mut p.dbb, &env_bytes(&p.dba, &renamed.op_id))
        .unwrap();

    let labels = |t: &Task| t.unknown.get("x_labels").map(|v| v.get().clone());
    let one = Value::Map(vec![(text("k0"), Value::Integer(1.into()))]);
    let merged = assert_converged(&p, task);
    assert_eq!(merged.title, "renamed");
    assert_eq!(labels(&merged), Some(one));

    set_clock(&p.cb, T0 + 3_000);
    let (later, _) = emit_patch(
        &p.eb,
        &mut p.dbb,
        &inbox(),
        &patch(task, false, vec![("x_labels", label("k1", 2))]),
    );
    p.ea.apply_remote(&mut p.dba, &later).unwrap();
    let both = Value::Map(vec![
        (text("k0"), Value::Integer(1.into())),
        (text("k1"), Value::Integer(2.into())),
    ]);
    let edited = assert_converged(&p, task);
    assert_eq!(
        labels(&edited).map(|v| sunrise_cbor::encode_canonical(&v).unwrap()),
        Some(sunrise_cbor::encode_canonical(&both).unwrap())
    );
}

/// Bug (c) of ADR-0065 PR 2: whether a replica kept an OR-set remove whose
/// add the legacy floor pruned depended on whether the remove arrived before
/// the floor rose, which pruned it with its add, or after, which stored it.
/// The remove is kept either way, so the merge state, and not only what it
/// projects, is the same whatever order the ops arrive in.
#[test]
fn an_or_set_remove_of_a_pruned_add_is_kept_whatever_order_the_floor_rises_in() {
    let mut p = pair();
    let task = shared_task(
        &mut p,
        TaskDraft {
            title: "t".into(),
            ..Default::default()
        },
    );
    let context = EntityRef::new(EntityKind::Context, [0xc1; 16]);
    let create = create_env_for(&p.dba, task.bytes());

    set_clock(&p.cb, T0 + 1_000);
    let (added, tag) = emit_patch(
        &p.eb,
        &mut p.dbb,
        &inbox(),
        &patch(task, false, vec![("contexts", add(vec![eref(context)]))]),
    );
    set_clock(&p.cb, T0 + 1_100);
    let (removed, _) = emit_patch(
        &p.eb,
        &mut p.dbb,
        &inbox(),
        &patch(
            task,
            false,
            vec![("contexts", remove(eref(context), &[tag]))],
        ),
    );
    // A full-state rename on A that never saw either, stamped after both:
    // its fold raises the legacy floor above the add.
    set_clock(&p.ca, T0 + 2_000);
    let renamed =
        p.ea.apply(
            &mut p.dba,
            Command::UpdateTask {
                id: task,
                patch: TaskPatch {
                    title: Some("renamed".into()),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    let renamed = env_bytes(&p.dba, &renamed.op_id);

    let receive = |order: [&Vec<u8>; 4]| -> (Task, Vec<(&'static str, Vec<String>)>) {
        let e = engine_seeded(
            ROOT,
            [9u8; 32],
            Arc::new(FakeClock(PLMutex::new(T0 + 9_000))),
        );
        let mut db = db_root(ROOT);
        trust(&e, &mut db, &p.ea);
        trust(&e, &mut db, &p.eb);
        for env in order {
            e.apply_remote(&mut db, env).unwrap();
        }
        let merge_state = dump(db.conn())
            .into_iter()
            .filter(|(table, _)| table.starts_with("merge_"))
            .collect();
        (task_at(&db, task), merge_state)
    };
    let floor_first = receive([&create, &added, &renamed, &removed]);
    let floor_last = receive([&create, &added, &removed, &renamed]);
    assert_eq!(floor_first.0, floor_last.0);
    assert_eq!(floor_first.1, floor_last.1);
    let removes = floor_first
        .1
        .iter()
        .find(|(table, _)| *table == "merge_orset_removes")
        .map(|(_, rows)| rows.len());
    assert_eq!(removes, Some(1), "the remove is kept, inert");
    assert!(
        floor_first.0.contexts.is_empty(),
        "and the add stays removed"
    );
}

/// The rule `project` reads an unknown map field by, case by case: a whole
/// write is the field's value as of its stamp, an entry before it is
/// superseded, an entry after it sets or (with `null`) removes its key, and
/// newer entries replace a whole value that is not a map.
#[test]
fn a_whole_write_and_the_map_entries_around_it_project_by_stamp() {
    use crate::engine::merge::{map_over_whole, Stamp};
    let at = |ms: u64| Stamp {
        hlc: Hlc::at(T0 + ms),
        device: [1; 16],
        seq: ms,
        stream: [0; 16],
    };
    let int = |n: i64| Value::Integer(n.into());
    let entry = |k: &str, v: Value, ms: u64| (k.to_owned(), v, at(ms));
    let map = |kv: &[(&str, i64)]| Value::Map(kv.iter().map(|(k, v)| (text(k), int(*v))).collect());

    // No whole write: the live entries, a null among them left out.
    assert_eq!(
        map_over_whole(
            None,
            vec![entry("a", int(1), 1), entry("b", Value::Null, 2)]
        ),
        Some(map(&[("a", 1)]))
    );
    // A whole map, an older entry it supersedes, and newer ones on top: one
    // replaces a key, one adds a key, and a null removes one.
    let whole = Some((map(&[("a", 1), ("b", 2), ("c", 3)]), at(10)));
    let merged = map_over_whole(
        whole,
        vec![
            entry("a", int(9), 5),
            entry("b", int(20), 11),
            entry("c", Value::Null, 12),
            entry("d", int(4), 13),
        ],
    );
    assert_eq!(merged, Some(map(&[("a", 1), ("b", 20), ("d", 4)])));
    // An entry at the whole write's own stamp is the same op, not newer.
    assert_eq!(
        map_over_whole(
            Some((map(&[("a", 1)]), at(10))),
            vec![entry("a", int(2), 10)]
        ),
        Some(map(&[("a", 1)]))
    );
    // A whole value that is not a map stands with nothing newer, and newer
    // entries replace it.
    assert_eq!(
        map_over_whole(Some((int(7), at(10))), vec![entry("a", int(2), 3)]),
        None
    );
    assert_eq!(
        map_over_whole(Some((int(7), at(10))), vec![entry("a", int(2), 11)]),
        Some(map(&[("a", 2)]))
    );
}

/// An occurrence is generated once per account, not once per device: a
/// device that already holds the occurrence a peer generated, and the peer's
/// edit of it, generates no create of its own when it materializes the same
/// window. One would be a full-state op stamped after the edit, and would
/// write the template's title back over it on every replica.
#[test]
fn an_occurrence_a_peer_generated_is_not_generated_again() {
    let clock = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let a = engine_seeded(ROOT, [1u8; 32], clock.clone());
    let b = engine_seeded(ROOT, [2u8; 32], clock.clone());
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&b, &mut dbb, &a);
    let routine = apply(
        &a,
        &mut dba,
        Command::CreateRoutine(routine_draft(
            inbox_stream_ref(),
            "FREQ=DAILY",
            NOW + 3_600_000,
            RoutineCatchupPolicy::Skip,
            Vec::new(),
        )),
    );
    let first = occurrence_tasks(&routine_of(&a, &dba, routine), 1)[0].0;
    apply(
        &a,
        &mut dba,
        Command::UpdateTask {
            id: first,
            patch: TaskPatch {
                title: Some("renamed on A".into()),
                ..Default::default()
            },
        },
    );
    for env in all_envelopes(&dba) {
        b.apply_remote_all(&mut dbb, &env).unwrap();
    }

    b.apply(
        &mut dbb,
        Command::MaterializeRoutines { now_ms: NOW as u64 },
    )
    .unwrap();
    let b_creates: i64 = dbb
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM ops WHERE inner_kind = 'task.create' AND device_id = ?",
            params![&b.keychain.device_id()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(b_creates, 0, "B generated nothing A had already generated");
    assert_eq!(task_of(&b, &dbb, first).title, "renamed on A");
}

/// A whole write of an unknown field that a later full-state op superseded
/// is not the base a later map entry lands on: the full-state op left the
/// field out, so the field reads as only what was written after it.
#[test]
fn a_whole_write_below_the_legacy_floor_is_no_base_for_later_entries() {
    let mut p = pair();
    let task = shared_task(
        &mut p,
        TaskDraft {
            title: "t".into(),
            ..Default::default()
        },
    );
    let whole = Value::Map(vec![(text("k0"), Value::Integer(1.into()))]);
    set_clock(&p.ca, T0 + 1_000);
    emit_patch(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &patch(task, false, vec![("x_labels", set(whole))]),
    );
    // A full-state op from a build that never saw the field, as an older
    // build's command writes one: the task with no unknown keys at all.
    let mut bare = task_at(&p.dba, task);
    bare.unknown = Unknowns::new();
    bare.title = "renamed elsewhere".into();
    set_clock(&p.ca, T0 + 2_000);
    emit_patch(&p.ea, &mut p.dba, &inbox(), &InnerOp::TaskUpdate(bare));
    set_clock(&p.ca, T0 + 3_000);
    let entry = op(
        "map",
        Value::Map(vec![(text("k1"), set(Value::Integer(2.into())))]),
    );
    emit_patch(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &patch(task, false, vec![("x_labels", entry)]),
    );

    let labels = task_at(&p.dba, task)
        .unknown
        .get("x_labels")
        .map(|v| v.get().clone());
    assert_eq!(
        labels,
        Some(Value::Map(vec![(text("k1"), Value::Integer(2.into()))])),
        "k0 went with the whole write the full-state op superseded"
    );
    p.ea.assert_rebuild_identity(&p.dba);
}
