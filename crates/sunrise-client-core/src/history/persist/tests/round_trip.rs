//! The property behind the module's reason to exist: everything an undo step
//! can hold survives the file, in all three states of a patch's double option.

use super::*;
use proptest::collection::vec;
use proptest::option::of;
use proptest::prelude::*;
use proptest::sample::select;
use proptest::strategy::{Union, ValueTree};
use proptest::test_runner::TestRunner;
use sunrise_domain::constraint::{ConstraintSeverity, DateRange, TimeOfDayRange, WeekdaySet};
use sunrise_domain::triage::DeferTarget;
use sunrise_domain::{Frequency, TriageAction, Unknowns, ViewFilter, ViewKind, ViewSort, Weekday};

fn id() -> impl Strategy<Value = EntityRef> {
    (
        select(vec![
            EntityKind::Task,
            EntityKind::Stream,
            EntityKind::Context,
            EntityKind::Routine,
            EntityKind::Block,
            EntityKind::SavedView,
        ]),
        any::<[u8; 16]>(),
    )
        .prop_map(|(kind, bytes)| EntityRef::new(kind, bytes))
}

fn ids() -> impl Strategy<Value = Vec<EntityRef>> {
    vec(id(), 0..3)
}

/// A patch's three states: `None`, `Some(None)` and `Some(Some(_))`.
fn clearable<S: Strategy>(value: S) -> impl Strategy<Value = Option<Option<S::Value>>> {
    of(of(value))
}

fn day() -> impl Strategy<Value = jiff::civil::Date> {
    (2000i16..2100, 1i8..=12, 1i8..=28).prop_map(|(y, m, d)| date(y, m, d))
}

fn instant() -> impl Strategy<Value = jiff::Timestamp> {
    (0i64..4_000_000_000).prop_map(|s| jiff::Timestamp::from_second(s).expect("in range"))
}

fn time() -> impl Strategy<Value = SunriseTime> {
    let civil = || (day(), 0i8..24, 0i8..60).prop_map(|(d, h, m)| d.at(h, m, 0, 0));
    prop_oneof![
        instant().prop_map(SunriseTime::instant),
        (civil(), select(vec!["America/New_York", "Europe/Berlin"]))
            .prop_map(|(c, tz)| SunriseTime::zoned(c, tz)),
        civil().prop_map(SunriseTime::floating),
        day().prop_map(SunriseTime::all_day),
        "[a-z_]{1,8}".prop_map(|kind| SunriseTime::Unknown {
            kind: format!("newer_{kind}"),
            raw: Unknowns::new(),
        }),
    ]
}

fn body() -> impl Strategy<Value = NoteBody> {
    vec(any::<u8>(), 0..8).prop_map(NoteBody)
}

/// A known value of a lossy enum, or a raw one this build does not know, which
/// the enum keeps verbatim.
fn raw(known: &'static [&'static str]) -> impl Strategy<Value = &'static str> {
    let mut all = known.to_vec();
    all.push("from_a_newer_build");
    select(all)
}

fn task_state() -> impl Strategy<Value = TaskState> {
    raw(&["todo", "in_progress", "done", "cancelled"]).prop_map(TaskState::from_raw)
}

fn energy() -> impl Strategy<Value = Energy> {
    raw(&["low", "med", "high"]).prop_map(Energy::from_raw)
}

fn color() -> impl Strategy<Value = StreamColor> {
    raw(&["slate", "rose", "pink"]).prop_map(StreamColor::from_raw)
}

fn cadence() -> impl Strategy<Value = StreamReviewCadence> {
    raw(&["weekly", "biweekly", "monthly", "none"]).prop_map(StreamReviewCadence::from_raw)
}

fn catchup() -> impl Strategy<Value = RoutineCatchupPolicy> {
    raw(&["skip", "merge", "queue"]).prop_map(RoutineCatchupPolicy::from_raw)
}

