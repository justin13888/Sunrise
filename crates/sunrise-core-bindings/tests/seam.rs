//! The seam, exercised against a real vault.
//!
//! These are the Rust half of the evidence. They cannot see the generated
//! Swift, but they do cover everything that would otherwise only fail once an
//! app was already written against it: that a command lowers into the core's
//! own type and comes back, that the split-optional edit shape means what it
//! says, that a bad id is a typed error rather than a panic, and that the
//! change stream delivers, lags, and cancels.
//!
//! `flavor = "multi_thread"` throughout on purpose. [`SunriseCore::open`]
//! captures `Handle::current()` and the subscription pump is spawned onto it;
//! a current-thread runtime would let a test pass while the shape it is
//! proving deadlocks under the multi-threaded runtime `UniFFI` actually
//! supplies.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use sunrise_core_bindings::dto::{CaptureIssue, Constraint, TaskDraftIn, TaskEdit, TimeValue};
use sunrise_core_bindings::vocab::{
    constraint_summary, duration_clock, energy_label, relative_day, short_duration, today_section,
};
use sunrise_core_bindings::{
    BindingError, ChangeEvent, ChangeListener, CoreCommand, CoreQuery, CoreQueryResult, SunriseCore,
};
use sunrise_domain::TodaySection::{Due, Overdue};
use sunrise_domain::{ConstraintSeverity, Energy, TaskState};

const ROOT: [u8; 32] = [42u8; 32];

async fn open_core() -> (tempfile::TempDir, Arc<SunriseCore>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = SunriseCore::open(
        dir.path().to_string_lossy().into_owned(),
        ROOT.to_vec(),
        "test".into(),
    )
    .await
    .expect("vault opens");
    (dir, core)
}

