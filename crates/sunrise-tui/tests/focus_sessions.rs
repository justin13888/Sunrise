//! Integration tests for the TUI's Focus-mode wiring against a **real**
//! `Core` (`docs/08-features/focus-mode.md`).
//!
//! The reducer tests in `runtime.rs` prove which `Command` each keypress
//! produces; these prove the other half — that those commands and queries do
//! what the Focus view claims they do once a core is behind them:
//!
//! * capture-aside really lands in the Inbox and not in the focused task's
//!   stream (the one thing a locally-built draft could get wrong);
//! * a session's elapsed time is derived from a caller-supplied clock rather
//!   than stored, so the same record answers differently per reading;
//! * an interruption leaves the session running;
//! * the break the TUI asks for is **sized by the core**, which is why the
//!   25/5/15-after-4 arithmetic is nowhere in this crate;
//! * the planner drops blocked work and ranks by leverage.

use std::sync::Arc;

use sunrise_core::commands::FocusStartDraft;
use sunrise_core::queries::{FocusPlanRow, FocusSessionRow};
use sunrise_core::{Command, Core, Query, QueryResult};
use sunrise_domain::{
    FocusKind, FocusStats, InterruptionReason, SessionLength, StreamDraft, Task, TaskDraft,
    TaskPatch, LONG_BREAK_MS, POMODORO_MS, SHORT_BREAK_MS,
};
use sunrise_id::EntityRef;
use sunrise_tui::livesync::{open_with_plan, SyncPlan};

/// Vault root for these tests; any fixed value works because nothing here
/// syncs.
const ROOT: [u8; 32] = [11u8; 32];

/// Open a throwaway vault. The `TempDir` is returned so the caller keeps it
/// alive for the length of the test.
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

async fn submit(core: &Core, cmd: Command) -> EntityRef {
    core.submit(cmd).await.expect("command accepted").entity
}

async fn task(core: &Core, title: &str, estimate_s: Option<u64>) -> EntityRef {
    submit(
        core,
        Command::CreateTask(TaskDraft {
            title: title.to_string(),
            estimated_duration_s: estimate_s,
            ..Default::default()
        }),
    )
    .await
}

async fn tasks_of(core: &Core, q: Query) -> Vec<Task> {
    match core.query(q).await.expect("query") {
        QueryResult::Tasks(v) | QueryResult::StreamTasks(v) => v,
        other => panic!("expected tasks, got {other:?}"),
    }
}

async fn running(core: &Core) -> Vec<FocusSessionRow> {
    match core
        .query(Query::RunningFocusSessions)
        .await
        .expect("running sessions")
    {
        QueryResult::FocusSessions(v) => v,
        other => panic!("expected FocusSessions, got {other:?}"),
    }
}

async fn plan(core: &Core) -> Vec<FocusPlanRow> {
    match core
        .query(Query::FocusPlan {
            stream: None,
            energy: None,
            length: SessionLength::SizedToEstimate,
            limit: 10,
        })
        .await
        .expect("focus plan")
    {
        QueryResult::FocusPlan(v) => v,
        other => panic!("expected FocusPlan, got {other:?}"),
    }
}

async fn stats(core: &Core) -> FocusStats {
    match core
        .query(Query::FocusStats {
            stream: None,
            since_ms: None,
            now_ms: core.now_ms(),
        })
        .await
        .expect("focus stats")
    {
        QueryResult::FocusStats(s) => *s,
        other => panic!("expected FocusStats, got {other:?}"),
    }
}

