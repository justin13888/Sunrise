//! The event catalogue, enforced.
//!
//! `docs/10-cross-cutting/logging.md` §3 says a new `ev` value "requires a
//! one-line entry in log-events.md so analysts can grep for meaning", and
//! §11 asked for a per-package snapshot test to hold that line. No such test
//! existed, and under `tracing` a snapshot of a hand-maintained constant list
//! would only prove the list matches itself.
//!
//! This checks the thing that actually matters instead, in two halves:
//!
//! - every `ev = "…"` literal in shipped source is grammatical and documented;
//! - every *field name* those events carry is on the redaction allowlist, so
//!   the record the catalogue promises can actually reach a subscriber.
//!
//! Both scan the `src` of every crate in the repository — the shipped
//! surfaces — so test fixtures inventing their own event names do not have to
//! be catalogued. The crate list is discovered from the manifests rather than
//! from the workspace members, because `tools/uniffi-bindgen` is a crate the
//! workspace does not contain.
//!
//! The field half **fails closed**: an invocation shape it cannot analyse is a
//! failure naming the form, not a silent "no fields here". See the note above
//! [`MACROS`] for why that is worth more than a scanner that knows more syntax.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root resolves")
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Every `ev = "…"` literal in shipped sources, with the file it came from.
///
/// Shares [`shipped_sources`] with the field gates below, so both cover the
/// same tree — including the crates that sit outside the workspace.
fn emitted_events() -> BTreeSet<(String, String)> {
    let mut found = BTreeSet::new();
    for (rel, text) in shipped_sources() {
        for line in text.lines() {
            // Skip comments, the same way `.github/scripts/grep-gate.sh` does:
            // prose that *names* an event — this crate's own doc comments, for
            // one — is not an emission.
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
                continue;
            }
            for (idx, _) in line.match_indices("ev = \"") {
                let rest = &line[idx + "ev = \"".len()..];
                let Some(end) = rest.find('"') else { continue };
                found.insert((rest[..end].to_string(), rel.clone()));
            }
        }
    }
    found
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
fn the_scan_actually_finds_events() {
    // Without this the two assertions above pass trivially if the scanner
    // breaks — which is how the previous redaction test managed to prove
    // nothing for a whole release cycle.
    let found = emitted_events();
    assert!(
        found.len() >= 10,
        "expected the workspace to emit at least 10 distinct events, found {}: {found:?}",
        found.len()
    );
}

// ---------------------------------------------------------------------------
// Field names
// ---------------------------------------------------------------------------
//
// The `ev` assertions above check event *names* and nothing else, which is how
// three unemittable events shipped: `RedactionLayer::event_enabled` refuses the
// whole event on the first field name outside the allowlist — a panic under
// `debug_assertions`, a silent drop plus a `violations()` bump in release — and
// no gate compared the field names in a `tracing::*!` call against that list.
//
// A catalogue row promising `stream_h`, `batch_id`, `first_seen_ms` is worth
// nothing if the record can never reach a subscriber, so the field vocabulary
// is scanned the same way the event vocabulary is.
//
// # Why this fails closed
//
// The scanner reads one shape — `ident = value`, plus the `%`/`?` sigils and
// bare shorthand — and every other legal `tracing` form is reported as a
// *failure* rather than scanned as "no fields here". That is deliberate. A
// scanner that silently yields nothing on a form it does not know is a gate
// that fails open, and the form nobody anticipated is precisely the one that
// would carry the next leak through. Four such shapes were found by review:
//
// - a quoted field name (`"task_title" = %t`), which `tracing` records and an
//   earlier version of this scanner mistook for the message, skipping it *and*
//   every field after it;
// - a braced field block (`{ ev = "x", task_title = %t }`), whose `=` sits at
//   bracket depth 1 where the scan does not look;
// - a raw-identifier key (`r#type = 1`), which `tracing` records as `type`;
// - a macro reached through an alias or a re-export (`t::warn!`, `self::warn!`,
//   or an imported `wlog!`), which the invocation scan never sees at all.
//
// None exists in the tree. Rather than teach the scanner four more grammars —
// which is a parser, and still leaves a fifth shape — each is a loud failure
// naming the form and asking for the plain one. If a site ever genuinely needs
// one, this gate failing is the right place to have that conversation.

