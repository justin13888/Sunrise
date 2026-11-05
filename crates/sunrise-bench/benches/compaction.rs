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
//!
//! # Version rows (ADR-0065 D25, issue #581)
//!
//! Before the timed sizes, a report of the version DAG a fold prunes: how
//! many `entity_versions` and `entity_frontier` rows an entity keeps against
//! how many versions it took and how many other devices there are. Twenty
//! tasks are each retitled `V` times through the command path, so each has
//! `V + 1` versions, and for each count of other devices the vault is folded
//! twice from the same copy:
//!
//! - **held:** the other devices are paired and silent, having acknowledged
//!   nothing. The op log folds (they are not known devices of the op-log
//!   floor), and every version row stays: a silent device may still name
//!   any of them.
//! - **passed:** every other device has published a frontier reaching this
//!   device's tip, and the fold prunes what they have all passed.
//!
//! The other devices are rows in `devices` and `peer_frontiers` and write
//! nothing, so the device count moves what the rule reads rather than the
//! frontier rows; `crates/sunrise-core/src/engine/tests/prune.rs` bounds the
//! rows of histories written on one to three devices. Each line prints the
//! rows before the fold, held and passed, one timed fold of each, and one
//! timed projection rebuild of each folded vault. `SUNRISE_BENCH_VERSIONS`
//! (comma-separated, default `10,100,1000`) and
//! `SUNRISE_BENCH_VERSION_DEVICES` (default `1,4`) set the sweep. It is a
//! report, not a Criterion timing: the fold of the sized vaults below already
//! runs the prune.

// The size report is the point of this bench, and it goes to stderr beside
// Criterion's own output.
#![allow(clippy::doc_markdown, clippy::print_stderr)]

use std::path::Path;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use sunrise_bench::{
    open_vault, seed_tasks, submit_draft, BenchVault, FIXED_NOW_MS, VAULT_ROOT_KEY_BYTES,
};
use sunrise_core::{Command, CompactionPolicy, CompactionReport, Engine};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_domain::{TaskPatch, INBOX_STREAM_BYTES};
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

// ---- version rows ----

/// A comma-separated sweep from `var`, or `default`; `smoke` under
/// `cargo test --all-targets`, which passes no `--bench`.
fn sweep(var: &str, default: &[usize], smoke: &[usize]) -> Vec<usize> {
    if !std::env::args().any(|a| a == "--bench") {
        return smoke.to_vec();
    }
    std::env::var(var).map_or_else(
        |_| default.to_vec(),
        |raw| {
            raw.split(',')
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().parse().expect("a count"))
                .collect()
        },
    )
}

/// Tasks the version report retitles.
const VERSION_TASKS: usize = 20;

/// [`VERSION_TASKS`] tasks, each retitled `versions` times, with every op
/// acknowledged by the relay.
fn seed_versions(versions: usize) -> BenchVault {
    let mut vault = open_vault(7);
    vault
        .db
        .conn()
        .execute_batch("PRAGMA synchronous = OFF")
        .expect("seed without a sync per commit");
    let ids: Vec<_> = (0..VERSION_TASKS)
        .map(|_| {
            vault
                .engine
                .apply(&mut vault.db, Command::CreateTask(submit_draft()))
                .expect("create")
                .entity
        })
        .collect();
    for v in 0..versions {
        for id in &ids {
            let patch = TaskPatch {
                title: Some(format!("version {v}")),
                ..Default::default()
            };
            vault
                .engine
                .apply(&mut vault.db, Command::UpdateTask { id: *id, patch })
                .expect("retitle");
        }
    }
    vault
        .db
        .conn()
        .execute(
            "UPDATE outbox SET acked_at_ms = ?1",
            [i64::try_from(FIXED_NOW_MS).expect("fixed now fits i64")],
        )
        .expect("ack outbox");
    vault
}

