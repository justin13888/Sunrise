//! A routine's edit scope across the seam (issue #331): `RoutineEdit.scope`
//! splits the series, the continuation reads back with `split_from`, and an
//! edit with no scope edits the whole series as it always did.
//! `flavor = "multi_thread"` for the reason `seam.rs` gives.

use sunrise_core_bindings::dto::{EditScopeIn, RoutineDraftIn, RoutineEdit, RoutineItem, Template};
use sunrise_core_bindings::vocab::parse_recurrence;
use sunrise_core_bindings::{CoreCommand, CoreQuery, CoreQueryResult, SunriseCore};

mod support;
use support::{draft, open_core};

async fn routines(core: &SunriseCore) -> Vec<RoutineItem> {
    match core.query(CoreQuery::Routines).await.expect("routines") {
        CoreQueryResult::Routines { routines } => routines,
        other => panic!("expected routines, got {other:?}"),
    }
}

fn template(title: &str) -> Template {
    Template {
        title: title.into(),
        stream_id: sunrise_domain::inbox_stream_ref(),
        contexts: Vec::new(),
        energy: None,
        priority: None,
        estimated_duration_s: None,
        body: None,
        extra: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_this_and_future_splits_the_series_across_the_seam() {
    let (_dir, core) = open_core().await;
    let tomorrow = jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .date()
        .tomorrow()
        .unwrap();
    let starts = tomorrow
        .at(9, 0, 0, 0)
        .to_zoned(jiff::tz::TimeZone::UTC)
        .unwrap()
        .timestamp();
    let created = core
        .submit(CoreCommand::CreateRoutine {
            draft: RoutineDraftIn {
                template: template("Water the plants"),
                rrule: parse_recurrence("every day".into()).expect("parses"),
                timezone: "UTC".into(),
                starts_at: starts,
                ends_at: None,
                scheduling_constraints: Vec::new(),
                catchup_policy: sunrise_domain::RoutineCatchupPolicy::Skip,
                anchor: None,
                ends_at_time: None,
            },
        })
        .await
        .expect("create routine");
    let key = format!(
        "{}T09:00",
        tomorrow.checked_add(jiff::Span::new().days(2)).unwrap()
    );
    let split = core
        .submit(CoreCommand::UpdateRoutine {
            id: created.entity,
            edit: RoutineEdit {
                template: Some(template("Water the garden")),
                scope: Some(EditScopeIn::ThisAndFuture { key }),
                ..RoutineEdit::default()
            },
        })
        .await
        .expect("split");
    assert_ne!(split.entity, created.entity);

    let rows = routines(&core).await;
    let old = rows.iter().find(|r| r.id == created.entity).expect("old");
    let next = rows.iter().find(|r| r.id == split.entity).expect("new");
    assert_eq!(old.split_from, None);
    assert_eq!(next.split_from, Some(created.entity));
    assert_eq!(next.template.title, "Water the garden");
    assert_eq!(old.template.title, "Water the plants");

    // No scope is the whole series.
    core.submit(CoreCommand::UpdateRoutine {
        id: split.entity,
        edit: RoutineEdit {
            template: Some(template("Water")),
            ..RoutineEdit::default()
        },
    })
    .await
    .expect("edit the series");
    let rows = routines(&core).await;
    assert_eq!(rows.len(), 2);
    assert!(rows
        .iter()
        .any(|r| r.id == split.entity && r.template.title == "Water"));
    core.shutdown().await;
}

/// The hard-constraint flag reads back on every task, false for one no
/// routine generated.
#[tokio::test(flavor = "multi_thread")]
async fn a_task_reads_its_hard_constraint_flag_across_the_seam() {
    let (_dir, core) = open_core().await;
    let id = core
        .submit(CoreCommand::CreateTask {
            draft: draft("Call home"),
        })
        .await
        .expect("create")
        .entity;
    match core
        .query(CoreQuery::EntityById { id })
        .await
        .expect("read")
    {
        CoreQueryResult::Task { task } => assert!(!task.hard_constraint_violated),
        other => panic!("expected a task, got {other:?}"),
    }
    core.shutdown().await;
}
