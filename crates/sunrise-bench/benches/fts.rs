//! `fts` bench: cost of `Query::Search` over a 10k-task vault, matching a word
//! present in ~1% of titles ([`sunrise_bench::RARE_WORD`]) so the FTS5 index
//! returns a realistically selective (~100-row) result set.

#![allow(clippy::doc_markdown)]

use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use sunrise_bench::{open_vault, seed_tasks, RARE_WORD};
use sunrise_core::Query;

fn bench_fts(c: &mut Criterion) {
    let mut vault = open_vault(3);
    seed_tasks(&mut vault, 10_000, 100);

    c.bench_function("fts", |b| {
        b.iter(|| {
            vault
                .engine
                .query(
                    &vault.db,
                    Query::Search {
                        text: RARE_WORD.to_string(),
                        limit: 200,
                    },
                )
                .expect("fts search");
        });
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(50)
        .measurement_time(Duration::from_secs(5));
    targets = bench_fts
}
criterion_main!(benches);
