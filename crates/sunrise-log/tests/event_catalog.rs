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
    const FLAG: &str = "--manifest-path";
    let root = workspace_root();
    let text = std::fs::read_to_string(root.join("mise.toml")).expect("mise.toml is readable");
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
            found.push(root.join(path));
        }
    }
    found.sort();
    found.dedup();
    found
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

/// Blank every byte in `regions`, keeping newlines so line structure survives.
///
/// Every byte written is ASCII and every region is a whole item, so the result
/// is still valid UTF-8 and still the same byte length.
fn blank_regions(s: &str, regions: &[(usize, usize)]) -> String {
    let mut bytes = s.as_bytes().to_vec();
    for &(at, end) in regions {
        for b in &mut bytes[at..end] {
            if *b != b'\n' {
                *b = b' ';
            }
        }
    }
    String::from_utf8(bytes).expect("blanking writes only ASCII over whole items")
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

/// The byte ranges of `#[cfg(test)]` items.
///
/// A unit test in `src/` is compiled only under `cfg(test)`, never shipped, and
/// is free to log whatever it likes. Gating it would have this file claim a
/// test fixture's field name is "logged from shipped code", which is both a
/// false positive and a lie about where the risk is.
fn cfg_test_regions(structural: &str) -> Vec<(usize, usize)> {
    const ATTR: &str = "#[cfg(test)]";
    let bytes = structural.as_bytes();
    let mut regions = Vec::new();
    let mut from = 0;
    while let Some(rel_at) = structural[from..].find(ATTR) {
        let at = from + rel_at;
        from = at + ATTR.len();
        let mut j = from;
        // Skip whitespace and any further attributes on the same item.
        loop {
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if bytes.get(j) == Some(&b'#') {
                let mut depth = 0i32;
                while j < bytes.len() {
                    match bytes[j] {
                        b'[' => depth += 1,
                        b']' => {
                            depth -= 1;
                            if depth == 0 {
                                j += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
            } else {
                break;
            }
        }
        // The item is either braced or terminated by `;`.
        let mut depth = 0i32;
        let mut end = None;
        while j < bytes.len() {
            match bytes[j] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(j + 1);
                        break;
                    }
                }
                b';' if depth == 0 => {
                    end = Some(j + 1);
                    break;
                }
                _ => {}
            }
            j += 1;
        }
        if let Some(end) = end {
            regions.push((at, end));
        }
    }
    regions
}

fn normalise(text: &str) -> Source {
    let structural = blank(text, true);
    let literal = blank(text, false);
    assert_eq!(structural.len(), text.len(), "blanking changed byte length");
    assert_eq!(literal.len(), text.len(), "blanking changed byte length");
    // Found once on the structural form and applied to both, so a `{` inside a
    // string literal cannot make the two disagree about where an item ends.
    let regions = cfg_test_regions(&structural);
    Source {
        structural: blank_regions(&structural, &regions),
        literal: blank_regions(&literal, &regions),
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

/// A `mod name;` declaration: the modules this file pulls in as other files.
struct FileMod {
    name: String,
    /// The value of a `#[path = "…"]` attribute on the declaration.
    path_attr: Option<String>,
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
                    path_attr: path_attribute_before(src, i),
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

/// The `#[path = "…"]` immediately preceding the item at `at`, if any.
fn path_attribute_before(src: &Source, at: usize) -> Option<String> {
    let bytes = src.structural.as_bytes();
    let mut j = at;
    loop {
        while j > 0 && bytes[j - 1].is_ascii_whitespace() {
            j -= 1;
        }
        // `pub`, `pub(crate)` and friends sit between the attribute and `mod`.
        let word_end = j;
        while j > 0 && (is_ident_byte(bytes[j - 1]) || bytes[j - 1] == b')') {
            if bytes[j - 1] == b')' {
                while j > 0 && bytes[j - 1] != b'(' {
                    j -= 1;
                }
            }
            j -= 1;
        }
        if j == word_end {
            break;
        }
    }
    while j > 0 && bytes[j - 1].is_ascii_whitespace() {
        j -= 1;
    }
    if bytes.get(j.checked_sub(1)?) != Some(&b']') {
        return None;
    }
    let close = j - 1;
    let mut depth = 0i32;
    let mut k = close;
    loop {
        match bytes[k] {
            b']' => depth += 1,
            b'[' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        k = k.checked_sub(1)?;
    }
    let attr = &src.structural[k..=close];
    if !attr.starts_with("[path") {
        return None;
    }
    // The value lives in the literal form, at the same offsets.
    let literal_attr = &src.literal[k..=close];
    let open = literal_attr.find('"')? + 1;
    let end = literal_attr[open..].find('"')? + open;
    Some(literal_attr[open..end].to_owned())
}

/// Every file the shipped targets compile, and everything the walk could not
/// follow.
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
            let candidates = m.path_attr.as_ref().map_or_else(
                || {
                    vec![
                        dir.join(format!("{}.rs", m.name)),
                        dir.join(&m.name).join("mod.rs"),
                    ]
                },
                |p| vec![dir.join(p)],
            );
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
                        m.path_attr
                            .as_ref()
                            .map_or_else(String::new, |p| format!(" with `#[path = \"{p}\"]`")),
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
        let logs = package["dependencies"]
            .as_array()
            .expect("a package lists dependencies")
            .iter()
            .any(|d| d["name"].as_str() == Some("tracing"));
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
    assert!(
        files.len() >= 100,
        "expected at least 100 shipped sources, found {}",
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
    // A real floor, not a token one: the workspace emits 45 today. A scanner
    // that quietly stopped matching half of them would still clear `>= 10`.
    assert!(
        events.len() >= 40,
        "expected at least 40 emitted events, found {}: {events:?}",
        events.len()
    );
    let fields = emitted_fields();
    let names: BTreeSet<&String> = fields.iter().map(|(f, _)| &f.name).collect();
    assert!(
        names.len() >= 20,
        "expected at least 20 distinct logged field names, found {}: {names:?}",
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

#[test]
fn a_unit_test_in_src_is_not_shipped_code() {
    // `#[cfg(test)]` never reaches an operator's binary, and gating it made
    // this file claim a fixture's field name was "logged from shipped code".
    let scanned = scan_source(
        r#"
        pub fn ship() { tracing::info!(ev = "srv.real", stream_h = %h, "m"); }

        #[cfg(test)]
        mod tests {
            #[test]
            fn t() { tracing::info!(ev = "srv.fixture", task_title = %x, "m"); }
        }
        "#,
    );
    assert_eq!(
        scanned,
        vec![Scanned::Fields(vec![
            Field::with_value("ev", "srv.real"),
            Field::named("stream_h"),
        ])],
        "only the shipped invocation is scanned"
    );
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