fn constraints() -> impl Strategy<Value = Vec<ScheduleConstraint>> {
    let one = (
        of((0i8..24, 0i8..24).prop_map(|(from, to)| {
            TimeOfDayRange::new(
                jiff::civil::time(from, 0, 0, 0),
                jiff::civil::time(to, 0, 0, 0),
            )
        })),
        of(day().prop_map(|d| DateRange::new(d, None))),
        select(vec![Weekday::Mo, Weekday::We, Weekday::Su]),
        raw(&["hard", "soft"]),
    )
        .prop_map(
            |(time_of_day, date_range, weekday, severity)| ScheduleConstraint {
                time_of_day,
                days_of_week: WeekdaySet::from_days([weekday]),
                date_range,
                severity: ConstraintSeverity::from_raw(severity),
                unknown: Unknowns::new(),
            },
        );
    vec(one, 0..2)
}

fn task_patch() -> impl Strategy<Value = TaskPatch> {
    (
        (
            of(any::<String>()),
            clearable(body()),
            clearable(id()),
            of(ids()),
            of(task_state()),
            clearable(1u8..=5),
            clearable(energy()),
            clearable(any::<u64>()),
        ),
        (
            clearable(time()),
            clearable(time()),
            clearable(time()),
            of(constraints()),
            of(ids()),
            clearable(id()),
            clearable(any::<u32>()),
            of(any::<bool>()),
            clearable("[A-Z]{1,4}"),
            clearable(instant()),
        ),
    )
        .prop_map(|(a, b)| TaskPatch {
            title: a.0,
            body: a.1,
            stream_id: a.2,
            contexts: a.3,
            state: a.4,
            priority: a.5,
            energy: a.6,
            estimated_duration_s: a.7,
            planned_at: b.0,
            target_at: b.1,
            hard_due_at: b.2,
            scheduling_constraints: b.3,
            blocked_by: b.4,
            assignee: b.5,
            reminder_lead_s: b.6,
            archived: b.7,
            sort_order: b.8,
            late_acknowledged_at: b.9,
        })
}

fn stream_patch() -> impl Strategy<Value = StreamPatch> {
    (
        (
            of(any::<String>()),
            clearable(body()),
            of(color()),
            clearable(id()),
            of(cadence()),
            of("[A-Z]{1,4}"),
        ),
        (
            of(any::<bool>()),
            of(any::<bool>()),
            clearable(time()),
            clearable(any::<u32>()),
            clearable(any::<String>()),
            clearable(id()),
        ),
    )
        .prop_map(|(a, b)| StreamPatch {
            name: a.0,
            description: a.1,
            color: a.2,
            parent_id: a.3,
            review_cadence: a.4,
            sort_order: a.5,
            archived: b.0,
            paused: b.1,
            paused_until: b.2,
            reminder_lead_s: b.3,
            icon: b.4,
            default_context: b.5,
        })
}

fn context_patch() -> impl Strategy<Value = ContextPatch> {
    (
        of(any::<String>()),
        clearable(any::<String>()),
        of(any::<bool>()),
    )
        .prop_map(|(name, description, archived)| ContextPatch {
            name,
            description,
            archived,
        })
}

fn template() -> impl Strategy<Value = TaskTemplate> {
    (
        any::<String>(),
        of(id()),
        ids(),
        of(energy()),
        of(1u8..=5),
        of(any::<u64>()),
        of(body()),
    )
        .prop_map(
            |(title, stream_id, contexts, energy, priority, estimated_duration_s, body)| {
                TaskTemplate {
                    title,
                    stream_id,
                    contexts,
                    energy,
                    priority,
                    estimated_duration_s,
                    body,
                    unknown: Unknowns::new(),
                }
            },
        )
}

fn rrule() -> impl Strategy<Value = RRule> {
    (
        select(vec![Frequency::Daily, Frequency::Weekly, Frequency::Yearly]),
        1u32..4,
        of(1u32..10),
        of(time()),
    )
        .prop_map(|(freq, interval, count, until)| RRule {
            freq,
            interval,
            by_day: vec![Weekday::Mo],
            by_month_day: vec![-1],
            by_month: vec![3],
            by_set_pos: vec![1],
            count,
            until,
            wkst: Some(Weekday::Su),
            unknown: Unknowns::new(),
        })
}

