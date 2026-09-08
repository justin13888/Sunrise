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
use std::sync::{Arc, Condvar, Mutex};

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
        None,
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
        None,
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

/// A latch the test closes over the pump and opens once, by hand.
///
/// `wait` parks the caller until `open` has been called and is a no-op
/// afterwards, so a listener built on it stalls exactly once — on the first
/// event — rather than on every one.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    opened: Condvar,
}

impl Gate {
    /// Park until [`Gate::open`] has run. Returns immediately once it has.
    ///
    /// A poisoned lock is recovered rather than propagated: the flag is a
    /// `bool` that cannot be left half-written, and panicking here would panic
    /// the pump task instead of the test, where the failure is legible.
    fn wait(&self) {
        let mut open = self
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !*open {
            open = self
                .opened
                .wait(open)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Release everyone parked in [`Gate::wait`], and everyone who arrives later.
    fn open(&self) {
        *self
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.opened.notify_all();
    }
}

/// Opens a [`Gate`] when it leaves scope, by *any* path including an unwind.
///
/// Not tidiness — liveness. A parked pump sits in synchronous code on a tokio
/// worker, and dropping a multi-threaded runtime joins its workers; a worker
/// blocked on a condvar never observes shutdown, so the drop never returns. If
/// a `submit` in the burst below panicked with the gate still shut, the test
/// future would unwind, the runtime would be dropped, and the process would
/// hang forever instead of failing — in CI, until the job's own ceiling. The
/// guard makes the release unconditional, so a panic reports itself.
struct GateGuard(Arc<Gate>);

impl Drop for GateGuard {
    fn drop(&mut self) {
        self.0.open();
    }
}

/// Holds the pump on its first event until the test opens the gate, then
/// forwards everything to `rec` as fast as it arrives.
struct Gated {
    rec: Arc<Recorder>,
    gate: Arc<Gate>,
}

impl ChangeListener for Gated {
    fn on_change(&self, event: ChangeEvent) {
        self.gate.wait();
        self.rec.on_change(event);
    }
    fn on_lagged(&self, skipped: u64) {
        self.rec.on_lagged(skipped);
    }
    fn on_closed(&self) {
        self.rec.on_closed();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_listener_is_told_it_fell_behind() {
    // The broadcast channel behind the stream holds 256 events and is lossy
    // past that. A listener that only implements `on_change` therefore shows
    // stale data after a sync burst, silently — which is exactly the catch-up
    // case. This pins that `on_lagged` is reachable, so a bridge that ignores
    // it is ignoring something real.
    //
    // The overrun is *constructed*, not raced for. `Gated` parks the pump
    // inside its first `on_change` and holds it there until the whole burst has
    // been published, so 400 events are sent into a 256-slot channel with the
    // only reader stopped: whatever the pump does next, it has already missed
    // at least 144 and its next `recv` can only be `Lagged`. If it never got a
    // first event to park on, it missed them the same way. There is no
    // interleaving in which this passes on one machine and fails on another.
    //
    // What it replaced did race: a listener that slept 2 ms per event, hoping
    // the pump could not keep up with 400 sends. On a fast machine it kept up
    // ("seen 400, lagged 0"), and that is what left `master` red.
    //
    // Only the pump parks — never the test's own thread, which keeps submitting
    // while the pump is stopped (`Core::submit` publishes to a broadcast
    // channel, which never blocks on a slow receiver, it drops for it). Two
    // worker threads are pinned rather than left to `num_cpus` so that the
    // parked pump cannot be the runtime's only worker on a single-core runner.
    //
    // The rule, for any test written after this one: a test that parks a
    // runtime worker pins `worker_threads` to at least 2, and must release the
    // park on every exit path, including unwind. Releasing it only on the happy
    // path turns an ordinary assertion failure into a hung job, because
    // dropping a multi-threaded runtime joins workers that are blocked in
    // synchronous code and can never notice they were asked to stop. Here that
    // is `GateGuard`, whose `Drop` opens the gate whatever happens.
    let (_dir, core) = open_core().await;

    let rec = Arc::new(Recorder::default());
    let gate = Arc::new(Gate::default());
    // Armed before the pump can possibly park, and covering everything below.
    let release = GateGuard(gate.clone());
    let sub = core.subscribe_changes(Arc::new(Gated {
        rec: rec.clone(),
        gate,
    }));

    for i in 0..400 {
        core.submit(CoreCommand::CreateTask {
            draft: draft(&format!("burst {i}")),
        })
        .await
        .expect("create");
    }
    // Everything is published; let the pump discover what it missed. Explicit,
    // so the happy path opens the gate here rather than wherever the guard
    // would otherwise fall out of scope.
    drop(release);

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

/// Creating is undoable, and the inverse is the delete of what the create
/// minted — read off the write, because a moment earlier there was no id to
/// name.
///
/// The **second** undo is the load-bearing half. A redone create is a
/// different entity, so a step that kept naming the first id would re-delete
/// an already-tombstoned row and leave the redone one live for ever.
#[tokio::test(flavor = "multi_thread")]
async fn undoing_a_create_deletes_it_and_redo_makes_it_again() {
    let (_dir, core) = open_core().await;
    let out = core
        .submit_undoable(
            CoreCommand::CreateTask {
                draft: draft("Renew passport"),
            },
            "new task \u{201c}Renew passport\u{201d}".into(),
        )
        .await
        .expect("create");

    assert!(out.not_undoable.is_none(), "a create is reversible");
    assert_eq!(
        core.undo_state().undo_label.as_deref(),
        Some("new task \u{201c}Renew passport\u{201d}"),
        "the menu item is offered rather than greyed out"
    );
    let first = out.outcome.entity;
    assert_eq!(inbox_len(&core).await, 1);

    core.undo().await.expect("undo");
    assert_eq!(inbox_len(&core).await, 0, "the delete reached storage");

    core.redo().await.expect("redo");
    assert_eq!(inbox_len(&core).await, 1, "and the redo put one back");

    // Honest about what a redo is: a fresh write, and therefore a fresh
    // entity. There is no id-preserving create in the core, and the step is
    // re-bound to what the replay actually minted rather than pretending.
    let second = only_inbox_task(&core).await;
    assert_ne!(second, first, "a replayed create mints a new entity");

    core.undo().await.expect("undo again");
    assert_eq!(
        inbox_len(&core).await,
        0,
        "the second undo deletes what the redo made, not the id from the first round"
    );
    core.shutdown().await;
}

/// The id of the one task in the Inbox.
async fn only_inbox_task(core: &SunriseCore) -> sunrise_id::EntityRef {
    match core.query(CoreQuery::Inbox).await.expect("inbox") {
        CoreQueryResult::Tasks { tasks } => {
            assert_eq!(tasks.len(), 1, "expected exactly one task");
            tasks[0].id
        }
        other => panic!("expected tasks, got {other:?}"),
    }
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
                icon: Some("airplane".into()),
                default_context: None,
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

// ---------------------------------------------------------------------------
// Pairing
// ---------------------------------------------------------------------------

/// Both halves of a pairing, driven the way the app drives them, ending in a
/// second vault opened on the transferred root.
///
/// This is the reachability claim for `sunrise-pairing`: it was correct and
/// unreachable from any shipping binary, which is why sync between devices
/// only ever worked in tests that handed both replicas the same literal key.
#[tokio::test(flavor = "multi_thread")]
async fn a_pairing_carries_the_vault_root_to_a_second_device() {
    use sunrise_core_bindings::{DevicePairing, PairingStep};

    let (dir, existing) = open_core().await;
    existing
        .submit(CoreCommand::CreateTask {
            draft: draft("Only on the first device"),
        })
        .await
        .expect("create");

    // The new device publishes a QR; the existing one reads it.
    let new_device = Arc::new(
        DevicePairing::offer("wss://relay.example/pair".into(), "Ada@Example.COM ".into())
            .expect("offer"),
    );
    let qr = new_device.qr_payload().expect("the new device has a QR");
    let old_device = Arc::new(DevicePairing::accept(qr).expect("accept"));
    assert!(
        old_device.qr_payload().is_none(),
        "only the new device publishes one"
    );

    // Noise XX: three messages, alternating, starting with the new device.
    old_device
        .receive_message(new_device.next_message().expect("msg1"))
        .expect("read msg1");
    new_device
        .receive_message(old_device.next_message().expect("msg2"))
        .expect("read msg2");
    old_device
        .receive_message(new_device.next_message().expect("msg3"))
        .expect("read msg3");

    assert_eq!(new_device.step(), PairingStep::AwaitingConfirmation);
    assert_eq!(old_device.step(), PairingStep::AwaitingConfirmation);

    let sas = new_device.sas().expect("sas");
    assert_eq!(sas.len(), 6);
    assert_eq!(sas, old_device.sas().expect("sas"), "both sides agree");

    new_device.confirm(true).expect("confirm");
    old_device.confirm(true).expect("confirm");
    assert_eq!(new_device.step(), PairingStep::Confirmed);

    // The existing device seals *its own* payload — the identity keys, every
    // Stream key and the root — and none of it crosses the seam in the clear.
    let sealed = existing
        .send_pairing_payload(old_device.clone())
        .expect("seal the payload");
    let bundle = new_device.open_pairing_payload(sealed).expect("open");
    assert_eq!(bundle.vault_root.len(), 32);
    assert!(
        !bundle.payload_bytes.is_empty(),
        "the bundle is what `open`'s `paired_bundle` takes"
    );
    assert_eq!(new_device.step(), PairingStep::Finished);

    // The proof that the payload is the right one: the same vault opens under
    // it, with the task only the first device ever saw.
    let path = dir.path().to_string_lossy().into_owned();
    existing.shutdown().await;
    drop(existing);
    let reopened = SunriseCore::open(
        path,
        bundle.vault_root,
        "test".into(),
        Some(bundle.payload_bytes),
    )
    .await
    .expect("the transferred payload opens the vault");
    assert_eq!(inbox_len(&reopened).await, 1);
}

/// A SAS mismatch is the one outcome a UI must not fall through. It ends the
/// pairing rather than returning a value the caller might ignore.
#[tokio::test(flavor = "multi_thread")]
async fn rejecting_the_sas_ends_the_pairing() {
    use sunrise_core_bindings::{DevicePairing, PairingStep};

    let new_device = DevicePairing::offer("wss://relay.example".into(), "ada@example.com".into())
        .expect("offer");
    let old_device = DevicePairing::accept(new_device.qr_payload().expect("qr")).expect("accept");

    old_device
        .receive_message(new_device.next_message().expect("msg1"))
        .expect("read");
    new_device
        .receive_message(old_device.next_message().expect("msg2"))
        .expect("read");
    old_device
        .receive_message(new_device.next_message().expect("msg3"))
        .expect("read");

    assert!(matches!(
        old_device.confirm(false),
        Err(BindingError::Pairing(_))
    ));
    assert_eq!(old_device.step(), PairingStep::Finished);
    assert!(matches!(
        old_device.seal_pairing_payload(vec![1u8; 32]),
        Err(BindingError::Pairing(_))
    ));
}

/// The SAS screen cannot be skipped: there is no path to the channel that does
/// not go through a confirmation, and none to a confirmation before the
/// transcript completes.
#[tokio::test(flavor = "multi_thread")]
async fn the_payload_cannot_move_before_the_sas_is_confirmed() {
    use sunrise_core_bindings::DevicePairing;

    let new_device = DevicePairing::offer("wss://relay.example".into(), "ada@example.com".into())
        .expect("offer");
    let old_device = DevicePairing::accept(new_device.qr_payload().expect("qr")).expect("accept");

    assert!(matches!(new_device.sas(), Err(BindingError::Pairing(_))));
    assert!(matches!(
        old_device.seal_pairing_payload(vec![1u8; 32]),
        Err(BindingError::Pairing(_))
    ));
    assert!(matches!(
        old_device.confirm(true),
        Err(BindingError::Pairing(_))
    ));
}

/// A mistyped or truncated paste is refused by the decoder that wrote it,
/// rather than producing a handshake that fails later for no visible reason.
#[tokio::test(flavor = "multi_thread")]
async fn a_qr_that_is_not_one_is_refused_at_the_door() {
    use sunrise_core_bindings::DevicePairing;

    assert!(matches!(
        DevicePairing::accept("not a qr payload".into()),
        Err(BindingError::Pairing(_))
    ));

    let good = DevicePairing::offer("wss://relay.example".into(), "ada@example.com".into())
        .expect("offer")
        .qr_payload()
        .expect("qr");
    let truncated = good[..good.len() / 2].to_string();
    assert!(matches!(
        DevicePairing::accept(truncated),
        Err(BindingError::Pairing(_))
    ));
}

/// The two sides are not interchangeable: only the device that holds the vault
/// sends the payload, and only the one being added receives it.
#[tokio::test(flavor = "multi_thread")]
async fn each_side_can_only_do_its_own_half() {
    use sunrise_core_bindings::DevicePairing;

    let new_device = DevicePairing::offer("wss://relay.example".into(), "ada@example.com".into())
        .expect("offer");
    let old_device = DevicePairing::accept(new_device.qr_payload().expect("qr")).expect("accept");

    assert!(matches!(
        new_device.seal_pairing_payload(vec![1u8; 32]),
        Err(BindingError::Pairing(_))
    ));
    assert!(matches!(
        old_device.open_pairing_payload("AAAA".into()),
        Err(BindingError::Pairing(_))
    ));
}

/// A device that is not the one the QR came from cannot finish the handshake,
/// even though its transcript is perfectly well-formed.
///
/// Noise XX authenticates that the two ends of a transcript agree; it has no
/// opinion about *which* static key the initiator should have presented, and
/// the responder learns that key only in the third message. So the QR value is
/// the only thing that can tell the honest new device from a relay sitting in
/// the middle, and it authenticates nothing unless the scanner compares it
/// (issue #152). The impostor here gets as far as the last message and no
/// further — in particular, not as far as a SAS screen, where it would be
/// playing for six digits.
#[tokio::test(flavor = "multi_thread")]
async fn a_peer_that_did_not_publish_the_qr_is_refused_before_the_sas() {
    use sunrise_core_bindings::{DevicePairing, PairingStep};

    let honest = DevicePairing::offer("wss://relay.example".into(), "ada@example.com".into())
        .expect("offer");
    let qr = honest.qr_payload().expect("qr");

    // Somebody else drives the transcript against the scanned QR.
    let impostor = DevicePairing::offer("wss://relay.example".into(), "ada@example.com".into())
        .expect("offer");
    let old_device = DevicePairing::accept(qr).expect("accept");

    let m1 = impostor.next_message().expect("-> e");
    old_device.receive_message(m1).expect("read e");
    let m2 = old_device.next_message().expect("<- e ee s es");
    impostor.receive_message(m2).expect("read ee es");
    let m3 = impostor.next_message().expect("-> s se");

    // The message decrypts. It is the identity behind it that is wrong.
    assert!(matches!(
        old_device.receive_message(m3),
        Err(BindingError::Pairing(_))
    ));
    assert_eq!(old_device.step(), PairingStep::Finished);
    assert!(matches!(old_device.sas(), Err(BindingError::Pairing(_))));
    assert!(matches!(
        old_device.confirm(true),
        Err(BindingError::Pairing(_))
    ));
}

/// The account tag in a QR names an account without naming a person: it is
/// four bytes of BLAKE3 over the normalized address.
#[tokio::test(flavor = "multi_thread")]
async fn the_account_tag_is_normalized_and_not_the_address() {
    use sunrise_core_bindings::pairing::pairing_account_tag;

    let tag = pairing_account_tag("ada@example.com".into());
    assert_eq!(tag.len(), 8, "four bytes as lowercase hex");
    assert_eq!(tag, pairing_account_tag("  Ada@Example.COM ".into()));
    assert_ne!(tag, pairing_account_tag("grace@example.com".into()));
}

/// The action buttons on every reminder, and the reason two of them are not
/// arithmetic: New York springs forward on 2026-03-08, so 09:00 to 09:00 is 23
/// hours. A client adding a day of milliseconds would fire an hour late.
#[test]
fn a_snooze_until_tomorrow_keeps_its_wall_clock_across_a_dst_boundary() {
    use sunrise_core_bindings::vocab::snooze_target_ms;
    use sunrise_domain::SnoozeSpan;

    let ny = "America/New_York";
    let saturday_nine = ms_at(2026, 3, 7, 9, ny);
    assert_eq!(
        snooze_target_ms(saturday_nine, SnoozeSpan::Tomorrow, ny.into()),
        i64::try_from(ms_at(2026, 3, 8, 9, ny)).expect("ms"),
    );
    assert_eq!(
        snooze_target_ms(saturday_nine, SnoozeSpan::OneHour, ny.into()),
        i64::try_from(saturday_nine + 60 * 60 * 1000).expect("ms"),
    );
    assert_eq!(
        snooze_target_ms(saturday_nine, SnoozeSpan::NextWeek, ny.into()),
        i64::try_from(ms_at(2026, 3, 14, 9, ny)).expect("ms"),
    );
}

// ---------------------------------------------------------------------------
// iCalendar interchange
// ---------------------------------------------------------------------------

use sunrise_integrations::ical::NoticeCode;
use sunrise_integrations::ical_vault::ExportWindow;

/// An `.ics` with one event starting **now**, so it is inside today and inside
/// this week in every timezone the suite might run in. The seam opens a vault
/// with the production clock, so the fixture is anchored to that same clock
/// rather than to a date that would only work in one week of one year.
fn ics_now(now_ms: u64, uid: &str, summary: &str) -> String {
    let start = jiff::Timestamp::from_millisecond(i64::try_from(now_ms).unwrap())
        .expect("now")
        .round(jiff::Unit::Second)
        .expect("round");
    let end = start + jiff::SignedDuration::from_hours(1);
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//seam//EN\r\n\
BEGIN:VEVENT\r\nUID:{uid}\r\nSUMMARY:{summary}\r\n\
DTSTART:{}\r\nDTEND:{}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        start.strftime("%Y%m%dT%H%M%SZ"),
        end.strftime("%Y%m%dT%H%M%SZ"),
    )
}

/// The property the macOS menu item depends on: a user who imports the same
/// file twice gets one calendar, so the app never has to ask them first.
#[tokio::test(flavor = "multi_thread")]
async fn importing_an_ics_is_idempotent_across_the_seam() {
    let (_dir, core) = open_core().await;
    let text = ics_now(core.now_ms(), "ev1@example.com", "Quarterly planning");

    let first = core
        .import_ical(text.clone(), None, None)
        .await
        .expect("import");
    assert_eq!(first.created, 1, "{first:?}");
    assert_eq!(first.updated, 0);
    assert_eq!(first.failed, 0);
    assert_eq!(first.blocks.len(), 1);
    assert!(first.blocks[0].block.to_str().starts_with("blk_"));
    assert_eq!(first.blocks[0].title, "Quarterly planning");
    assert!(first.blocks[0].created);

    let second = core.import_ical(text, None, None).await.expect("re-import");
    assert_eq!(second.created, 0, "nothing is new the second time");
    assert_eq!(second.updated, 1);
    assert!(!second.blocks[0].created);
    assert_eq!(
        second.blocks[0].block, first.blocks[0].block,
        "the same UID must land on the same block"
    );
}

/// The counts are the seam's, not the client's, so every client says the same
/// thing about the same file — and a distinct `source` really is a distinct
/// calendar.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_source_is_a_second_calendar() {
    let (_dir, core) = open_core().await;
    let text = ics_now(core.now_ms(), "shared", "Standup");
    core.import_ical(text.clone(), None, None)
        .await
        .expect("import");
    let other = core
        .import_ical(text, None, Some("team-calendar".into()))
        .await
        .expect("import");
    assert_eq!(other.created, 1);
    assert_ne!(
        other.blocks[0].block,
        core.import_ical(ics_now(core.now_ms(), "shared", "Standup"), None, None)
            .await
            .expect("import")
            .blocks[0]
            .block
    );
}

/// A blank `source` is not a source. It falls back to the default rather than
/// silently keying every import under the empty string.
#[tokio::test(flavor = "multi_thread")]
async fn a_blank_source_falls_back_to_the_default() {
    let (_dir, core) = open_core().await;
    let text = ics_now(core.now_ms(), "ev1", "Standup");
    let first = core
        .import_ical(text.clone(), None, None)
        .await
        .expect("import");
    let blank = core
        .import_ical(text, None, Some("   ".into()))
        .await
        .expect("import");
    assert_eq!(blank.created, 0);
    assert_eq!(blank.blocks[0].block, first.blocks[0].block);
}

/// The notices have to reach the client, or the app is silently dropping the
/// user's data on their behalf.
#[tokio::test(flavor = "multi_thread")]
async fn what_a_block_cannot_hold_reaches_the_client() {
    let (_dir, core) = open_core().await;
    let text = ics_now(core.now_ms(), "ev1", "Design review").replace(
        "END:VEVENT",
        "DESCRIPTION:Bring the roadmap\r\nRRULE:FREQ=WEEKLY\r\n\
BEGIN:VALARM\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\nEND:VEVENT",
    );
    let report = core.import_ical(text, None, None).await.expect("import");
    assert_eq!(report.created, 1, "the event still lands");

    let details: Vec<&str> = report.notices.iter().map(|n| n.detail.as_str()).collect();
    assert!(
        details.iter().any(|d| d.starts_with("DESCRIPTION")),
        "{details:?}"
    );
    assert!(
        details.iter().any(|d| d.starts_with("RRULE")),
        "{details:?}"
    );
    assert!(
        details.iter().any(|d| d.starts_with("VALARM")),
        "{details:?}"
    );
    assert!(report
        .notices
        .iter()
        .any(|n| n.code == NoticeCode::UnsupportedComponent));
    assert!(report
        .notices
        .iter()
        .any(|n| n.code == NoticeCode::UnmappedProperty));
}

/// Export → import is the identity across the seam too, so an app that offers
/// both cannot double a user's calendar.
#[tokio::test(flavor = "multi_thread")]
async fn exporting_and_re_importing_across_the_seam_is_the_identity() {
    let (_dir, core) = open_core().await;
    let first = core
        .import_ical(ics_now(core.now_ms(), "ev1", "Standup"), None, None)
        .await
        .expect("import");

    let doc = core
        .export_ical(ExportWindow::Week, core.now_ms())
        .await
        .expect("export");
    assert!(doc.starts_with("BEGIN:VCALENDAR\r\n"), "got {doc:?}");
    assert!(doc.contains("SUMMARY:Standup"));

    let back = core.import_ical(doc, None, None).await.expect("re-import");
    assert_eq!(
        back.created, 0,
        "an exported calendar re-imports onto itself"
    );
    assert_eq!(back.blocks[0].block, first.blocks[0].block);

    let again = core
        .export_ical(ExportWindow::Week, core.now_ms())
        .await
        .expect("export");
    assert_eq!(again.matches("BEGIN:VEVENT").count(), 1);
}

/// A day export is a narrower window over the same vault, not a different
/// document format.
#[tokio::test(flavor = "multi_thread")]
async fn a_day_export_is_a_valid_document_even_when_empty() {
    let (_dir, core) = open_core().await;
    let empty = core
        .export_ical(ExportWindow::Day, core.now_ms())
        .await
        .expect("export");
    assert!(empty.starts_with("BEGIN:VCALENDAR\r\n"));
    assert_eq!(empty.matches("BEGIN:VEVENT").count(), 0);

    core.import_ical(ics_now(core.now_ms(), "ev1", "Standup"), None, None)
        .await
        .expect("import");
    let one = core
        .export_ical(ExportWindow::Day, core.now_ms())
        .await
        .expect("export");
    assert_eq!(one.matches("BEGIN:VEVENT").count(), 1);
}

/// A file that is not a calendar is a typed error, not an import of zero
/// events that a user would read as success.
#[tokio::test(flavor = "multi_thread")]
async fn text_that_is_not_a_calendar_is_a_typed_error() {
    let (_dir, core) = open_core().await;
    let err = core
        .import_ical("this is not a calendar\n".into(), None, None)
        .await
        .expect_err("refused");
    assert!(matches!(err, BindingError::Calendar(_)), "got {err:?}");
}

/// A calendar imports into a stream. Handing it a task id has to fail at the
/// seam, where the message can still name what was passed.
#[tokio::test(flavor = "multi_thread")]
async fn importing_into_something_that_is_not_a_stream_is_refused() {
    let (_dir, core) = open_core().await;
    let task = core
        .submit(CoreCommand::CreateTask {
            draft: draft("Renew passport"),
        })
        .await
        .expect("create")
        .entity;
    let err = core
        .import_ical(ics_now(core.now_ms(), "ev1", "Standup"), Some(task), None)
        .await
        .expect_err("refused");
    assert!(matches!(err, BindingError::BadId { .. }), "got {err:?}");
}
