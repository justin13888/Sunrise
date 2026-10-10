//! `compaction` bench: how big the op log is at 10k and 100k tasks, how much
//! of it op-log compaction folds away, and what the fold and a stream
//! snapshot cost (ADR-0059, issue #330).
//!
//! Each size is seeded once through the real command path, with every op
//! acknowledged by the relay and older than the retention window, so the
//! whole log is eligible. The sizes before and after the fold, and the size of
//! the stream's snapshot record, are printed once per size; Criterion then
//! times the fold on a fresh copy of the vault per iteration, and the
//! snapshot write on the folded one.
//!
//! What "log size" counts: the rows in `ops` and the bytes of their sealed
//! envelopes. The file's page count is printed beside them. A run that
//! deletes rows ends with an incremental vacuum (issue #461), so the count
//! after it is what the fold returned to the filesystem net of the pages the
//! snapshot it writes takes.
//!
//! Sizes come from `SUNRISE_BENCH_COMPACT_TASKS` (comma-separated task
//! counts) and default to `10000,100000`. Seeding is the slow part: 100k
//! tasks through the command path took about 24 minutes on an M-series Mac,
//! and the fold itself about 43 s, snapshot write included.

// The size report is the point of this bench, and it goes to stderr beside
// Criterion's own output.
#![allow(clippy::doc_markdown, clippy::print_stderr)]

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use sunrise_bench::{open_vault, seed_tasks, FIXED_NOW_MS, VAULT_ROOT_KEY_BYTES};
use sunrise_core::CompactionPolicy;
use sunrise_crypto::keys::VaultRootKey;
use sunrise_domain::INBOX_STREAM_BYTES;
use sunrise_storage::Db;

/// Task counts to sweep, from the environment or the default pair.
///
/// Under `cargo test --all-targets` this target runs as a smoke test, with
/// no `--bench` argument, and seeding a hundred thousand tasks through the
/// command path in a debug build takes hours. There it runs one small size.
fn sizes() -> Vec<usize> {
    if !std::env::args().any(|a| a == "--bench") {
        return vec![100];
    }
    std::env::var("SUNRISE_BENCH_COMPACT_TASKS").map_or_else(
        |_| vec![10_000, 100_000],
        |raw| {
            raw.split(',')
                .filter(|s| !s.trim().is_empty())
                .map(|s| {
                    s.trim()
                        .parse()
                        .expect("SUNRISE_BENCH_COMPACT_TASKS task count")
                })
                .collect()
        },
    )
}

/// Every op is eligible: the bench vault's clock is fixed, so a zero
/// retention window is what "older than the window" means here.
fn policy() -> CompactionPolicy {
    CompactionPolicy {
        retention_ms: 0,
        ..CompactionPolicy::default()
    }
}

/// `(rows in ops, bytes of their envelopes, file pages, free pages)`.
fn log_size(db: &Db) -> (i64, i64, i64, i64) {
    let one = |sql: &str| {
        db.conn()
            .query_row(sql, [], |r| r.get::<_, i64>(0))
            .expect("log size")
    };
    (
        one("SELECT COUNT(*) FROM ops"),
        one("SELECT COALESCE(SUM(LENGTH(envelope)), 0) FROM ops"),
        one("PRAGMA page_count"),
        one("PRAGMA freelist_count"),
    )
}

fn bench_compaction(c: &mut Criterion) {
    let mut group = c.benchmark_group("compaction");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(20));
    let key = VaultRootKey::from_bytes(VAULT_ROOT_KEY_BYTES);

    for n in sizes() {
        let mut vault = open_vault(5);
        // A wall-clock reading of the bench's own setup, not of the vault's
        // clock, which is fixed.
        #[allow(clippy::disallowed_methods)]
        let seeding = std::time::Instant::now();
        seed_tasks(&mut vault, n, 11);
        eprintln!(
            "compaction/{n} tasks: seeded in {:.1} s",
            seeding.elapsed().as_secs_f64()
        );
        // The relay has acknowledged every op: compaction never folds one
        // still waiting to be sent.
        vault
            .db
            .conn()
            .execute(
                "UPDATE outbox SET acked_at_ms = ?1",
                [i64::try_from(FIXED_NOW_MS).expect("fixed now fits i64")],
            )
            .expect("ack outbox");
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

        group.bench_function(format!("fold/{n}"), |b| {
            let mut i = 0u32;
            b.iter_batched(
                || {
                    i += 1;
                    let copy = dir.path().join(format!("copy-{i}.db"));
                    std::fs::copy(&pristine, &copy).expect("copy vault file");
                    Db::open(&copy, &key).expect("open copy")
                },
                |mut db| {
                    vault
                        .engine
                        .compact_op_log(&mut db, &policy())
                        .expect("compact")
                },
                BatchSize::PerIteration,
            );
        });

        let before = log_size(&vault.db);
        let report = vault
            .engine
            .compact_op_log(&mut vault.db, &policy())
            .expect("compact");
        let after = log_size(&vault.db);
        let record = vault
            .engine
            .write_stream_snapshot(&mut vault.db, &INBOX_STREAM_BYTES)
            .expect("snapshot")
            .expect("a frontier to state");
        eprintln!(
            "compaction/{n} tasks: ops {} -> {} rows, envelopes {} -> {} bytes, \
             file {} pages ({} free after), {} rows removed, snapshot record {} bytes",
            before.0,
            after.0,
            before.1,
            after.1,
            after.2,
            after.3,
            report.ops_removed,
            record.len()
        );

        group.bench_function(format!("snapshot/{n}"), |b| {
            b.iter(|| {
                vault
                    .engine
                    .write_stream_snapshot(&mut vault.db, &INBOX_STREAM_BYTES)
                    .expect("snapshot")
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_compaction);
criterion_main!(benches);
