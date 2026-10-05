//! `baseline` — parse Criterion's raw sample data and merge computed p50/p99
//! percentiles into `bench/baseline.json` for the current platform.
//!
//! Criterion writes `target/criterion/<bench>/new/sample.json` containing two
//! parallel arrays: `iters[i]` (iterations in sample `i`) and `times[i]` (total
//! nanoseconds for those iterations). The per-iteration time is
//! `times[i] / iters[i]`; percentiles are taken across those per-sample values.
//!
//! [`BENCHES`] is the bench → metric-key mapping, one row per directory
//! Criterion writes. A grouped bench writes `<group>/<function>/`, with every
//! `/` inside the function name made filename-safe as `_`, so
//! `prime_hlc`'s `scan/10000` lands in `prime_hlc/scan_10000/`.
//!
//! The table and `benches/*.rs` are held to each other by a unit test: every
//! `bench_function` or `benchmark_group` name in a bench source must head a
//! row, and every row must name one. Without that, renaming a bench left its
//! row reading a directory nothing writes any more, and the `None` arm skipped
//! it without a word (#366).
//!
//! The platform key is derived from the build target (`linux-x86_64`,
//! `darwin-aarch64`, …). Other platforms and the schema fields in
//! `baseline.json` are preserved on merge.
//!
//! Usage:
//! - `baseline [CRITERION_DIR] [BASELINE_JSON]` — merge current samples in.
//! - `baseline --check [CRITERION_DIR] [BASELINE_JSON] [TOLERANCE_PCT]` —
//!   compare without writing, exiting non-zero if any metric regressed by more
//!   than the tolerance, or if a row's directory is missing or a directory no
//!   row names is present. Both are printed in either mode; only `--check`
//!   fails on them, because merging the results of `cargo bench --bench
//!   submit` alone is a legitimate partial update.
//!
//! On the tolerance: `docs/10-cross-cutting/testing.md` specifies a 5% gate.
//! That figure assumes stable hardware. On GitHub's shared runners, run-to-run
//! variance on these benches routinely exceeds 5%, so a 5% gate there would
//! fail constantly on noise — which is worse than no gate, because a check
//! that cries wolf gets ignored. CI therefore runs this with a wide tolerance
//! to catch order-of-magnitude regressions, and the spec's 5% gate belongs on
//! dedicated hardware. The default here is the spec's 5%.

// This is a developer CLI tool, not a core hot path: printing a human-readable
// summary to stdout is intended, and the percentile math casts freely between
// integer counts and f64.
#![allow(
    clippy::print_stdout,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::doc_markdown
)]

use std::path::Path;
use std::process::ExitCode;

use serde_json::Value;

/// Default regression tolerance, per `testing.md`.
const DEFAULT_TOLERANCE_PCT: f64 = 5.0;

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().collect();
    let check_mode = raw.iter().any(|a| a == "--check");
    let args: Vec<String> = raw.into_iter().filter(|a| a != "--check").collect();
    let criterion_dir = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "target/criterion".to_string());
    let baseline_path = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| "bench/baseline.json".to_string());
    let tolerance_pct: f64 = args
        .get(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_TOLERANCE_PCT);

    let platform = platform_key();
    println!("baseline: platform = {platform}");
    println!("baseline: reading Criterion samples from {criterion_dir}");

    let Collected {
        metrics,
        missing,
        unknown,
    } = collect_metrics(Path::new(&criterion_dir));
    for dir in &missing {
        println!(
            "baseline: missing {criterion_dir}/{dir}/new/sample.json — that bench did not run"
        );
    }
    for dir in &unknown {
        println!(
            "baseline: unknown bench directory {criterion_dir}/{dir} — no row in BENCHES reads it"
        );
    }
    if metrics.is_empty() {
        println!("baseline: no sample.json found under {criterion_dir}; run `cargo bench` first.");
        return ExitCode::FAILURE;
    }

    for (key, value) in &metrics {
        println!("  {key} = {value}");
    }

    if check_mode {
        let compared = check_against(
            Path::new(&baseline_path),
            &platform,
            &metrics,
            tolerance_pct,
        );
        if !missing.is_empty() || !unknown.is_empty() {
            println!(
                "::error::baseline: {} missing and {} unknown bench director(ies); see above",
                missing.len(),
                unknown.len()
            );
            return ExitCode::FAILURE;
        }
        return compared;
    }

    merge_into(Path::new(&baseline_path), &platform, &metrics)
        .expect("merge metrics into baseline.json");
    println!(
        "baseline: merged {} metric(s) into {baseline_path}",
        metrics.len()
    );
    ExitCode::SUCCESS
}

