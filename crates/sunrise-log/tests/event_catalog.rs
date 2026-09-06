//! The event catalogue and the log-field vocabulary, enforced against the files
//! the compiler actually builds.
//!
//! `docs/10-cross-cutting/logging.md` §3 says a new `ev` value "requires a
//! one-line entry in log-events.md so analysts can grep for meaning", and §11
//! asked for a per-package snapshot test to hold that line. No such test
//! existed, and under `tracing` a snapshot of a hand-maintained constant list
//! would only prove the list matches itself.
//!
//! This checks the two things that actually matter:
//!
//! - every event name emitted from shipped code is grammatical and catalogued;
//! - every *field name* those events carry is on the redaction allowlist, so
//!   the record the catalogue promises can reach a subscriber at all.
//!
//! # Why the file set is derived rather than walked
//!
//! Earlier versions walked `crates/*/src`, then every manifest directory, and
//! pinned the result. Both were assertions about where code lives, and code
//! does not have to live there: a `#[path = "../generated/planted.rs"]` module
//! or an `include!` pulls a file into a shipped crate from anywhere, and
//! `crates/sunrise-relay-client/src/lib.rs` already does exactly that with
//! eleven thousand generated lines out of `target/`. A directory walk cannot
//! see them, and pinning the walk's output only pinned the blind spot. It also
//! fired on `cargo vendor`, on a `tests/fixtures` crate and on
//! `--target-dir ./build` — and a gate that fires on ordinary work is a gate
//! someone switches off.
//!
//! So the set is taken from `cargo metadata`, which is the compiler's own view:
//! every shipped target and its root source file. From each root the **module
//! graph** is followed through `mod` and `#[path]`. Tests, benches, examples
//! and build scripts are excluded, which is the rule this file has always had —
//! a fixture inventing its own event name does not have to be catalogued.
//!
//! # What this gate does not see
//!
//! Enumerated rather than implied, because the failure mode of every earlier
//! version of this file was a totality claim nobody had written down and so
//! nobody could check:
//!
//! - **Anything that arrives by macro expansion.** The walk reads source, not
//!   expansions. A `macro_rules!` written here is covered, because its body is
//!   source like any other — but an *external* macro that expands to an
//!   `include!`, or to a `tracing::` call of its own, is invisible. A literal
//!   `include!` is refused; one arriving through a macro is not seen at all.
//! - **Code that is not part of a shipped target.** Tests, benches, examples
//!   and build scripts are excluded from the file set on purpose: a fixture may
//!   invent its own event names, and a `build.rs` runs at build time rather
//!   than in the artefact whose logs an operator reads. Unit tests *inside* a
//!   shipped `src/` file are scanned, because they are not separable from it.
//! - **Spans.** `redact.rs` argues deliberately that spans are ungated, and
//!   names a second span site as the trigger to revisit — but nothing here
//!   detects a second span site, so that trigger is a note, not an alarm. One
//!   span exists today (`api/observe.rs`, `method` and `endpoint`, both
//!   allowlisted and server-derived).
//! - **A third-party action compiling a foreign manifest.** Such an
//!   invocation lives in that action's repository, not this one, and nothing
//!   here can reach it. What holds instead is that the `uses:` list is short
//!   and reviewed.
//! - **A build reached through a script.** `.github/scripts/*` are invoked
//!   bare today. Following them properly means following arbitrary shell and
//!   Python; following them partly would report coverage that does not exist,
//!   which is the exact defect this file keeps having to remove. So they are
//!   not followed at all, and this paragraph is the record of that choice.
//!
//! Everything else the gate refuses rather than skips. Where the two are hard
//! to tell apart, prefer reducing what this file *claims* over extending what
//! it does.
//!
//! # Why both halves fail closed
//!
//! Every gate here reports what it cannot analyse rather than skipping it.
//! Unread syntax and unreachable files are the same defect wearing different
//! clothes: something is compiled into a shipped binary that no gate has seen.
//! An `include!`, an unresolvable `#[path]`, a braced or bracketed invocation, a
//! quoted or raw-identifier field name, a macro reached under another name —
//! each is a named failure telling the author what to write instead, or to
//! decide deliberately that this one is fine.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root resolves")
}