fn routine_patch() -> impl Strategy<Value = RoutinePatch> {
    (
        (
            of(template()),
            of(rrule()),
            of(any::<String>()),
            of(time()),
            clearable(time()),
            of(constraints()),
        ),
        (
            of(catchup()),
            clearable(any::<u64>()),
            of(any::<bool>()),
            of(any::<bool>()),
            clearable(time()),
            of(any::<bool>()),
        ),
    )
        .prop_map(|(a, b)| RoutinePatch {
            template: a.0,
            rrule: a.1,
            timezone: a.2,
            starts_at: a.3,
            ends_at: a.4,
            scheduling_constraints: a.5,
            catchup_policy: b.0,
            grace_window_s: b.1,
            forgiveness_enabled: b.2,
            paused: b.3,
            paused_until: b.4,
            archived: b.5,
        })
}

fn edit_scope() -> impl Strategy<Value = EditScope> {
    prop_oneof![
        Just(EditScope::All),
        any::<String>().prop_map(|key| EditScope::This { key }),
        any::<String>().prop_map(|key| EditScope::ThisAndFuture { key }),
    ]
}

fn triage_action() -> impl Strategy<Value = TriageAction> {
    prop_oneof![
        prop_oneof![
            time().prop_map(DeferTarget::At),
            Just(DeferTarget::NextFreeSlot),
        ]
        .prop_map(|to| TriageAction::Defer { to }),
        time().prop_map(|completed_at| TriageAction::AlreadyDone { completed_at }),
        of(any::<String>()).prop_map(|reason| TriageAction::Drop { reason }),
        Just(TriageAction::Keep),
    ]
}

fn bulk_item() -> impl Strategy<Value = BulkTaskItem> {
    let action = prop_oneof![
        Just(BulkTaskAction::Complete),
        time().prop_map(BulkTaskAction::DeferTo),
        task_patch().prop_map(|patch| BulkTaskAction::Update(Box::new(patch))),
        Just(BulkTaskAction::Delete),
        Just(BulkTaskAction::Restore),
    ];
    (id(), action).prop_map(|(id, action)| BulkTaskItem { id, action })
}

fn task_draft() -> impl Strategy<Value = TaskDraft> {
    (
        (
            any::<String>(),
            of(body()),
            of(id()),
            ids(),
            of(1u8..=5),
            of(energy()),
            of(any::<u64>()),
        ),
        (
            of(time()),
            of(time()),
            of(time()),
            constraints(),
            of(id()),
            of(any::<u32>()),
        ),
    )
        .prop_map(|(a, b)| TaskDraft {
            title: a.0,
            body: a.1,
            stream_id: a.2,
            contexts: a.3,
            priority: a.4,
            energy: a.5,
            estimated_duration_s: a.6,
            planned_at: b.0,
            target_at: b.1,
            hard_due_at: b.2,
            scheduling_constraints: b.3,
            assignee: b.4,
            reminder_lead_s: b.5,
        })
}

fn stream_draft() -> impl Strategy<Value = StreamDraft> {
    (
        any::<String>(),
        of(body()),
        of(color()),
        of(id()),
        of(cadence()),
        of(any::<u32>()),
        of(any::<String>()),
        of(id()),
    )
        .prop_map(
            |(
                name,
                description,
                color,
                parent_id,
                review_cadence,
                reminder_lead_s,
                icon,
                default_context,
            )| {
                StreamDraft {
                    name,
                    description,
                    color,
                    parent_id,
                    review_cadence,
                    reminder_lead_s,
                    icon,
                    default_context,
                }
            },
        )
}

fn routine_draft() -> impl Strategy<Value = RoutineDraft> {
    (
        template(),
        rrule(),
        any::<String>(),
        time(),
        of(time()),
        constraints(),
        catchup(),
    )
        .prop_map(
            |(
                template,
                rrule,
                timezone,
                starts_at,
                ends_at,
                scheduling_constraints,
                catchup_policy,
            )| {
                RoutineDraft {
                    template,
                    rrule,
                    timezone,
                    starts_at,
                    ends_at,
                    scheduling_constraints,
                    catchup_policy,
                }
            },
        )
}

