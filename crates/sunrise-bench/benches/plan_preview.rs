//! `plan_preview` bench: the cost of one planner preview over a typical week
//! (`docs/10-cross-cutting/performance-budgets.md` §Planner preview, #343;
//! budget p95 ≤ 16 ms, p99 ≤ 33 ms on Baseline A).
//!
//! The week is seven days from the Monday after [`FIXED_NOW_MS`]: 200 open
//! tasks, 60 blocks, 40 blocks standing in for external events (every block
//! is fixed until #342 adds the flexible kind, and external events are not
//! stored until #930, so both are fixed intervals the solver works around
//! either way) and 20 dependencies. It is seeded once through the command
//! path, outside the measured loop. Each iteration gathers the inputs of the
//! days the gesture touches and solves:
//!
//! - `ripple`: a 90-minute block dropped onto three 30-minute tasks planted
//!   at 12:00 (and the day's own tasks around them, so the ripple chains);
//! - `cross_day`: a task dragged onto a busy hour of the next day;
//! - `auto_place`: twenty unplanned tasks placed into a day's free time.

#![allow(clippy::doc_markdown)]

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use jiff::civil::{date, Date};
use sunrise_bench::{open_vault, BenchVault, FIXED_NOW_MS};
use sunrise_core::Command;
use sunrise_domain::planner::{PlanIntent, SlotTarget};
use sunrise_domain::{BlockDraft, SunriseTime, TaskDraft, TaskPatch};
use sunrise_id::EntityRef;

/// The Monday after `FIXED_NOW_MS` (2025-06-15).
const WEEK: Date = date(2025, 6, 16);

fn at(day: i8, minutes: i32) -> SunriseTime {
    let d = WEEK
        .checked_add(jiff::Span::new().days(day))
        .expect("in range");
    let t = jiff::civil::Time::midnight()
        .checked_add(jiff::Span::new().minutes(minutes))
        .expect("in range");
    SunriseTime::floating(d.to_datetime(t))
}

fn task(v: &mut BenchVault, title: String, start: Option<SunriseTime>, min: u64) -> EntityRef {
    v.engine
        .apply(
            &mut v.db,
            Command::CreateTask(TaskDraft {
                title,
                planned_at: start,
                estimated_duration_s: Some(min * 60),
                ..TaskDraft::default()
            }),
        )
        .expect("create task")
        .entity
}

fn block(v: &mut BenchVault, title: String, start: SunriseTime, end: SunriseTime) -> EntityRef {
    v.engine
        .apply(
            &mut v.db,
            Command::CreateBlock(BlockDraft {
                stream_id: None,
                starts_at: start,
                ends_at: end,
                title: Some(title),
                title_track_task: false,
                tasks: Vec::new(),
            }),
        )
        .expect("create block")
        .entity
}

/// The gestures the bench measures, over the seeded week.
struct Week {
    ripple: PlanIntent,
    cross_day: PlanIntent,
    auto_place: PlanIntent,
}

fn seed(v: &mut BenchVault) -> Week {
    // Day 0, 07:00–08:30: the block the ripple drags, onto three tasks at
    // 12:00, 12:30 and 13:00.
    let dragged = block(v, "Deep work".into(), at(0, 7 * 60), at(0, 8 * 60 + 30));
    for k in 0..3 {
        task(v, format!("ripple {k}"), Some(at(0, 12 * 60 + k * 30)), 30);
    }
    // 59 more blocks and 40 event stand-ins, spread over the week's mornings
    // and afternoons.
    for n in 0..59i32 {
        let day = i8::try_from(n % 7).expect("day");
        let start = 9 * 60 + (n / 7) * 75;
        block(v, format!("block {n}"), at(day, start), at(day, start + 60));
    }
    for n in 0..40i32 {
        let day = i8::try_from(n % 7).expect("day");
        let start = 15 * 60 + (n / 7) * 60;
        block(v, format!("event {n}"), at(day, start), at(day, start + 30));
    }
    // 177 planned tasks over the week's evenings and gaps, the dependent
    // half of 20 pairs among them, and 20 unplanned for the auto-place.
    let mut planned = Vec::new();
    for n in 0..177i32 {
        let day = i8::try_from(n % 7).expect("day");
        let start = 7 * 60 + ((n / 7) * 30) % (16 * 60);
        planned.push(task(v, format!("task {n}"), Some(at(day, start)), 30));
    }
    for pair in planned.chunks(2).take(20) {
        v.engine
            .apply(
                &mut v.db,
                Command::UpdateTask {
                    id: pair[1],
                    patch: TaskPatch {
                        blocked_by: Some(vec![pair[0]]),
                        ..TaskPatch::default()
                    },
                },
            )
            .expect("dependency");
    }
    let loose: Vec<EntityRef> = (0..20)
        .map(|n| task(v, format!("loose {n}"), None, 30))
        .collect();
    Week {
        ripple: PlanIntent::MoveBlock {
            block: dragged,
            to_start: at(0, 12 * 60),
        },
        cross_day: PlanIntent::MoveTask {
            task: planned[1],
            to: SlotTarget::At(at(2, 10 * 60)),
        },
        auto_place: PlanIntent::AutoPlace {
            tasks: loose,
            day: WEEK
                .checked_add(jiff::Span::new().days(3))
                .expect("in range"),
            not_before: None,
        },
    }
}

fn bench_plan_preview(c: &mut Criterion) {
    let mut vault = open_vault(3);
    let week = seed(&mut vault);
    let ripple = vault
        .engine
        .plan_preview(&vault.db, &week.ripple, FIXED_NOW_MS)
        .expect("ripple");
    assert!(
        ripple.moves.len() >= 4,
        "the ripple fixture moves the block and its three tasks: {ripple:#?}"
    );

    let mut group = c.benchmark_group("plan_preview");
    for (name, intent) in [
        ("ripple", &week.ripple),
        ("cross_day", &week.cross_day),
        ("auto_place", &week.auto_place),
    ] {
        group.bench_function(BenchmarkId::from_parameter(name), |b| {
            b.iter(|| {
                vault
                    .engine
                    .plan_preview(&vault.db, intent, FIXED_NOW_MS)
                    .expect("preview");
            });
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(100)
        .measurement_time(Duration::from_secs(5));
    targets = bench_plan_preview
}
criterion_main!(benches);