fn draft(title: &str) -> TaskDraftIn {
    TaskDraftIn {
        title: title.into(),
        body: None,
        stream_id: None,
        contexts: Vec::new(),
        priority: None,
        energy: None,
        estimated_duration_s: None,
        scheduled_at: None,
        due_at: None,
        scheduling_constraints: Vec::new(),
        assignee: None,
        reminder_lead_s: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_lowers_into_the_core_and_the_result_comes_back() {
    let (_dir, core) = open_core().await;
    let out = core
        .submit(CoreCommand::CreateTask {
            draft: draft("Renew passport"),
        })
        .await
        .expect("create");
    assert!(out.entity.to_str().starts_with("tsk_"));
    assert_eq!(out.op_id.len(), 32, "op id is 16 bytes of hex");
    assert!(out.soft_violations.is_empty());

    let CoreQueryResult::Tasks { tasks } = core.query(CoreQuery::Inbox).await.expect("inbox")
    else {
        panic!("Inbox must return tasks");
    };
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].title, "Renew passport");
    assert_eq!(tasks[0].state, TaskState::Todo);
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn setting_and_clearing_are_separate_decisions() {
    // `TaskPatch` distinguishes "leave alone" from "set to nothing", and
    // UniFFI cannot carry `Option<Option<T>>` in every target language. The
    // seam splits the two into `set_x` and `clear_x`; this pins that both
    // halves reach storage.
    let (_dir, core) = open_core().await;
    let id = core
        .submit(CoreCommand::CreateTask {
            draft: draft("Groceries"),
        })
        .await
        .expect("create")
        .entity;

    core.submit(CoreCommand::UpdateTask {
        id,
        edit: TaskEdit {
            set_priority: Some(2),
            set_energy: Some(Energy::High),
            ..TaskEdit::default()
        },
    })
    .await
    .expect("set");
    let CoreQueryResult::Task { task } = core
        .query(CoreQuery::EntityById { id })
        .await
        .expect("read")
    else {
        panic!("EntityById on a task must return a task");
    };
    assert_eq!(task.priority, Some(2));
    assert_eq!(task.energy, Some(Energy::High));

    core.submit(CoreCommand::UpdateTask {
        id,
        edit: TaskEdit {
            clear_priority: true,
            ..TaskEdit::default()
        },
    })
    .await
    .expect("clear");
    let CoreQueryResult::Task { task } = core
        .query(CoreQuery::EntityById { id })
        .await
        .expect("read")
    else {
        panic!("EntityById on a task must return a task");
    };
    assert_eq!(task.priority, None, "clear reaches storage");
    assert_eq!(
        task.energy,
        Some(Energy::High),
        "a field the edit did not mention is untouched"
    );
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_and_clear_together_is_a_clear() {
    // A contradiction has to resolve somewhere. `clear` wins because it is the
    // decision that cannot be expressed any other way.
    let (_dir, core) = open_core().await;
    let id = core
        .submit(CoreCommand::CreateTask {
            draft: draft("Contradiction"),
        })
        .await
        .expect("create")
        .entity;
    core.submit(CoreCommand::UpdateTask {
        id,
        edit: TaskEdit {
            set_priority: Some(3),
            ..TaskEdit::default()
        },
    })
    .await
    .expect("set");
    core.submit(CoreCommand::UpdateTask {
        id,
        edit: TaskEdit {
            set_priority: Some(5),
            clear_priority: true,
            ..TaskEdit::default()
        },
    })
    .await
    .expect("both");
    let CoreQueryResult::Task { task } = core
        .query(CoreQuery::EntityById { id })
        .await
        .expect("read")
    else {
        panic!("EntityById on a task must return a task");
    };
    assert_eq!(task.priority, None);
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn capture_parses_its_annotations_and_commits() {
    let (_dir, core) = open_core().await;
    let out = core
        .capture("Renew passport !1 ~1h".into(), "UTC".into())
        .await
        .expect("capture");
    let CoreQueryResult::Task { task } = core
        .query(CoreQuery::EntityById { id: out.entity })
        .await
        .expect("read")
    else {
        panic!("EntityById on a task must return a task");
    };
    assert_eq!(task.title, "Renew passport");
    assert_eq!(task.priority, Some(1));
    assert_eq!(task.estimated_duration_s, Some(3600));
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_timezone_falls_back_rather_than_losing_the_capture() {
    let (_dir, core) = open_core().await;
    let out = core
        .capture("Something".into(), "Mars/Olympus_Mons".into())
        .await
        .expect("capture still succeeds");
    assert!(out.entity.to_str().starts_with("tsk_"));
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_vault_root_is_a_typed_error_not_a_panic() {
    let dir = tempfile::tempdir().expect("tempdir");
    let err = SunriseCore::open(
        dir.path().to_string_lossy().into_owned(),
        vec![1, 2, 3],
        "test".into(),
    )
    .await
    .expect_err("three bytes is not a vault root");
    assert!(
        matches!(err, BindingError::BadVaultRoot { len: 3 }),
        "{err}"
    );
}

#[test]
fn a_bad_id_is_rejected_by_the_custom_type_rather_than_guessed_at() {
    // The `EntityRef` custom type is what carries every id across the seam.
    // Garbage must surface as a typed error on the foreign side, not as a
    // zeroed id that then addresses nothing.
    assert!(sunrise_id::EntityRef::parse_any("not-an-id").is_err());
    let id = sunrise_id::EntityRef::new(sunrise_id::EntityKind::Task, [7u8; 16]);
    assert_eq!(
        sunrise_id::EntityRef::parse_any(&id.to_str()).expect("round trips"),
        id
    );
}

/// Records what the pump delivers.
#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<ChangeEvent>>,
    lagged: AtomicU64,
    closed: AtomicBool,
}

impl ChangeListener for Recorder {
    fn on_change(&self, event: ChangeEvent) {
        if let Ok(mut g) = self.events.lock() {
            g.push(event);
        }
    }
    fn on_lagged(&self, skipped: u64) {
        self.lagged.fetch_add(skipped, Ordering::SeqCst);
    }
    fn on_closed(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

impl Recorder {
    fn count(&self) -> usize {
        self.events.lock().map(|g| g.len()).unwrap_or(0)
    }
}

/// Consumes slowly enough to fall behind a burst.
struct Slow(Arc<Recorder>);

impl ChangeListener for Slow {
    fn on_change(&self, event: ChangeEvent) {
        std::thread::sleep(std::time::Duration::from_millis(2));
        self.0.on_change(event);
    }
    fn on_lagged(&self, skipped: u64) {
        self.0.on_lagged(skipped);
    }
    fn on_closed(&self) {
        self.0.on_closed();
    }
}

/// Poll until `f` holds or the budget runs out. The pump is a spawned task, so
/// delivery is asynchronous by construction; a bare sleep would either be flaky
/// or slow.
async fn until(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if f() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn the_change_stream_delivers_and_cancels() {
    let (_dir, core) = open_core().await;
    let rec = Arc::new(Recorder::default());
    let sub = core.subscribe_changes(rec.clone());

    for i in 0..3 {
        core.submit(CoreCommand::CreateTask {
            draft: draft(&format!("task {i}")),
        })
        .await
        .expect("create");
    }
    assert!(
        until(|| rec.count() >= 3).await,
        "three writes must reach the listener, saw {}",
        rec.count()
    );
    assert!(matches!(
        rec.events.lock().expect("lock")[0],
        ChangeEvent::Created { .. }
    ));

    sub.cancel();
    let after_cancel = rec.count();
    core.submit(CoreCommand::CreateTask {
        draft: draft("after cancel"),
    })
    .await
    .expect("create");
    // Give the pump every chance to wrongly deliver.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        rec.count(),
        after_cancel,
        "cancel stops the pump; a cancelled subscription that keeps firing \
         would keep a deallocated view model alive"
    );
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_listener_is_told_it_fell_behind() {
    // The broadcast channel behind the stream holds 256 events and is lossy
    // past that. A listener that only implements `on_change` therefore shows
    // stale data after a sync burst, silently — which is exactly the catch-up
    // case. This pins that `on_lagged` is reachable, so a bridge that ignores
    // it is ignoring something real.
    let (_dir, core) = open_core().await;

    let rec = Arc::new(Recorder::default());
    let sub = core.subscribe_changes(Arc::new(Slow(rec.clone())));

    for i in 0..400 {
        core.submit(CoreCommand::CreateTask {
            draft: draft(&format!("burst {i}")),
        })
        .await
        .expect("create");
    }
    assert!(
        until(|| rec.lagged.load(Ordering::SeqCst) > 0).await,
        "a slow listener must be told it fell behind; seen {}, lagged {}",
        rec.count(),
        rec.lagged.load(Ordering::SeqCst)
    );
    sub.cancel();
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_whole_read_surface_answers() {
    // Not an assertion about *what* each query returns — the core's own tests
    // do that — but that every variant lowers, runs, and lifts back into the
    // right result shape. A variant that panics in `try_from_core` would reach
    // Swift as a trapped `rustPanic`, i.e. a crash, so "it does not throw" is
    // the property worth pinning here.
    let (_dir, core) = open_core().await;
    let id = core
        .submit(CoreCommand::CreateTask {
            draft: draft("Something to read back"),
        })
        .await
        .expect("create")
        .entity;
    let now = core.now_ms();

    let queries = vec![
        CoreQuery::Today {
            now_ms: now,
            contexts: vec![],
        },
        CoreQuery::Inbox,
        CoreQuery::EntityById { id },
        CoreQuery::DeviceList,
        CoreQuery::SyncStatus,
        CoreQuery::StreamList,
        CoreQuery::Contexts,
        CoreQuery::Routines,
        CoreQuery::Actionable {
            stream: None,
            limit: 10,
        },
        CoreQuery::FocusPlan {
            stream: None,
            energy: None,
            length: sunrise_domain::SessionLength::OnePomodoro,
            limit: 5,
        },
        CoreQuery::TaskFocusSessions { task: id, limit: 5 },
        CoreQuery::RunningFocusSessions,
        CoreQuery::FocusStats {
            stream: None,
            since_ms: None,
            now_ms: now,
        },
        CoreQuery::UnblockCascade { task: id },
        CoreQuery::WeeklyReview {
            week_start_ms: None,
            now_ms: now,
        },
        CoreQuery::DailyReview {
            since_ms: now.saturating_sub(86_400_000),
            now_ms: now,
        },
        CoreQuery::StreamTrends {
            weeks: 4,
            now_ms: now,
        },
        CoreQuery::ActivityTimeline {
            entity: id,
            limit: 10,
        },
        CoreQuery::ReviewHistory { limit: 10 },
        CoreQuery::ExportStats {
            dataset: sunrise_domain::ExportDataset::Trends,
            format: sunrise_domain::ExportFormat::Json,
            weeks: 4,
            now_ms: now,
        },
        CoreQuery::Search {
            text: "something".into(),
            limit: 10,
        },
    ];
    for q in queries {
        let label = format!("{q:?}");
        core.query(q)
            .await
            .unwrap_or_else(|e| panic!("{label} failed: {e}"));
    }
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_planner_row_carries_the_shared_explanation() {
    let (_dir, core) = open_core().await;
    core.submit(CoreCommand::CreateTask {
        draft: draft("Actionable thing"),
    })
    .await
    .expect("create");
    let CoreQueryResult::FocusPlan { rows } = core
        .query(CoreQuery::FocusPlan {
            stream: None,
            energy: None,
            length: sunrise_domain::SessionLength::OnePomodoro,
            limit: 5,
        })
        .await
        .expect("plan")
    else {
        panic!("FocusPlan must return plan rows");
    };
    assert_eq!(rows.len(), 1);
    // The phrasing comes from `sunrise_domain::plan_reason`, so the app and
    // the CLI say the same thing about the same row.
    assert!(rows[0].reason.contains("unblocks nothing"), "{:?}", rows[0]);
    core.shutdown().await;
}

/// The login record must never print its tokens. It crosses the FFI seam and
/// is exactly the kind of value someone debug-prints while wiring up Swift.
#[test]
fn login_credentials_redact_under_debug() {
    let c = sunrise_core_bindings::LoginCredentials {
        access_token: "super-secret-access".into(),
        refresh_token: Some("super-secret-refresh".into()),
        expires_at_ms: 10,
        renew_at_ms: 5,
    };
    let s = format!("{c:?}");
    assert!(!s.contains("super-secret-access"), "{s}");
    assert!(!s.contains("super-secret-refresh"), "{s}");
    assert!(s.contains("expires_at_ms: 10"), "{s}");
}

/// And a login object does not print the client id or a PKCE verifier.
#[test]
fn a_login_reports_only_whether_one_is_in_flight() {
    let l = sunrise_core_bindings::SunriseLogin::new(
        "https://issuer.example".into(),
        "the-client-id".into(),
    );
    let s = format!("{l:?}");
    assert!(!s.contains("the-client-id"), "{s}");
    assert!(s.contains("login_in_progress: false"), "{s}");
}

// ---------------------------------------------------------------------------
// Capture preview and the shared vocabulary
// ---------------------------------------------------------------------------

/// A preview is a read. The whole reason it exists is that a capture field
/// re-parses on every keystroke, and a version of it that wrote would create
/// one task per character.
#[tokio::test(flavor = "multi_thread")]
async fn previewing_a_capture_writes_nothing() {
    let (_dir, core) = open_core().await;
    let before = inbox_len(&core).await;

    for prefix in ["R", "Re", "Ren", "Renew passport"] {
        let p = core
            .preview_capture(prefix.into(), "UTC".into())
            .await
            .expect("preview");
        assert_eq!(p.draft.title, prefix);
    }

    assert_eq!(inbox_len(&core).await, before, "a preview created a task");
}

/// The preview's draft is the value the commit uses, so the two cannot
/// disagree about what was parsed.
#[tokio::test(flavor = "multi_thread")]
async fn a_previewed_draft_is_what_gets_committed() {
    let (_dir, core) = open_core().await;
    let p = core
        .preview_capture("Renew passport !1 ~1h".into(), "UTC".into())
        .await
        .expect("preview");
    assert_eq!(p.draft.title, "Renew passport");
    assert_eq!(p.draft.priority, Some(1));
    assert_eq!(p.draft.estimated_duration_s, Some(3600));
    assert!(p.issues.is_empty(), "{:?}", p.issues);

    let out = core
        .submit(CoreCommand::CreateTask { draft: p.draft })
        .await
        .expect("create");
    let CoreQueryResult::Task { task } = core
        .query(CoreQuery::EntityById { id: out.entity })
        .await
        .expect("read back")
    else {
        panic!("expected a task");
    };
    assert_eq!(task.title, "Renew passport");
    assert_eq!(task.priority, Some(1));
    assert_eq!(task.estimated_duration_s, Some(3600));
}

/// An annotation that cannot be applied is reported, and its text stays in the
/// title. Losing what someone typed is the worse failure.
#[tokio::test(flavor = "multi_thread")]
async fn a_preview_explains_what_it_could_not_resolve() {
    let (_dir, core) = open_core().await;
    let p = core
        .preview_capture("Call the vet #nosuchstream".into(), "UTC".into())
        .await
        .expect("preview");
    match p.issues.as_slice() {
        [CaptureIssue::UnknownStream { name }] => assert_eq!(name, "nosuchstream"),
        other => panic!("expected one unknown stream, got {other:?}"),
    }
    assert!(p.draft.title.contains("nosuchstream"), "{}", p.draft.title);
}

async fn inbox_len(core: &SunriseCore) -> usize {
    match core.query(CoreQuery::Inbox).await.expect("inbox") {
        CoreQueryResult::Tasks { tasks } => tasks.len(),
        other => panic!("expected tasks, got {other:?}"),
    }
}

/// The overdue boundary is `due_at < start_of_today_local`. A client that
/// wrote `due_at < now` would fail exactly this case, which is why the
/// function is exported instead of documented.
#[test]
fn a_deadline_earlier_today_crosses_as_due_not_overdue() {
    let tz = "America/New_York";
    let due = TimeValue::Zoned {
        civil: jiff::civil::date(2026, 3, 10).at(9, 0, 0, 0),
        tz: tz.into(),
    };
    let now_ms = ms_at(2026, 3, 10, 17, tz);
    assert_eq!(today_section(None, Some(due), now_ms, tz.into()), Due);

    let yesterday = TimeValue::AllDay {
        date: jiff::civil::date(2026, 3, 9),
    };
    assert_eq!(
        today_section(None, Some(yesterday), now_ms, tz.into()),
        Overdue
    );
}

/// An unknown zone must not take the screen down; the seam falls back to UTC
/// the same way `capture` does.
#[test]
fn an_unknown_zone_falls_back_rather_than_failing_the_row() {
    let now_ms = ms_at(2026, 3, 10, 17, "UTC");
    let due = TimeValue::AllDay {
        date: jiff::civil::date(2026, 3, 10),
    };
    assert_eq!(
        today_section(None, Some(due), now_ms, "Mars/Olympus_Mons".into()),
        Due
    );
}

#[test]
fn the_day_and_duration_words_come_from_the_domain() {
    let tz = "America/New_York";
    let now_ms = ms_at(2026, 3, 10, 17, tz);
    let tomorrow = relative_day(
        TimeValue::AllDay {
            date: jiff::civil::date(2026, 3, 11),
        },
        now_ms,
        tz.into(),
    );
    assert_eq!(tomorrow.text, "tomorrow");
    assert!(!tomorrow.is_past);

    let last_week = relative_day(
        TimeValue::AllDay {
            date: jiff::civil::date(2026, 3, 3),
        },
        now_ms,
        tz.into(),
    );
    assert_eq!(last_week.text, "-7d");
    assert!(last_week.is_past);

    assert_eq!(short_duration(5400), "1h30");
    assert_eq!(duration_clock(3_661_000), "1:01:01");
}

/// `None` is "any" — the value that drops energy from the planner's ranking —
/// and not "none", which would read as an energy level of its own.
#[test]
fn an_absent_energy_facet_reads_as_any() {
    assert_eq!(energy_label(None), "any");
    assert_eq!(energy_label(Some(Energy::Med)), "med");
    assert_eq!(energy_label(Some(Energy::High)), "high");
}

#[test]
fn a_constraint_summary_counts_the_hard_ones() {
    let hard = Constraint {
        time_of_day: None,
        days_of_week: Vec::new(),
        date_range: None,
        severity: ConstraintSeverity::Hard,
    };
    let soft = Constraint {
        severity: ConstraintSeverity::Soft,
        ..hard.clone()
    };
    assert_eq!(
        constraint_summary(vec![hard, soft]),
        "2 constraints (1 hard)"
    );
    assert_eq!(constraint_summary(Vec::new()), "0 constraints (0 hard)");
}

/// Epoch ms for `H:00` on a civil date in `tz`.
fn ms_at(y: i16, m: i8, d: i8, hour: i8, tz: &str) -> u64 {
    let zone = jiff::tz::TimeZone::get(tz).expect("tz");
    u64::try_from(
        zone.to_zoned(jiff::civil::date(y, m, d).at(hour, 0, 0, 0))
            .expect("civil time exists")
            .timestamp()
            .as_millisecond(),
    )
    .expect("after the epoch")
}

// ---------------------------------------------------------------------------
// Recurrence
// ---------------------------------------------------------------------------

/// The phrase people type is read by the domain, not by a client.
///
/// `weekdays` is the case that matters: it is five days, it is not derivable
/// from the word, and a client that expanded it itself would be the place the
/// app and `sunrise-cli` first disagreed about which days a routine fires on.
#[test]
fn a_recurrence_phrase_is_read_by_the_domain() {
    use sunrise_core_bindings::vocab::parse_recurrence;
    use sunrise_domain::{Frequency, Weekday};

    let r = parse_recurrence("every 2 weeks on tue".into()).expect("phrase parses");
    assert_eq!(r.freq, Frequency::Weekly);
    assert_eq!(r.interval, 2);
    assert_eq!(r.by_day, vec![Weekday::Tu]);

    let weekdays = parse_recurrence("weekdays".into()).expect("weekdays parses");
    assert_eq!(
        weekdays.by_day,
        vec![
            Weekday::Mo,
            Weekday::Tu,
            Weekday::We,
            Weekday::Th,
            Weekday::Fr
        ],
        "five days, and not derivable from the word"
    );

    // The RFC 5545 body still goes through, for anyone who wants to be exact.
    let raw = parse_recurrence("FREQ=MONTHLY;BYMONTHDAY=-1".into()).expect("rfc body parses");
    assert_eq!(raw.freq, Frequency::Monthly);
    assert_eq!(raw.by_month_day, vec![-1]);
}

/// An unreadable cadence is refused with what was typed, not rounded to the
/// nearest thing that parses.
#[test]
fn an_unreadable_recurrence_is_a_typed_error_carrying_the_phrase() {
    use sunrise_core_bindings::vocab::parse_recurrence;

    let err = parse_recurrence("every blue moon".into()).expect_err("must not guess");
    match err {
        BindingError::BadRecurrence { text, cause } => {
            assert_eq!(
                text, "every blue moon",
                "the phrase comes back for the field"
            );
            assert!(
                cause.contains("blue"),
                "the domain names the token: {cause}"
            );
        }
        other => panic!("{other:?}"),
    }
}

/// Parsing and describing are inverses, and both live in the domain — which is
/// what lets a routine editor show the rule it is about to save.
#[test]
fn a_parsed_rule_describes_itself_in_the_words_it_came_from() {
    use sunrise_core_bindings::vocab::{parse_recurrence, recurrence_summary};

    let rule = parse_recurrence("every 2 weeks on mon, wed".into()).expect("parses");
    assert_eq!(recurrence_summary(rule), "every 2 weeks on Mo, We");
    assert_eq!(
        recurrence_summary(parse_recurrence("daily x12".into()).expect("parses")),
        "every day \u{00d7}12"
    );
}

/// The rule a routine carries round-trips through the seam's record, so an
/// editor that reads a routine, shows its cadence and saves it back cannot
/// quietly rewrite the schedule.
#[tokio::test(flavor = "multi_thread")]
async fn a_routine_round_trips_its_recurrence_through_the_seam() {
    use sunrise_core_bindings::dto::{RoutineDraftIn, RoutineEdit, Template};
    use sunrise_core_bindings::vocab::{parse_recurrence, recurrence_summary};

    let (_dir, core) = open_core().await;
    let stream = sunrise_domain::inbox_stream_ref();
    let created = core
        .submit(CoreCommand::CreateRoutine {
            draft: RoutineDraftIn {
                template: Template {
                    title: "Water the plants".into(),
                    stream_id: stream,
                    contexts: Vec::new(),
                    energy: None,
                    priority: None,
                    estimated_duration_s: None,
                    body: None,
                },
                rrule: parse_recurrence("every monday".into()).expect("parses"),
                timezone: "UTC".into(),
                starts_at: jiff::Timestamp::from_millisecond(1_700_000_000_000).expect("ts"),
                ends_at: None,
                scheduling_constraints: Vec::new(),
                catchup_policy: sunrise_domain::RoutineCatchupPolicy::Skip,
            },
        })
        .await
        .expect("create routine");

    let CoreQueryResult::Routines { routines } =
        core.query(CoreQuery::Routines).await.expect("routines")
    else {
        panic!("Routines must return routines");
    };
    let row = routines
        .iter()
        .find(|r| r.id == created.entity)
        .expect("the routine we just made");
    assert_eq!(recurrence_summary(row.rrule.clone()), "every week on Mo");

    // Re-save it under a phrase the user retyped.
    core.submit(CoreCommand::UpdateRoutine {
        id: created.entity,
        edit: RoutineEdit {
            rrule: Some(parse_recurrence("weekdays".into()).expect("parses")),
            ..RoutineEdit::default()
        },
    })
    .await
    .expect("update routine");

    let CoreQueryResult::Routines { routines } =
        core.query(CoreQuery::Routines).await.expect("routines")
    else {
        panic!("Routines must return routines");
    };
    let row = routines
        .iter()
        .find(|r| r.id == created.entity)
        .expect("still there");
    assert_eq!(
        recurrence_summary(row.rrule.clone()),
        "every week on Mo, Tu, We, Th, Fr"
    );
    core.shutdown().await;
}

// ---------------------------------------------------------------------------
// Undo / redo and saved views
// ---------------------------------------------------------------------------

/// Undo is a **new write**, and the round trip proves it reaches storage
/// rather than only the stack.
#[tokio::test(flavor = "multi_thread")]
async fn undo_reopens_a_completed_task_and_redo_finishes_it_again() {
    let (_dir, core) = open_core().await;
    let created = core
        .submit(CoreCommand::CreateTask {
            draft: draft("Renew passport"),
        })
        .await
        .expect("create");

    let done = core
        .submit_undoable(
            CoreCommand::CompleteTask { id: created.entity },
            "complete \u{201c}Renew passport\u{201d}".into(),
        )
        .await
        .expect("complete");
    assert!(done.not_undoable.is_none(), "a completion is reversible");
    assert_eq!(
        core.undo_state().undo_label.as_deref(),
        Some("complete \u{201c}Renew passport\u{201d}"),
        "the menu says what it would undo"
    );
    assert_eq!(state_of(&core, created.entity).await, TaskState::Done);

    assert_eq!(
        core.undo().await.expect("undo"),
        Some("complete \u{201c}Renew passport\u{201d}".into())
    );
    assert_eq!(
        state_of(&core, created.entity).await,
        TaskState::Todo,
        "the reopen reached storage"
    );

    let after = core.undo_state();
    assert!(after.undo_label.is_none(), "the stack is empty again");
    assert!(after.redo_label.is_some(), "and the step is redoable");

    core.redo().await.expect("redo");
    assert_eq!(state_of(&core, created.entity).await, TaskState::Done);
    core.shutdown().await;
}

/// A delete is submitted and reported as un-undoable — not refused, and not
/// silently accepted onto a stack that would do nothing.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_still_happens_and_says_it_cannot_be_undone() {
    use sunrise_core_bindings::client::undo_refusal_explanation;
    use sunrise_core_bindings::UndoRefusal;

    let (_dir, core) = open_core().await;
    let created = core
        .submit(CoreCommand::CreateTask {
            draft: draft("Cancel the gym"),
        })
        .await
        .expect("create");

    let out = core
        .submit_undoable(
            CoreCommand::DeleteTask { id: created.entity },
            "delete \u{201c}Cancel the gym\u{201d}".into(),
        )
        .await
        .expect("delete");

    assert_eq!(out.not_undoable, Some(UndoRefusal::Deleted));
    assert!(
        undo_refusal_explanation(UndoRefusal::Deleted).contains("tombstone"),
        "the seam words it, not each client"
    );
    assert_eq!(inbox_len(&core).await, 0, "the delete still happened");
    assert!(
        core.undo_state().undo_label.is_none(),
        "nothing went on the stack, so nothing is offered"
    );
    core.shutdown().await;
}

/// A defer's date comes back; its counter does not. `deferred_count` is a
/// PN-counter, and a review that said "deferred three times" should not change
/// its mind because one of them was undone.
#[tokio::test(flavor = "multi_thread")]
async fn undoing_a_defer_restores_the_date_but_not_the_counter() {
    let (_dir, core) = open_core().await;
    let created = core
        .submit(CoreCommand::CreateTask {
            draft: draft("Book the ferry"),
        })
        .await
        .expect("create");

    let to = core.now_ms() + 86_400_000;
    core.submit_undoable(
        CoreCommand::DeferTask {
            id: created.entity,
            to_ms: to,
        },
        "defer".into(),
    )
    .await
    .expect("defer");
    assert_eq!(task_of(&core, created.entity).await.deferred_count, 1);

    core.undo().await.expect("undo");

    let after = task_of(&core, created.entity).await;
    assert!(after.scheduled_at.is_none(), "the date came back");
    assert_eq!(after.deferred_count, 1, "the history did not");
    core.shutdown().await;
}

/// A new write after an undo closes the branch that was undone: replaying it
/// would submit a command built against a vault that has since moved.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_step_clears_the_redo_stack() {
    let (_dir, core) = open_core().await;
    let a = core
        .submit(CoreCommand::CreateTask { draft: draft("A") })
        .await
        .expect("create");
    let b = core
        .submit(CoreCommand::CreateTask { draft: draft("B") })
        .await
        .expect("create");

    core.submit_undoable(CoreCommand::CompleteTask { id: a.entity }, "a".into())
        .await
        .expect("complete a");
    core.undo().await.expect("undo");
    assert!(core.undo_state().redo_label.is_some());

    core.submit_undoable(CoreCommand::CompleteTask { id: b.entity }, "b".into())
        .await
        .expect("complete b");

    assert!(
        core.undo_state().redo_label.is_none(),
        "the undone branch is no longer reachable"
    );
    core.shutdown().await;
}

/// An edit inverts only the fields it touched, read from the vault rather than
/// from whatever the client was holding.
#[tokio::test(flavor = "multi_thread")]
async fn undoing_an_edit_restores_only_what_it_changed() {
    let (_dir, core) = open_core().await;
    let created = core
        .submit(CoreCommand::CreateTask {
            draft: TaskDraftIn {
                priority: Some(2),
                estimated_duration_s: Some(1800),
                ..draft("Write the brief")
            },
        })
        .await
        .expect("create");

    core.submit_undoable(
        CoreCommand::UpdateTask {
            id: created.entity,
            edit: TaskEdit {
                set_priority: Some(5),
                ..TaskEdit::default()
            },
        },
        "set priority".into(),
    )
    .await
    .expect("edit");
    assert_eq!(task_of(&core, created.entity).await.priority, Some(5));

    core.undo().await.expect("undo");

    let after = task_of(&core, created.entity).await;
    assert_eq!(after.priority, Some(2), "restored");
    assert_eq!(
        after.estimated_duration_s,
        Some(1800),
        "untouched fields stay untouched"
    );
    core.shutdown().await;
}

/// Saved views survive a write and a re-read, and carry the domain's own
/// one-line summary rather than each client's.
#[test]
fn a_saved_view_round_trips_through_the_file() {
    use sunrise_core_bindings::{PrimaryView, SavedView, SavedViews};

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nested").join("views.toml");
    let store = SavedViews::at_path(path.to_string_lossy().into_owned());

    assert!(
        store.load().views.is_empty(),
        "an absent file is an empty set, not an error"
    );

    store
        .save(vec![SavedView {
            name: "errands".into(),
            view: PrimaryView::Search,
            query: "passport".into(),
            contexts: vec!["errands".into(), "home".into()],
            summary: String::new(),
        }])
        .expect("save");

    let read = store.load();
    assert!(read.warnings.is_empty());
    assert_eq!(read.views.len(), 1);
    assert_eq!(read.views[0].name, "errands");
    assert_eq!(read.views[0].view, PrimaryView::Search);
    assert_eq!(read.views[0].contexts, vec!["errands", "home"]);
    assert_eq!(
        read.views[0].summary, "search \u{00b7} /passport \u{00b7} @errands @home",
        "the summary is the domain's, not a client's"
    );
}

/// One bad line costs that view and nothing else. A preference file that took
/// the whole set down with it is how a typo costs someone their app.
#[test]
fn a_malformed_line_is_a_warning_not_a_lost_file() {
    use sunrise_core_bindings::SavedViews;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("views.toml");
    std::fs::write(
        &path,
        "good = \"view=today\"\nbad = \"view=nonesuch\"\nalso = \"context=typo\"\n",
    )
    .expect("write");

    let read = SavedViews::at_path(path.to_string_lossy().into_owned()).load();
    assert_eq!(read.views.len(), 1);
    assert_eq!(read.views[0].name, "good");
    assert_eq!(read.warnings.len(), 2, "{:?}", read.warnings);
}

/// A spec can be validated before it is written, by the same reader that will
/// parse it back.
#[test]
fn a_typed_spec_is_checked_by_the_reader_that_will_read_it() {
    use sunrise_core_bindings::client::parse_saved_view;

    let ok = parse_saved_view("deep".into(), "view=today;contexts=deep-work".into())
        .expect("a good spec");
    assert_eq!(ok.contexts, vec!["deep-work"]);

    // A typo'd key is refused rather than ignored: silently saving no filter
    // would recall the wrong thing forever.
    assert!(parse_saved_view("oops".into(), "view=today;context=deep".into()).is_err());
}

async fn task_of(core: &SunriseCore, id: sunrise_id::EntityRef) -> sunrise_core_bindings::TaskItem {
    let CoreQueryResult::Task { task } = core
        .query(CoreQuery::EntityById { id })
        .await
        .expect("entity by id")
    else {
        panic!("a task id must return a task");
    };
    task
}

async fn state_of(core: &SunriseCore, id: sunrise_id::EntityRef) -> TaskState {
    task_of(core, id).await.state
}

/// A split-optional edit that names one field must leave every other field
/// alone — including the clearable ones.
///
/// The Rust side gets that from `Default`; the *foreign* side gets it from the
/// `#[uniffi(default)]` on every field, without which a Swift caller has to
/// spell out all thirteen and the twelve it does not care about are exactly
/// where a `true` lands on a `clear_` flag by accident. This pins the
/// behaviour those defaults are there to produce.
#[tokio::test(flavor = "multi_thread")]
async fn a_one_field_stream_edit_leaves_the_rest_alone() {
    use sunrise_core_bindings::dto::{StreamDraftIn, StreamEdit};
    use sunrise_domain::StreamColor;

    let (_dir, core) = open_core().await;
    let created = core
        .submit(CoreCommand::CreateStream {
            draft: StreamDraftIn {
                name: "Travel".into(),
                description: None,
                color: Some(StreamColor::Emerald),
                parent_id: None,
                review_cadence: Some(sunrise_domain::StreamReviewCadence::Weekly),
                reminder_lead_s: Some(900),
            },
        })
        .await
        .expect("create stream");

    core.submit(CoreCommand::UpdateStream {
        id: created.entity,
        edit: StreamEdit {
            archived: Some(true),
            ..StreamEdit::default()
        },
    })
    .await
    .expect("archive");

    let CoreQueryResult::Stream { stream } = core
        .query(CoreQuery::EntityById { id: created.entity })
        .await
        .expect("read back")
    else {
        panic!("a stream id must return a stream");
    };
    assert!(stream.archived);
    assert_eq!(stream.name, "Travel");
    assert_eq!(stream.color, StreamColor::Emerald);
    assert_eq!(
        stream.reminder_lead_s,
        Some(900),
        "a clearable field nobody named stays set"
    );
    core.shutdown().await;
}

/// A client seeding a date picker gets the domain's resolution of a
/// `SunriseTime`, not its own reading of the enum.
#[test]
fn a_time_value_resolves_through_the_domain() {
    use sunrise_core_bindings::dto::TimeValue;
    use sunrise_core_bindings::vocab::time_value_ms;

    let instant = TimeValue::Instant {
        at: jiff::Timestamp::from_millisecond(1_700_000_000_000).expect("ts"),
    };
    assert_eq!(time_value_ms(instant, "UTC".into()), 1_700_000_000_000);

    // An all-day value is midnight *in the reader's zone*, which is the whole
    // reason a client must not do this arithmetic itself.
    let all_day = TimeValue::AllDay {
        date: "2026-03-10".parse().expect("date"),
    };
    let utc = time_value_ms(all_day.clone(), "UTC".into());
    let ny = time_value_ms(all_day, "America/New_York".into());
    // 10 March 2026 is after the US DST change, so New York is UTC-4 — which
    // is precisely the arithmetic a client must not attempt on its own.
    assert_eq!(ny - utc, 4 * 3_600_000);
}

// ---------------------------------------------------------------------------
// Attachments
// ---------------------------------------------------------------------------

/// The whole attachment path, from the app's side of the boundary.
///
/// This is the reachability claim: `Command::AttachFile` and
/// `Query::TaskAttachments` have existed since A.3 with nothing able to
/// produce a sealed chunk, so metadata could be written for bytes no build
/// could store or read back. A green test on either half alone would not have
/// caught that.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_attaches_and_reads_back_across_the_seam() {
    let (_dir, core) = open_core().await;
    let task = core
        .submit(CoreCommand::CreateTask {
            draft: draft("File the tax return"),
        })
        .await
        .expect("create")
        .entity;

    let bytes = b"%PDF-1.7 return".to_vec();
    let att = core
        .attach_file(
            task,
            "return.pdf".into(),
            "application/pdf".into(),
            bytes.clone(),
        )
        .await
        .expect("attach");

    assert_eq!(att.filename, "return.pdf");
    assert_eq!(att.size_bytes, bytes.len() as u64);
    assert_eq!(att.blob_key.len(), 32);
    assert_eq!(att.blob_id.len(), 32, "16 bytes as lowercase hex");
    assert!(
        core.attachment_is_local(att.clone()).expect("locality"),
        "the device that sealed the bytes has them"
    );
    assert_eq!(
        core.attachment_bytes(att.id).await.expect("read back"),
        bytes
    );

    let CoreQueryResult::Attachments { attachments } = core
        .query(CoreQuery::TaskAttachments { task })
        .await
        .expect("list")
    else {
        panic!("wrong result variant");
    };
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].id, att.id);
}

/// An empty file is refused as a typed error rather than recorded as an
/// attachment with nothing behind it.
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_attachment_is_a_typed_error() {
    let (_dir, core) = open_core().await;
    let task = core
        .submit(CoreCommand::CreateTask {
            draft: draft("Nothing to attach"),
        })
        .await
        .expect("create")
        .entity;

    assert!(matches!(
        core.attach_file(task, "empty.txt".into(), "text/plain".into(), Vec::new())
            .await,
        Err(BindingError::Attachment(_))
    ));
}

/// The one attachment failure a client renders rather than reports. Its own
/// variant so "not downloaded" and "something went wrong" stay distinguishable
/// on the Swift side.
#[tokio::test(flavor = "multi_thread")]
async fn an_attachment_whose_bytes_are_elsewhere_has_its_own_error() {
    let (dir, core) = open_core().await;
    let task = core
        .submit(CoreCommand::CreateTask {
            draft: draft("Attached elsewhere"),
        })
        .await
        .expect("create")
        .entity;
    let att = core
        .attach_file(task, "n.txt".into(), "text/plain".into(), b"hello".to_vec())
        .await
        .expect("attach");

    // What a replica that synced the metadata and not the chunks looks like.
    std::fs::remove_dir_all(dir.path().join("blobs")).expect("drop the chunks");

    assert!(!core.attachment_is_local(att.clone()).expect("locality"));
    assert!(matches!(
        core.attachment_bytes(att.id).await,
        Err(BindingError::AttachmentNotHere { .. })
    ));
}

/// A row whose fixed-width fields were tampered with is refused at the
/// boundary, not passed into the vault.
#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_attachment_row_is_refused_at_the_boundary() {
    let (_dir, core) = open_core().await;
    let task = core
        .submit(CoreCommand::CreateTask {
            draft: draft("Malformed"),
        })
        .await
        .expect("create")
        .entity;
    let mut att = core
        .attach_file(task, "n.txt".into(), "text/plain".into(), b"hello".to_vec())
        .await
        .expect("attach");

    att.blob_key.truncate(16);
    assert!(matches!(
        core.attachment_is_local(att),
        Err(BindingError::BadFixedBytes { .. })
    ));
}

// ---------------------------------------------------------------------------
// Calendar conflicts
// ---------------------------------------------------------------------------

/// The Resolve menu, end to end across the seam: two overlapping blocks are
/// reported as one shaded region, and merging them tombstones both and writes
/// their union.
///
/// The whole point of running this against a real vault is that the merge is
/// three commands the client submits — so a draft the seam produced but the
/// core would reject would fail *here*, not in the app.
#[tokio::test(flavor = "multi_thread")]
async fn overlapping_blocks_are_reported_and_merge_into_their_union() {
    use sunrise_core_bindings::dto::BlockDraftIn;
    use sunrise_core_bindings::vocab::{block_conflicts, merged_block_draft, time_value_ms};

    let (_dir, core) = open_core().await;
    let inbox = sunrise_core_bindings::vocab::inbox_stream_id();
    let tz = "UTC";

    let nine = TimeValue::Floating {
        civil: "2026-03-04T09:00:00".parse().expect("civil"),
    };
    let ten = TimeValue::Floating {
        civil: "2026-03-04T10:00:00".parse().expect("civil"),
    };
    let eleven = TimeValue::Floating {
        civil: "2026-03-04T11:00:00".parse().expect("civil"),
    };
    let thirteen = TimeValue::Floating {
        civil: "2026-03-04T13:00:00".parse().expect("civil"),
    };

    for (starts_at, ends_at, title) in [
        (nine.clone(), eleven.clone(), "Deep work"),
        (ten.clone(), thirteen.clone(), "Design review"),
    ] {
        core.submit(CoreCommand::CreateBlock {
            draft: BlockDraftIn {
                stream_id: inbox,
                starts_at,
                ends_at,
                title: Some(title.into()),
                title_track_task: false,
                tasks: Vec::new(),
            },
        })
        .await
        .expect("create block");
    }

    let day_ms = u64::try_from(time_value_ms(nine.clone(), tz.into())).expect("day");
    let CoreQueryResult::Blocks { blocks } = core
        .query(CoreQuery::DayBlocks { day_ms })
        .await
        .expect("day grid")
    else {
        panic!("wrong result variant");
    };
    assert_eq!(blocks.len(), 2);

    let conflicts = block_conflicts(blocks.clone());
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].from_ms, time_value_ms(ten, tz.into()));
    assert_eq!(conflicts[0].to_ms, time_value_ms(eleven, tz.into()));

    let draft = merged_block_draft(blocks[0].clone(), blocks[1].clone(), tz.into()).expect("merge");
    assert_eq!(draft.title.as_deref(), Some("Deep work + Design review"));

    // The three commands the Resolve menu's Merge submits, in the order it
    // submits them.
    for id in [blocks[0].block.id, blocks[1].block.id] {
        core.submit(CoreCommand::DeleteBlock { id })
            .await
            .expect("tombstone the original");
    }
    core.submit(CoreCommand::CreateBlock { draft })
        .await
        .expect("the merged block is one the core accepts");

    let CoreQueryResult::Blocks { blocks } = core
        .query(CoreQuery::DayBlocks { day_ms })
        .await
        .expect("day grid")
    else {
        panic!("wrong result variant");
    };
    assert_eq!(blocks.len(), 1, "the two originals are tombstoned");
    assert_eq!(
        blocks[0].title.as_deref(),
        Some("Deep work + Design review")
    );
    assert_eq!(
        time_value_ms(blocks[0].block.starts_at.clone(), tz.into()),
        time_value_ms(nine, tz.into())
    );
    assert_eq!(
        time_value_ms(blocks[0].block.ends_at.clone(), tz.into()),
        time_value_ms(thirteen, tz.into())
    );
    assert!(block_conflicts(blocks).is_empty());
}
