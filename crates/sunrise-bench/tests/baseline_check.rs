//! `baseline --check` fails, by exit status, when a bench it expects did not
//! run, and names the directory it looked for (#366).
//!
//! Driven through the built binary rather than its functions, because the
//! defect was in what the process reported and returned: the missing
//! `sync_session` samples produced no line and a zero exit.

#![allow(clippy::missing_panics_doc)]

use std::path::Path;
use std::process::Command;

/// A Criterion `sample.json` with two samples, so every percentile has a value.
const SAMPLE: &str = r#"{"iters":[1.0,1.0],"times":[1000.0,3000.0]}"#;

fn write_sample(root: &Path, dir: &str) {
    let mut path = root.to_path_buf();
    path.extend(dir.split('/'));
    let path = path.join("new");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("sample.json"), SAMPLE).unwrap();
}

fn run_check(criterion: &Path, baseline: &Path) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_baseline"))
        .arg("--check")
        .arg(criterion)
        .arg(baseline)
        .arg("1000000")
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
        "fts",
        "sync_session",
        "prime_hlc/scan_10000",
        "prime_hlc/indexed_10000",
        "prime_hlc/scan_100000",
        "prime_hlc/indexed_100000",
        "compaction/fold_10000",
        "compaction/snapshot_10000",
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
