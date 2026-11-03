//! The file set: the shipped targets, and the files rustc compiled for them,
//! read from the dep-info it wrote.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use crate::manifests::{is_shipped_kind, metadata_documents};
use crate::{rel, workspace_root};

/// An out-of-workspace target that is a build tool rather than shipped code.
///
/// The gate's subject is code whose log output an operator reads. A crate
/// outside the workspace is not automatically that, and `cargo test` never
/// builds one, so there is no dep-info for it — deriving its file set is not
/// possible from here and pretending otherwise would be the old defect again.
///
/// So each is an argued exception with a guard, the same shape as everything
/// else here: code that cannot reach `tracing` cannot emit an event.
pub(crate) struct BuildTool {
    pub(crate) package: &'static str,
    pub(crate) why: &'static str,
}

pub(crate) const BUILD_TOOLS: &[BuildTool] = &[
    BuildTool {
        package: "uniffi-bindgen",
        why: "eleven lines calling `uniffi::uniffi_bindgen_main()`, quarantined from \
              the workspace on purpose and driven only by `mise run apple-xcframework`. \
              Its output is Swift source, not operator logs. Its own manifest says \
              `Build tool only; never shipped`. Those eleven lines are read by the \
              guard rather than described to it.",
    },
    BuildTool {
        package: "sunrise-fuzz",
        why: "six `cargo-fuzz` harnesses, one file per target, each of which decodes \
              attacker bytes through a workspace crate's public API and asserts an \
              invariant. Quarantined from the workspace because `cargo-fuzz` needs a \
              nightly rustc for `-Zsanitizer=address` and this one is pinned to \
              1.91.1 (ADR-0026). Its output is a libFuzzer corpus and, on a finding, \
              a reproducer file — never operator logs. Each of the six files is read \
              by the guard, and the code they *drive* is workspace source, scanned \
              from dep-info like everything else.",
    },
];

