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
    constraint_summary, duration_clock, relative_day, short_duration, today_section,
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