#[tokio::test]
async fn capture_aside_lands_in_the_inbox_not_the_focused_tasks_stream() {
    let (_dir, core) = open_core().await;
    let tz = jiff::tz::TimeZone::UTC;
    let work = submit(
        &core,
        Command::CreateStream(StreamDraft {
            name: "Work".into(),
            ..Default::default()
        }),
    )
    .await;

    // The ordinary capture parser resolves `#Work` and files it there — which
    // is exactly the filing decision an aside must not drag the user into.
    let normal = core
        .capture("call the vet #Work", &tz)
        .await
        .expect("capture parses");
    assert_eq!(normal.draft.stream_id, Some(work));

    // `Core::capture_aside` runs the same parser and then drops the stream.
    // This is the call the TUI's `Outcome::CaptureAside` arm makes.
    let aside = core
        .capture_aside("call the vet #Work", &tz)
        .await
        .expect("aside parses");
    assert_eq!(
        aside.draft.stream_id, None,
        "an aside never inherits a stream"
    );
    // Everything else the user typed survives; only the destination is forced.
    assert_eq!(aside.draft.title, normal.draft.title);
    submit(&core, Command::CreateTask(aside.draft)).await;

    let inbox = tasks_of(&core, Query::Inbox).await;
    let landed = inbox
        .iter()
        .find(|t| t.title.contains("call the vet"))
        .expect("the aside is in the Inbox");
    assert_eq!(landed.stream_id, sunrise_domain::inbox_stream_ref());
    let in_work = tasks_of(&core, Query::StreamTasks(work)).await;
    assert!(
        !in_work.iter().any(|t| t.title.contains("call the vet")),
        "the aside did not land in the focused task's stream"
    );
}

#[tokio::test]
async fn a_running_session_derives_its_elapsed_time_and_stores_none_of_it() {
    let (_dir, core) = open_core().await;
    // 90 minutes of estimate against a 25-minute sitting: four chunks.
    let id = task(&core, "Ship the release", Some(90 * 60)).await;
    let session = submit(
        &core,
        Command::StartFocus(FocusStartDraft {
            task_id: id,
            kind: FocusKind::Work,
            length: SessionLength::SizedToEstimate,
            energy: None,
        }),
    )
    .await;

    let rows = running(&core).await;
    assert_eq!(rows.len(), 1, "a start with no end reads as still running");
    let view = &rows[0].session;
    assert!(rows[0].running && view.is_running());
    assert_eq!(view.start.id, session);
    assert_eq!(view.start.planned_ms, Some(POMODORO_MS));
    assert_eq!(
        view.start.chunk.map(|c| (c.index, c.total)),
        Some((1, 4)),
        "the chunk marker is derived by the core, not by the client"
    );

    // The same immutable record, read against three clocks, gives three
    // answers — which is the whole reason the TUI keeps no timer of its own.
    let t0 = view.start.started_at_ms;
    assert_eq!(view.elapsed_ms(t0), 0);
    assert_eq!(view.elapsed_ms(t0 + 300_000), 300_000);
    assert_eq!(view.remaining_ms(t0 + 300_000), Some(POMODORO_MS - 300_000));
    assert!(view.overran(t0 + POMODORO_MS + 1));

    // An interruption is a note in the distraction journal, not an ending.
    submit(
        &core,
        Command::LogInterruption {
            session,
            reason: InterruptionReason::Meeting,
        },
    )
    .await;
    let still = running(&core).await;
    assert_eq!(
        still.len(),
        1,
        "logging an interruption never ends anything"
    );
    assert_eq!(still[0].session.interruptions.len(), 1);

    // Ending with `actual_focused_ms: None` — what the TUI sends — lets the
    // core freeze the span it derives from its own clock.
    submit(
        &core,
        Command::EndFocus {
            session,
            actual_focused_ms: None,
            completed_task: false,
        },
    )
    .await;
    assert!(running(&core).await.is_empty());

    let s = stats(&core).await;
    assert_eq!(s.work_sessions, 1);
    assert_eq!(s.running, 0);
    assert_eq!(s.interruptions, 1);
    assert_eq!(
        s.top_interruptions.first().map(|t| t.reason),
        Some(InterruptionReason::Meeting)
    );
}