/// Every shipped target, as `(package, src_path, in the workspace)`.
pub(crate) fn shipped_targets() -> Vec<(String, PathBuf, bool)> {
    let mut out = Vec::new();
    for (doc, pkg) in metadata_documents().iter().flat_map(|d| {
        d["packages"]
            .as_array()
            .expect("packages")
            .iter()
            .map(move |p| (d, p))
    }) {
        let members = doc["workspace_members"]
            .as_array()
            .expect("metadata lists workspace members");
        let in_workspace = members.iter().any(|m| m == &pkg["id"])
            && doc["workspace_root"].as_str() == Some(&workspace_root().display().to_string());
        let name = pkg["name"].as_str().expect("a package name").to_owned();
        for target in pkg["targets"].as_array().expect("a package lists targets") {
            let shipped = target["kind"]
                .as_array()
                .expect("a target lists kinds")
                .iter()
                .filter_map(serde_json::Value::as_str)
                .any(is_shipped_kind);
            if shipped {
                out.push((
                    name.clone(),
                    PathBuf::from(target["src_path"].as_str().expect("a src_path")),
                    in_workspace,
                ));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Every workspace package, as `(name, the directory its manifest sits in)`.
///
/// The manifest directory rather than a target's `src_path`, because a `bin`
/// target lives at `src/bin/<name>.rs` and its parent is not the crate's source
/// root. Out-of-workspace packages are excluded: `cargo test` never builds one,
/// so there is nothing to be complete against.
fn workspace_package_dirs() -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for doc in metadata_documents() {
        if doc["workspace_root"].as_str() != Some(&workspace_root().display().to_string()) {
            continue;
        }
        let members = doc["workspace_members"]
            .as_array()
            .expect("metadata lists workspace members");
        for pkg in doc["packages"].as_array().expect("packages") {
            if !members.iter().any(|m| m == &pkg["id"]) {
                continue;
            }
            let manifest = PathBuf::from(pkg["manifest_path"].as_str().expect("a manifest_path"));
            if let Some(dir) = manifest.parent() {
                out.push((
                    pkg["name"].as_str().expect("a package name").to_owned(),
                    dir.to_path_buf(),
                ));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Every `.rs` file git tracks under a workspace package's `src/` directory.
///
/// The set the scan has to be *complete against*, and the answer to the floor
/// this test used to carry. `>= 50` against a measured 185 tolerated losing
/// 72% of the file set — precisely the failure #84 describes — and any other
/// constant would be the same bet at a different odds. This is a derivation
/// instead: git says which files exist, cargo's own records say which were
/// compiled, and a file in the first set and not the second is named.
///
/// Note the direction. Every tracked `src/` file must be covered; a covered
/// file that git does not track is fine and expected —
/// `sunrise-relay-client`'s generated `api.rs` lives under `target/` and is in
/// the set exactly as it should be. And a source reached by `#[path]` from
/// outside `src/` is covered by the dep-info union without being asserted here,
/// which is the honest statement: this is a floor derived from the tree, not a
/// claim of exactness.
pub(crate) fn tracked_package_sources() -> Vec<String> {
    let root = workspace_root();
    let out = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["ls-files", "-z", "--", "*.rs"])
        .output()
        .expect("git ls-files runs");
    assert!(
        out.status.success(),
        "git ls-files failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let listed = String::from_utf8(out.stdout).expect("git ls-files emits UTF-8");
    let prefixes: Vec<String> = workspace_package_dirs()
        .into_iter()
        .map(|(_, dir)| format!("{}/src/", rel(&dir, &root)))
        .collect();
    let mut found: Vec<String> = listed
        .split('\0')
        .filter(|f| !f.is_empty())
        .filter(|f| prefixes.iter().any(|p| f.starts_with(p.as_str())))
        .map(str::to_owned)
        .collect();
    found.sort();
    found
}

/// Cargo's `target/` directory, from cargo rather than by assumption.
fn target_directory() -> PathBuf {
    PathBuf::from(
        metadata_documents()[0]["target_directory"]
            .as_str()
            .expect("metadata names the target directory"),
    )
}

/// One dep-info file: where it is, the sources rustc read, and its timestamp.
struct DepInfo {
    /// The `.d` itself, so a diagnostic can name the record it is complaining
    /// about rather than only the target the record belongs to.
    from: PathBuf,
    deps: Vec<PathBuf>,
    written: std::time::SystemTime,
}

/// Split one dependency line into paths, honouring make's `\ ` escape.
///
/// Splitting on whitespace loses a path containing a space: cargo writes it as
/// `a\ b`, and two bogus tokens then come back, each reported as "not on disk".
/// No such path exists in this repository, and that is exactly why it is worth
/// handling here — the failure would arrive as a mysterious red gate on somebody
/// else's checkout rather than as a defect anyone could see.
fn split_dep_line(rhs: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for ch in rhs.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Read one `.d`, returning every path on its dependency lines.
fn read_dep_info(path: &Path) -> Option<DepInfo> {
    let text = std::fs::read_to_string(path).ok()?;
    let written = std::fs::metadata(path).ok()?.modified().ok()?;
    let root = workspace_root();
    let mut deps = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        let Some((_, rhs)) = line.split_once(':') else {
            continue;
        };
        for token in split_dep_line(rhs) {
            let p = Path::new(&token);
            deps.push(if p.is_absolute() {
                p.to_path_buf()
            } else {
                root.join(p)
            });
        }
    }
    (!deps.is_empty()).then_some(DepInfo {
        from: path.to_path_buf(),
        deps,
        written,
    })
}

/// Whether this `.d` is the per-unit record rustc wrote, rather than the
/// uplifted copy cargo writes beside the finished artefact.
///
/// The distinction is the whole of #84's second half. Only the per-unit file —
/// always under a `deps/` directory — begins its dependency line with the crate
/// root rustc was given. The uplifted `target/debug/<name>.d` lists the *whole
/// transitive source closure* in topological order, so its first path is
/// whatever crate happened to sort first: `target/debug/libsunrise_bench.d`
/// names 95 files across 10 crates, and two such files begin with a path that is
/// a real target's `src_path`. Indexing those by their first path filed a
/// ten-crate closure under one target, and being newest they won the selection —
/// which is how `crates/sunrise-sync/src/sse/mod.rs` came to be covered by a
/// benchmark crate's artefact record rather than by sunrise-sync's own.
///
/// `clippy-driver` also writes `.d` files whose first path is `clippy.toml`.
/// Those are excluded by [`indexable_dep_info`] below rather than here, because
/// the reason is different: they are per-unit records, they just do not start
/// with a source file.
fn is_per_unit_dep_info(path: &Path) -> bool {
    path.parent()
        .and_then(|d| d.file_name())
        .and_then(|d| d.to_str())
        == Some("deps")
}

/// The crate root a `.d` records, when it records one at all.
///
/// `None` for a `clippy-driver` record, whose dependency line starts with
/// `clippy.toml`. Left in the index those would be filed under a path no target
/// has, which is harmless, but reading them costs nothing to skip and keeps the
/// invariant this index depends on — first path is a Rust source — true rather
/// than nearly true.
fn indexable_dep_info(info: &DepInfo) -> Option<PathBuf> {
    let first = info.deps.first()?;
    (first.extension().is_some_and(|e| e == "rs")).then(|| first.clone())
}

/// Every source a shipped workspace target compiled, and everything that made
/// that question unanswerable.
///
/// This is the whole of the file set. It is not a parse of Rust's module
/// grammar — that parser shipped two Criticals, silently skipping `mod r#type;`
/// and resolving `#[path]` against the wrong directory — but a lookup of what
/// the compiler recorded it had read. It covers `#[path]`, raw identifiers,
/// `include!` and generated code under `target/` for the same reason and at the
/// same cost: rustc already knew.
fn dep_info_sources() -> (BTreeMap<String, String>, Vec<String>) {
    let root = workspace_root();
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    let mut problems: Vec<String> = Vec::new();

    // Index every *per-unit* dep-info by the first source on its dependency
    // line, which for those and only those is the crate root rustc was given.
    let mut by_root: BTreeMap<PathBuf, Vec<DepInfo>> = BTreeMap::new();
    let mut clippy_only = true;
    let mut stack = vec![target_directory()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "d") && is_per_unit_dep_info(&path) {
                if let Some(info) = read_dep_info(&path) {
                    if let Some(first) = indexable_dep_info(&info) {
                        clippy_only = false;
                        by_root.entry(first).or_default().push(info);
                    }
                }
            }
        }
    }

    for (package, src_path, in_workspace) in shipped_targets() {
        if !in_workspace {
            // Covered by BUILD_TOOLS and its guard, checked separately.
            continue;
        }
        let canonical = src_path.canonicalize().unwrap_or_else(|_| src_path.clone());
        // *Every* dep-info for this root, not the newest one. Cargo keeps a `.d`
        // per feature set, profile and target platform it has built, and each
        // describes a different slice of the crate: a module behind
        // `#[cfg(feature = "…")]` is absent from every record of a build that
        // did not enable it. Taking the newest made the covered set a function
        // of which cargo command ran last — same tree, three answers — and a
        // file no selected `.d` named was checked by nothing. The union is a
        // superset of every configuration built, which is the safe direction:
        // it can scan a file some configuration does not compile, and it cannot
        // silently stop scanning one that ships.
        //
        // Recency still has a job, and it is a different one. It decides
        // *currency*, below: the newest record is the one a stale-build check
        // has to be measured against. Conflating the two questions is what #84
        // reported.
        let mut infos: Vec<&DepInfo> = by_root
            .iter()
            .filter(|(k, _)| k.canonicalize().unwrap_or_else(|_| (*k).clone()) == canonical)
            .flat_map(|(_, v)| v.iter())
            .collect();
        infos.sort_by_key(|i| i.written);
        let Some(newest) = infos.last().copied() else {
            problems.push(format!(
                "{package}: no per-unit dep-info for {}. Build the workspace first — \
                 `mise run log-fields` does that build and then this gate — because \
                 it reads what the compiler recorded rather than parsing the module \
                 tree.{}",
                rel(&src_path, &root),
                if clippy_only {
                    " Nothing under `target/` carries a dep-info naming a Rust source \
                     at all, which is what a `target/` populated only by `cargo clippy` \
                     looks like: clippy-driver writes `.d` files whose first path is \
                     `clippy.toml`."
                } else {
                    ""
                }
            ));
            continue;
        };

        for info in &infos {
            for dep in &info.deps {
                if dep.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                if !dep.exists() {
                    // Expected of an older record: the file was deleted after
                    // that build. Only `newest` is held to naming files that
                    // still exist, below.
                    continue;
                }
                if let Ok(text) = std::fs::read_to_string(dep) {
                    files.insert(rel(dep, &root), text);
                }
            }
        }

        // Currency, against the newest record only. Every entry, not only the
        // `.rs` ones: an `include_str!`ed fixture is a compile-time input like
        // any other, and a change to one that no record has seen means the set
        // may be missing whatever else that build would have read.
        for dep in &newest.deps {
            let Ok(meta) = std::fs::metadata(dep) else {
                problems.push(format!(
                    "{package}: {} names {} but it is not on disk",
                    rel(&newest.from, &root),
                    rel(dep, &root)
                ));
                continue;
            };
            if meta.modified().is_ok_and(|m| m > newest.written) {
                problems.push(format!(
                    "{package}: {} is newer than the newest dep-info naming it, so the \
                     file set may be missing something. Rebuild the workspace",
                    rel(dep, &root)
                ));
            }
        }
    }
    (files, problems)
}

pub(crate) fn sources() -> &'static (BTreeMap<String, String>, Vec<String>) {
    static SOURCES: OnceLock<(BTreeMap<String, String>, Vec<String>)> = OnceLock::new();
    SOURCES.get_or_init(dep_info_sources)
}

/// Every shipped source, as `(path relative to the root, text)`.
pub(crate) fn shipped_sources() -> Vec<(String, String)> {
    sources()
        .0
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}
