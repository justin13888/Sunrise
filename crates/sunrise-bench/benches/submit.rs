//! `submit` bench: cost of one `Engine::apply(CreateTask)` against a real
//! tempdir-backed SQLCipher DB in WAL mode. Because every apply commits a
//! `BEGIN IMMEDIATE` transaction, this includes the WAL fsync — the dominant
//! term — so the number lands in the sub-millisecond-to-millisecond range, not
//! microseconds. That is the realistic per-op capture-commit cost.
//!
//! Sample sizes are kept modest so the suite runs in seconds locally; CI /
//! nightly can raise `sample_size` / `measurement_time` for tighter intervals
//! (see `docs/10-cross-cutting/testing.md` §perf-bench CI integration).

#![allow(clippy::doc_markdown)]

use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use sunrise_bench::{open_vault, submit_draft};
use sunrise_core::Command;

fn bench_submit(c: &mut Criterion) {
    let mut vault = open_vault(1);
    let draft = submit_draft();

    c.bench_function("submit", |b| {
        b.iter(|| {
            vault
                .engine
                .apply(&mut vault.db, Command::CreateTask(draft.clone()))
                .expect("apply create task");
        });
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(30)
        .measurement_time(Duration::from_secs(6));
    targets = bench_submit
}
criterion_main!(benches);