fn block_draft() -> impl Strategy<Value = BlockDraft> {
    (
        of(id()),
        time(),
        time(),
        of(any::<String>()),
        any::<bool>(),
        ids(),
    )
        .prop_map(
            |(stream_id, starts_at, ends_at, title, title_track_task, tasks)| BlockDraft {
                stream_id,
                starts_at,
                ends_at,
                title,
                title_track_task,
                tasks,
            },
        )
}

fn view_filter() -> impl Strategy<Value = ViewFilter> {
    any::<String>().prop_map(|text| ViewFilter {
        text,
        ..ViewFilter::default()
    })
}

fn saved_view_draft() -> impl Strategy<Value = SavedViewDraft> {
    (any::<String>(), "[a-z]{1,6}", view_filter()).prop_map(|(name, view, filter)| SavedViewDraft {
        name,
        view: ViewKind::from_raw(&view),
        filter,
        sort: ViewSort::Manual,
    })
}

fn saved_view_patch() -> impl Strategy<Value = SavedViewPatch> {
    (
        of(any::<String>()),
        of("[a-z]{1,6}"),
        of(view_filter()),
        clearable("[A-Z]{1,4}"),
    )
        .prop_map(|(name, view, filter, sort_order)| SavedViewPatch {
            name,
            view: view.as_deref().map(ViewKind::from_raw),
            filter,
            sort: None,
            sort_order,
        })
}

/// One strategy per [`Cmd`] variant, at the index [`cmd_index`] gives it.
fn command_arms() -> Vec<BoxedStrategy<Command>> {
    vec![
        task_draft().prop_map(Command::CreateTask).boxed(),
        (id(), task_patch())
            .prop_map(|(id, patch)| Command::UpdateTask { id, patch })
            .boxed(),
        id().prop_map(Command::CompleteTask).boxed(),
        (id(), any::<u64>())
            .prop_map(|(id, to_ms)| Command::DeferTask { id, to_ms })
            .boxed(),
        (id(), time())
            .prop_map(|(id, to)| Command::DeferTaskTo { id, to })
            .boxed(),
        id().prop_map(Command::DeleteTask).boxed(),
        vec(bulk_item(), 0..3).prop_map(Command::BulkTasks).boxed(),
        id().prop_map(Command::Restore).boxed(),
        (id(), id())
            .prop_map(|(id, stream)| Command::PromoteToStream { id, stream })
            .boxed(),
        stream_draft().prop_map(Command::CreateStream).boxed(),
        (id(), stream_patch())
            .prop_map(|(id, patch)| Command::UpdateStream { id, patch })
            .boxed(),
        id().prop_map(|id| Command::DeleteStream {
            id,
            disposition: Disposition::Detach,
        })
        .boxed(),
        (id(), id())
            .prop_map(|(id, into)| Command::DeleteStream {
                id,
                disposition: Disposition::MoveTo(into),
            })
            .boxed(),
        ids().prop_map(|ids| Command::MarkTriaged { ids }).boxed(),
        ids().prop_map(|ids| Command::ReturnToInbox { ids }).boxed(),
        (ids(), triage_action())
            .prop_map(|(ids, action)| Command::Triage { ids, action })
            .boxed(),
        (any::<String>(), of(any::<String>()))
            .prop_map(|(name, description)| {
                Command::CreateContext(ContextDraft { name, description })
            })
            .boxed(),
        (id(), context_patch())
            .prop_map(|(id, patch)| Command::UpdateContext { id, patch })
            .boxed(),
        id().prop_map(Command::DeleteContext).boxed(),
        routine_draft().prop_map(Command::CreateRoutine).boxed(),
        (id(), edit_scope(), routine_patch())
            .prop_map(|(id, scope, patch)| Command::UpdateRoutine { id, scope, patch })
            .boxed(),
        id().prop_map(Command::DeleteRoutine).boxed(),
        block_draft().prop_map(Command::CreateBlock).boxed(),
        id().prop_map(Command::DeleteBlock).boxed(),
        id().prop_map(Command::DetachFile).boxed(),
        (ids(), of(id()), of(id()))
            .prop_map(|(ids, after, before)| Command::MoveTasks { ids, after, before })
            .boxed(),
        saved_view_draft()
            .prop_map(Command::CreateSavedView)
            .boxed(),
        (id(), saved_view_patch())
            .prop_map(|(id, patch)| Command::UpdateSavedView { id, patch })
            .boxed(),
        id().prop_map(Command::DeleteSavedView).boxed(),
    ]
}

