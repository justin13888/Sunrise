//! The weekly fold, step by step, and the snapshot draft it turns into.
//!
//! The fixture helpers are shared with the sibling `review` modules' tests.

use super::*;
use crate::activity::{fold_activity, OpPayload, OpRecord};
use crate::routine::{RoutineCatchupPolicy, TaskTemplate};
use crate::rrule::RRule;
use crate::stats::{fold_trends, WeekGrid};
use crate::stream::{StreamColor, StreamReviewCadence};
use jiff::Timestamp;
use sunrise_id::EntityKind;

/// Monday 2026-01-05T00:00:00Z.
pub(in crate::review) const MON: u64 = 1_767_571_200_000;
pub(in crate::review) const DAY: u64 = 24 * 60 * 60 * 1000;
pub(in crate::review) const WEEK: u64 = 7 * DAY;

pub(in crate::review) fn eref(kind: EntityKind, b: u8) -> EntityRef {
    EntityRef::new(kind, [b; 16])
}

fn ts(ms: u64) -> Timestamp {
    Timestamp::from_millisecond(i64::try_from(ms).unwrap()).unwrap()
}

pub(in crate::review) fn task(id: u8, stream: EntityRef, title: &str, created_ms: u64) -> Task {
    Task {
        reminder_lead_s: None,
        id: eref(EntityKind::Task, id),
        created_at: ts(created_ms),
        updated_at: ts(created_ms),
        title: title.into(),
        body: None,
        stream_id: crate::home_stream(Some(stream)),
        contexts: BTreeSet::new(),
        state: TaskState::Todo,
        priority: None,
        energy: None,
        estimated_duration_s: None,
        planned_at: None,
        target_at: None,
        hard_due_at: None,
        scheduling_constraints: Vec::new(),
        completed_at: None,
        deferred_count: 0,
        blocks: BTreeSet::new(),
        blocked_by: BTreeSet::new(),
        assignee: None,
        routine_id: None,
        routine_occurrence: None,
        hard_constraint_violated: false,
        sort_order: None,
        triaged_at: None,
        cancel_reason: None,
        late_acknowledged_at: None,
        archived: false,
        deleted: false,
        unknown: Unknowns::new(),
    }
}

fn stream(id: u8, name: &str) -> Stream {
    Stream {
        reminder_lead_s: None,
        id: eref(EntityKind::Stream, id),
        created_at: ts(MON),
        updated_at: ts(MON),
        name: name.into(),
        description: None,
        color: StreamColor::Slate,
        icon: None,
        parent_id: None,
        sort_order: "a0".into(),
        archived: false,
        paused: false,
        paused_until: None,
        review_cadence: StreamReviewCadence::Weekly,
        default_context: None,
        rehomed_to: None,
        deleted: false,
        unknown: Unknowns::new(),
    }
}

fn op(n: u8, at_ms: u64, payload: OpPayload) -> OpRecord {
    let target = match &payload {
        OpPayload::TaskCreated(t) | OpPayload::TaskUpdated(t) => t.id,
        _ => eref(EntityKind::Task, 0),
    };
    OpRecord {
        op_id: [n; 16],
        at_ms,
        device: [1; 16],
        target,
        payload,
    }
}

fn empty_focus() -> FocusStats {
    FocusStats {
        sessions: 0,
        work_sessions: 0,
        running: 0,
        total_focused_ms: 0,
        interruptions: 0,
        per_stream: Vec::new(),
        per_energy: Vec::new(),
        overall: None,
        top_interruptions: Vec::new(),
    }
}

fn window() -> ReviewWindow {
    ReviewWindow::new(MON, MON + WEEK)
}

/// A vault with one Stream, four tasks and a week of history:
/// `t1` completed, `t2` deferred, `t3` created and untouched,
/// `t4` created and then edited.
struct Fixture {
    tasks: Vec<Task>,
    ops: Vec<OpRecord>,
}

