//! `baseline --check` fails, by exit status, when a bench it expects did not
//! run, and names the directory it looked for (#366). Its verdict against the
//! recorded baseline is held here too (#731): a metric over the tolerance
//! fails naming its key, a baseline it cannot read or parse fails, and a
//! metric with nothing recorded is skipped rather than failed.
//!
//! Driven through the built binary rather than its functions, because the
//! defect was in what the process reported and returned: the missing
//! `sync_session` samples produced no line and a zero exit.

#![allow(clippy::missing_panics_doc)]

use std::path::Path;
use std::process::Command;

/// A Criterion `sample.json` with two samples, so every percentile has a value.
/// Per-iteration times are 1000 ns and 3000 ns, so `submit`'s p50 is 1.0 µs
/// and its p99 3.0 µs.
const SAMPLE: &str = r#"{"iters":[1.0,1.0],"times":[1000.0,3000.0]}"#;

/// [`SAMPLE`] with every time doubled: `submit`'s p99 becomes 6.0 µs.
const SAMPLE_2X: &str = r#"{"iters":[1.0,1.0],"times":[2000.0,6000.0]}"#;

/// The p99 key `submit` writes, and its value under [`SAMPLE`].
const SUBMIT_P99: &str = "submit_create_task_p99_us";
const SUBMIT_P99_AT_SAMPLE: f64 = 3.0;

/// The tolerance the verdict tests run at: a 2x sample is +100%, far over it,
/// and an unchanged one is 0%, well inside it.
const TOLERANCE_PCT: &str = "50";

fn write_sample(root: &Path, dir: &str) {
    write_sample_with(root, dir, SAMPLE);
}

fn write_sample_with(root: &Path, dir: &str, sample: &str) {
    let mut path = root.to_path_buf();
    path.extend(dir.split('/'));
    let path = path.join("new");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("sample.json"), sample).unwrap();
}

/// The key the binary records this platform under, derived the way its
/// `platform_key` does: the OS, with `macos` written `darwin`, then the arch.
fn platform_key() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    format!("{os}-{}", std::env::consts::ARCH)
}

fn run_check(criterion: &Path, baseline: &Path) -> (bool, String) {
    run_check_at(criterion, baseline, "1000000")
}

fn run_check_at(criterion: &Path, baseline: &Path, tolerance_pct: &str) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_baseline"))
        .arg("--check")
        .arg(criterion)
        .arg(baseline)
        .arg(tolerance_pct)
        .output()
        .expect("run baseline");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// The directories the binary's `BENCHES` table reads, restated. A row added
/// there and not here fails the complete-run test below, reported missing.
fn expected_dirs() -> Vec<&'static str> {
    vec![
        "submit",
        "query_today",
        "query_upcoming",
        "fts/query_10000",
        "sync_session",
        "prime_hlc/scan_10000",
        "prime_hlc/indexed_10000",
        "prime_hlc/scan_100000",
        "prime_hlc/indexed_100000",
        "compaction/fold_10000",
        "compaction/snapshot_10000",
        "rebuild/refold_10000",
        "plan_preview/ripple",
        "plan_preview/cross_day",
        "plan_preview/auto_place",
    ]
}

