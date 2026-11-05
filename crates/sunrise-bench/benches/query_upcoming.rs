//! `query_upcoming` bench: cost of `Query::Upcoming` at its widest span, 30
//! days, over a 10k-task vault (#351; budget p95 ≤ 100 ms).
//!
//! The vault is seeded once in setup (outside the measured loop); each
//! iteration re-runs the whole view: the undated and in-span task scan, row
//! hydrate, the block overlap read and the civil-day fold.

#![allow(clippy::doc_markdown)]

use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use sunrise_bench::{open_vault, seed_tasks, FIXED_NOW_MS};
use sunrise_core::Query;

fn bench_query_upcoming(c: &mut Criterion) {
    let mut vault = open_vault(2);
    seed_tasks(&mut vault, 10_000, 100);

    c.bench_function("query_upcoming", |b| {
        b.iter(|| {
            vault
                .engine
                .query(
                    &vault.db,
                    Query::Upcoming {
                        now_ms: FIXED_NOW_MS,
                        span_days: Some(30),
                        contexts: Vec::new(),
                    },
                )
                .expect("query upcoming");
        });
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(50)
        .measurement_time(Duration::from_secs(5));
    targets = bench_query_upcoming
}
criterion_main!(benches);