fn fixture() -> Fixture {
    let s = eref(EntityKind::Stream, 3);
    let t1 = task(1, s, "finish the report", MON + DAY);
    let mut t1_done = t1.clone();
    t1_done.state = TaskState::Done;
    t1_done.completed_at = Some(ts(MON + 2 * DAY).into());

    let t2 = task(2, s, "call the bank", MON + DAY);
    let mut t2_def = t2.clone();
    t2_def.deferred_count = 1;
    t2_def.planned_at = Some(ts(MON + 5 * DAY).into());

    let t3 = task(3, s, "read the RFC", MON + 3 * DAY);

    let t4 = task(4, s, "book flights", MON + 3 * DAY);
    let mut t4_edited = t4.clone();
    t4_edited.priority = Some(1);

    let ops = vec![
        op(1, MON + DAY, OpPayload::TaskCreated(Box::new(t1.clone()))),
        op(2, MON + DAY, OpPayload::TaskCreated(Box::new(t2.clone()))),
        op(
            3,
            MON + 2 * DAY,
            OpPayload::TaskUpdated(Box::new(t1_done.clone())),
        ),
        op(
            4,
            MON + 2 * DAY,
            OpPayload::TaskUpdated(Box::new(t2_def.clone())),
        ),
        op(
            5,
            MON + 3 * DAY,
            OpPayload::TaskCreated(Box::new(t3.clone())),
        ),
        op(
            6,
            MON + 3 * DAY,
            OpPayload::TaskCreated(Box::new(t4.clone())),
        ),
        op(
            7,
            MON + 4 * DAY,
            OpPayload::TaskUpdated(Box::new(t4_edited.clone())),
        ),
    ];
    Fixture {
        tasks: vec![t1_done, t2_def, t3, t4_edited],
        ops,
    }
}

fn review_from(f: &Fixture, streams: Vec<ReviewStream>) -> WeeklyReview {
    let grid = WeekGrid::trailing(
        MON + 6 * DAY,
        3,
        &jiff::tz::TimeZone::UTC,
        &crate::Weekday::Mo,
    )
    .unwrap();
    build_weekly_review(WeeklyReviewInput {
        window: window(),
        streams,
        tasks: f.tasks.clone(),
        activity: fold_activity(&f.ops),
        routines: Vec::new(),
        drift: Vec::new(),
        focus: empty_focus(),
        trends: fold_trends(&f.ops, &grid),
        zone: grid.zone().clone(),
    })
}

#[test]
fn step_one_splits_a_streams_week_into_completed_deferred_and_untouched() {
    let f = fixture();
    let r = review_from(&f, vec![ReviewStream::from(&stream(3, "Work"))]);
    assert_eq!(r.streams.len(), 1);
    let s = &r.streams[0];
    assert_eq!(s.name, "Work");
    assert_eq!(
        s.completed
            .iter()
            .map(|t| t.title.as_str())
            .collect::<Vec<_>>(),
        vec!["finish the report"]
    );
    assert_eq!(
        s.deferred
            .iter()
            .map(|t| t.title.as_str())
            .collect::<Vec<_>>(),
        vec!["call the bank"]
    );
    assert_eq!(
        s.created_untouched
            .iter()
            .map(|t| t.title.as_str())
            .collect::<Vec<_>>(),
        vec!["read the RFC"],
        "`book flights` was edited after creation, so it has been acted on"
    );
}

#[test]
fn step_five_counts_match_the_activity_feed() {
    let f = fixture();
    let r = review_from(&f, vec![ReviewStream::from(&stream(3, "Work"))]);
    assert_eq!(
        r.totals,
        ReviewTotals {
            completed: 1,
            deferred: 1,
            dropped: 0,
            created: 4,
            reopened: 0,
            unknown: Unknowns::new(),
        }
    );
}

#[test]
fn a_reopen_inside_the_window_removes_the_task_from_completed() {
    let mut f = fixture();
    let mut reopened = f.tasks[0].clone();
    reopened.state = TaskState::Todo;
    reopened.completed_at = None;
    f.ops.push(op(
        8,
        MON + 5 * DAY,
        OpPayload::TaskUpdated(Box::new(reopened.clone())),
    ));
    f.tasks[0] = reopened;

    let r = review_from(&f, vec![ReviewStream::from(&stream(3, "Work"))]);
    assert!(
        r.streams[0].completed.is_empty(),
        "a task re-opened before the review is not a completion: {:#?}",
        r.streams[0].completed
    );
    assert_eq!(r.totals.completed, 1, "the event still happened…");
    assert_eq!(r.totals.reopened, 1, "…and so did the resurrection");
}