#[tokio::test]
async fn the_break_the_client_asks_for_is_sized_by_the_core() {
    let (_dir, core) = open_core().await;
    let id = task(&core, "Ship the release", Some(90 * 60)).await;

    // Run three work segments, taking the break the client asks for after
    // each. The client never names a duration: it submits
    // `StartFocus { kind: Break }` and the core applies `break_after`.
    let mut breaks = Vec::new();
    for _ in 0..4 {
        let work = submit(
            &core,
            Command::StartFocus(FocusStartDraft {
                task_id: id,
                kind: FocusKind::Work,
                length: SessionLength::SizedToEstimate,
                energy: None,
            }),
        )
        .await;
        submit(
            &core,
            Command::EndFocus {
                session: work,
                actual_focused_ms: None,
                completed_task: false,
            },
        )
        .await;
        let brk = submit(
            &core,
            Command::StartFocus(FocusStartDraft {
                task_id: id,
                kind: FocusKind::Break,
                length: SessionLength::SizedToEstimate,
                energy: None,
            }),
        )
        .await;
        let planned = running(&core)
            .await
            .into_iter()
            .find(|r| r.session.start.id == brk)
            .expect("the break is running")
            .session
            .start
            .planned_ms;
        breaks.push(planned);
        submit(
            &core,
            Command::EndFocus {
                session: brk,
                actual_focused_ms: None,
                completed_task: false,
            },
        )
        .await;
    }
    // 5/5/5/15: the long break lands on the fourth cycle, and the client did
    // no arithmetic to get there.
    assert_eq!(
        breaks,
        vec![
            Some(SHORT_BREAK_MS),
            Some(SHORT_BREAK_MS),
            Some(SHORT_BREAK_MS),
            Some(LONG_BREAK_MS)
        ]
    );
    // Breaks are recorded but are not focused work.
    let s = stats(&core).await;
    assert_eq!(s.work_sessions, 4);
    assert_eq!(s.sessions, 8);
}

#[tokio::test]
async fn the_planner_drops_blocked_work_and_ranks_by_leverage() {
    let (_dir, core) = open_core().await;
    let build = task(&core, "Build", Some(20 * 60)).await;
    let lonely = task(&core, "Lonely chore", Some(10 * 60)).await;
    let deploy = task(&core, "Deploy", None).await;
    let qa = task(&core, "QA", None).await;
    // `deploy` and `qa` both wait on `build`, so finishing `build` releases
    // two tasks; `lonely` releases nothing.
    for dependent in [deploy, qa] {
        submit(
            &core,
            Command::UpdateTask {
                id: dependent,
                patch: TaskPatch {
                    blocked_by: Some(vec![build]),
                    ..Default::default()
                },
            },
        )
        .await;
    }

    let rows = plan(&core).await;
    let titles: Vec<&str> = rows.iter().map(|r| r.task.title.as_str()).collect();
    assert!(
        !titles.contains(&"Deploy") && !titles.contains(&"QA"),
        "blocked work never appears in the planner: {titles:?}"
    );
    assert_eq!(titles.first(), Some(&"Build"), "leverage ranks first");
    assert_eq!(rows[0].unblocks, 2, "the reason it ranks first");
    assert_eq!(rows[0].task.id, build);
    assert_eq!(rows[0].suggested.planned_ms, Some(20 * 60 * 1000));

    // Completing it releases both dependents — the cascade the Focus view
    // reports, informational and score-free.
    submit(&core, Command::CompleteTask(build)).await;
    let cascade = match core
        .query(Query::UnblockCascade(build))
        .await
        .expect("cascade")
    {
        QueryResult::UnblockCascade(c) => *c,
        other => panic!("expected UnblockCascade, got {other:?}"),
    };
    assert_eq!(cascade.completed, build);
    assert_eq!(cascade.released.len(), 2);
    assert!(cascade.still_blocked.is_empty());
    assert!(!cascade.is_empty());
    // And `lonely` is still there for the next pick.
    let after = plan(&core).await;
    assert!(after.iter().any(|r| r.task.id == lonely));
}