fn baseline_file(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("baseline.json");
    std::fs::write(&path, r#"{"platforms":{},"schema_version":1}"#).unwrap();
    path
}

#[test]
fn a_missing_bench_directory_fails_the_check_and_is_named() {
    let criterion = tempfile::tempdir().unwrap();
    for dir in expected_dirs().into_iter().filter(|d| *d != "sync_session") {
        write_sample(criterion.path(), dir);
    }
    let baseline = baseline_file(criterion.path());

    let (ok, stdout) = run_check(criterion.path(), &baseline);
    assert!(!ok, "--check passed with sync_session missing:\n{stdout}");
    assert!(
        stdout.contains("missing") && stdout.contains("sync_session/new/sample.json"),
        "the missing directory is not named:\n{stdout}"
    );
}

#[test]
fn an_unknown_bench_directory_fails_the_check_and_is_named() {
    let criterion = tempfile::tempdir().unwrap();
    for dir in expected_dirs() {
        write_sample(criterion.path(), dir);
    }
    write_sample(criterion.path(), "ws_handshake");
    let baseline = baseline_file(criterion.path());

    let (ok, stdout) = run_check(criterion.path(), &baseline);
    assert!(!ok, "--check passed with an unknown directory:\n{stdout}");
    assert!(
        stdout.contains("unknown bench directory") && stdout.contains("ws_handshake"),
        "the unknown directory is not named:\n{stdout}"
    );
}

#[test]
fn a_complete_run_with_no_recorded_baseline_passes() {
    let criterion = tempfile::tempdir().unwrap();
    for dir in expected_dirs() {
        write_sample(criterion.path(), dir);
    }
    let baseline = baseline_file(criterion.path());

    let (ok, stdout) = run_check(criterion.path(), &baseline);
    assert!(ok, "a complete run failed the check:\n{stdout}");
    assert!(!stdout.contains("missing"), "{stdout}");
    assert!(!stdout.contains("unknown bench directory"), "{stdout}");
}

/// A complete run with `submit` written from `submit_sample` and every other
/// directory from [`SAMPLE`], so the only thing the verdict can turn on is the
/// baseline.
fn complete_run(submit_sample: &str) -> tempfile::TempDir {
    let criterion = tempfile::tempdir().unwrap();
    for dir in expected_dirs() {
        let sample = if dir == "submit" {
            submit_sample
        } else {
            SAMPLE
        };
        write_sample_with(criterion.path(), dir, sample);
    }
    criterion
}

/// A baseline recording `metrics` (a JSON object body) for this platform.
fn recorded_baseline(dir: &Path, metrics: &str) -> std::path::PathBuf {
    let path = dir.join("baseline.json");
    let platform = platform_key();
    std::fs::write(
        &path,
        format!(r#"{{"platforms":{{"{platform}":{{{metrics}}}}},"schema_version":1}}"#),
    )
    .unwrap();
    path
}

#[test]
fn a_metric_twice_its_recorded_value_fails_the_check_and_is_named() {
    let criterion = complete_run(SAMPLE_2X);
    let baseline = recorded_baseline(
        criterion.path(),
        &format!(r#""{SUBMIT_P99}":{SUBMIT_P99_AT_SAMPLE}"#),
    );

    let (ok, stdout) = run_check_at(criterion.path(), &baseline, TOLERANCE_PCT);
    assert!(!ok, "--check passed a 2x regression:\n{stdout}");
    assert!(
        stdout.contains(&format!("::error::{SUBMIT_P99} regressed +100.0%")),
        "the regressed key is not named:\n{stdout}"
    );
}

#[test]
fn a_metric_at_its_recorded_value_passes_the_check() {
    let criterion = complete_run(SAMPLE);
    let baseline = recorded_baseline(
        criterion.path(),
        &format!(r#""{SUBMIT_P99}":{SUBMIT_P99_AT_SAMPLE}"#),
    );

    let (ok, stdout) = run_check_at(criterion.path(), &baseline, TOLERANCE_PCT);
    assert!(ok, "--check failed an unchanged metric:\n{stdout}");
    assert!(
        stdout.contains("1 metric(s) within"),
        "the metric was not compared:\n{stdout}"
    );
    assert!(!stdout.contains("::error::"), "{stdout}");
}

#[test]
fn a_null_or_absent_recorded_metric_is_skipped_not_failed() {
    // `submit`'s p99 is recorded as null and its p50 not at all, while the
    // sample has doubled: neither has anything to compare against.
    let criterion = complete_run(SAMPLE_2X);
    let baseline = recorded_baseline(criterion.path(), &format!(r#""{SUBMIT_P99}":null"#));

    let (ok, stdout) = run_check_at(criterion.path(), &baseline, TOLERANCE_PCT);
    assert!(ok, "--check failed with nothing recorded:\n{stdout}");
    let platform = platform_key();
    for key in [SUBMIT_P99, "submit_create_task_p50_us"] {
        assert!(
            stdout.contains(&format!("{key}: no baseline for {platform}")),
            "{key} was not reported skipped:\n{stdout}"
        );
    }
    assert!(stdout.contains("nothing comparable"), "{stdout}");
    assert!(!stdout.contains("::error::"), "{stdout}");
}

#[test]
fn an_invalid_json_baseline_fails_the_check() {
    let criterion = complete_run(SAMPLE);
    let baseline = criterion.path().join("baseline.json");
    std::fs::write(&baseline, r#"{"platforms":"#).unwrap();

    let (ok, stdout) = run_check_at(criterion.path(), &baseline, TOLERANCE_PCT);
    assert!(!ok, "--check passed an invalid baseline:\n{stdout}");
    assert!(stdout.contains("is not valid JSON"), "{stdout}");
}

#[test]
fn an_unreadable_baseline_fails_the_check() {
    let criterion = complete_run(SAMPLE);
    let baseline = criterion.path().join("no-such-baseline.json");

    let (ok, stdout) = run_check_at(criterion.path(), &baseline, TOLERANCE_PCT);
    assert!(!ok, "--check passed with no baseline to read:\n{stdout}");
    assert!(stdout.contains("cannot read"), "{stdout}");
}