/// Compare `metrics` against the recorded baseline for `platform`.
///
/// Every metric here is a latency, so *higher is worse* and only upward moves
/// are regressions. A metric with no recorded baseline (`null`, or a platform
/// we have never recorded) is reported and skipped rather than treated as a
/// pass or a failure — there is nothing to compare against, and silently
/// passing would let an unrecorded platform drift forever.
fn check_against(
    baseline_path: &Path,
    platform: &str,
    metrics: &[(String, f64)],
    tolerance_pct: f64,
) -> ExitCode {
    let Ok(text) = std::fs::read_to_string(baseline_path) else {
        println!("baseline: cannot read {}", baseline_path.display());
        return ExitCode::FAILURE;
    };
    let Ok(doc) = serde_json::from_str::<Value>(&text) else {
        println!("baseline: {} is not valid JSON", baseline_path.display());
        return ExitCode::FAILURE;
    };
    let recorded = doc.get("platforms").and_then(|p| p.get(platform));

    let mut regressions = Vec::new();
    let mut compared = 0usize;
    for (key, current) in metrics {
        let previous = recorded
            .and_then(|r| r.get(key))
            .and_then(serde_json::Value::as_f64);
        let Some(previous) = previous else {
            println!("  {key}: no baseline for {platform} — skipped");
            continue;
        };
        if previous <= 0.0 {
            println!("  {key}: baseline is not positive ({previous}) — skipped");
            continue;
        }
        compared += 1;
        let delta_pct = (current - previous) / previous * 100.0;
        let verdict = if delta_pct > tolerance_pct {
            regressions.push((key.clone(), previous, *current, delta_pct));
            "REGRESSION"
        } else if delta_pct < -tolerance_pct {
            "improved"
        } else {
            "ok"
        };
        println!("  {key}: {previous:.3} -> {current:.3} ({delta_pct:+.1}%) {verdict}");
    }

    if compared == 0 {
        println!(
            "baseline: nothing comparable for {platform}; not failing on an empty comparison."
        );
        return ExitCode::SUCCESS;
    }
    if regressions.is_empty() {
        println!("baseline: {compared} metric(s) within {tolerance_pct:.0}% tolerance.");
        return ExitCode::SUCCESS;
    }
    for (key, prev, cur, pct) in &regressions {
        println!("::error::{key} regressed {pct:+.1}% ({prev:.3} -> {cur:.3})");
    }
    ExitCode::FAILURE
}

/// Build-target platform key, e.g. `linux-x86_64` or `darwin-aarch64`.
fn platform_key() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    format!("{os}-{}", std::env::consts::ARCH)
}

/// The unit a metric is recorded in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    /// Microseconds.
    Us,
    /// Milliseconds.
    Ms,
}

/// One directory Criterion writes, and the baseline keys its percentiles land
/// under.
#[derive(Debug)]
struct Bench {
    /// Relative to the Criterion output directory: a `bench_function` name, or
    /// `<group>/<function>` with the function's own `/` written as `_`.
    dir: &'static str,
    /// The p50 key, for the benches whose median is part of the budget.
    p50: Option<&'static str>,
    /// The p99 key. Every bench has one.
    p99: &'static str,
    unit: Unit,
}