fn rel(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Manifests this repository builds that the workspace does not contain.
///
/// Derived from `mise.toml`, which is where every build command in this repo
/// lives: a `--manifest-path` there is the build system naming a crate outside
/// the workspace, and `tools/uniffi-bindgen` is the one it names today.
///
/// This is deliberately *not* a directory walk. What matters is what the repo
/// builds, and a walk answers a different question — it finds `cargo vendor`
/// output, `tests/fixtures` crates and `--target-dir ./build` leftovers, none
/// of which anyone ships, and firing on those is how a gate gets switched off.
fn extra_manifests() -> Vec<PathBuf> {
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
/// [`no_build_config_names_a_manifest_the_scan_does_not_cover`], so the
/// derivation and the gate that checks it cannot disagree about what a
/// `--manifest-path` says — the same reason an event name is read out of the
/// field parse rather than by a second search of its own.
fn manifest_path_args(text: &str) -> Vec<String> {
    const FLAG: &str = "--manifest-path";
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find(FLAG).map(|r| from + r) {
        from = at + FLAG.len();
        let rest = text[from..].trim_start_matches([' ', '=']);
        let path = rest
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_matches(['"', '\'']);
        if !path.is_empty() {
            found.push(path.to_owned());
        }
    }
    found
}

/// The workspace manifest plus every manifest the build config names beside it
/// — the complete set these gates derive their file list from.
fn derived_manifests() -> BTreeSet<PathBuf> {
    let mut set = vec![workspace_root().join("Cargo.toml")];
    set.extend(extra_manifests());
    set.into_iter()
        .map(|p| p.canonicalize().unwrap_or(p))
        .collect()
}

/// Whether this build-config file is a container build.
///
/// Asked by both the general directory rule, which skips these, and the copy
/// rule, which covers them. One predicate rather than two spellings of a
/// filename: the two rules divide the read set between them, and a filter that
/// disagreed with the reader is the exact drift that let a mise task, a
/// composite action and a `WORKDIR` through a check written for them.
fn is_container_build(file: &str) -> bool {
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
fn build_config_files() -> Vec<(String, String)> {
    let root = workspace_root();
    let mut files = Vec::new();
    let mut stack = vec![
        root.join(".github/workflows"),
        // Composite actions run steps of their own, and were not read at all.
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
fn metadata_documents() -> &'static Vec<serde_json::Value> {
    static META: OnceLock<Vec<serde_json::Value>> = OnceLock::new();
    META.get_or_init(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
        let mut manifests = vec![workspace_root().join("Cargo.toml")];
        manifests.extend(extra_manifests());
        manifests
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

/// Every package across every manifest this repo builds.
fn packages() -> Vec<&'static serde_json::Value> {
    metadata_documents()
        .iter()
        .flat_map(|doc| {
            doc["packages"]
                .as_array()
                .expect("metadata lists packages")
                .iter()
        })
        .collect()
}

/// The target kinds that end up in something a user runs or links.
///
/// `test`, `bench` and `example` are excluded deliberately — a fixture may
/// invent its own event names — and so is `custom-build`: a build script runs
/// at build time and is not part of the artefact whose logs an operator reads.
fn is_shipped_kind(kind: &str) -> bool {
    matches!(
        kind,
        "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro" | "bin"
    )
}

/// Every shipped target's root source file, from the compiler's own view.
fn shipped_target_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for pkg in packages() {
        for target in pkg["targets"].as_array().expect("a package lists targets") {
            let shipped = target["kind"]
                .as_array()
                .expect("a target lists kinds")
                .iter()
                .filter_map(serde_json::Value::as_str)
                .any(is_shipped_kind);
            if shipped {
                roots.push(PathBuf::from(
                    target["src_path"]
                        .as_str()
                        .expect("a target has a src_path"),
                ));
            }
        }
    }
    roots.sort();
    roots.dedup();
    roots
}

// ---------------------------------------------------------------------------
// Source normalisation
// ---------------------------------------------------------------------------

/// One source in two aligned forms.
///
/// Both are byte-for-byte the same length as the original, so an offset found
/// in one indexes the other. `structural` has comments, `#[cfg(test)]` items
/// and string *contents* blanked, which is what makes brace matching, argument
/// splitting and word search safe. `literal` blanks the same regions but keeps
/// string contents, which is where an event name is read from.
struct Source {
    structural: String,
    literal: String,
}

/// Comments blanked, and string contents blanked when `blank_strings`.
///
/// One long function on purpose: it is a single lexer state machine, and
/// splitting it into arms that each need the cursor and the mode would make it
/// harder to check, not easier.
///
/// Byte length is preserved exactly: a multi-byte char that is blanked becomes
/// that many spaces. Both halves of a [`Source`] therefore share one offset
/// space.
#[allow(clippy::too_many_lines)]
fn blank(text: &str, blank_strings: bool) -> String {
    let src: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let push_blank = |out: &mut String, c: char| {
        if c == '\n' {
            out.push('\n');
        } else {
            for _ in 0..c.len_utf8() {
                out.push(' ');
            }
        }
    };
    while i < src.len() {
        let c = src[i].1;
        // Raw string: `r`, zero or more `#`, then a quote.
        if c == 'r' {
            let mut h = i + 1;
            while h < src.len() && src[h].1 == '#' {
                h += 1;
            }
            if h < src.len() && src[h].1 == '"' {
                let hashes = h - i - 1;
                for (_, ch) in &src[i..=h] {
                    out.push(*ch);
                }
                i = h + 1;
                while i < src.len() {
                    if src[i].1 == '"' {
                        let mut n = 0;
                        while n < hashes && src.get(i + 1 + n).is_some_and(|c| c.1 == '#') {
                            n += 1;
                        }
                        if n == hashes {
                            out.push('"');
                            (0..hashes).for_each(|_| out.push('#'));
                            i += 1 + hashes;
                            break;
                        }
                    }
                    if blank_strings {
                        push_blank(&mut out, src[i].1);
                    } else {
                        out.push(src[i].1);
                    }
                    i += 1;
                }
                continue;
            }
        }
        if c == '"' {
            out.push('"');
            i += 1;
            while i < src.len() && src[i].1 != '"' {
                if src[i].1 == '\\' {
                    if blank_strings {
                        push_blank(&mut out, src[i].1);
                    } else {
                        out.push(src[i].1);
                    }
                    i += 1;
                }
                if i < src.len() {
                    if blank_strings {
                        push_blank(&mut out, src[i].1);
                    } else {
                        out.push(src[i].1);
                    }
                    i += 1;
                }
            }
            if i < src.len() {
                out.push('"');
                i += 1;
            }
            continue;
        }
        // A char literal (`'x'`, `'\n'`, `'\''`) can hold a quote or a paren; a
        // lifetime (`'a`, `'static`) cannot, and must be copied through.
        if c == '\'' {
            let end = if src.get(i + 1).is_some_and(|c| c.1 == '\\') {
                src.get(i + 3).filter(|c| c.1 == '\'').map(|_| i + 3)
            } else {
                src.get(i + 2).filter(|c| c.1 == '\'').map(|_| i + 2)
            };
            if let Some(end) = end {
                out.push('\'');
                for (_, ch) in &src[i + 1..end] {
                    push_blank(&mut out, *ch);
                }
                out.push('\'');
                i = end + 1;
                continue;
            }
        }
        if c == '/' && src.get(i + 1).is_some_and(|c| c.1 == '/') {
            while i < src.len() && src[i].1 != '\n' {
                push_blank(&mut out, src[i].1);
                i += 1;
            }
            continue;
        }
        if c == '/' && src.get(i + 1).is_some_and(|c| c.1 == '*') {
            while i + 1 < src.len() && !(src[i].1 == '*' && src[i + 1].1 == '/') {
                push_blank(&mut out, src[i].1);
                i += 1;
            }
            if i < src.len() {
                push_blank(&mut out, src[i].1);
                i += 1;
            }
            if i < src.len() {
                push_blank(&mut out, src[i].1);
                i += 1;
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

fn normalise(text: &str) -> Source {
    let structural = blank(text, true);
    let literal = blank(text, false);
    assert_eq!(structural.len(), text.len(), "blanking changed byte length");
    assert_eq!(literal.len(), text.len(), "blanking changed byte length");
    Source {
        structural,
        literal,
    }
}

// ---------------------------------------------------------------------------
// The module graph
// ---------------------------------------------------------------------------

/// Something a shipped crate compiles that the walk cannot follow.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Unresolved {
    file: String,
    what: String,
}

/// `include!`s that cannot be resolved statically, with the argument for each.
///
/// The gate fails closed on `include!`, so an entry here is a deliberate,
/// argued exception — and every one carries a guard that fails if the argument
/// stops holding. This is the same shape as `field.rs`'s `NOT_ENTITY_IDS`: an
/// exception nobody can widen quietly.
struct AllowedInclude {
    /// The file holding the `include!`, relative to the workspace root.
    file: &'static str,
    /// Why the walk cannot follow it.
    why: &'static str,
    /// The package whose manifest must not gain a `tracing` dependency. Code
    /// that cannot reach `tracing` cannot emit an event, so this is what makes
    /// "the generated file logs nothing" an enforced fact rather than a
    /// snapshot of today.
    ///
    /// # When this guard fails, resolve the include — never relax the guard
    ///
    /// The guard is a *dependency* check standing in for a *content* claim.
    /// The moment the dependency appears, the claim it was proxying for is no
    /// longer supported by anything, so relaxing the guard would leave an
    /// exception whose justification had expired — which is the exact defect
    /// this whole file exists to catch, sitting inside the gate itself.
    ///
    /// The fix is to make the include resolvable: run the build script from
    /// the gate and read its output, or have spargen emit into the source tree
    /// where the module walk can follow it. Either way the generated code gets
    /// read like everything else, and the exception disappears rather than
    /// being widened.
    guarded_package: &'static str,
}

const ALLOWED_INCLUDES: &[AllowedInclude] = &[AllowedInclude {
    file: "crates/sunrise-relay-client/src/lib.rs",
    why: "`include!(concat!(env!(\"OUT_DIR\"), \"/api.rs\"))` — spargen's generated \
          client, some eleven thousand lines compiled out of `target/`, whose path \
          exists only while the build script that wrote it is running.",
    guarded_package: "sunrise-relay-client",
}];

/// Whether `b` can appear inside a Rust identifier.
fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Every whole-word occurrence of `word` followed — across any whitespace — by
/// a `!`, as `(start of the word, index of the bang)`.
///
/// The whitespace tolerance is the point. `tracing::warn !(…)` and
/// `tracing::warn` + newline + `!(…)` are both legal, both record their fields,
/// and both are invisible to a scanner looking for the literal `warn!`.
fn word_bang_hits(text: &str, word: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut hits = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find(word).map(|r| from + r) {
        from = at + word.len();
        if at > 0 && is_ident_byte(bytes[at - 1]) {
            continue;
        }
        let mut j = at + word.len();
        if bytes.get(j).copied().is_some_and(is_ident_byte) {
            continue;
        }
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if bytes.get(j) == Some(&b'!') {
            hits.push((at, j));
        }
    }
    hits
}

/// What the attribute block before a `mod` says about where it lives.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PathSpec {
    /// No attribute mentions a path; the name resolves conventionally.
    None,
    /// Exactly one `#[path = "literal"]`, and nothing else mentions a path.
    Literal(String),
    /// Something in the block mentions a path this scanner will not interpret —
    /// a `cfg_attr`, a second `path`, anything but the simple form.
    Unresolvable(String),
}

/// A `mod name;` declaration: the modules this file pulls in as other files.
struct FileMod {
    name: String,
    /// Where the attribute block says the file is.
    path: PathSpec,
    /// Brace depth of the declaration. Anything but zero means it sits inside
    /// an inline `mod`, which changes the directory the name resolves against —
    /// a rule this walk does not implement and therefore refuses.
    depth: i32,
}

/// The `mod name;` declarations in one source.
fn file_mods(src: &Source) -> Vec<FileMod> {
    let bytes = src.structural.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            _ => {}
        }
        let is_word_start = i == 0 || !is_ident_byte(bytes[i - 1]);
        if is_word_start && src.structural[i..].starts_with("mod") {
            let mut j = i + 3;
            if bytes.get(j).copied().is_some_and(is_ident_byte) {
                i += 1;
                continue;
            }
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let name_start = j;
            while bytes.get(j).copied().is_some_and(is_ident_byte) {
                j += 1;
            }
            let name = src.structural[name_start..j].to_owned();
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if !name.is_empty() && bytes.get(j) == Some(&b';') {
                out.push(FileMod {
                    name,
                    path: path_spec_before(src, i),
                    depth,
                });
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// The whole attribute block immediately before the item at `at`, as byte
/// ranges in source order.
///
/// The *whole* block, not the nearest attribute. Inspecting only the nearest
/// one made `#[path = "imp_unix.rs"] #[cfg(unix)] mod imp;` look like a plain
/// `mod imp;` — refused for resolving to nothing, with no mention of the
/// `#[path]` two lines up — and let `#[path = "…"] #[allow(…)] mod x;` through
/// silently whenever a benign file sat at the default name.
fn attribute_block_before(src: &Source, at: usize) -> Vec<(usize, usize)> {
    let bytes = src.structural.as_bytes();
    let mut j = at;
    // `pub`, `pub(crate)` and friends sit between the block and the `mod`.
    while j > 0 && bytes[j - 1].is_ascii_whitespace() {
        j -= 1;
    }
    let before_vis = j;
    if j > 0 && bytes[j - 1] == b')' {
        while j > 0 && bytes[j - 1] != b'(' {
            j -= 1;
        }
        j = j.saturating_sub(1);
    }
    while j > 0 && bytes[j - 1].is_ascii_whitespace() {
        j -= 1;
    }
    let ident_end = j;
    while j > 0 && is_ident_byte(bytes[j - 1]) {
        j -= 1;
    }
    if &src.structural[j..ident_end] != "pub" {
        j = before_vis;
    }

    let mut block = Vec::new();
    loop {
        while j > 0 && bytes[j - 1].is_ascii_whitespace() {
            j -= 1;
        }
        if j == 0 || bytes[j - 1] != b']' {
            break;
        }
        let close = j - 1;
        let mut depth = 0i32;
        let mut k = close;
        let open = loop {
            match bytes[k] {
                b']' => depth += 1,
                b'[' => {
                    depth -= 1;
                    if depth == 0 {
                        break Some(k);
                    }
                }
                _ => {}
            }
            let Some(next) = k.checked_sub(1) else {
                break None;
            };
            k = next;
        };
        let Some(open) = open else { break };
        // A `#` before the `[`, or this is an index expression rather than an
        // attribute and the block has ended.
        if open == 0 || bytes[open - 1] != b'#' {
            break;
        }
        block.push((open - 1, close + 1));
        j = open - 1;
    }
    block.reverse();
    block
}

/// What the attribute block before `at` says about where the module lives.
///
/// Deliberately not an attribute-grammar parser. Anything mentioning a path
/// that is not one plain `#[path = "literal"]` is refused by name: a
/// `cfg_attr` that resolves per platform is exactly what this scanner cannot
/// know, so it says so rather than guessing — and guessing is silent in
/// whichever direction a decoy file at the default name happens to make it.
fn path_spec_before(src: &Source, at: usize) -> PathSpec {
    let mut literal = None;
    let mut mentions = 0usize;
    let mut offenders = Vec::new();
    for (from, to) in attribute_block_before(src, at) {
        let attr = &src.structural[from..to];
        if !attr.contains("path") {
            continue;
        }
        mentions += 1;
        let simple = attr
            .strip_prefix("#[")
            .and_then(|a| a.strip_suffix(']'))
            .map(str::trim)
            .and_then(|a| a.strip_prefix("path"))
            .map(str::trim_start)
            .and_then(|a| a.strip_prefix('='))
            .map(str::trim)
            .is_some_and(|a| a.len() >= 2 && a.starts_with('"') && a.ends_with('"'));
        if simple && mentions == 1 {
            // The value is read out of the literal form, at the same offsets.
            let lit = &src.literal[from..to];
            let open = lit.find('"').expect("the structural form found one") + 1;
            let end = lit[open..].find('"').map_or(lit.len(), |i| i + open);
            literal = Some(lit[open..end].to_owned());
        } else {
            offenders.push(snippet(attr));
        }
    }
    if mentions > 1 || !offenders.is_empty() {
        let mut all = offenders;
        if let Some(v) = literal {
            all.push(format!("#[path = \"{v}\"]"));
        }
        return PathSpec::Unresolvable(all.join(" "));
    }
    literal.map_or(PathSpec::None, PathSpec::Literal)
}

/// Every file the shipped targets compile, and everything the walk could not
/// follow.
#[allow(clippy::too_many_lines)]
fn module_graph() -> (BTreeMap<String, String>, Vec<Unresolved>) {
    let root = workspace_root();
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    let mut problems: Vec<Unresolved> = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    // `true` marks a crate root: its `mod` names resolve against its own
    // directory rather than against a directory named after it.
    let mut queue: Vec<(PathBuf, bool)> = shipped_target_roots()
        .into_iter()
        .map(|p| (p, true))
        .collect();

    while let Some((path, is_root)) = queue.pop() {
        if !seen.insert(path.clone()) {
            continue;
        }
        let rel_path = rel(&path, &root);
        let Ok(text) = std::fs::read_to_string(&path) else {
            problems.push(Unresolved {
                file: rel_path,
                what: "a source file cargo names but that cannot be read".to_owned(),
            });
            continue;
        };
        let src = normalise(&text);

        for (at, _) in word_bang_hits(&src.structural, "include") {
            let line = src.structural[..at].lines().count();
            problems.push(Unresolved {
                file: rel_path.clone(),
                what: format!(
                    "an `include!` at line {line}, whose contents are compiled into this \
                     crate but cannot be followed from the source tree"
                ),
            });
        }

        let dir = if is_root || path.file_name().is_some_and(|f| f == "mod.rs") {
            path.parent().map(Path::to_path_buf)
        } else {
            path.parent()
                .zip(path.file_stem())
                .map(|(p, stem)| p.join(stem))
        };
        let Some(dir) = dir else {
            problems.push(Unresolved {
                file: rel_path.clone(),
                what: "a source file with no parent directory".to_owned(),
            });
            files.insert(rel_path, text);
            continue;
        };

        for m in file_mods(&src) {
            if m.depth != 0 {
                problems.push(Unresolved {
                    file: rel_path.clone(),
                    what: format!(
                        "`mod {};` inside an inline module, which resolves against a \
                         directory this walk does not track",
                        m.name
                    ),
                });
                continue;
            }
            let candidates = match &m.path {
                PathSpec::None => vec![
                    dir.join(format!("{}.rs", m.name)),
                    dir.join(&m.name).join("mod.rs"),
                ],
                PathSpec::Literal(p) => vec![dir.join(p)],
                PathSpec::Unresolvable(attrs) => {
                    problems.push(Unresolved {
                        file: rel_path.clone(),
                        what: format!(
                            "`mod {};` carries an attribute block naming a path this \
                             scanner will not interpret ({attrs}). Write one plain \
                             `#[path = \"literal\"]`, or bring the file into the module tree",
                            m.name
                        ),
                    });
                    continue;
                }
            };
            match candidates.iter().find(|c| c.is_file()) {
                Some(found) => queue.push((
                    found.canonicalize().unwrap_or_else(|_| found.clone()),
                    false,
                )),
                None => problems.push(Unresolved {
                    file: rel_path.clone(),
                    what: format!(
                        "`mod {};`{} resolves to no file on disk (tried {})",
                        m.name,
                        match &m.path {
                            PathSpec::Literal(p) => format!(" with `#[path = \"{p}\"]`"),
                            _ => String::new(),
                        },
                        candidates
                            .iter()
                            .map(|c| rel(c, &root))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                }),
            }
        }
        files.insert(rel_path, text);
    }
    (files, problems)
}

fn graph() -> &'static (BTreeMap<String, String>, Vec<Unresolved>) {
    static GRAPH: OnceLock<(BTreeMap<String, String>, Vec<Unresolved>)> = OnceLock::new();
    GRAPH.get_or_init(module_graph)
}

/// Every shipped source, as `(path relative to the root, text)`.
fn shipped_sources() -> Vec<(String, String)> {
    let files = &graph().0;
    assert!(!files.is_empty(), "found no shipped sources to scan");
    files.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

// ---------------------------------------------------------------------------
// Invocation scanning
// ---------------------------------------------------------------------------

/// The level names, which are what both scans actually look for.
///
/// A fixed `"tracing::warn!"` needle would be wrong twice over: Rust allows
/// whitespace and a newline between a macro path and its `!`, and the path
/// itself is checked separately so an invocation reaching the macro under any
/// other name is reported rather than missed.
const LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error", "event"];

/// One field an invocation declares.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    name: String,
    /// The string literal on the right-hand side, when there is one. This is
    /// where an event name comes from, so the catalogue half and the allowlist
    /// half read one invocation exactly once and cannot disagree about it.
    value: Option<String>,
}

impl Field {
    fn named(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            value: None,
        }
    }

    fn with_value(name: &str, value: &str) -> Self {
        Self {
            name: name.to_owned(),
            value: Some(value.to_owned()),
        }
    }
}

/// What the scanner made of one invocation.
#[derive(Debug, PartialEq, Eq)]
enum Scanned {
    Fields(Vec<Field>),
    /// A form the scanner will not guess at, and why.
    Unanalysable(String),
}

/// Index of the `)` closing the `(` at `open`.
fn matching_paren(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// One invocation's arguments as absolute byte ranges, split on the commas that
/// separate them rather than the ones inside a call or a collection literal.
fn split_args(structural: &str, span: (usize, usize)) -> Vec<(usize, usize)> {
    let bytes = structural.as_bytes();
    let mut args = Vec::new();
    let mut depth = 0i32;
    let mut start = span.0;
    for (i, b) in bytes.iter().enumerate().take(span.1).skip(span.0) {
        match *b {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                args.push((start, i));
                start = i + 1;
            }
            _ => {}
        }
    }
    if structural[start..span.1].trim().is_empty() {
        return args;
    }
    args.push((start, span.1));
    args
}

/// Whether `s` is shaped like a `tracing` field name — an identifier, or the
/// dotted form (`error.message`) the macros also accept.
fn is_field_name(s: &str) -> bool {
    s.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// The text after the string literal `arg` opens with, or `None` if it does not
/// open with one.
fn after_string_literal(arg: &str) -> Option<&str> {
    let b = arg.as_bytes();
    if b.first() == Some(&b'"') {
        return arg[1..].find('"').map(|i| &arg[i + 2..]);
    }
    if b.first() == Some(&b'r') {
        let hashes = arg[1..].bytes().take_while(|&c| c == b'#').count();
        let open = 1 + hashes;
        if b.get(open) == Some(&b'"') {
            let close = format!("\"{}", "#".repeat(hashes));
            return arg[open + 1..]
                .find(&close)
                .map(|i| &arg[open + 1 + i + close.len()..]);
        }
    }
    None
}

/// Whether `arg` is a `concat!(…)` call, the one macro `tracing` accepts where
/// the message literal goes.
fn is_concat_call(arg: &str) -> bool {
    word_bang_hits(arg, "concat")
        .first()
        .is_some_and(|&(at, _)| at == 0)
}

/// A short, single-line form of an argument, for a failure message.
fn snippet(arg: &str) -> String {
    let flat = arg.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 72 {
        format!("{}…", flat.chars().take(72).collect::<String>())
    } else {
        flat
    }
}

/// The fields one invocation declares, or the reason it cannot be read.
///
/// `tracing`'s grammar puts fields before the message, so the field list ends
/// at the first string literal that is *not* followed by `=`. `takes_level`
/// covers `event!`, whose first non-directive argument is the `Level`.
fn fields_of(src: &Source, span: (usize, usize), takes_level: bool) -> Scanned {
    let mut fields = Vec::new();
    let mut level_pending = takes_level;
    for (from, to) in split_args(&src.structural, span) {
        let raw = &src.structural[from..to];
        let arg = raw.trim();
        if arg.is_empty() {
            continue;
        }
        let arg_start = from + (raw.len() - raw.trim_start().len());
        if ["target:", "parent:", "name:"]
            .iter()
            .any(|d| arg.starts_with(d))
        {
            continue;
        }
        if level_pending {
            level_pending = false;
            continue;
        }
        if arg.starts_with('{') {
            return Scanned::Unanalysable(format!(
                "a braced field block, whose fields this scanner cannot see: {}",
                snippet(arg)
            ));
        }
        if let Some(rest) = after_string_literal(arg) {
            let rest = rest.trim_start();
            if rest.starts_with('=') && !rest.starts_with("==") {
                return Scanned::Unanalysable(format!(
                    "a quoted field name, which this scanner cannot read: {}",
                    snippet(arg)
                ));
            }
            break;
        }
        let bytes = arg.as_bytes();
        let mut depth = 0i32;
        let mut eq = None;
        for (k, &ch) in bytes.iter().enumerate() {
            match ch {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => depth -= 1,
                b'=' if depth == 0 => {
                    let prev = if k == 0 { b' ' } else { bytes[k - 1] };
                    if bytes.get(k + 1) != Some(&b'=') && !matches!(prev, b'!' | b'<' | b'>' | b'=')
                    {
                        eq = Some(k);
                    }
                    break;
                }
                _ => {}
            }
        }
        // `concat!` expands to a string literal, so it is legal where the
        // message goes and declares no field. Refusing it would be a refusal
        // that misdiagnoses — telling the author to write plain field pairs
        // when field pairs were never the problem — and a gate that
        // misdiagnoses is one people learn to work around.
        if eq.is_none() && is_concat_call(arg) {
            break;
        }
        let name = match eq {
            Some(k) => arg[..k].trim().to_owned(),
            None => arg.trim_start_matches(['%', '?']).trim().to_owned(),
        };
        if name.starts_with("r#") {
            return Scanned::Unanalysable(format!(
                "a raw-identifier field name, which `tracing` records with the `r#` stripped: {}",
                snippet(arg)
            ));
        }
        if !is_field_name(&name) {
            return Scanned::Unanalysable(format!(
                "an argument that is neither `ident = value` nor a bare field name: {}",
                snippet(arg)
            ));
        }
        // A string-literal value is read out of the `literal` form at the same
        // offsets, which is how the catalogue half gets an event name from the
        // very same parse the allowlist half uses.
        let value = eq.and_then(|k| {
            let rhs_from = arg_start + k + 1;
            let rhs = src.literal[rhs_from..to].trim();
            let inner = rhs.strip_prefix('"')?.strip_suffix('"')?;
            (!inner.contains('"')).then(|| inner.to_owned())
        });
        fields.push(Field { name, value });
    }
    Scanned::Fields(fields)
}

/// The invocation whose `!` is at `bang`.
///
/// `!` may be followed by `(`, `[` or `{` — all three are legal and all three
/// record their fields. Only the first is read, and the other two are
/// *refused*: skipping them is a gate that fails open on a shape the compiler
/// is perfectly happy with.
fn analyse(src: &Source, bang: usize, takes_level: bool) -> Scanned {
    let bytes = src.structural.as_bytes();
    let mut j = bang + 1;
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    match bytes.get(j) {
        Some(b'(') => matching_paren(bytes, j).map_or_else(
            || {
                Scanned::Unanalysable(
                    "an invocation whose argument list does not close".to_owned(),
                )
            },
            |end| fields_of(src, (j + 1, end), takes_level),
        ),
        Some(&d @ (b'[' | b'{')) => Scanned::Unanalysable(format!(
            "an invocation delimited by `{}…{}` rather than `(…)`, which this scanner does not read",
            d as char,
            if d == b'[' { ']' } else { '}' }
        )),
        _ => Scanned::Unanalysable(
            "an invocation with no argument list this scanner can find".to_owned(),
        ),
    }
}

/// Every fully-qualified `tracing::*!` invocation in `text`, analysed.
fn scan_source(text: &str) -> Vec<Scanned> {
    let src = normalise(text);
    let mut out = Vec::new();
    for lvl in LEVELS {
        for (at, bang) in word_bang_hits(&src.structural, lvl) {
            // `trim_end` because a qualified path may be split across lines.
            if !src.structural[..at].trim_end().ends_with("tracing::") {
                continue;
            }
            out.push(analyse(&src, bang, *lvl == "event"));
        }
    }
    out
}

/// Every field logged from shipped code, as `(field, file)`.
fn emitted_fields() -> BTreeSet<(Field, String)> {
    let mut found = BTreeSet::new();
    for (rel_path, text) in shipped_sources() {
        for scanned in scan_source(&text) {
            if let Scanned::Fields(fields) = scanned {
                for f in fields {
                    found.insert((f, rel_path.clone()));
                }
            }
        }
    }
    found
}

impl PartialOrd for Field {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Field {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (&self.name, &self.value).cmp(&(&other.name, &other.value))
    }
}

/// Every event name emitted from shipped code, with the file it came from.
///
/// Read out of the same parse the field gate uses, so a call the two halves
/// disagree about is not possible: an `ev="Srv.Planted"` with no spaces used to
/// be an event to one half and nothing at all to the other.
fn emitted_events() -> BTreeSet<(String, String)> {
    emitted_fields()
        .into_iter()
        .filter(|(f, _)| f.name == "ev")
        .filter_map(|(f, file)| f.value.map(|v| (v, file)))
        .collect()
}

/// Every event name appearing in a table row of the catalogue.
fn catalogued_events() -> BTreeSet<String> {
    let path = workspace_root().join("docs/10-cross-cutting/log-events.md");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let mut names = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("| `") {
            continue;
        }
        let rest = &line[3..];
        if let Some(end) = rest.find('`') {
            names.insert(rest[..end].to_string());
        }
    }
    assert!(
        names.len() > 20,
        "the catalogue parser found only {} entries — did the table format change?",
        names.len()
    );
    names
}

// ---------------------------------------------------------------------------
// The gates
// ---------------------------------------------------------------------------

#[test]
fn the_module_graph_resolves_completely() {
    let allowed: BTreeMap<&str, &AllowedInclude> =
        ALLOWED_INCLUDES.iter().map(|a| (a.file, a)).collect();
    let mut refused = Vec::new();
    for problem in &graph().1 {
        if problem.what.starts_with("an `include!`") && allowed.contains_key(problem.file.as_str())
        {
            continue;
        }
        refused.push(format!("{}: {}", problem.file, problem.what));
    }
    assert!(
        refused.is_empty(),
        "these are compiled into a shipped crate but the module walk cannot reach \
         them, so no gate in this file has read them. Bring the code into the \
         module tree, or add an argued entry to ALLOWED_INCLUDES with a guard \
         that fails when the argument stops holding:\n  {}",
        refused.join("\n  ")
    );
}

#[test]
fn every_allowed_include_still_earns_its_exception() {
    // An entry that no longer corresponds to a real `include!` would sit here
    // widening the rule for nothing.
    let problems = &graph().1;
    for allowed in ALLOWED_INCLUDES {
        assert!(
            problems
                .iter()
                .any(|p| p.file == allowed.file && p.what.starts_with("an `include!`")),
            "stale ALLOWED_INCLUDES entry: {} no longer holds an unresolvable \
             `include!` ({})",
            allowed.file,
            allowed.why
        );

        // The guard. Code that cannot reach `tracing` cannot emit an event, so
        // the day someone adds the dependency is the day this exception has to
        // be argued again rather than inherited.
        let package = packages()
            .into_iter()
            .find(|p| p["name"].as_str() == Some(allowed.guarded_package))
            .unwrap_or_else(|| panic!("no package named {}", allowed.guarded_package));
        // Normal dependencies only. A `tracing` under `[dev-dependencies]` or
        // `[build-dependencies]` cannot reach the shipped artefact, so firing
        // on one would demand a substantial refactor at a non-problem.
        let logs = package["dependencies"]
            .as_array()
            .expect("a package lists dependencies")
            .iter()
            .any(|d| {
                d["name"].as_str() == Some("tracing")
                    && d["kind"].as_str().is_none_or(|k| k == "normal")
            });
        assert!(
            !logs,
            "{} now depends on `tracing`, so the generated code included by {} can \
             emit events no gate here has read.\n\n\
             Resolve the include — do not relax this guard. It is a dependency \
             check standing in for a content claim, and that claim is now \
             unsupported: an exception kept past the expiry of its own \
             justification is the defect this file exists to catch. Make the \
             include readable instead — run the build script here and scan its \
             output, or have spargen emit into the source tree — and delete the \
             ALLOWED_INCLUDES entry.",
            allowed.guarded_package, allowed.file
        );
    }
}

#[test]
fn every_manifest_the_build_config_names_exists() {
    // The out-of-workspace half of the file set is derived from `mise.toml`, so
    // a renamed or moved manifest must fail here rather than quietly shrink the
    // set of files these gates read.
    for manifest in extra_manifests() {
        assert!(
            manifest.is_file(),
            "mise.toml names --manifest-path {} but no manifest is there",
            manifest.display()
        );
    }
}

#[test]
fn the_scan_reaches_what_cargo_compiles() {
    // Derived, so there is nothing to pin — but a scan that silently found
    // nothing would make every gate below vacuous.
    let files = &graph().0;
    // A floor against a broken scan, not against ordinary work. 181 files
    // today; deleting a crate should not read as "the walk stopped working".
    assert!(
        files.len() >= 50,
        "expected at least 50 shipped sources, found {} — the module walk has \
         stopped following something",
        files.len()
    );
    for expected in [
        "crates/sunrise-server/src/api/sync.rs",
        "crates/sunrise-core/src/sync_driver.rs",
        "crates/sunrise-log/src/field.rs",
        "tools/uniffi-bindgen/src/main.rs",
    ] {
        assert!(
            files.contains_key(expected),
            "the module walk did not reach {expected}"
        );
    }
    assert!(
        !files.keys().any(|f| f.starts_with("legacy/")),
        "`legacy` is not a workspace member and must not be scanned"
    );
    // Test-only sources are out of scope by design: a fixture may invent its
    // own event names.
    assert!(
        !files.keys().any(|f| f.contains("/tests/")),
        "integration tests are not shipped code: {files:?}",
    );
}

#[test]
fn every_tracing_invocation_is_in_the_analysable_form() {
    let mut refused = Vec::new();
    for (rel_path, text) in shipped_sources() {
        for scanned in scan_source(&text) {
            if let Scanned::Unanalysable(why) = scanned {
                refused.push(format!("{rel_path}: {why}"));
            }
        }
    }
    assert!(
        refused.is_empty(),
        "these `tracing` invocations use a form this gate cannot analyse, so it \
         cannot confirm their field names are on the redaction allowlist. Write \
         the fields as plain `ident = value` pairs (with `%` or `?` for the \
         value, or bare shorthand) — or, if one of these forms is genuinely \
         needed, decide that deliberately and teach this scanner to read it:\n  {}",
        refused.join("\n  ")
    );
}

#[test]
fn every_emitted_field_name_is_on_the_redaction_allowlist() {
    let mut offenders = Vec::new();
    for (field, file) in emitted_fields() {
        if !sunrise_log::is_allowed(&field.name) {
            offenders.push(format!("{} (logged from {file})", field.name));
        }
    }
    offenders.dedup();
    assert!(
        offenders.is_empty(),
        "these field names are logged from shipped code but are not on the \
         allowlist in crates/sunrise-log/src/field.rs. `RedactionLayer` refuses \
         the *whole event* on the first one it sees — a panic under \
         debug_assertions, a silent drop and a violations() bump in release — so \
         each of these is a log record that can never exist:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn every_emitted_event_is_catalogued() {
    let catalogued = catalogued_events();
    let mut undocumented = Vec::new();
    for (ev, file) in emitted_events() {
        if !catalogued.contains(&ev) {
            undocumented.push(format!("{ev} (emitted from {file})"));
        }
    }
    assert!(
        undocumented.is_empty(),
        "these events are emitted but not in docs/10-cross-cutting/log-events.md:\n  {}",
        undocumented.join("\n  ")
    );
}

#[test]
fn every_emitted_event_name_is_grammatical() {
    for (ev, file) in emitted_events() {
        assert!(
            sunrise_log::is_valid_name(&ev),
            "event name {ev:?} in {file} violates the logging.md §3 grammar"
        );
    }
}

#[test]
fn every_catalogued_event_name_is_grammatical() {
    for ev in catalogued_events() {
        assert!(
            sunrise_log::is_valid_name(&ev),
            "catalogued event name {ev:?} violates the logging.md §3 grammar"
        );
    }
}

#[test]
fn every_tracing_macro_is_called_by_its_full_path() {
    // The scan keys on the `tracing::` qualifier, so an invocation that reaches
    // the macro by any other name is invisible to it: `t::warn!` behind a
    // `use tracing as t`, or a `self::`/`crate::` re-export.
    let mut aliased = Vec::new();
    for (rel_path, text) in shipped_sources() {
        let src = normalise(&text);
        for lvl in LEVELS {
            for (at, _) in word_bang_hits(&src.structural, lvl) {
                // `trim_end` because a qualified path may be split across
                // lines; without it `tracing::` + newline + `warn!` was
                // reported as a bare `warn!`, which is a misdiagnosis rather
                // than a catch.
                let before = src.structural[..at].trim_end();
                if before.ends_with("tracing::") {
                    continue;
                }
                if before.ends_with("::") {
                    aliased.push(format!(
                        "{rel_path}: a `…::{lvl}!` that is not `tracing::{lvl}!`"
                    ));
                } else {
                    aliased.push(format!("{rel_path}: a bare `{lvl}!`"));
                }
            }
        }
    }
    assert!(
        aliased.is_empty(),
        "these reach a `tracing` macro by a name the field scan does not look \
         for. Call them as `tracing::<level>!` so the gate can read their \
         fields:\n  {}",
        aliased.join("\n  ")
    );
}

#[test]
fn the_tracing_macros_are_never_imported() {
    // Importing a level macro is what makes a bare or renamed invocation
    // possible in the first place, and a renamed one cannot be recognised by
    // shape. Refusing the import is what closes that off at the source.
    let mut imports = Vec::new();
    for (rel_path, text) in shipped_sources() {
        let src = normalise(&text);
        let cleaned = &src.structural;
        let mut from = 0;
        while let Some(at) = cleaned[from..].find("use ").map(|r| from + r) {
            from = at + 4;
            let Some(end) = cleaned[at..].find(';') else {
                continue;
            };
            let stmt = &cleaned[at..at + end];
            let rest = stmt[4..].trim_start().trim_start_matches("::");
            if !rest.starts_with("tracing") {
                continue;
            }
            if rest
                .trim_start_matches("tracing")
                .trim_start()
                .starts_with("as ")
            {
                imports.push(format!(
                    "{rel_path}: `{}` renames the whole crate",
                    snippet(stmt)
                ));
                continue;
            }
            if rest
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .any(|tok| LEVELS.contains(&tok))
            {
                imports.push(format!(
                    "{rel_path}: `{}` imports a level macro",
                    snippet(stmt)
                ));
            }
        }
    }
    assert!(
        imports.is_empty(),
        "these import a `tracing` level macro (or the crate under another \
         name), which allows a bare or renamed invocation the field scan cannot \
         see. Call the macros by their full `tracing::<level>!` path \
         instead:\n  {}",
        imports.join("\n  ")
    );
}

#[test]
fn the_scan_actually_finds_events_and_fields() {
    // A scanner that silently stopped matching would turn every gate above
    // into a no-op — which is how the previous redaction test managed to prove
    // nothing for a whole release cycle.
    let events = emitted_events();
    // 45 today. The floor is for a scanner that has stopped matching, not for
    // ordinary consolidation: deleting six events is normal work and must not
    // trip a gate whose message reads like a scanner failure.
    assert!(
        events.len() >= 20,
        "expected at least 20 emitted events, found {} — the scan has stopped \
         matching: {events:?}",
        events.len()
    );
    let fields = emitted_fields();
    let names: BTreeSet<&String> = fields.iter().map(|(f, _)| &f.name).collect();
    // 28 today, same reasoning.
    assert!(
        names.len() >= 12,
        "expected at least 12 distinct logged field names, found {} — the scan \
         has stopped matching: {names:?}",
        names.len()
    );
    for expected in ["ev", "err_code", "stream_h", "cause"] {
        assert!(
            names.iter().any(|n| n.as_str() == expected),
            "the scan missed {expected:?}, which is logged all over the server"
        );
    }
}

// ---------------------------------------------------------------------------
// The scanner, on the shapes it has to survive
// ---------------------------------------------------------------------------

#[test]
fn the_field_scanner_reads_the_shapes_it_must() {
    let scanned = scan_source(
        r#"
        fn emit() {
            tracing::warn!(
                ev = "srv.thing.happened",
                // batch_id and first_seen_ms are named here but not declared
                stream_h = %crate::logging::id_h(&stream_id, 4),
                n_ops = ops.len() as u64,
                first_seen_ms,
                ?err,
                "a message with = and , and ( inside it",
                extra_format_arg,
            );
        }
        "#,
    );
    assert_eq!(
        scanned,
        vec![Scanned::Fields(vec![
            Field::with_value("ev", "srv.thing.happened"),
            Field::named("stream_h"),
            Field::named("n_ops"),
            Field::named("first_seen_ms"),
            Field::named("err"),
        ])],
        "the scanner must read the declared fields and stop at the message"
    );
}

#[test]
fn the_field_scanner_catches_a_planted_violation() {
    let scanned = scan_source(r#"tracing::info!(ev = "srv.oops", task_title = %t, "x");"#);
    assert_eq!(
        scanned,
        vec![Scanned::Fields(vec![
            Field::with_value("ev", "srv.oops"),
            Field::named("task_title"),
        ])]
    );
    assert!(
        !sunrise_log::is_allowed("task_title"),
        "and the allowlist must refuse it"
    );
}

/// The forms this scanner will not guess at, each a named refusal.
#[test]
fn the_field_scanner_refuses_the_forms_it_cannot_read() {
    let why = |src: &str| match scan_source(src).pop().expect("one invocation") {
        Scanned::Unanalysable(why) => why,
        Scanned::Fields(f) => panic!("expected a refusal, got fields {f:?}"),
    };
    assert!(
        why(r#"tracing::info!(ev = "x", "task_title" = %t, "m");"#).contains("a quoted field name"),
        "a quoted field name must be refused"
    );
    assert!(
        why(r#"tracing::info!({ ev = "x", task_title = %t }, "m");"#)
            .contains("a braced field block"),
        "a braced field block must be refused"
    );
    assert!(
        why(r#"tracing::info!(ev = "x", r#type = 1, "m");"#)
            .contains("a raw-identifier field name"),
        "a raw-identifier field name must be refused"
    );
    assert!(
        why(r#"tracing::info!(ev = "x", some.call(1) + 2, "m");"#)
            .contains("neither `ident = value` nor a bare field name"),
        "an unreadable argument must be refused"
    );
}

#[test]
fn the_field_scanner_refuses_a_bang_it_cannot_reach_the_usual_way() {
    for src in [
        r#"tracing::warn !(ev = "srv.x", task_title = %t, "m");"#,
        "tracing::warn\n    !(ev = \"srv.x\", task_title = %t, \"m\");",
        "tracing::\n    warn!(ev = \"srv.x\", task_title = %t, \"m\");",
    ] {
        assert_eq!(
            scan_source(src),
            vec![Scanned::Fields(vec![
                Field::with_value("ev", "srv.x"),
                Field::named("task_title"),
            ])],
            "a bang reached across whitespace must still be scanned: {src:?}"
        );
    }
    for (src, delim) in [
        (
            r#"tracing::warn![ev = "srv.x", task_title = %t, "m"];"#,
            "`[…]`",
        ),
        (
            r#"tracing::warn!{ev = "srv.x", task_title = %t, "m"}"#,
            "`{…}`",
        ),
    ] {
        match scan_source(src).pop().expect("one invocation") {
            Scanned::Unanalysable(why) => assert!(
                why.contains(delim) && why.contains("rather than `(…)`"),
                "expected a delimiter refusal naming {delim}, got: {why}"
            ),
            Scanned::Fields(f) => panic!("expected a refusal, got fields {f:?}"),
        }
    }
}

#[test]
fn a_concat_message_is_not_mistaken_for_a_field() {
    assert_eq!(
        scan_source(r#"tracing::info!(ev = "srv.x", concat!("event ", "stream ended"));"#),
        vec![Scanned::Fields(vec![Field::with_value("ev", "srv.x")])],
        "a `concat!` message declares no field and is not an unreadable argument"
    );
}

#[test]
fn an_event_name_is_read_from_the_same_parse_as_its_fields() {
    // The two halves used to read one invocation separately: a raw-line search
    // for the literal `ev = "` and a parse for the fields. `ev="…"` without
    // spaces was an event to neither, so a badly-named event shipped past both
    // the catalogue gate and the grammar gate.
    let scanned = scan_source(r#"tracing::info!(ev="Srv.Planted BAD NAME", task_h = %t, "m");"#);
    assert_eq!(
        scanned,
        vec![Scanned::Fields(vec![
            Field::with_value("ev", "Srv.Planted BAD NAME"),
            Field::named("task_h"),
        ])],
        "an event name must be read whatever the spacing around its `=`"
    );
    assert!(
        !sunrise_log::is_valid_name("Srv.Planted BAD NAME"),
        "and the grammar gate must then refuse it"
    );
}

/// The attribute block before a `mod`, which decides which file the walk reads
/// next — or refuses to guess.
#[test]
fn the_module_walk_reads_the_whole_attribute_block() {
    let spec = |src: &str| {
        let source = normalise(src);
        let at = source
            .structural
            .find("mod imp;")
            .expect("the fixture declares a module");
        path_spec_before(&source, at)
    };

    // Plain, and the only shape this scanner will act on.
    assert_eq!(
        spec("#[path = \"imp_unix.rs\"]\nmod imp;"),
        PathSpec::Literal("imp_unix.rs".to_owned())
    );
    assert_eq!(spec("mod imp;"), PathSpec::None);

    // Previously silent: only the nearest attribute was read, so the `#[path]`
    // was missed and a benign file at the default name hid the real one.
    assert_eq!(
        spec("#[path = \"generated/imp.rs\"]\n#[allow(dead_code)]\nmod imp;"),
        PathSpec::Literal("generated/imp.rs".to_owned())
    );

    // Previously a false positive: refused as "resolves to no file" without
    // ever mentioning the `#[path]` two lines up.
    assert_eq!(
        spec("#[path = \"imp_unix.rs\"]\n#[cfg(unix)]\npub mod imp;"),
        PathSpec::Literal("imp_unix.rs".to_owned())
    );

    // A path this scanner cannot resolve is named, not guessed at. Which file
    // a `cfg_attr` selects depends on the target, which is not knowable here.
    for src in [
        "#[cfg_attr(target_os = \"macos\", path = \"imp_mac.rs\")]\nmod imp;",
        "#[path = \"a.rs\"]\n#[cfg_attr(unix, path = \"b.rs\")]\nmod imp;",
        "#[cfg(feature = \"x\")]\n#[path = \"a.rs\"]\n#[path = \"b.rs\"]\nmod imp;",
    ] {
        assert!(
            matches!(spec(src), PathSpec::Unresolvable(_)),
            "must refuse rather than guess: {src:?}"
        );
    }
}

#[test]
fn an_aliased_macro_path_is_refused_by_the_path_gates() {
    for src in [
        "fn f() { t::warn!(ev = \"x\", task_title = %t, \"m\"); }",
        "fn f() { self::warn!(ev = \"x\", \"m\"); }",
        "fn f() { warn!(ev = \"x\", \"m\"); }",
    ] {
        assert!(
            scan_source(src).is_empty(),
            "the field scan cannot see {src:?} — which is why the path gates exist"
        );
    }
    for stmt in [
        "use tracing::warn;",
        "use tracing::{info, warn};",
        "use tracing::warn as wlog;",
        "use tracing as t;",
    ] {
        let src = normalise(stmt);
        let rest = src.structural[4..].trim_start().trim_start_matches("::");
        let renames_crate = rest
            .trim_start_matches("tracing")
            .trim_start()
            .starts_with("as ");
        let imports_level = rest
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|tok| LEVELS.contains(&tok));
        assert!(
            renames_crate || imports_level,
            "{stmt:?} must be refused by the import gate"
        );
    }
}

// ---------------------------------------------------------------------------
// The derivation's own boundary
// ---------------------------------------------------------------------------
//
// The file set comes from `cargo metadata` plus the `--manifest-path` flags in
// `mise.toml`. The first half is the compiler's own view and cannot drift; the
// second is a *convention* — that everything this repo builds goes through a
// mise task. A workflow that reached around mise and ran cargo against a
// manifest of its own would build a crate no gate here reads, and nothing would
// say so.
//
// Nothing does that today. These check it rather than trust it.

/// Offsets where `cargo` is invoked as a *command*.
///
/// Not a substring search. `~/.cargo/registry`, `id=cargo-registry` and
/// `/usr/local/cargo` are not cargo invocations, and treating them as such
/// meant that adding the conventional Rust cache step would trip a gate whose
/// failure message talks about log vocabulary — a misdiagnosis of exactly the
/// kind this file keeps having to remove.
fn cargo_command_hits(text: &str) -> Vec<usize> {
    let bytes = text.as_bytes();
    let mut hits = Vec::new();
    for (at, _) in text.match_indices("cargo") {
        // A whole word, and not glued to a path or a longer token.
        if at > 0 && (is_ident_byte(bytes[at - 1]) || matches!(bytes[at - 1], b'/' | b'.' | b'-')) {
            continue;
        }
        if bytes
            .get(at + "cargo".len())
            .copied()
            .is_some_and(is_ident_byte)
        {
            continue;
        }
        // In command position: the start, a new line, a shell separator, or a
        // YAML `run:`/list dash.
        let mut j = at;
        while j > 0 && matches!(bytes[j - 1], b' ' | b'\t') {
            j -= 1;
        }
        if j == 0
            || matches!(
                bytes[j - 1],
                b'\n' | b'&' | b';' | b'|' | b'(' | b':' | b'-'
            )
        {
            hits.push(at);
        }
    }
    hits
}

/// Whether a build-config fragment invokes cargo.
fn mentions_cargo(text: &str) -> bool {
    !cargo_command_hits(text).is_empty()
}

/// Whether a fragment moves out of the directory it started in.
fn changes_directory(text: &str) -> bool {
    text.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("cd ")
            || t.starts_with("working-directory:")
            || t.starts_with("WORKDIR ")
            || l.contains("&& cd ")
    })
}

/// One build-config file split into the scopes a directory change applies to.
///
/// A YAML step, a mise task, or — where a file has no such structure — the
/// whole file. Splitting on any indentation, because hard-coding six spaces
/// collapsed an eight-space workflow into a single pseudo-step and made the
/// gate report the file's own `name:` line as the offender.
fn scopes(file: &str, text: &str) -> Vec<String> {
    let is_yaml = Path::new(file)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("yml") || e.eq_ignore_ascii_case("yaml"));
    let starts_scope: fn(&str) -> bool = if is_yaml {
        |l: &str| l.trim_start().starts_with("- ")
    } else if file.ends_with("mise.toml") {
        |l: &str| l.starts_with("[tasks.")
    } else {
        return vec![text.to_owned()];
    };
    let mut out = vec![String::new()];
    for line in text.lines() {
        if starts_scope(line) {
            out.push(String::new());
        }
        let last = out.last_mut().expect("one scope always exists");
        last.push_str(line);
        last.push('\n');
    }
    out
}

#[test]
fn no_build_config_names_a_manifest_the_scan_does_not_cover() {
    let root = workspace_root();
    let derived = derived_manifests();
    let mut offenders = Vec::new();
    for (file, text) in build_config_files() {
        for arg in manifest_path_args(&text) {
            // An interpolated path cannot be resolved here, and a manifest this
            // gate cannot name is a manifest it cannot check. Same rule as an
            // `include!`: refuse it and say so.
            if arg.contains("${{") || arg.contains('$') || arg.contains('`') {
                offenders.push(format!(
                    "{file}: `--manifest-path {arg}` is interpolated, so this gate cannot \
                     tell which manifest it resolves to"
                ));
                continue;
            }
            let path = root.join(&arg);
            let path = path.canonicalize().unwrap_or(path);
            if !derived.contains(&path) {
                offenders.push(format!(
                    "{file}: `--manifest-path {arg}` names a manifest the file set does \
                     not include"
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "these build a crate with a manifest these gates do not read, so its log \
         vocabulary is checked by nothing. Route the build through a `mise.toml` \
         task — which is where the out-of-workspace half of the file set is \
         derived from — or make the manifest a workspace member:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn no_build_config_runs_cargo_from_a_directory_of_its_own() {
    // The other way to reach a foreign manifest: leave the workspace root and
    // let cargo find whatever is there. This used to be filtered to workflows,
    // so a mise task, a composite action and a Dockerfile `WORKDIR` each got a
    // free pass from a check written precisely for them.
    //
    // A scope that both moves and runs cargo must say which manifest it means.
    // Only doing *both* is flagged: a `working-directory` on a bun step is
    // ordinary work, and a gate that fires on ordinary work is one someone
    // switches off.
    let mut offenders = Vec::new();
    for (file, text) in build_config_files() {
        // A container build has no directory structure this gate can model —
        // `/src` is a path inside an image. What decides its manifest is which
        // `Cargo.toml` is copied in, which
        // `the_container_build_copies_only_manifests_the_scan_covers` checks.
        if is_container_build(&file) {
            continue;
        }
        for scope in scopes(&file, &text) {
            let cargo = cargo_command_hits(&scope).len();
            if cargo == 0 || !changes_directory(&scope) {
                continue;
            }
            if manifest_path_args(&scope).len() < cargo {
                let name = scope
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .map_or_else(String::new, |l| snippet(l.trim()));
                offenders.push(format!(
                    "{file}: a scope that changes directory and runs cargo without \
                     naming a manifest ({name})"
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "these run cargo somewhere other than the workspace root, so which \
         manifest they build is not something this gate can determine. Give the \
         invocation an explicit `--manifest-path`, or route it through a \
         `mise.toml` task:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn the_container_build_copies_only_manifests_the_scan_covers() {
    // The Dockerfile's cargo build runs against whatever was copied into the
    // image, so `COPY` is the thing that decides its manifest — not `WORKDIR`,
    // and not anything this gate could learn by modelling container paths.
    let root = workspace_root();
    let derived = derived_manifests();
    let containers: Vec<(String, String)> = build_config_files()
        .into_iter()
        .filter(|(f, _)| is_container_build(f))
        .collect();
    // Read from the same set the skip above consumes, so the two rules cannot
    // come to disagree about which files each is responsible for.
    assert!(
        !containers.is_empty(),
        "no container build was read, so the rule that skips them in \
         `no_build_config_runs_cargo_from_a_directory_of_its_own` covers nothing"
    );
    let mut offenders = Vec::new();
    for (file, text) in containers {
        for line in text.lines() {
            let t = line.trim_start();
            if !t.starts_with("COPY ") {
                continue;
            }
            for token in t.split_whitespace().skip(1) {
                if !token.ends_with("Cargo.toml") {
                    continue;
                }
                let path = root.join(token);
                let path = path.canonicalize().unwrap_or(path);
                if !derived.contains(&path) {
                    offenders.push(format!("{file}: `COPY {token}`"));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "the container build copies a manifest these gates do not read, so the \
         crate it builds has its log vocabulary checked by nothing:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn the_build_config_scan_actually_reads_cargo_invocations() {
    // A floor near the real count, not far below it. The workflows, the
    // Dockerfile and mise.toml carry dozens of cargo invocations between them;
    // a scanner that quietly stopped matching would otherwise pass both gates
    // above by finding nothing to check.
    let files = build_config_files();
    let with_cargo = files.iter().filter(|(_, t)| mentions_cargo(t)).count();
    assert!(
        with_cargo >= 2,
        "expected cargo invocations in at least 2 build-config files, found \
         {with_cargo} — the command scan has stopped matching: {:?}",
        files.iter().map(|(f, _)| f).collect::<Vec<_>>()
    );
    // And the one `--manifest-path` the repo actually has must be found, or the
    // argument parser has stopped working and `extra_manifests` with it.
    let found: Vec<String> = files
        .iter()
        .flat_map(|(_, t)| manifest_path_args(t))
        .collect();
    assert!(
        found.iter().any(|p| p.contains("uniffi-bindgen")),
        "the `--manifest-path` parser found {found:?}, which does not include the \
         one manifest mise.toml names — `extra_manifests` reads the same parser, \
         so the file set would be short a crate"
    );
}