/// The macros this scanner reads, fully qualified.
///
/// [`every_tracing_macro_is_called_by_its_full_path`] and
/// [`the_tracing_macros_are_never_imported`] are what make this list
/// sufficient: between them, no invocation can reach `tracing` by a name that
/// is not on it.
const MACROS: &[&str] = &[
    "tracing::trace!",
    "tracing::debug!",
    "tracing::info!",
    "tracing::warn!",
    "tracing::error!",
    "tracing::event!",
];

/// The level names, without the `tracing::` qualifier.
const LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error", "event"];

/// What the scanner made of one invocation.
#[derive(Debug, PartialEq, Eq)]
enum Scanned {
    /// The field names it declares, in order.
    Fields(Vec<String>),
    /// A form the scanner will not guess at, and why.
    Unanalysable(String),
}

/// The `src` directory of every crate in the repository.
///
/// Discovered from the manifests rather than hardcoded to `crates/*`: the
/// workspace is `members = ["crates/*"]`, but `tools/uniffi-bindgen` is a crate
/// outside it, and a scan that cannot see a source tree silently stops covering
/// it the moment someone adds one.
fn source_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    manifest_dirs(&workspace_root(), 0, &mut roots);
    roots.sort();
    roots
}

fn manifest_dirs(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Vendored, generated, or deliberately out of scope. `legacy` is
        // excluded from the workspace by the root manifest for the same reason.
        if name.starts_with('.') || matches!(name.as_ref(), "legacy" | "target" | "node_modules") {
            continue;
        }
        if path.join("Cargo.toml").is_file() && path.join("src").is_dir() {
            out.push(path.join("src"));
        }
        manifest_dirs(&path, depth + 1, out);
    }
}

/// Every shipped Rust source in the repository, with its path relative to the
/// root.
fn shipped_sources() -> Vec<(String, String)> {
    let root = workspace_root();
    let mut files = Vec::new();
    for src in source_roots() {
        rust_sources(&src, &mut files);
    }
    assert!(!files.is_empty(), "found no crate sources to scan");
    files
        .into_iter()
        .filter_map(|f| {
            let rel = f.strip_prefix(&root).unwrap_or(&f).display().to_string();
            std::fs::read_to_string(&f).ok().map(|text| (rel, text))
        })
        .collect()
}

