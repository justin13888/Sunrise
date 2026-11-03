//! The manifests and build-config files this repository builds from, and
//! `cargo metadata` over them.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use crate::{rel, workspace_root};

/// Manifests this repository builds that the workspace does not contain.
///
/// Derived from `mise.toml`, which is where every build command in this repo
/// lives: a `--manifest-path` there is the build system naming a crate outside
/// the workspace. Two are named today — `tools/uniffi-bindgen`, by
/// `apple-xcframework`, and `fuzz/`, by `rust-fmt-check`. `fuzz/` is reached
/// by `cargo fuzz --fuzz-dir` rather than by `--manifest-path` when it is
/// *fuzzed*, which this scan would not see; what puts it in the set is that
/// its formatting is checked by manifest path like any other crate's.
///
/// This is deliberately *not* a directory walk. What matters is what the repo
/// builds, and a walk answers a different question — it finds `cargo vendor`
/// output, `tests/fixtures` crates and `--target-dir ./build` leftovers, none
/// of which anyone ships, and firing on those is how a gate gets switched off.
pub(crate) fn extra_manifests() -> Vec<PathBuf> {
    let root = workspace_root();
    let text = std::fs::read_to_string(root.join("mise.toml")).expect("mise.toml is readable");
    let mut found: Vec<PathBuf> = manifest_path_args(&text)
        .into_iter()
        .map(|p| root.join(p))
        .collect();
    found.sort();
    found.dedup();
    found
}

/// Every `--manifest-path` argument in `text`, in source order.
///
/// Shared by [`extra_manifests`] and by
/// [`crate::build_config_gates::no_build_config_names_a_manifest_the_scan_does_not_cover`],
/// so the
/// derivation and the gate that checks it cannot disagree about what a
/// `--manifest-path` says — the same reason an event name is read out of the
/// field parse rather than by a second search of its own.
///
/// # Comments are not invocations
///
/// A whole-line comment that merely *names* the flag is skipped. Every format
/// this scan reads comments with `#` — TOML, YAML, `Dockerfile` and the shell
/// inside a mise task — so one rule covers all four. Without it a reworded
/// sentence in `mise.toml` fails the gate, and the failure names this test
/// rather than the comment, which costs a full `rust-test` cycle to work out
/// (#147). The scan is line-oriented for that reason; a flag and its argument
/// on separate lines are not read, and no invocation here writes one.
///
/// A comment *trailing* real content on the same line is still read. Narrowing
/// further would mean knowing where each format's strings and comments start,
/// which is a parser for four grammars in service of a case nothing has hit.
///
/// # Punctuation is not part of the path
///
/// The argument is the next whitespace-delimited word, so whatever closes the
/// string or the list around it arrives attached to it: `"…fuzz/Cargo.toml",`
/// at the end of a TOML array element used to read as `fuzz/Cargo.toml",` and
/// fail closed on a manifest that is not there (#166). Quotes, a comma and a
/// closing bracket are trimmed from both ends for that reason. It is not a
/// TOML parser — it does not need to be, because no path this repository
/// builds ends in one of those characters, and the flag's argument is a path.
pub(crate) fn manifest_path_args(text: &str) -> Vec<String> {
    const FLAG: &str = "--manifest-path";
    let mut found = Vec::new();
    for line in text.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        let mut from = 0;
        while let Some(at) = line[from..].find(FLAG).map(|r| from + r) {
            from = at + FLAG.len();
            let rest = line[from..].trim_start_matches([' ', '=']);
            let path = rest
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_matches(['"', '\'', ',', ']']);
            if !path.is_empty() {
                found.push(path.to_owned());
            }
        }
    }
    found
}

