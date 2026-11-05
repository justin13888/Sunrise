//! The weekly and daily reviews, their snapshots and the exports.

use super::*;

#[test]
fn the_weekly_review_counts_the_week_it_covers_and_nothing_else() {
    let (e, mut db, clock) = review_fixture();

    // Last week: one task created and completed. It must not be counted.
    set_clock(&clock, REVIEW_MON - 4 * REVIEW_DAY);
    let old = review_task(&e, &mut db, "last week's work", None);
    set_clock(&clock, REVIEW_MON - 3 * REVIEW_DAY);
    e.apply(&mut db, Command::CompleteTask(old)).unwrap();

    // This week.
    set_clock(&clock, REVIEW_MON + REVIEW_DAY);
    let done = review_task(&e, &mut db, "finish the report", None);
    let deferred = review_task(&e, &mut db, "call the bank", None);
    let untouched = review_task(&e, &mut db, "read the RFC", None);
    let dropped = review_task(&e, &mut db, "abandon this", None);

    set_clock(&clock, REVIEW_MON + 2 * REVIEW_DAY);
    e.apply(&mut db, Command::CompleteTask(done)).unwrap();
    e.apply(
        &mut db,
        Command::DeferTask {
            id: deferred,
            to_ms: REVIEW_MON + 6 * REVIEW_DAY,
        },
    )
    .unwrap();
    e.apply(&mut db, Command::DeleteTask(dropped)).unwrap();

    let r = weekly(&e, &db, REVIEW_MON + 4 * REVIEW_DAY);
    assert_eq!(r.window.start_ms, REVIEW_MON);
    assert_eq!(r.window.end_ms, REVIEW_MON + REVIEW_WEEK);
    assert_eq!(r.totals.completed, 1, "last week's completion is excluded");
    assert_eq!(r.totals.deferred, 1);
    assert_eq!(r.totals.dropped, 1);
    assert_eq!(r.totals.created, 4);

    // The Inbox row is the stream everything landed in.
    let inbox = r
        .streams
        .iter()
        .find(|s| s.stream == inbox_stream_ref())
        .expect("the inbox is reviewed too");
    assert_eq!(
        inbox
            .completed
            .iter()
            .map(|t| t.title.as_str())
            .collect::<Vec<_>>(),
        vec!["finish the report"]
    );
    assert_eq!(
        inbox
            .deferred
            .iter()
            .map(|t| t.title.as_str())
            .collect::<Vec<_>>(),
        vec!["call the bank"]
    );
    assert_eq!(
        inbox
            .created_untouched
            .iter()
            .map(|t| t.title.as_str())
            .collect::<Vec<_>>(),
        vec!["read the RFC"],
        "the deleted one was acted on; the others were completed or deferred"
    );
    assert!(
        r.inbox.iter().any(|t| t.id == untouched),
        "step 2 offers the untriaged capture for triage"
    );
}

