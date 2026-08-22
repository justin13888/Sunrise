//! Integration tests for the Review view against a **real** `Core`
//! (`docs/08-features/reviews-and-stats.md`).
//!
//! The render tests in `render.rs` prove the panels lay out; the reducer tests
//! prove which command each key produces. These prove the half neither can:
//! that the queries behind the view return what the view claims, once a vault
//! with real history is behind them — and, in particular, that a snapshot
//! saved from the screen carries the counts the screen showed.
//!
//! That last one is the whole point of the artefact. A snapshot that
//! recomputed its own totals could disagree with the review it came from, and
//! the disagreement would only ever be discovered months later, in History.

use std::sync::Arc;

use sunrise_core::{Command, Core, Query, QueryResult};
use sunrise_domain::{TaskDraft, WeeklyReview};
use sunrise_id::EntityRef;
use sunrise_tui::livesync::{open_with_plan, SyncPlan};
use sunrise_tui::{render_review, ReviewPane, View, ViewState};

/// Vault root for these tests; any fixed value works because nothing syncs.
const ROOT: [u8; 32] = [23u8; 32];

async fn open_core() -> (tempfile::TempDir, Arc<Core>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let plan = SyncPlan {
        sync: None,
        export_cert: None,
        trust_cert: None,
    };
    let (core, _log) = open_with_plan(dir.path().join("vault"), "sunrise-tui-test", ROOT, &plan)
        .await
        .expect("open a vault");
    (dir, core)
}

async fn task(core: &Core, title: &str) -> EntityRef {
    core.submit(Command::CreateTask(TaskDraft {
        title: title.to_string(),
        ..Default::default()
    }))
    .await
    .expect("create")
    .entity
}

async fn weekly(core: &Core) -> Box<WeeklyReview> {
    match core
        .query(Query::WeeklyReview {
            week_start_ms: None,
            now_ms: core.now_ms(),
        })
        .await
        .expect("weekly review")
    {
        QueryResult::WeeklyReview(w) => w,
        other => panic!("expected WeeklyReview, got {other:?}"),
    }
}

/// Render the Review view to text, the way the render tests do.
fn frame(state: &ViewState) -> String {
    let backend = ratatui::backend::TestBackend::new(100, 40);
    let mut term = ratatui::Terminal::new(backend).expect("terminal");
    term.draw(|f| render_review(f, f.area(), state))
        .expect("draw");
    let buf = term.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[tokio::test]
async fn the_weekly_review_reports_the_week_that_actually_happened() {
    let (_dir, core) = open_core().await;
    let done = task(&core, "file the tax return").await;
    let slipping = task(&core, "renew the passport").await;
    task(&core, "buy milk").await;
    core.submit(Command::CompleteTask(done))
        .await
        .expect("done");
    core.submit(Command::DeferTask {
        id: slipping,
        to_ms: core.now_ms() + 86_400_000,
    })
    .await
    .expect("defer");

    let w = weekly(&core).await;
    assert_eq!(w.totals.created, 3, "three captures this week");
    assert_eq!(w.totals.completed, 1);
    assert_eq!(w.totals.deferred, 1);

    let mut state = ViewState {
        view: View::Review,
        ..Default::default()
    };
    state.review.weekly = Some(w);
    let s = frame(&state);
    assert!(s.contains("completed 1"), "got:\n{s}");
    assert!(s.contains("created 3"), "got:\n{s}");
    // Step 2 is the untriaged inbox, which is where all three landed.
    assert!(s.contains("2 · Inbox"), "got:\n{s}");
}

#[tokio::test]
async fn a_saved_snapshot_carries_the_counts_the_screen_showed() {
    let (_dir, core) = open_core().await;
    let a = task(&core, "one").await;
    task(&core, "two").await;
    core.submit(Command::CompleteTask(a)).await.expect("done");

    let w = weekly(&core).await;
    let shown = w.totals;
    core.submit(Command::SaveReviewSnapshot(w.to_draft(None)))
        .await
        .expect("snapshot saved");

    let rows = match core
        .query(Query::ReviewHistory { limit: 10 })
        .await
        .expect("history")
    {
        QueryResult::ReviewSnapshots(rows) => rows,
        other => panic!("expected ReviewSnapshots, got {other:?}"),
    };
    assert_eq!(rows.len(), 1, "one review, one snapshot");
    assert_eq!(
        rows[0].totals, shown,
        "a snapshot must not recompute what the review already decided"
    );
    assert_eq!(rows[0].window_start_ms, w.window.start_ms);

    // …and History renders it.
    let mut state = ViewState {
        view: View::Review,
        ..Default::default()
    };
    state.review.pane = ReviewPane::History;
    state.review.history = rows;
    let s = frame(&state);
    assert!(s.contains("completed 1"), "got:\n{s}");
}

#[tokio::test]
async fn the_daily_glance_separates_what_is_blocked_from_what_is_planned() {
    let (_dir, core) = open_core().await;
    let blocker = task(&core, "wait for the locksmith").await;
    let blocked = task(&core, "move the desk").await;
    let now = core.now_ms();
    for (id, blocked_by) in [(blocked, Some(vec![blocker]))] {
        core.submit(Command::UpdateTask {
            id,
            patch: sunrise_domain::TaskPatch {
                scheduled_at: jiff::Timestamp::from_millisecond(
                    i64::try_from(now).expect("in range"),
                )
                .ok()
                .map(Some),
                blocked_by,
                ..Default::default()
            },
        })
        .await
        .expect("patch");
    }

    let daily = match core
        .query(Query::DailyReview {
            since_ms: now.saturating_sub(86_400_000),
            now_ms: now,
        })
        .await
        .expect("daily review")
    {
        QueryResult::DailyReview(d) => d,
        other => panic!("expected DailyReview, got {other:?}"),
    };
    assert!(
        daily.blocked.iter().any(|t| t.id == blocked),
        "the spec's \"any blocker for today?\" is answered, not asked"
    );

    let mut state = ViewState {
        view: View::Review,
        ..Default::default()
    };
    state.review.pane = ReviewPane::Daily;
    state.review.daily = Some(daily);
    let s = frame(&state);
    assert!(s.contains("Blocked (1)"), "got:\n{s}");
    assert!(s.contains("move the desk"), "got:\n{s}");
}

#[tokio::test]
async fn the_trend_fold_reaches_the_screen_as_a_readable_bar() {
    let (_dir, core) = open_core().await;
    for i in 0..3 {
        let id = task(&core, &format!("task {i}")).await;
        core.submit(Command::CompleteTask(id)).await.expect("done");
    }
    let trends = match core
        .query(Query::StreamTrends {
            weeks: 12,
            now_ms: core.now_ms(),
        })
        .await
        .expect("trends")
    {
        QueryResult::Trends(t) => t,
        other => panic!("expected Trends, got {other:?}"),
    };
    assert_eq!(trends.overall.len(), 12, "twelve buckets, per the spec");
    assert_eq!(
        trends.overall.last().expect("this week").completed,
        3,
        "all three completions land in the current week"
    );

    let mut state = ViewState {
        view: View::Review,
        ..Default::default()
    };
    state.review.pane = ReviewPane::Trends;
    state.review.trends = Some(trends);
    let s = frame(&state);
    assert!(s.contains("Whole vault"), "got:\n{s}");
    assert!(s.contains('█'), "the peak week draws a full bar:\n{s}");
}
