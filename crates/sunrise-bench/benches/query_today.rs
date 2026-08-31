//! `query_today` bench: cost of `Query::Today` over a 10k-task vault.
//!
//! The vault is seeded once in setup (outside the measured loop); each iteration
//! re-runs the Today projection query (scheduled/due window scan + row hydrate).

#![allow(clippy::doc_markdown)]

use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use sunrise_bench::{open_vault, seed_tasks, FIXED_NOW_MS};
use sunrise_core::Query;

fn bench_query_today(c: &mut Criterion) {
    let mut vault = open_vault(2);
    seed_tasks(&mut vault, 10_000, 100);

    c.bench_function("query_today", |b| {
        b.iter(|| {
            vault
                .engine
                .query(
                    &vault.db,
                    Query::Today {
                        now_ms: FIXED_NOW_MS,
                        contexts: Vec::new(),
                    },
                )
                .expect("query today");
        });
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(50)
        .measurement_time(Duration::from_secs(5));
    targets = bench_query_today
}
criterion_main!(benches);