#[test]
fn a_paused_stream_drops_out_of_the_review_and_the_pause_survives_a_reread() {
    let (e, mut db, clock) = review_fixture();
    let work = e
        .apply(
            &mut db,
            Command::CreateStream(StreamDraft {
                name: "Work".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    set_clock(&clock, REVIEW_MON + REVIEW_DAY);
    let task = review_task(&e, &mut db, "in the paused stream", Some(work));
    set_clock(&clock, REVIEW_MON + 2 * REVIEW_DAY);
    e.apply(&mut db, Command::CompleteTask(task)).unwrap();

    // Before the pause the stream is reviewed.
    let r = weekly(&e, &db, REVIEW_MON + 4 * REVIEW_DAY);
    assert!(r.streams.iter().any(|s| s.stream == work));

    e.apply(
        &mut db,
        Command::UpdateStream {
            id: work,
            patch: StreamPatch {
                paused: Some(true),
                review_cadence: Some(StreamReviewCadence::Monthly),
                ..Default::default()
            },
        },
    )
    .unwrap();

    // The pause is a persisted projection now, not a value that lives only
    // inside the op that set it.
    match e.query(&db, Query::EntityById(work)).unwrap() {
        QueryResult::Stream(s) => {
            assert!(s.paused, "the pause survived the round trip through SQL");
            assert_eq!(s.review_cadence, StreamReviewCadence::Monthly);
        }
        other => panic!("expected Stream, got {other:?}"),
    }

    let r = weekly(&e, &db, REVIEW_MON + 4 * REVIEW_DAY);
    assert!(
        !r.streams.iter().any(|s| s.stream == work),
        "the spec reviews non-archived, NON-PAUSED streams: {:#?}",
        r.streams.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
    assert_eq!(
        r.totals.completed, 1,
        "pausing a stream hides its section, it does not rewrite the week's counts"
    );
}

#[test]
fn the_weekly_review_lists_a_commitment_that_came_due_and_stayed_open() {
    let (e, mut db, clock) = review_fixture();
    set_clock(&clock, REVIEW_MON);
    let slipped = review_task(&e, &mut db, "renew the domain", None);
    let met = review_task(&e, &mut db, "pay the invoice", None);
    for id in [slipped, met] {
        e.apply(
            &mut db,
            Command::UpdateTask {
                id,
                patch: TaskPatch {
                    hard_due_at: Some(Some(ms_to_ts((REVIEW_MON + 2 * REVIEW_DAY) as i64).into())),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    }
    set_clock(&clock, REVIEW_MON + 3 * REVIEW_DAY);
    e.apply(&mut db, Command::CompleteTask(met)).unwrap();

    let r = weekly(&e, &db, REVIEW_MON + 4 * REVIEW_DAY);
    assert_eq!(
        r.slipped
            .iter()
            .map(|t| t.title.as_str())
            .collect::<Vec<_>>(),
        vec!["renew the domain"],
        "a commitment that was met is not a slipped one"
    );
}

#[test]
fn the_weekly_review_surfaces_the_estimate_calibration_focus_produced() {
    let (e, mut db, clock) = review_fixture();
    // A task estimated at 30 minutes that actually took 51 — the spec's
    // "your 30-minute estimates run ~1.7x long".
    set_clock(&clock, REVIEW_MON + REVIEW_DAY);
    let task = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "the 30-minute job".into(),
                estimated_duration_s: Some(30 * 60),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    let session = e
        .apply(
            &mut db,
            Command::StartFocus(FocusStartDraft {
                task_id: task,
                kind: FocusKind::Work,
                length: SessionLength::UntilDone,
                energy: None,
            }),
        )
        .unwrap()
        .entity;
    set_clock(&clock, REVIEW_MON + REVIEW_DAY + 51 * 60 * 1000);
    e.apply(
        &mut db,
        Command::EndFocus {
            session,
            actual_focused_ms: Some(51 * 60 * 1000),
            completed_task: true,
        },
    )
    .unwrap();

    let r = weekly(&e, &db, REVIEW_MON + 4 * REVIEW_DAY);
    let c = r.focus.overall.expect("a calibration factor");
    assert_eq!(c.samples, 1);
    assert!(
        (c.factor - 1.7).abs() < 1e-9,
        "51 minutes against a 30-minute estimate is 1.7x, got {}",
        c.factor
    );
    assert_eq!(r.focus.total_focused_ms, 51 * 60 * 1000);
}

#[test]
fn the_trend_is_stable_across_a_reopen() {
    let (e, mut db, clock) = review_fixture();
    // Completed in the week before last.
    set_clock(&clock, REVIEW_MON - 10 * REVIEW_DAY);
    let task = review_task(&e, &mut db, "the wobbly one", None);
    set_clock(&clock, REVIEW_MON - 9 * REVIEW_DAY);
    e.apply(&mut db, Command::CompleteTask(task)).unwrap();

    let before = trends_of(&e, &db, REVIEW_MON + REVIEW_DAY);
    assert_eq!(
        before.overall.iter().map(|b| b.completed).sum::<i64>(),
        1,
        "one completion in the window"
    );

    // Re-open it this week. The spec: the decrement lands on the week of
    // the ORIGINAL completion, so the total goes to zero and the *earlier*
    // bucket is the one that moved.
    set_clock(&clock, REVIEW_MON + 2 * REVIEW_DAY);
    e.apply(
        &mut db,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                state: Some(TaskState::Todo),
                ..Default::default()
            },
        },
    )
    .unwrap();

    let after = trends_of(&e, &db, REVIEW_MON + 3 * REVIEW_DAY);
    assert_eq!(
        after.overall.iter().map(|b| b.completed).sum::<i64>(),
        0,
        "the chart nets out: {:#?}",
        after.overall
    );
    assert!(
        after.overall.last().is_some_and(|b| b.completed == 0),
        "the current week was never credited a completion it did not have"
    );
}

#[test]
fn a_saved_review_snapshot_is_queryable_in_history() {
    let (e, mut db, clock) = review_fixture();
    set_clock(&clock, REVIEW_MON + REVIEW_DAY);
    let task = review_task(&e, &mut db, "something to count", None);
    set_clock(&clock, REVIEW_MON + 2 * REVIEW_DAY);
    e.apply(&mut db, Command::CompleteTask(task)).unwrap();

    let r = weekly(&e, &db, REVIEW_MON + 4 * REVIEW_DAY);
    set_clock(&clock, REVIEW_MON + 4 * REVIEW_DAY);
    let saved = e
        .apply(
            &mut db,
            Command::SaveReviewSnapshot(r.to_draft(Some("a quiet week".into()))),
        )
        .unwrap();
    assert_eq!(saved.entity.kind(), EntityKind::ReviewSnapshot);

    let history = match e.query(&db, Query::ReviewHistory { limit: 10 }).unwrap() {
        QueryResult::ReviewSnapshots(v) => v,
        other => panic!("expected ReviewSnapshots, got {other:?}"),
    };
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id, saved.entity);
    assert_eq!(history[0].window_start_ms(), REVIEW_MON);
    assert_eq!(history[0].window_end_ms(), REVIEW_MON + REVIEW_WEEK);
    assert_eq!(history[0].totals.completed, 1);
    assert_eq!(history[0].note.as_deref(), Some("a quiet week"));

    // Saving again records a *second* review rather than replacing the
    // first: a snapshot is an event, not a register.
    set_clock(&clock, REVIEW_MON + 5 * REVIEW_DAY);
    e.apply(&mut db, Command::SaveReviewSnapshot(r.to_draft(None)))
        .unwrap();
    let history = match e.query(&db, Query::ReviewHistory { limit: 10 }).unwrap() {
        QueryResult::ReviewSnapshots(v) => v,
        other => panic!("expected ReviewSnapshots, got {other:?}"),
    };
    assert_eq!(history.len(), 2);
}

#[test]
fn export_csv_survives_a_task_title_full_of_delimiters() {
    let (e, mut db, clock) = review_fixture();
    set_clock(&clock, REVIEW_MON + REVIEW_DAY);
    // A title with a comma, a quote pair and a newline: the three things
    // that shift a column in a naive writer.
    let hostile = "Ship \"v1\", then\nrest";
    let task = review_task(&e, &mut db, hostile, None);
    set_clock(&clock, REVIEW_MON + 2 * REVIEW_DAY);
    e.apply(&mut db, Command::CompleteTask(task)).unwrap();

    let csv = match e
        .query(
            &db,
            Query::ExportStats {
                dataset: ExportDataset::Activity,
                format: ExportFormat::Csv,
                weeks: 2,
                now_ms: REVIEW_MON + 3 * REVIEW_DAY,
            },
        )
        .unwrap()
    {
        QueryResult::Export(s) => s,
        other => panic!("expected Export, got {other:?}"),
    };
    // The label column holds the title verbatim, quoted, and every record
    // still has the header's six fields.
    assert!(
        csv.contains("\"Ship \"\"v1\"\", then\nrest\""),
        "title was not quoted correctly:\n{csv}"
    );
    let header_fields = csv.lines().next().unwrap().split(',').count();
    assert_eq!(header_fields, 6);

    let json = match e
        .query(
            &db,
            Query::ExportStats {
                dataset: ExportDataset::Activity,
                format: ExportFormat::Json,
                weeks: 2,
                now_ms: REVIEW_MON + 3 * REVIEW_DAY,
            },
        )
        .unwrap()
    {
        QueryResult::Export(s) => s,
        other => panic!("expected Export, got {other:?}"),
    };
    assert!(
        json.contains(r#"Ship \"v1\", then\nrest"#),
        "JSON escaping lost the title:\n{json}"
    );
}

#[test]
fn export_trends_and_streaks_render_in_both_formats() {
    let (e, mut db, clock) = review_fixture();
    set_clock(&clock, REVIEW_MON + REVIEW_DAY);
    review_task(&e, &mut db, "a task", None);
    for (dataset, format) in [
        (ExportDataset::Trends, ExportFormat::Csv),
        (ExportDataset::Trends, ExportFormat::Json),
        (ExportDataset::Focus, ExportFormat::Csv),
        (ExportDataset::Streaks, ExportFormat::Json),
    ] {
        let out = match e
            .query(
                &db,
                Query::ExportStats {
                    dataset,
                    format,
                    weeks: 3,
                    now_ms: REVIEW_MON + 3 * REVIEW_DAY,
                },
            )
            .unwrap()
        {
            QueryResult::Export(s) => s,
            other => panic!("expected Export, got {other:?}"),
        };
        match format {
            ExportFormat::Csv => assert!(
                out.ends_with("\r\n") && !out.contains(",,,,,,"),
                "{dataset:?} CSV is malformed:\n{out}"
            ),
            ExportFormat::Json => assert!(
                out.starts_with(&format!("{{\"table\":\"{}\"", dataset.as_str())),
                "{dataset:?} JSON is not the named table:\n{out}"
            ),
        }
    }
}

#[test]
fn the_daily_review_shows_last_nights_captures_and_the_blocked_subset() {
    let (e, mut db, clock) = review_fixture();
    // Captured a week ago — outside the glance window. First: a create's
    // `created_at` is its op's clock reading, which never runs backwards.
    set_clock(&clock, REVIEW_MON - 3 * REVIEW_DAY);
    review_task(&e, &mut db, "old capture", None);
    // Captured last night.
    set_clock(&clock, REVIEW_MON + 22 * 3_600_000);
    let capture = review_task(&e, &mut db, "shower thought", None);

    // Two tasks planned for today, one waiting on the other. They live in
    // a real Stream so the inbox assertion below is about captures alone.
    set_clock(&clock, REVIEW_MON + 25 * 3_600_000);
    let work = e
        .apply(
            &mut db,
            Command::CreateStream(StreamDraft {
                name: "Work".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    let blocker = review_task(&e, &mut db, "the blocker", Some(work));
    let waiting = review_task(&e, &mut db, "the dependent", Some(work));
    for id in [blocker, waiting] {
        e.apply(
            &mut db,
            Command::UpdateTask {
                id,
                patch: TaskPatch {
                    planned_at: Some(Some(ms_to_ts((REVIEW_MON + 25 * 3_600_000) as i64).into())),
                    blocked_by: if id == waiting {
                        Some(vec![blocker])
                    } else {
                        None
                    },
                    ..Default::default()
                },
            },
        )
        .unwrap();
    }

    let daily = match e
        .query(
            &db,
            Query::DailyReview {
                since_ms: REVIEW_MON + 18 * 3_600_000,
                now_ms: REVIEW_MON + 26 * 3_600_000,
            },
        )
        .unwrap()
    {
        QueryResult::DailyReview(d) => *d,
        other => panic!("expected DailyReview, got {other:?}"),
    };
    assert_eq!(
        daily.inbox.iter().map(|t| t.id).collect::<Vec<_>>(),
        vec![capture],
        "only captures inside the glance window: {:#?}",
        daily.inbox
    );
    assert_eq!(
        daily.blocked.iter().map(|t| t.id).collect::<Vec<_>>(),
        vec![waiting],
        "the blocker itself is not blocked"
    );
}