#[test]
fn a_dropped_task_is_counted_and_never_listed_as_completed() {
    let mut f = fixture();
    let mut cancelled = f.tasks[2].clone();
    cancelled.state = TaskState::Cancelled;
    f.ops.push(op(
        8,
        MON + 5 * DAY,
        OpPayload::TaskUpdated(Box::new(cancelled.clone())),
    ));
    f.tasks[2] = cancelled;
    let r = review_from(&f, vec![ReviewStream::from(&stream(3, "Work"))]);
    assert_eq!(r.totals.dropped, 1);
    assert!(r.streams[0].created_untouched.is_empty(), "it was acted on");
}

#[test]
fn activity_outside_the_window_does_not_reach_the_review() {
    let f = fixture();
    let grid = WeekGrid::trailing(
        MON + 6 * DAY,
        3,
        &jiff::tz::TimeZone::UTC,
        &crate::Weekday::Mo,
    )
    .unwrap();
    // Review the *previous* week; nothing in the fixture happened then.
    let r = build_weekly_review(WeeklyReviewInput {
        window: ReviewWindow::new(MON - WEEK, MON),
        streams: vec![ReviewStream::from(&stream(3, "Work"))],
        tasks: f.tasks.clone(),
        activity: fold_activity(&f.ops),
        routines: Vec::new(),
        drift: Vec::new(),
        focus: empty_focus(),
        trends: fold_trends(&f.ops, &grid),
        zone: grid.zone().clone(),
    });
    assert_eq!(r.totals, ReviewTotals::default());
    assert!(r.streams[0].completed.is_empty());
}

#[test]
fn archived_and_paused_streams_are_skipped() {
    let f = fixture();
    let mut archived = stream(3, "Work");
    archived.archived = true;
    assert!(review_from(&f, vec![ReviewStream::from(&archived)])
        .streams
        .is_empty());

    let mut paused = stream(3, "Work");
    paused.paused = true;
    assert!(review_from(&f, vec![ReviewStream::from(&paused)])
        .streams
        .is_empty());
}

#[test]
fn step_two_lists_only_untriaged_inbox_items() {
    use crate::inbox::inbox_stream_ref;
    let inbox = inbox_stream_ref();
    let mut f = fixture();
    let capture = task(9, inbox, "idea from the shower", MON + DAY);
    let mut promoted = task(10, inbox, "already handled", MON + DAY);
    promoted.state = TaskState::Done;
    let mut archived = task(11, inbox, "filed away", MON + DAY);
    archived.archived = true;
    f.tasks.extend([capture, promoted, archived]);

    let r = review_from(&f, vec![ReviewStream::from(&stream(3, "Work"))]);
    assert_eq!(
        r.inbox.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(),
        vec!["idea from the shower"]
    );
}

/// ADR-0045 §6: an inbox task in a state this build does not know reads
/// as `todo`, its fallback, so it is still offered for triage.
#[test]
fn step_two_reads_an_unknown_state_as_untriaged() {
    let inbox = crate::inbox::inbox_stream_ref();
    let mut f = fixture();
    let mut unknown = task(9, inbox, "in a newer build's state", MON + DAY);
    unknown.state = TaskState::from_raw("snoozed");
    f.tasks.push(unknown);

    let r = review_from(&f, vec![ReviewStream::from(&stream(3, "Work"))]);
    assert_eq!(
        r.inbox.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(),
        vec!["in a newer build's state"]
    );
}