/// Every bench the baseline records.
///
/// The sized groups list their *default* sizes only — `vault_open` at 10k and
/// 100k rows, `compaction` at 10k tasks. A run at another size writes a
/// directory no row names, which `--check` reports as unknown rather than
/// comparing against a baseline taken at a different size. `compaction` at
/// 100k is left out because seeding it takes about 24 minutes, longer than the
/// whole nightly job is allowed; CI sets `SUNRISE_BENCH_COMPACT_TASKS=10000`.
const BENCHES: &[Bench] = &[
    Bench {
        dir: "submit",
        p50: Some("submit_create_task_p50_us"),
        p99: "submit_create_task_p99_us",
        unit: Unit::Us,
    },
    Bench {
        dir: "query_today",
        p50: None,
        p99: "query_today_10k_tasks_p99_ms",
        unit: Unit::Ms,
    },
    Bench {
        dir: "fts",
        p50: None,
        p99: "fts_query_10k_p99_ms",
        unit: Unit::Ms,
    },
    Bench {
        dir: "sync_session",
        p50: None,
        p99: "sync_session_p99_ms",
        unit: Unit::Ms,
    },
    Bench {
        dir: "prime_hlc/scan_10000",
        p50: None,
        p99: "vault_open_prime_hlc_scan_10k_ops_p99_ms",
        unit: Unit::Ms,
    },
    Bench {
        dir: "prime_hlc/indexed_10000",
        p50: None,
        p99: "vault_open_prime_hlc_indexed_10k_ops_p99_ms",
        unit: Unit::Ms,
    },
    Bench {
        dir: "prime_hlc/scan_100000",
        p50: None,
        p99: "vault_open_prime_hlc_scan_100k_ops_p99_ms",
        unit: Unit::Ms,
    },
    Bench {
        dir: "prime_hlc/indexed_100000",
        p50: None,
        p99: "vault_open_prime_hlc_indexed_100k_ops_p99_ms",
        unit: Unit::Ms,
    },
    Bench {
        dir: "compaction/fold_10000",
        p50: None,
        p99: "compaction_fold_10k_tasks_p99_ms",
        unit: Unit::Ms,
    },
    Bench {
        dir: "compaction/snapshot_10000",
        p50: None,
        p99: "compaction_snapshot_10k_tasks_p99_ms",
        unit: Unit::Ms,
    },
];

/// Criterion's own summary directory, which holds no samples.
const NOT_A_BENCH: &[&str] = &["report"];

/// What one pass over the Criterion output found.
#[derive(Debug, Default)]
struct Collected {
    /// `(metric_key, value)` in [`BENCHES`] order.
    metrics: Vec<(String, f64)>,
    /// Rows whose `sample.json` is absent or unreadable.
    missing: Vec<&'static str>,
    /// Directories holding a `sample.json` that no row names.
    unknown: Vec<String>,
}

/// Read every [`BENCHES`] row under `criterion_dir`, and say which rows had
/// nothing to read and which directories no row accounts for.
fn collect_metrics(criterion_dir: &Path) -> Collected {
    let mut out = Collected::default();
    for bench in BENCHES {
        let sample_path = sample_path(criterion_dir, bench.dir);
        match read_percentiles(&sample_path) {
            Some((p50_ns, p99_ns)) => out.metrics.extend(metrics_for(bench, p50_ns, p99_ns)),
            None => out.missing.push(bench.dir),
        }
    }
    out.unknown = sampled_dirs(criterion_dir)
        .into_iter()
        .filter(|dir| !BENCHES.iter().any(|b| b.dir == dir))
        .collect();
    out
}

fn sample_path(criterion_dir: &Path, dir: &str) -> std::path::PathBuf {
    let mut p = criterion_dir.to_path_buf();
    p.extend(dir.split('/'));
    p.join("new").join("sample.json")
}

/// Every directory, one or two levels deep, that holds a `new/sample.json`,
/// as a `/`-joined path relative to `criterion_dir`, sorted.
fn sampled_dirs(criterion_dir: &Path) -> Vec<String> {
    fn subdirs(dir: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.sort();
        names
    }
    let has_sample = |rel: &str| sample_path(criterion_dir, rel).is_file();
    let mut out = Vec::new();
    for top in subdirs(criterion_dir) {
        if NOT_A_BENCH.contains(&top.as_str()) {
            continue;
        }
        if has_sample(&top) {
            out.push(top.clone());
        }
        for inner in subdirs(&criterion_dir.join(&top)) {
            let rel = format!("{top}/{inner}");
            if has_sample(&rel) {
                out.push(rel);
            }
        }
    }
    out
}

