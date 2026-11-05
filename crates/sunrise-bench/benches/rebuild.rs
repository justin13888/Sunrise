//! `rebuild` bench: what rebuilding the projection from the op log costs at
//! 10k and 100k tasks (issue #441, ADR-0065 PR 2, the R1 baseline).
//!
//! Each size is seeded once through the real command path, about 1.1 ops a
//! task: a create each, and a completion for one task in ten. The vault is
//! then copied, and Criterion times [`Engine::rebuild_projection`] on a fresh
//! copy per iteration, opened cold as `Core::open` would open it. One timed
//! rebuild of the seeded vault is printed beside Criterion's output with the
//! op count, so the cost per op can be read off directly.
//!
//! Sizes come from `SUNRISE_BENCH_REBUILD_TASKS` (comma-separated task
//! counts) and default to `10000,100000`. Seeding 100k tasks through the
//! command path is the slow part; it runs with `synchronous = OFF`, which
//! changes how durable each commit is and nothing about what is written.
//!
//! [`Engine::rebuild_projection`]: sunrise_core::Engine::rebuild_projection

// The size report is the point of this bench, and it goes to stderr beside
// Criterion's own output.
#![allow(clippy::doc_markdown, clippy::print_stderr)]

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use sunrise_bench::{open_vault, seed_tasks, VAULT_ROOT_KEY_BYTES};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_storage::Db;

/// Task counts to sweep, from the environment or the default pair.
///
/// Under `cargo test --all-targets` this target runs as a smoke test, with
/// no `--bench` argument, and seeding a hundred thousand tasks in a debug
/// build takes hours. There it runs one small size.
fn sizes() -> Vec<usize> {
    if !std::env::args().any(|a| a == "--bench") {
        return vec![100];
    }
    std::env::var("SUNRISE_BENCH_REBUILD_TASKS").map_or_else(
        |_| vec![10_000, 100_000],
        |raw| {
            raw.split(',')
                .filter(|s| !s.trim().is_empty())
                .map(|s| {
                    s.trim()
                        .parse()
                        .expect("SUNRISE_BENCH_REBUILD_TASKS task count")
                })
                .collect()
        },
    )
}

fn bench_rebuild(c: &mut Criterion) {
    let mut group = c.benchmark_group("rebuild");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(20));
    let key = VaultRootKey::from_bytes(VAULT_ROOT_KEY_BYTES);

    for n in sizes() {
        let mut vault = open_vault(5);
        vault
            .db
            .conn()
            .execute_batch("PRAGMA synchronous = OFF")
            .expect("seed without a sync per commit");
        // A wall-clock reading of the bench's own setup, not of the vault's
        // clock, which is fixed.
        #[allow(clippy::disallowed_methods)]
        let seeding = std::time::Instant::now();
        seed_tasks(&mut vault, n, 11);
        let ops: i64 = vault
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM ops", [], |r| r.get(0))
            .expect("count ops");
        eprintln!(
            "rebuild/{n} tasks: seeded {ops} ops in {:.1} s",
            seeding.elapsed().as_secs_f64()
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let pristine = dir.path().join("pristine.db");
        vault
            .db
            .conn()
            .execute(
                "VACUUM INTO ?1",
                [pristine.to_str().expect("utf-8 temp path")],
            )
            .expect("copy vault");
        let copy = |i: u32| {
            let path = dir.path().join(format!("copy-{i}.db"));
            std::fs::copy(&pristine, &path).expect("copy vault file");
            Db::open(&path, &key).expect("open copy")
        };

        let mut once = copy(0);
        #[allow(clippy::disallowed_methods)]
        let timed = std::time::Instant::now();
        let report = vault.engine.rebuild_projection(&mut once).expect("rebuild");
        let secs = timed.elapsed().as_secs_f64();
        eprintln!(
            "rebuild/{n} tasks: {} ops refolded in {secs:.2} s ({:.1} us/op), one transaction",
            report.ops_replayed,
            secs * 1e6 / f64::from(u32::try_from(report.ops_replayed.max(1)).unwrap_or(u32::MAX))
        );
        drop(once);

        group.bench_function(format!("refold/{n}"), |b| {
            let mut i = 0u32;
            b.iter_batched(
                || {
                    i += 1;
                    copy(i)
                },
                |mut db| vault.engine.rebuild_projection(&mut db).expect("rebuild"),
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_rebuild);
criterion_main!(benches);