/// The no-stream bucket (ADR-0046) is reviewed when any one of its five
/// parts holds something, and dropped when all are empty; a named stream
/// is reviewed even when it has nothing to say.
#[test]
fn the_no_stream_bucket_is_reviewed_when_any_part_holds_something() {
    let none = inbox_stream_ref();
    let build = |tasks: Vec<Task>, ops: Vec<OpRecord>, focus: FocusStats| {
        let grid = WeekGrid::trailing(
            MON + 6 * DAY,
            3,
            &jiff::tz::TimeZone::UTC,
            &crate::Weekday::Mo,
        )
        .unwrap();
        build_weekly_review(WeeklyReviewInput {
            window: window(),
            streams: vec![ReviewStream::from(&stream(3, "Empty"))],
            tasks,
            activity: fold_activity(&ops),
            routines: Vec::new(),
            drift: Vec::new(),
            focus,
            trends: fold_trends(&ops, &grid),
            zone: grid.zone().clone(),
        })
    };
    let keys = |r: &WeeklyReview| r.streams.iter().map(|s| s.stream).collect::<Vec<_>>();
    let named = eref(EntityKind::Stream, 3);

    let r = build(Vec::new(), Vec::new(), empty_focus());
    assert_eq!(keys(&r), vec![named], "an empty named stream stays");

    let t = task(1, none, "done", MON + DAY);
    let mut done = t.clone();
    done.state = TaskState::Done;
    done.completed_at = Some(ts(MON + 2 * DAY).into());
    let r = build(
        vec![done.clone()],
        vec![
            op(1, MON + DAY, OpPayload::TaskCreated(Box::new(t))),
            op(2, MON + 2 * DAY, OpPayload::TaskUpdated(Box::new(done))),
        ],
        empty_focus(),
    );
    let b = r.streams.iter().find(|s| s.stream == none).expect("kept");
    assert_eq!(b.completed.len(), 1, "completed only");
    assert!(b.deferred.is_empty() && b.created_untouched.is_empty());

    let t = task(2, none, "deferred", MON + DAY);
    let mut def = t.clone();
    def.deferred_count = 1;
    def.planned_at = Some(ts(MON + 5 * DAY).into());
    let r = build(
        vec![def.clone()],
        vec![
            op(1, MON + DAY, OpPayload::TaskCreated(Box::new(t))),
            op(2, MON + 2 * DAY, OpPayload::TaskUpdated(Box::new(def))),
        ],
        empty_focus(),
    );
    let b = r.streams.iter().find(|s| s.stream == none).expect("kept");
    assert_eq!(b.deferred.len(), 1, "deferred only");
    assert!(b.completed.is_empty() && b.created_untouched.is_empty());

    let t = task(3, none, "untouched", MON + DAY);
    let r = build(
        vec![t.clone()],
        vec![op(1, MON + DAY, OpPayload::TaskCreated(Box::new(t)))],
        empty_focus(),
    );
    let b = r.streams.iter().find(|s| s.stream == none).expect("kept");
    assert_eq!(b.created_untouched.len(), 1, "untouched only");
    assert!(b.completed.is_empty() && b.deferred.is_empty());

    let mut focus = empty_focus();
    focus.per_stream.push(StreamFocus {
        stream: none,
        sessions: 1,
        focused_ms: 60_000,
        calibration: None,
    });
    let r = build(Vec::new(), Vec::new(), focus);
    let b = r.streams.iter().find(|s| s.stream == none).expect("kept");
    assert!(b.focus.is_some(), "focus only");
    assert!(b.completed.is_empty() && b.trend.is_empty());

    // Created the week before: a trend, and nothing in this window.
    let t = task(4, none, "last week", MON - 3 * DAY);
    let r = build(
        vec![t.clone()],
        vec![op(1, MON - 3 * DAY, OpPayload::TaskCreated(Box::new(t)))],
        empty_focus(),
    );
    let b = r.streams.iter().find(|s| s.stream == none).expect("kept");
    assert!(!b.trend.is_empty(), "trend only");
    assert!(b.completed.is_empty() && b.created_untouched.is_empty() && b.focus.is_none());
}

#[test]
fn step_four_lists_commitments_that_came_due_and_stayed_open() {
    let s = eref(EntityKind::Stream, 3);
    let mut due_and_open = task(20, s, "renew the domain", MON);
    due_and_open.hard_due_at = Some(ts(MON + 2 * DAY).into());
    let mut due_and_done = task(21, s, "pay the invoice", MON);
    due_and_done.hard_due_at = Some(ts(MON + 2 * DAY).into());
    due_and_done.state = TaskState::Done;
    let mut scheduled_open = task(22, s, "draft the memo", MON);
    scheduled_open.planned_at = Some(ts(MON + 3 * DAY).into());
    let mut future = task(23, s, "next month's thing", MON);
    future.hard_due_at = Some(ts(MON + 40 * DAY).into());
    let unscheduled = task(24, s, "someday", MON);

    let f = Fixture {
        tasks: vec![
            due_and_open,
            due_and_done,
            scheduled_open,
            future,
            unscheduled,
        ],
        ops: Vec::new(),
    };
    let r = review_from(&f, vec![ReviewStream::from(&stream(3, "Work"))]);
    let titles: Vec<&str> = r.slipped.iter().map(|t| t.title.as_str()).collect();
    assert_eq!(titles, vec!["renew the domain", "draft the memo"]);
}

