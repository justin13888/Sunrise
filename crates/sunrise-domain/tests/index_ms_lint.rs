//! `index_ms` is a storage key, never a comparison
//! (`docs/10-cross-cutting/time.md` §2 rule 3, issue #336).
//!
//! `SunriseTime::index_ms` anchors a floating or all-day value in UTC, up to
//! 14 hours from where any real reader resolves it. A comparison through it —
//! a validator, a view, an overlap, a sort a user sees — is right only for a
//! reader in UTC. Every domain decision resolves in the reader's zone instead
//! (`resolve_in`, `due_in`, `day_in`, `cmp_in`), and this holds the tree to
//! it: outside the places that write or mirror the storage index, no source
//! line names `index_ms` or `index_key`.
//!
//! A grep, deliberately. What it guards is a spelling, not a module graph, so
//! a walk of every `.rs` file under `crates/` is the whole of it; a file the
//! walk sees that no target compiles can only make it stricter.

use std::path::{Path, PathBuf};

/// Where the storage key may be named, each with the reason.
const ALLOWED: &[(&str, &str)] = &[
    (
        "crates/sunrise-domain/src/time.rs",
        "the definition, and `to_parts`, which projects a value onto its columns",
    ),
    (
        "crates/sunrise-domain/src/time/",
        "the wire and sidecar codec of the same value",
    ),
    (
        "crates/sunrise-storage/",
        "storage: the key is what the `*_at_ms` columns hold",
    ),
    (
        "crates/sunrise-e2e/src/lib.rs",
        "projects a task onto the `tasks` row columns to compare two replicas' storage",
    ),
    (
        "crates/sunrise-domain/tests/time_matrix.rs",
        "measures the key against the resolved instant: the bound the prefilter relies on",
    ),
    ("crates/sunrise-domain/tests/index_ms_lint.rs", "this file"),
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/<name> sits two below the workspace root")
        .to_path_buf()
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path
                .file_name()
                .is_some_and(|n| n == "target" || n == "node_modules")
            {
                continue;
            }
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The spellings of a use of the key: a method call or a path to it.
fn names_the_key(line: &str) -> bool {
    let code = line.trim_start();
    if code.starts_with("//") {
        return false;
    }
    ["index_ms", "index_key"]
        .iter()
        .any(|name| code.contains(&format!(".{name}(")) || code.contains(&format!("::{name}")))
}

#[test]
fn no_index_ms_comparison_is_left_outside_storage() {
    let root = workspace_root();
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    rust_files(&root.join("fuzz"), &mut files);
    assert!(
        files.len() > 100,
        "the walk found {} files; it is not looking at the workspace",
        files.len()
    );

    let mut found = Vec::new();
    for file in files {
        let rel = file
            .strip_prefix(&root)
            .expect("under the root")
            .to_string_lossy()
            .replace('\\', "/");
        if ALLOWED.iter().any(|(prefix, _)| rel.starts_with(prefix)) {
            continue;
        }
        let text = std::fs::read_to_string(&file).expect("readable source");
        for (n, line) in text.lines().enumerate() {
            if names_the_key(line) {
                found.push(format!("{rel}:{}: {}", n + 1, line.trim()));
            }
        }
    }
    assert!(
        found.is_empty(),
        "`index_ms` is a storage key, not a comparison (docs/10-cross-cutting/time.md §2): \
         resolve in the reader's zone with `resolve_in`, `due_in`, `day_in` or `cmp_in`, or, \
         for a new storage projection, add the file to ALLOWED with its reason.\n{}",
        found.join("\n")
    );
}

/// The rule bites: a call and a path are both caught, a comment is not.
#[test]
fn the_lint_recognises_a_use_and_ignores_a_mention() {
    assert!(names_the_key("    if a.index_ms() < b.index_ms() {"));
    assert!(names_the_key("    .map(SunriseTime::index_ms)"));
    assert!(names_the_key("    t.index_key().is_none()"));
    assert!(!names_the_key(
        "    /// [`SunriseTime::index_ms`] is a storage key."
    ));
    assert!(!names_the_key("    let window_end_ms = 3;"));
}
