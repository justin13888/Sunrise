//! `vault_open` bench: what `Engine::prime_hlc` costs at open, as the op log
//! grows, with and without an index on `ops (ts_ms)`.
//!
//! `prime_hlc` runs on **every** `Core::open` and issues two queries —
//! `SELECT MAX(ts_ms) FROM ops` and `SELECT envelope FROM ops WHERE ts_ms = ?`.
//! Whether those want an index of their own is a question about how the cost
//! grows with a vault that only ever gets bigger, so both arms are measured at
//! several sizes rather than at one (issue #156).
//!
//! Each iteration runs on a **freshly opened** `Db`. A reused connection would
//! answer the second and later iterations out of a page cache that a real open
//! does not have, which is exactly the cost in question.
//!
//! Sizes come from `SUNRISE_BENCH_OPEN_OPS` (comma-separated row counts) and
//! default to `10000,100000`. A million-row vault takes minutes to build and is
//! left out of the default sweep; ask for it explicitly.

#![allow(clippy::doc_markdown)]

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use sunrise_bench::{grow_op_log, open_vault, VAULT_ROOT_KEY_BYTES};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_storage::Db;

/// Row counts to sweep, from the environment or the default pair.
fn sizes() -> Vec<usize> {
    std::env::var("SUNRISE_BENCH_OPEN_OPS").map_or_else(
        |_| vec![10_000, 100_000],
        |raw| {
            raw.split(',')
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().parse().expect("SUNRISE_BENCH_OPEN_OPS row count"))
                .collect()
        },
    )
}

fn bench_prime_hlc(c: &mut Criterion) {
    let mut group = c.benchmark_group("prime_hlc");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(10));

    for n in sizes() {
        let mut vault = open_vault(3);
        let rows = grow_op_log(&mut vault, n);
        let path = vault.db_path().to_path_buf();
        let key = VaultRootKey::from_bytes(VAULT_ROOT_KEY_BYTES);

        for indexed in [false, true] {
            // The index is created and dropped here rather than assumed, so
            // this bench reads the same on either side of the migration that
            // adds one. The scan arm drops the migration's index too, or it
            // would silently measure the indexed plan twice.
            vault
                .db
                .conn()
                .execute_batch(if indexed {
                    "CREATE INDEX IF NOT EXISTS bench_ops_by_ts ON ops (ts_ms)"
                } else {
                    "DROP INDEX IF EXISTS bench_ops_by_ts;
                     DROP INDEX IF EXISTS ops_by_ts;"
                })
                .expect("toggle ts_ms index");

            let arm = if indexed { "indexed" } else { "scan" };
            group.bench_function(format!("{arm}/{rows}"), |b| {
                b.iter_batched(
                    || Db::open(&path, &key).expect("reopen vault"),
                    |db| vault.engine.prime_hlc(&db).expect("prime hlc"),
                    BatchSize::PerIteration,
                );
            });
        }
    }
    group.finish();
}

criterion_group!(benches, bench_prime_hlc);
criterion_main!(benches);