/// The source with comment bodies and string/char literal *contents* blanked,
/// every delimiter left in place.
///
/// Both halves are load-bearing. A `//` line inside a multi-line invocation is
/// not an argument — several of these calls carry one — and a `(` inside a
/// message string is not a nesting level, which would send the paren matcher
/// off the end of the file. `export.rs` and `config.rs` both hold `'"'` and
/// `')'` char literals, so those need the same treatment.
fn strip_noise(text: &str) -> String {
    let src: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < src.len() {
        let c = src[i];
        // Raw string: `r`, zero or more `#`, then a quote.
        if c == 'r' {
            let mut h = i + 1;
            while h < src.len() && src[h] == '#' {
                h += 1;
            }
            if h < src.len() && src[h] == '"' {
                let hashes = h - i - 1;
                out.extend(src[i..=h].iter());
                i = h + 1;
                while i < src.len() {
                    if src[i] == '"' {
                        let mut n = 0;
                        while n < hashes && src.get(i + 1 + n) == Some(&'#') {
                            n += 1;
                        }
                        if n == hashes {
                            out.push('"');
                            (0..hashes).for_each(|_| out.push('#'));
                            i += 1 + hashes;
                            break;
                        }
                    }
                    out.push(if src[i] == '\n' { '\n' } else { ' ' });
                    i += 1;
                }
                continue;
            }
        }
        if c == '"' {
            out.push('"');
            i += 1;
            while i < src.len() && src[i] != '"' {
                if src[i] == '\\' {
                    out.push(' ');
                    i += 1;
                }
                if i < src.len() {
                    out.push(if src[i] == '\n' { '\n' } else { ' ' });
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
            let end = if src.get(i + 1) == Some(&'\\') {
                src.get(i + 3).filter(|&&q| q == '\'').map(|_| i + 3)
            } else {
                src.get(i + 2).filter(|&&q| q == '\'').map(|_| i + 2)
            };
            if let Some(end) = end {
                out.push('\'');
                (i + 1..end).for_each(|_| out.push(' '));
                out.push('\'');
                i = end + 1;
                continue;
            }
        }
        if c == '/' && src.get(i + 1) == Some(&'/') {
            while i < src.len() && src[i] != '\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        if c == '/' && src.get(i + 1) == Some(&'*') {
            while i < src.len() && !(src[i] == '*' && src.get(i + 1) == Some(&'/')) {
                out.push(if src[i] == '\n' { '\n' } else { ' ' });
                i += 1;
            }
            out.push_str("  ");
            i = (i + 2).min(src.len());
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
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

/// One invocation's arguments, split on the commas that separate them rather
/// than on the ones inside a call or a collection literal.
fn split_args(body: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in body.chars() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                args.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    if !cur.trim().is_empty() {
        args.push(cur);
    }
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
///
/// Contents are already blanked by [`strip_noise`], so the literal is found by
/// its delimiters alone.
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

/// A short, single-line form of an argument, for a failure message.
fn snippet(arg: &str) -> String {
    let flat = arg.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 72 {
        format!("{}…", flat.chars().take(72).collect::<String>())
    } else {
        flat
    }
}

/// The field names one invocation declares, or the reason it cannot be read.
///
/// `tracing`'s grammar puts fields before the message, so an unquoted-name
/// field list ends at the first string literal that is *not* followed by `=`.
/// `takes_level` covers `event!`, whose first non-directive argument is the
/// `Level` rather than a field.
fn fields_of(body: &str, takes_level: bool) -> Scanned {
    let mut names = Vec::new();
    let mut level_pending = takes_level;
    for arg in split_args(body) {
        let arg = arg.trim();
        if arg.is_empty() {
            continue;
        }
        // Macro directives, not fields.
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
        // A braced field block. Its `=` signs sit at bracket depth 1, where the
        // scan below does not look, so this would otherwise read as an
        // invocation with no fields at all.
        if arg.starts_with('{') {
            return Scanned::Unanalysable(format!(
                "a braced field block, whose fields this scanner cannot see: {}",
                snippet(arg)
            ));
        }
        if let Some(rest) = after_string_literal(arg) {
            let rest = rest.trim_start();
            // A quoted field name is a field `tracing` records; anything else
            // is the message, which ends the field list.
            if rest.starts_with('=') && !rest.starts_with("==") {
                return Scanned::Unanalysable(format!(
                    "a quoted field name, which this scanner cannot read: {}",
                    snippet(arg)
                ));
            }
            break;
        }
        let chars: Vec<char> = arg.chars().collect();
        let mut depth = 0i32;
        let mut eq = None;
        for (k, &ch) in chars.iter().enumerate() {
            match ch {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                '=' if depth == 0 => {
                    let prev = if k == 0 { ' ' } else { chars[k - 1] };
                    if chars.get(k + 1) != Some(&'=') && !matches!(prev, '!' | '<' | '>' | '=') {
                        eq = Some(k);
                    }
                    break;
                }
                _ => {}
            }
        }
        let name = match eq {
            Some(k) => chars[..k].iter().collect::<String>().trim().to_owned(),
            // Shorthand: `field`, `%field`, `?field`.
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
        names.push(name);
    }
    Scanned::Fields(names)
}

/// Every `tracing::*!` invocation in `text`, analysed.
fn scan_source(text: &str) -> Vec<Scanned> {
    let cleaned = strip_noise(text);
    assert!(
        cleaned.is_ascii(),
        "non-ASCII survived comment and string blanking, so the scanner's byte \
         offsets would not line up with its char offsets"
    );
    let bytes = cleaned.as_bytes();
    let mut out = Vec::new();
    for mac in MACROS {
        let mut from = 0;
        while let Some(hit) = cleaned[from..].find(mac) {
            let after = from + hit + mac.len();
            from = after;
            let mut j = after;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if bytes.get(j) != Some(&b'(') {
                continue;
            }
            let Some(end) = matching_paren(bytes, j) else {
                out.push(Scanned::Unanalysable(
                    "an invocation whose argument list does not close".to_owned(),
                ));
                continue;
            };
            out.push(fields_of(&cleaned[j + 1..end], *mac == "tracing::event!"));
        }
    }
    out
}

/// Every field name logged from shipped sources, with its file.
fn emitted_fields() -> BTreeSet<(String, String)> {
    let mut found = BTreeSet::new();
    for (rel, text) in shipped_sources() {
        for scanned in scan_source(&text) {
            if let Scanned::Fields(names) = scanned {
                for name in names {
                    found.insert((name, rel.clone()));
                }
            }
        }
    }
    found
}

#[test]
fn every_tracing_invocation_is_in_the_analysable_form() {
    let mut refused = Vec::new();
    for (rel, text) in shipped_sources() {
        for scanned in scan_source(&text) {
            if let Scanned::Unanalysable(why) = scanned {
                refused.push(format!("{rel}: {why}"));
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
    for (name, file) in emitted_fields() {
        if !sunrise_log::is_allowed(&name) {
            offenders.push(format!("{name} (logged from {file})"));
        }
    }
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
fn every_tracing_macro_is_called_by_its_full_path() {
    // The scan keys on the literal `tracing::` qualifier, so an invocation that
    // reaches the macro by any other name is invisible to it: `t::warn!` behind
    // a `use tracing as t`, or a `self::`/`crate::` re-export.
    let mut aliased = Vec::new();
    for (rel, text) in shipped_sources() {
        let cleaned = strip_noise(&text);
        for lvl in LEVELS {
            let needle = format!("{lvl}!");
            let mut from = 0;
            while let Some(hit) = cleaned[from..].find(&needle) {
                let at = from + hit;
                from = at + needle.len();
                let before = &cleaned[..at];
                if before.ends_with("tracing::") {
                    continue;
                }
                match before.chars().next_back() {
                    // Reached through some other path.
                    Some(':') => aliased.push(format!(
                        "{rel}: a `…::{needle}` that is not `tracing::{needle}`"
                    )),
                    // Part of a longer identifier — an unrelated macro. An
                    // *alias* of a tracing macro can only get its name from a
                    // `use`, which `the_tracing_macros_are_never_imported`
                    // refuses outright.
                    Some(c) if c.is_ascii_alphanumeric() || c == '_' => {}
                    // A bare invocation.
                    _ => aliased.push(format!("{rel}: a bare `{needle}`")),
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
    for (rel, text) in shipped_sources() {
        let cleaned = strip_noise(&text);
        let mut from = 0;
        while let Some(hit) = cleaned[from..].find("use ") {
            let at = from + hit;
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
                    "{rel}: `{}` renames the whole crate",
                    snippet(stmt)
                ));
                continue;
            }
            let imported = rest
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .any(|tok| LEVELS.contains(&tok));
            if imported {
                imports.push(format!("{rel}: `{}` imports a level macro", snippet(stmt)));
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
fn the_scan_covers_every_crate_in_the_repository() {
    // The walk finds manifests rather than assuming `crates/*`, and this is
    // what proves it: `tools/uniffi-bindgen` sits outside the workspace, so a
    // scan hardcoded to the workspace members would silently skip it.
    let root = workspace_root();
    let rel: BTreeSet<String> = source_roots()
        .iter()
        .map(|p| p.strip_prefix(&root).unwrap_or(p).display().to_string())
        .collect();
    assert!(
        rel.contains("crates/sunrise-server/src"),
        "the walk lost the workspace crates: {rel:?}"
    );
    assert!(
        rel.contains("tools/uniffi-bindgen/src"),
        "the walk does not reach crates outside the workspace: {rel:?}"
    );
    assert!(
        rel.len() >= 24,
        "expected at least 24 crate source roots, found {}: {rel:?}",
        rel.len()
    );
    assert!(
        !rel.iter().any(|p| p.starts_with("legacy/")),
        "`legacy` is excluded from the workspace and from this scan: {rel:?}"
    );
}

#[test]
fn the_field_scan_actually_finds_fields() {
    // The counterpart to `the_scan_actually_finds_events`: a scanner that
    // silently stopped matching would turn the gates above into no-ops.
    let found = emitted_fields();
    let distinct: BTreeSet<&String> = found.iter().map(|(n, _)| n).collect();
    assert!(
        distinct.len() >= 20,
        "expected at least 20 distinct logged field names, found {}: {distinct:?}",
        distinct.len()
    );
    for expected in ["ev", "err_code", "stream_h", "cause"] {
        assert!(
            distinct.iter().any(|n| n.as_str() == expected),
            "the scan missed {expected:?}, which is logged all over the server"
        );
    }
}

#[test]
fn the_field_scanner_reads_the_shapes_it_must() {
    // Every shape the real sources use: a multi-line invocation, a comment
    // inside it naming fields it does not declare, `%` and `?` sigils, a value
    // expression carrying its own commas and parens, bare shorthand, and the
    // message that ends the field list.
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
        vec![Scanned::Fields(
            ["ev", "stream_h", "n_ops", "first_seen_ms", "err"]
                .map(str::to_owned)
                .to_vec()
        )],
        "the scanner must read the declared fields and stop at the message"
    );
}

#[test]
fn the_field_scanner_catches_a_planted_violation() {
    // Proof the allowlist gate has teeth: the shape a leak actually takes, seen
    // by the scanner and refused by the allowlist.
    let scanned = scan_source(r#"tracing::info!(ev = "srv.oops", task_title = %t, "x");"#);
    assert_eq!(
        scanned,
        vec![Scanned::Fields(
            ["ev", "task_title"].map(str::to_owned).to_vec()
        )]
    );
    assert!(
        !sunrise_log::is_allowed("task_title"),
        "and the allowlist must refuse it"
    );
}

/// The four bypass shapes, each of which must now be a *refusal* rather than a
/// silent miss. Every one of these previously scanned as "no offending fields".
#[test]
fn the_field_scanner_refuses_the_forms_it_cannot_read() {
    let why = |src: &str| match scan_source(src).pop().expect("one invocation") {
        Scanned::Unanalysable(why) => why,
        Scanned::Fields(f) => panic!("expected a refusal, got fields {f:?}"),
    };

    // 1. A quoted field name, which `tracing` records and which the old
    //    scanner took for the message — skipping it and everything after it.
    assert!(
        why(r#"tracing::info!(ev = "x", "task_title" = %t, "m");"#).contains("a quoted field name"),
        "a quoted field name must be refused"
    );

    // 2. A braced field block: the `=` sits at bracket depth 1.
    assert!(
        why(r#"tracing::info!({ ev = "x", task_title = %t }, "m");"#)
            .contains("a braced field block"),
        "a braced field block must be refused"
    );

    // 3. A raw-identifier key, recorded by `tracing` as `type`.
    assert!(
        why(r#"tracing::info!(ev = "x", r#type = 1, "m");"#)
            .contains("a raw-identifier field name"),
        "a raw-identifier field name must be refused"
    );

    // 4. Anything else that is not an `ident = value` pair or a bare field.
    assert!(
        why(r#"tracing::info!(ev = "x", some.call(1) + 2, "m");"#)
            .contains("neither `ident = value` nor a bare field name"),
        "an unreadable argument must be refused"
    );
}

/// The fourth shape is a *path* problem rather than an argument problem, so it
/// is caught by the two path gates rather than by `fields_of`.
#[test]
fn an_aliased_macro_path_is_refused_by_the_path_gates() {
    // `t::warn!` and `self::warn!` are both `…::` hits that are not
    // `tracing::`, which is exactly what `every_tracing_macro_is_called_by_its
    // _full_path` reports; a bare `warn!` is the third arm of that match. The
    // `use` that gives an alias its name is what
    // `the_tracing_macros_are_never_imported` refuses.
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
        let cleaned = strip_noise(stmt);
        let rest = cleaned[4..].trim_start().trim_start_matches("::");
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