/// The workspace manifest plus every manifest the build config names beside it.
///
/// The one place this set is built. `metadata_documents` and
/// `derived_manifests` each assembled it independently, and they have to agree
/// or the gate blesses a manifest whose sources nobody reads.
fn all_manifests() -> Vec<PathBuf> {
    let mut set = vec![workspace_root().join("Cargo.toml")];
    set.extend(extra_manifests());
    set.into_iter()
        .map(|p| p.canonicalize().unwrap_or(p))
        .collect()
}

/// [`all_manifests`] as a set, for membership tests.
pub(crate) fn derived_manifests() -> BTreeSet<PathBuf> {
    all_manifests().into_iter().collect()
}

/// Whether this build-config file is a container build.
///
/// Asked by both the general directory rule, which skips these, and the copy
/// rule, which covers them. One predicate rather than two spellings of a
/// filename: the two rules divide the read set between them, and a filter that
/// disagreed with the reader is the exact drift that let a mise task, a
/// composite action and a `WORKDIR` through a check written for them.
pub(crate) fn is_container_build(file: &str) -> bool {
    Path::new(file)
        .file_name()
        .and_then(|f| f.to_str())
        .is_some_and(|f| f.starts_with("Dockerfile"))
}

/// The files that say how this repository is built.
///
/// `mise.toml` is included deliberately: it is where the derivation reads from,
/// so scanning it too means the gate below checks the derivation against
/// itself rather than only against everything else.
pub(crate) fn build_config_files() -> Vec<(String, String)> {
    let root = workspace_root();
    let mut files = Vec::new();
    let mut stack = vec![
        root.join(".github/workflows"),
        // Composite actions run steps of their own. No such directory exists
        // today; it is listed so that adding one is covered on the first
        // commit rather than the first review.
        root.join(".github/actions"),
    ];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(text) = std::fs::read_to_string(&path) {
                files.push((rel(&path, &root), text));
            }
        }
    }
    // Container builds are discovered, not named: a second `Dockerfile.dev`
    // that copied a foreign manifest in and built it was read by nothing, and
    // so covered by nothing.
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || !is_container_build(&entry.file_name().to_string_lossy()) {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path) {
                files.push((rel(&path, &root), text));
            }
        }
    }
    let mise = root.join("mise.toml");
    if let Ok(text) = std::fs::read_to_string(&mise) {
        files.push((rel(&mise, &root), text));
    }
    // Four today: two workflows, the Dockerfile, mise.toml. The floor is
    // against a reader that has stopped finding files, not against deleting a
    // workflow.
    assert!(
        files.len() >= 2,
        "expected to read the build config, found {}: {:?}",
        files.len(),
        files.iter().map(|(f, _)| f).collect::<Vec<_>>()
    );
    files
}

/// `cargo metadata --no-deps` for the workspace and for every manifest the
/// build config names beside it.
pub(crate) fn metadata_documents() -> &'static Vec<serde_json::Value> {
    static META: OnceLock<Vec<serde_json::Value>> = OnceLock::new();
    META.get_or_init(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
        all_manifests()
            .into_iter()
            .map(|manifest| {
                let out = Command::new(&cargo)
                    .args(["metadata", "--no-deps", "--format-version", "1"])
                    .arg("--manifest-path")
                    .arg(&manifest)
                    .output()
                    .unwrap_or_else(|e| {
                        panic!("cargo metadata runs for {}: {e}", manifest.display())
                    });
                assert!(
                    out.status.success(),
                    "cargo metadata failed for {}: {}",
                    manifest.display(),
                    String::from_utf8_lossy(&out.stderr)
                );
                serde_json::from_slice(&out.stdout).expect("cargo metadata emits JSON")
            })
            .collect()
    })
}

/// The target kinds that end up in something a user runs or links.
///
/// `test`, `bench` and `example` are excluded deliberately — a fixture may
/// invent its own event names — and so is `custom-build`: a build script runs
/// at build time and is not part of the artefact whose logs an operator reads.
pub(crate) fn is_shipped_kind(kind: &str) -> bool {
    matches!(
        kind,
        "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro" | "bin"
    )
}
