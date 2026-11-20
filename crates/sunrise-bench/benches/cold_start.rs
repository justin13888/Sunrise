//! `cold_start` bench: a vault opened and its first `Query::Today` answered,
//! timed as one unit (`docs/10-cross-cutting/performance-budgets.md` §Cold
//! start).
//!
//! Each iteration is what the core does between an app launching and its
//! Today screen having rows to draw: `Core::open` over a 10k-task vault that
//! nothing else holds open (lock, SQLCipher key derivation, migrations check,
//! keychain load, `prime_hlc`, the open-time rebuild and index checks), then
//! `Core::query(Query::Today)`. The `Core` is closed and dropped outside the
//! timed span. The UI's own launch and first render are not in it; they are
//! the platform harnesses' part of the budget.
//!
//! Cold for the process, not for the machine: every iteration is a new
//! connection with an empty page cache, but the operating system's file cache
//! is warm after the first one. Dropping that needs root, which no CI runner
//! here grants.

#![allow(clippy::doc_markdown)]

use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use sunrise_bench::{bench_unlock, core_config, seeded_vault_dir, FIXED_NOW_MS};
use sunrise_core::{Core, Query};

/// Tasks in the seeded vault; the size `baseline.rs` records.
const TASKS: usize = 10_000;

fn bench_cold_start(c: &mut Criterion) {
    let dir = seeded_vault_dir(TASKS, 100);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    // The first open after seeding writes what an established vault already
    // holds (the device cert's op, the search indexes). Do it once here so
    // every sample measures the steady relaunch, not that one-time write.
    rt.block_on(async {
        Core::open(core_config(dir.path()), bench_unlock())
            .await
            .expect("first open")
            .close()
            .await
            .expect("close");
    });

    let mut group = c.benchmark_group("cold_start");
    group
        .sample_size(20)
        .measurement_time(Duration::from_secs(10));
    group.bench_function(format!("today_{TASKS}"), |b| {
        b.iter_custom(|iters| {
            rt.block_on(async {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    // A wall-clock reading of the bench's own measurement.
                    #[allow(clippy::disallowed_methods)]
                    let at = std::time::Instant::now();
                    let core = Core::open(core_config(dir.path()), bench_unlock())
                        .await
                        .expect("open vault");
                    let today = core
                        .query(Query::Today {
                            now_ms: FIXED_NOW_MS,
                            contexts: Vec::new(),
                        })
                        .await
                        .expect("query today");
                    total += at.elapsed();
                    drop(today);
                    core.close().await.expect("close");
                }
                total
            })
        });
    });
    group.finish();
}

criterion_group!(benches, bench_cold_start);
criterion_main!(benches);