/// The arm of [`command_arms`] that draws this variant. No wildcard, so a
/// variant added to [`Cmd`] fails this build until it is given an arm here and
/// there.
fn cmd_index(cmd: &Cmd) -> usize {
    match cmd {
        Cmd::CreateTask(_) => 0,
        Cmd::UpdateTask { .. } => 1,
        Cmd::CompleteTask(_) => 2,
        Cmd::DeferTask { .. } => 3,
        Cmd::DeferTaskTo { .. } => 4,
        Cmd::DeleteTask(_) => 5,
        Cmd::BulkTasks(_) => 6,
        Cmd::Restore(_) => 7,
        Cmd::PromoteToStream { .. } => 8,
        Cmd::CreateStream(_) => 9,
        Cmd::UpdateStream { .. } => 10,
        Cmd::DeleteStream(_) => 11,
        Cmd::DeleteStreamInto { .. } => 12,
        Cmd::MarkTriaged(_) => 13,
        Cmd::ReturnToInbox(_) => 14,
        Cmd::Triage { .. } => 15,
        Cmd::CreateContext(_) => 16,
        Cmd::UpdateContext { .. } => 17,
        Cmd::DeleteContext(_) => 18,
        Cmd::CreateRoutine(_) => 19,
        Cmd::UpdateRoutine { .. } => 20,
        Cmd::DeleteRoutine(_) => 21,
        Cmd::CreateBlock(_) => 22,
        Cmd::DeleteBlock(_) => 23,
        Cmd::DetachFile(_) => 24,
        Cmd::MoveTasks { .. } => 25,
        Cmd::CreateSavedView(_) => 26,
        Cmd::UpdateSavedView { .. } => 27,
        Cmd::DeleteSavedView(_) => 28,
    }
}

/// The property below covers what these strategies draw, so each of them has
/// to draw the variant it stands for, and nothing is left out.
#[test]
fn every_persisted_command_has_a_strategy_that_draws_it() {
    let arms = command_arms();
    assert_eq!(arms.len(), 29, "one arm per variant `cmd_index` names");
    let mut runner = TestRunner::deterministic();
    for (index, arm) in arms.iter().enumerate() {
        let command = arm.new_tree(&mut runner).expect("draws").current();
        let cmd = Cmd::of(&command).expect("a command this encoding carries");
        assert_eq!(cmd_index(&cmd), index, "{command:?}");
    }
}

fn step() -> impl Strategy<Value = UndoEntry> {
    let command = || Union::new(command_arms());
    (any::<String>(), vec(command(), 1..3), vec(command(), 0..3)).prop_map(
        |(label, forward, backward)| UndoEntry {
            label,
            forward,
            backward,
        },
    )
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        // The repository's convention (docs/10-cross-cutting/testing.md §2):
        // a shrunken counterexample is committed, at a path named here.
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/round_trip.txt",
            ),
        )),
        ..ProptestConfig::default()
    })]

    /// Whatever an undo step can hold reads back as itself, with a clear still
    /// a clear and an untouched field still untouched.
    #[test]
    fn every_persistable_command_round_trips(
        done in vec(step(), 0..3),
        undone in vec(step(), 0..3),
    ) {
        let stacks = UndoStacks { done, undone };
        let (json, dropped) = to_json(&stacks).expect("writes");
        prop_assert_eq!(dropped, 0);
        let back = from_json(&json).expect("reads back");
        prop_assert_eq!(format!("{stacks:?}"), format!("{back:?}"));
    }
}