#[test]
fn step_three_reports_streaks_longest_first_and_only_drifting_routines() {
    fn routine(id: u8, title: &str, streak: i64) -> Routine {
        Routine {
            id: eref(EntityKind::Routine, id),
            created_at: ts(MON),
            updated_at: ts(MON),
            template: TaskTemplate {
                title: title.into(),
                stream_id: Some(eref(EntityKind::Stream, 3)),
                contexts: Vec::new(),
                energy: None,
                priority: None,
                estimated_duration_s: None,
                body: None,
                unknown: crate::Unknowns::new(),
            },
            rrule: RRule::parse("FREQ=DAILY").unwrap(),
            timezone: "UTC".into(),
            starts_at: ts(MON).into(),
            ends_at: None,
            scheduling_constraints: Vec::new(),
            skip_dates: Vec::new(),
            skipped_keys: Vec::new(),
            catchup_policy: RoutineCatchupPolicy::Skip,
            streak_counter: streak,
            last_completed_at: Some(ts(MON + DAY).into()),
            grace_window_s: None,
            forgiveness_enabled: true,
            streak_started_at: None,
            forgivenesses_in_window: 0,
            split_from: None,
            streak_keys: Vec::new(),
            paused: false,
            paused_until: None,
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        }
    }
    fn drift(id: u8, drift: f64, over: bool) -> RoutineDrift {
        RoutineDrift {
            routine: eref(EntityKind::Routine, id),
            title: "r".into(),
            expected: 10,
            completed: 5,
            skipped: 0,
            missed: 5,
            drift,
            over_threshold: over,
            streak: 0,
            last_completed_at_ms: None,
            paused: false,
        }
    }

    let f = fixture();
    let grid = WeekGrid::trailing(
        MON + 6 * DAY,
        3,
        &jiff::tz::TimeZone::UTC,
        &crate::Weekday::Mo,
    )
    .unwrap();
    let mut deleted = routine(3, "gone", 99);
    deleted.deleted = true;
    let r = build_weekly_review(WeeklyReviewInput {
        window: window(),
        streams: vec![ReviewStream::from(&stream(3, "Work"))],
        tasks: f.tasks.clone(),
        activity: fold_activity(&f.ops),
        routines: vec![routine(1, "stretch", 4), routine(2, "journal", 12), deleted],
        drift: vec![
            drift(1, 0.5, true),
            drift(2, 0.1, false),
            drift(4, 0.9, true),
        ],
        focus: empty_focus(),
        trends: fold_trends(&f.ops, &grid),
        zone: grid.zone().clone(),
    });

    assert_eq!(
        r.streaks
            .iter()
            .map(|s| (s.title.as_str(), s.streak))
            .collect::<Vec<_>>(),
        vec![("journal", 12), ("stretch", 4)],
        "longest first, deleted routines excluded"
    );
    assert_eq!(
        r.drifting_routines
            .iter()
            .map(|d| d.routine)
            .collect::<Vec<_>>(),
        vec![eref(EntityKind::Routine, 4), eref(EntityKind::Routine, 1)],
        "worst drift first, sub-threshold routines omitted"
    );
}

#[test]
fn a_snapshot_draft_agrees_with_the_review_it_was_saved_from() {
    let f = fixture();
    let r = review_from(&f, vec![ReviewStream::from(&stream(3, "Work"))]);
    let draft = r.to_draft(Some("felt scattered".into()));
    assert_eq!(draft.window_start_ms, MON);
    assert_eq!(draft.window_end_ms, MON + WEEK);
    assert_eq!(draft.totals, r.totals);
    assert_eq!(draft.streams.len(), 1);
    assert_eq!(draft.streams[0].completed, 1);
    assert_eq!(draft.streams[0].deferred, 1);
    assert_eq!(draft.note.as_deref(), Some("felt scattered"));
}