/// Pair `n` other devices, which write nothing; when `acked`, each has
/// published a frontier reaching every prefix this replica holds.
fn pair_peers(db: &Db, n: usize, acked: bool) {
    let now = i64::try_from(FIXED_NOW_MS).expect("fixed now fits i64");
    for k in 0..n {
        let peer = [0xB0u8.wrapping_add(u8::try_from(k).expect("few peers")); 16];
        db.conn()
            .execute(
                "INSERT INTO devices (device_id, cert_blob, nickname, platform, created_at_ms)
                 VALUES (?1, x'00', 'bench peer', 'bench', ?2)",
                rusqlite::params![&peer[..], now],
            )
            .expect("pair a peer");
        if acked {
            db.conn()
                .execute(
                    "INSERT INTO peer_frontiers
                       (stream_id, peer_device_id, device_id, seq, recorded_at_ms)
                     SELECT stream_id, ?1, device_id, last_applied_seq, ?2 FROM sync_cursors",
                    rusqlite::params![&peer[..], now],
                )
                .expect("acknowledge");
        }
    }
}

/// `(entity_versions rows, entity_frontier rows)`.
fn dag_rows(db: &Db) -> (i64, i64) {
    let one = |sql: &str| {
        db.conn()
            .query_row(sql, [], |r| r.get::<_, i64>(0))
            .expect("count")
    };
    (
        one("SELECT COUNT(*) FROM entity_versions"),
        one("SELECT COUNT(*) FROM entity_frontier"),
    )
}

fn open_copy(pristine: &Path, dir: &Path, name: &str) -> Db {
    let path = dir.join(format!("{name}.db"));
    std::fs::copy(pristine, &path).expect("copy vault file");
    Db::open(&path, &VaultRootKey::from_bytes(VAULT_ROOT_KEY_BYTES)).expect("open copy")
}

/// Fold `db` once, and rebuild its projection once after, each timed in
/// seconds.
fn fold_then_rebuild(engine: &Engine, db: &mut Db) -> (CompactionReport, f64, f64) {
    // Wall-clock readings of the bench's own work, not of the vault's clock,
    // which is fixed.
    #[allow(clippy::disallowed_methods)]
    let timed = std::time::Instant::now();
    let report = engine.compact_op_log(db, &policy()).expect("compact");
    let fold = timed.elapsed().as_secs_f64();
    #[allow(clippy::disallowed_methods)]
    let timed = std::time::Instant::now();
    engine.rebuild_projection(db).expect("rebuild");
    (report, fold, timed.elapsed().as_secs_f64())
}

/// The version-row report the module docs describe.
fn report_versions() {
    #[allow(clippy::cast_precision_loss)]
    let per = |rows: i64| rows as f64 / VERSION_TASKS as f64;
    for versions in sweep("SUNRISE_BENCH_VERSIONS", &[10, 100, 1000], &[10]) {
        let vault = seed_versions(versions);
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
        let before = dag_rows(&vault.db);
        for peers in sweep("SUNRISE_BENCH_VERSION_DEVICES", &[1, 4], &[1]) {
            let mut held = open_copy(&pristine, dir.path(), &format!("held-{peers}"));
            pair_peers(&held, peers, false);
            let (_, held_fold, held_rebuild) = fold_then_rebuild(&vault.engine, &mut held);
            let mut passed = open_copy(&pristine, dir.path(), &format!("passed-{peers}"));
            pair_peers(&passed, peers, true);
            let (report, passed_fold, passed_rebuild) =
                fold_then_rebuild(&vault.engine, &mut passed);
            let (h, p) = (dag_rows(&held), dag_rows(&passed));
            eprintln!(
                "compaction/versions {versions} a task, {} devices: version rows {} -> held {} \
                 -> passed {} ({:.1} -> {:.1} -> {:.1} a task), frontier rows {} -> {} -> {}; \
                 fold {:.3} s held, {:.3} s passed ({} pruned); \
                 rebuild {:.3} s held, {:.3} s passed",
                peers + 1,
                before.0,
                h.0,
                p.0,
                per(before.0),
                per(h.0),
                per(p.0),
                before.1,
                h.1,
                p.1,
                held_fold,
                passed_fold,
                report.versions_pruned,
                held_rebuild,
                passed_rebuild,
            );
        }
    }
}

// ---- the op log ----

fn bench_compaction(c: &mut Criterion) {
    report_versions();
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
