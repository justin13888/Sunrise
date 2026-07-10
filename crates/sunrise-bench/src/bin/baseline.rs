//! `baseline` — parse Criterion's raw sample data and merge computed p50/p99
//! percentiles into `bench/baseline.json` for the current platform.
//!
//! Criterion writes `target/criterion/<bench>/new/sample.json` containing two
//! parallel arrays: `iters[i]` (iterations in sample `i`) and `times[i]` (total
//! nanoseconds for those iterations). The per-iteration time is
//! `times[i] / iters[i]`; percentiles are taken across those per-sample values.
//!
//! Bench → metric-key mapping (with unit conversion):
//! - `submit`       → `submit_create_task_p50_us`, `submit_create_task_p99_us` (ns→µs)
//! - `query_today`  → `query_today_10k_tasks_p99_ms` (ns→ms)
//! - `fts`          → `fts_query_10k_p99_ms` (ns→ms)
//! - `ws_handshake` → `ws_handshake_p99_ms` (ns→ms)
//!
//! The platform key is derived from the build target (`linux-x86_64`,
//! `darwin-aarch64`, …). Other platforms and the schema fields in
//! `baseline.json` are preserved on merge.
//!
//! Usage: `baseline [CRITERION_DIR] [BASELINE_JSON]`
//! (defaults: `target/criterion`, `bench/baseline.json`).

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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let criterion_dir = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "target/criterion".to_string());
    let baseline_path = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| "bench/baseline.json".to_string());

    let platform = platform_key();
    println!("baseline: platform = {platform}");
    println!("baseline: reading Criterion samples from {criterion_dir}");

    let metrics = collect_metrics(Path::new(&criterion_dir));
    if metrics.is_empty() {
        println!("baseline: no sample.json found under {criterion_dir}; run `cargo bench` first.");
        return ExitCode::FAILURE;
    }

    for (key, value) in &metrics {
        println!("  {key} = {value}");
    }

    merge_into(Path::new(&baseline_path), &platform, &metrics)
        .expect("merge metrics into baseline.json");
    println!(
        "baseline: merged {} metric(s) into {baseline_path}",
        metrics.len()
    );
    ExitCode::SUCCESS
}

/// Build-target platform key, e.g. `linux-x86_64` or `darwin-aarch64`.
fn platform_key() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    format!("{os}-{}", std::env::consts::ARCH)
}

/// Walk the known bench directories under `criterion_dir`, compute each bench's
/// percentiles, and return the flattened `(metric_key, value)` list in
/// baseline-schema order.
fn collect_metrics(criterion_dir: &Path) -> Vec<(String, f64)> {
    const BENCHES: &[&str] = &["submit", "query_today", "fts", "ws_handshake"];
    let mut out = Vec::new();
    for bench in BENCHES {
        let sample_path = criterion_dir.join(bench).join("new").join("sample.json");
        let Some((p50_ns, p99_ns)) = read_percentiles(&sample_path) else {
            continue;
        };
        out.extend(metrics_for(bench, p50_ns, p99_ns));
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
fn metrics_for(bench: &str, p50_ns: f64, p99_ns: f64) -> Vec<(String, f64)> {
    let us = |ns: f64| round2(ns / 1_000.0);
    let ms = |ns: f64| round2(ns / 1_000_000.0);
    match bench {
        "submit" => vec![
            ("submit_create_task_p50_us".to_string(), us(p50_ns)),
            ("submit_create_task_p99_us".to_string(), us(p99_ns)),
        ],
        "query_today" => vec![("query_today_10k_tasks_p99_ms".to_string(), ms(p99_ns))],
        "fts" => vec![("fts_query_10k_p99_ms".to_string(), ms(p99_ns))],
        "ws_handshake" => vec![("ws_handshake_p99_ms".to_string(), ms(p99_ns))],
        _ => Vec::new(),
    }
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
        let m = metrics_for("submit", 1_500.0, 2_500.0);
        assert_eq!(
            m,
            vec![
                ("submit_create_task_p50_us".to_string(), 1.5),
                ("submit_create_task_p99_us".to_string(), 2.5),
            ]
        );
        // query_today: 2_000_000ns → 2.0ms.
        let q = metrics_for("query_today", 0.0, 2_000_000.0);
        assert_eq!(q, vec![("query_today_10k_tasks_p99_ms".to_string(), 2.0)]);
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
            "ws_handshake_p99_ms": null
        },
        "linux-x86_64": {
            "submit_create_task_p50_us": null,
            "ws_handshake_p99_ms": null
        }
    }
}
"#;
        std::fs::write(&path, fixture).unwrap();

        let metrics = vec![
            ("submit_create_task_p50_us".to_string(), 1.23),
            ("ws_handshake_p99_ms".to_string(), 4.56),
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
            out["platforms"]["linux-x86_64"]["ws_handshake_p99_ms"],
            4.56
        );
        // Other platform untouched (still null).
        assert!(out["platforms"]["darwin-aarch64"]["submit_create_task_p50_us"].is_null());
        assert!(out["platforms"]["darwin-aarch64"]["ws_handshake_p99_ms"].is_null());
    }
}