/// Read `sample.json` and return `(p50_ns, p99_ns)` of the per-iteration times,
/// or `None` if the file is missing/unreadable.
fn read_percentiles(sample_path: &Path) -> Option<(f64, f64)> {
    let text = std::fs::read_to_string(sample_path).ok()?;
    let mut per_iter = per_iteration_ns(&text)?;
    per_iter.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some((percentile(&per_iter, 50.0), percentile(&per_iter, 99.0)))
}

/// Extract per-iteration nanosecond values (`times[i] / iters[i]`) from a
/// Criterion `sample.json` document.
fn per_iteration_ns(text: &str) -> Option<Vec<f64>> {
    let doc: Value = serde_json::from_str(text).ok()?;
    let iters = doc.get("iters")?.as_array()?;
    let times = doc.get("times")?.as_array()?;
    if iters.len() != times.len() || iters.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(iters.len());
    for (it, t) in iters.iter().zip(times.iter()) {
        let it = it.as_f64()?;
        let t = t.as_f64()?;
        if it > 0.0 {
            out.push(t / it);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Nearest-rank percentile of an already-sorted, non-empty slice.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

/// Map a bench's `(p50_ns, p99_ns)` to its baseline metric keys, converting
/// units and rounding to two decimals.
fn metrics_for(bench: &Bench, p50_ns: f64, p99_ns: f64) -> Vec<(String, f64)> {
    let convert = |ns: f64| match bench.unit {
        Unit::Us => round2(ns / 1_000.0),
        Unit::Ms => round2(ns / 1_000_000.0),
    };
    bench
        .p50
        .map(|key| (key.to_string(), convert(p50_ns)))
        .into_iter()
        .chain([(bench.p99.to_string(), convert(p99_ns))])
        .collect()
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Merge `metrics` into `platform`'s object inside the baseline JSON at `path`,
/// preserving every other platform and all schema/comment fields. Writes the
/// file back with the repo's 4-space indent + trailing newline.
fn merge_into(path: &Path, platform: &str, metrics: &[(String, f64)]) -> std::io::Result<()> {
    let text = std::fs::read_to_string(path)?;
    let mut root: Value = serde_json::from_str(&text)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    // Ensure `platforms` is an object.
    if !root.get("platforms").is_some_and(Value::is_object) {
        root["platforms"] = Value::Object(serde_json::Map::new());
    }
    let platforms = root
        .get_mut("platforms")
        .and_then(Value::as_object_mut)
        .expect("platforms is an object");

    // Ensure this platform's entry exists.
    platforms
        .entry(platform.to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let plat = platforms
        .get_mut(platform)
        .and_then(Value::as_object_mut)
        .expect("platform is an object");

    for (key, value) in metrics {
        plat.insert(key.clone(), json_number(*value));
    }

    // `to_vec_pretty` (2-space indent) is the alloc-only pretty printer; the
    // std-gated custom-indent `Serializer` isn't available under the workspace's
    // `default-features = false` serde_json. This tool owns the file format from
    // here on, so a stable 2-space layout is fine.
    let mut buf = serde_json::to_vec_pretty(&root)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    buf.push(b'\n');
    std::fs::write(path, buf)
}

/// Build a JSON number value from an `f64`, falling back to `null` for
/// non-finite inputs (which should never occur for real timings).
fn json_number(x: f64) -> Value {
    serde_json::Number::from_f64(x).map_or(Value::Null, Value::Number)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_math_on_fixture_sample() {
        // Two samples: 10 iters totalling 1000ns, and 10 iters totalling
        // 3000ns → per-iteration times [100, 300] ns.
        let sample = r#"{"sampling_mode":"Linear","iters":[10.0,10.0],"times":[1000.0,3000.0]}"#;
        let mut per_iter = per_iteration_ns(sample).expect("parse sample");
        per_iter.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(per_iter, vec![100.0, 300.0]);
        assert!((percentile(&per_iter, 50.0) - 100.0).abs() < f64::EPSILON);
        assert!((percentile(&per_iter, 99.0) - 300.0).abs() < f64::EPSILON);
    }

    #[test]
    fn percentile_p99_of_hundred_values() {
        let sorted: Vec<f64> = (1..=100).map(f64::from).collect();
        // Nearest-rank: ceil(0.99*100)=99 → index 98 → value 99.
        assert!((percentile(&sorted, 99.0) - 99.0).abs() < f64::EPSILON);
        // p50: ceil(0.5*100)=50 → index 49 → value 50.
        assert!((percentile(&sorted, 50.0) - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn unit_mapping_and_rounding() {
        // submit: 1500ns → 1.5µs (p50), 2500ns → 2.5µs (p99).
        let m = metrics_for(row("submit"), 1_500.0, 2_500.0);
        assert_eq!(
            m,
            vec![
                ("submit_create_task_p50_us".to_string(), 1.5),
                ("submit_create_task_p99_us".to_string(), 2.5),
            ]
        );
        // query_today: 2_000_000ns → 2.0ms.
        let q = metrics_for(row("query_today"), 0.0, 2_000_000.0);
        assert_eq!(q, vec![("query_today_10k_tasks_p99_ms".to_string(), 2.0)]);
        // sync_session: 1_234_567ns → 1.23ms, under the key that replaced
        // `ws_handshake_p99_ms`.
        let s = metrics_for(row("sync_session"), 0.0, 1_234_567.0);
        assert_eq!(s, vec![("sync_session_p99_ms".to_string(), 1.23)]);
    }

    fn row(dir: &str) -> &'static Bench {
        BENCHES
            .iter()
            .find(|b| b.dir == dir)
            .unwrap_or_else(|| panic!("no BENCHES row for {dir}"))
    }

    /// Every string-literal `bench_function` and `benchmark_group` name in
    /// `benches/*.rs`, with the file it came from.
    fn bench_source_names() -> Vec<(String, String)> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("benches");
        let mut out = Vec::new();
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .expect("read benches/")
            .map(|e| e.expect("dir entry").path())
            .filter(|p| p.extension().is_some_and(|x| x == "rs"))
            .collect();
        files.sort();
        assert!(
            !files.is_empty(),
            "no bench sources under {}",
            dir.display()
        );
        for path in files {
            let text = std::fs::read_to_string(&path).expect("read bench source");
            let file = path.file_name().unwrap().to_string_lossy().into_owned();
            let mut found = 0;
            for call in ["benchmark_group(\"", "bench_function(\""] {
                for (at, _) in text.match_indices(call) {
                    let rest = &text[at + call.len()..];
                    let name = &rest[..rest.find('"').expect("closing quote")];
                    out.push((file.clone(), name.to_string()));
                    found += 1;
                }
            }
            // A bench whose every name is built with `format!` and sits in no
            // literal group would escape both directions of the check below.
            assert!(
                found > 0,
                "{file} names no bench with a string literal; give it a benchmark_group"
            );
        }
        out
    }

    /// The defect #366 found: `sync_session` replaced `ws_handshake`, the
    /// table kept the old name, and nothing noticed. Each direction is a
    /// failure: a bench with no row is never compared, and a row with no bench
    /// reads a directory nothing writes.
    #[test]
    fn every_bench_has_a_row_and_every_row_a_bench() {
        let names = bench_source_names();
        let head = |b: &Bench| b.dir.split('/').next().unwrap_or(b.dir);
        for (file, name) in &names {
            assert!(
                BENCHES.iter().any(|b| head(b) == name),
                "{file}: bench `{name}` has no row in BENCHES, so baseline.rs never reads it"
            );
        }
        for b in BENCHES {
            assert!(
                names.iter().any(|(_, n)| n == head(b)),
                "BENCHES row `{}` names no bench in benches/*.rs",
                b.dir
            );
        }
    }

    /// The keys are the schema of `bench/baseline.json`, so two rows must never
    /// write the same one.
    #[test]
    fn metric_keys_are_unique() {
        let mut keys: Vec<&str> = BENCHES
            .iter()
            .flat_map(|b| b.p50.into_iter().chain([b.p99]))
            .collect();
        let n = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), n, "a metric key appears twice in BENCHES");
    }

    /// Every key in `bench/baseline.json` is one this tool writes, and every
    /// key it writes is recorded for every platform: a key only one side has
    /// is a comparison that silently compares nothing on the other.
    #[test]
    fn committed_baseline_carries_exactly_the_collected_keys() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/baseline.json");
        let doc: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read baseline.json"))
                .expect("baseline.json parses");
        let mut expected: Vec<&str> = BENCHES
            .iter()
            .flat_map(|b| b.p50.into_iter().chain([b.p99]))
            .collect();
        expected.sort_unstable();
        let platforms = doc["platforms"].as_object().expect("platforms object");
        assert!(!platforms.is_empty());
        for (platform, metrics) in platforms {
            let mut keys: Vec<&str> = metrics
                .as_object()
                .expect("platform object")
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            assert_eq!(keys, expected, "{platform}'s keys differ from BENCHES");
        }
    }

    fn write_sample(root: &Path, dir: &str) {
        let path = sample_path(root, dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"iters":[1.0,1.0],"times":[1000.0,3000.0]}"#).unwrap();
    }

    #[test]
    fn a_missing_and_an_unknown_directory_are_both_reported() {
        let root = tempfile::tempdir().unwrap();
        for b in BENCHES.iter().filter(|b| b.dir != "sync_session") {
            write_sample(root.path(), b.dir);
        }
        // The pre-#366 bench name, and a sized run nobody recorded.
        write_sample(root.path(), "ws_handshake");
        write_sample(root.path(), "prime_hlc/scan_1000");
        // Criterion's summary directory is not a bench.
        std::fs::create_dir_all(root.path().join("report")).unwrap();

        let got = collect_metrics(root.path());
        assert_eq!(got.missing, vec!["sync_session"]);
        assert_eq!(got.unknown, vec!["prime_hlc/scan_1000", "ws_handshake"]);
        assert!(got.metrics.iter().all(|(k, _)| k != "sync_session_p99_ms"));
    }

    #[test]
    fn a_complete_run_reports_nothing_missing_or_unknown() {
        let root = tempfile::tempdir().unwrap();
        for b in BENCHES {
            write_sample(root.path(), b.dir);
        }
        let got = collect_metrics(root.path());
        assert!(got.missing.is_empty(), "{:?}", got.missing);
        assert!(got.unknown.is_empty(), "{:?}", got.unknown);
        let n: usize = BENCHES
            .iter()
            .map(|b| 1 + usize::from(b.p50.is_some()))
            .sum();
        assert_eq!(got.metrics.len(), n);
    }

    #[test]
    fn merge_preserves_other_platforms_and_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("baseline.json");
        // A fixture baseline with both platforms nulled + schema fields.
        let fixture = r#"{
    "_comment": "keep me",
    "schema_version": 1,
    "platforms": {
        "darwin-aarch64": {
            "submit_create_task_p50_us": null,
            "sync_session_p99_ms": null
        },
        "linux-x86_64": {
            "submit_create_task_p50_us": null,
            "sync_session_p99_ms": null
        }
    }
}
"#;
        std::fs::write(&path, fixture).unwrap();

        let metrics = vec![
            ("submit_create_task_p50_us".to_string(), 1.23),
            ("sync_session_p99_ms".to_string(), 4.56),
        ];
        merge_into(&path, "linux-x86_64", &metrics).unwrap();

        let out: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // Schema + comment preserved.
        assert_eq!(out["_comment"], "keep me");
        assert_eq!(out["schema_version"], 1);
        // Target platform updated.
        assert_eq!(
            out["platforms"]["linux-x86_64"]["submit_create_task_p50_us"],
            1.23
        );
        assert_eq!(
            out["platforms"]["linux-x86_64"]["sync_session_p99_ms"],
            4.56
        );
        // Other platform untouched (still null).
        assert!(out["platforms"]["darwin-aarch64"]["submit_create_task_p50_us"].is_null());
        assert!(out["platforms"]["darwin-aarch64"]["sync_session_p99_ms"].is_null());
    }
}
