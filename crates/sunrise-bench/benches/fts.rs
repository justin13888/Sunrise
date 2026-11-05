//! `fts` bench: what `Query::SearchGrouped` costs over a vault of 10k and of
//! 100k tasks, with notes, blocks and streams in proportion (issue #345,
//! ADR-0052 §9, `docs/10-cross-cutting/performance-budgets.md` §Search).
//!
//! Each size is seeded once through the real command path by
//! [`sunrise_bench::seed_search_vault`]: a third of the tasks carry a note,
//! one block per twenty tasks, one stream per thousand. Criterion times the
//! word query ([`sunrise_bench::RARE_WORD`], in about 1% of titles and of
//! notes) as `fts/query_<n>`, the series the nightly baseline records.
//!
//! Beside Criterion's output, each size prints what the budget is stated in
//! and Criterion does not report: the p95 over a mix of query shapes (a
//! word, a substring the trigram index answers, a two-character CJK term the
//! title scan answers, a stream filter with a word, a negation and a state
//! filter, and a typed prefix), the bytes both indexes occupy, and the
//! process's peak resident set (Linux; `VmHWM`).
//!
//! Sizes come from `SUNRISE_BENCH_FTS_TASKS` (comma-separated task counts)
//! and default to `10000,100000`. Under `cargo test --all-targets`, with no
//! `--bench` argument, it runs one small size as a smoke test.

// The budget report is the point of this bench, and it goes to stderr beside
// Criterion's own output.
#![allow(clippy::doc_markdown, clippy::print_stderr)]

use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use sunrise_bench::{open_vault, seed_search_vault, BenchVault, FIXED_NOW_MS, RARE_WORD};
use sunrise_core::Query;

fn sizes() -> Vec<usize> {
    if !std::env::args().any(|a| a == "--bench") {
        return vec![100];
    }
    std::env::var("SUNRISE_BENCH_FTS_TASKS").map_or_else(
        |_| vec![10_000, 100_000],
        |raw| {
            raw.split(',')
                .filter(|s| !s.trim().is_empty())
                .map(|s| {
                    s.trim()
                        .parse()
                        .expect("SUNRISE_BENCH_FTS_TASKS task count")
                })
                .collect()
        },
    )
}

fn search(vault: &BenchVault, text: &str, prefix: bool) {
    vault
        .engine
        .query(
            &vault.db,
            Query::SearchGrouped {
                text: text.to_owned(),
                now_ms: FIXED_NOW_MS,
                prefix,
                per_group: None,
            },
        )
        .expect("search");
}

/// The bytes the two indexes and their doc table occupy: their pages, when
/// this SQLite has `dbstat`, else the text they hold.
fn index_bytes(vault: &BenchVault) -> (u64, &'static str) {
    let pages: Result<i64, _> = vault.db.conn().query_row(
        "SELECT SUM(pgsize) FROM dbstat
         WHERE name LIKE 'search_words%' OR name LIKE 'search_tri%' OR name = 'search_docs'",
        [],
        |r| r.get(0),
    );
    if let Ok(n) = pages {
        return (n.unsigned_abs(), "pages");
    }
    let text: i64 = vault
        .db
        .conn()
        .query_row(
            "SELECT (SELECT COALESCE(SUM(length(title) + length(aux) + length(body)), 0)
                       FROM search_words)
                  + (SELECT COALESCE(SUM(length(title) + length(body)), 0) FROM search_tri)",
            [],
            |r| r.get(0),
        )
        .expect("index text");
    (
        text.unsigned_abs(),
        "stored text, before FTS5's own index pages",
    )
}

/// `bytes` in MiB, to the KiB.
fn mib(bytes: u64) -> f64 {
    f64::from(u32::try_from(bytes / 1024).unwrap_or(u32::MAX)) / 1024.0
}

/// The process's peak resident set in KiB, where the OS reports it.
fn peak_rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
}

/// The p95 of a sample, in milliseconds.
fn p95(samples: &mut [f64]) -> f64 {
    samples.sort_by(f64::total_cmp);
    let rank = (samples.len() * 95).div_ceil(100).saturating_sub(1);
    samples[rank]
}

/// The p95 of `rounds` passes over `queries`, in milliseconds, and the
/// slowest query by its own p95.
fn p95_ms<'q>(
    vault: &BenchVault,
    queries: &[(&'q str, bool)],
    rounds: usize,
) -> (f64, &'q str, f64) {
    let mut all = Vec::with_capacity(queries.len() * rounds);
    let mut each = vec![Vec::with_capacity(rounds); queries.len()];
    for _ in 0..rounds {
        for (i, (q, prefix)) in queries.iter().enumerate() {
            // A wall-clock reading of the bench's own measurement, not of the
            // vault's clock, which is fixed.
            #[allow(clippy::disallowed_methods)]
            let at = std::time::Instant::now();
            search(vault, q, *prefix);
            let ms = at.elapsed().as_secs_f64() * 1e3;
            all.push(ms);
            each[i].push(ms);
        }
    }
    let (slowest, worst) = each
        .iter_mut()
        .map(|s| p95(s))
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap_or((0, 0.0));
    (p95(&mut all), queries[slowest].0, worst)
}

fn bench_fts(c: &mut Criterion) {
    let mut group = c.benchmark_group("fts");
    group
        .sample_size(50)
        .measurement_time(Duration::from_secs(5));
    for n in sizes() {
        let mut vault = open_vault(3);
        vault
            .db
            .conn()
            .execute_batch("PRAGMA synchronous = OFF")
            .expect("seed without a sync per commit");
        #[allow(clippy::disallowed_methods)]
        let seeding = std::time::Instant::now();
        let streams = seed_search_vault(&mut vault, n, 100);
        eprintln!(
            "fts/{n} tasks: seeded in {:.1} s",
            seeding.elapsed().as_secs_f64()
        );
        let stream_query = format!("#\"{}\" {RARE_WORD}", streams[0]);
        let queries: Vec<(&str, bool)> = vec![
            (RARE_WORD, false),
            ("okk", false),
            ("東京", false),
            (stream_query.as_str(), false),
            ("budget -report state:open", false),
            ("invo", true),
        ];
        let (p95, slowest, slowest_p95) = p95_ms(&vault, &queries, 20);
        let budget = if n <= 10_000 { 100.0 } else { 250.0 };
        let (bytes, measured) = index_bytes(&vault);
        let rss = peak_rss_kib().map_or_else(
            || "unavailable on this OS".to_owned(),
            |k| format!("{:.1} MiB", mib(k * 1024)),
        );
        eprintln!(
            "fts/{n} tasks: p95 {p95:.2} ms over {} queries (budget {budget} ms), \
             slowest `{slowest}` at p95 {slowest_p95:.2} ms; \
             index {:.1} MiB ({measured}); peak RSS {rss}",
            queries.len() * 20,
            mib(bytes),
        );

        group.bench_function(format!("query_{n}"), |b| {
            b.iter(|| search(&vault, RARE_WORD, false));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_fts);
criterion_main!(benches);
