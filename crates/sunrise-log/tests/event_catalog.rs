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
//! Both scan `crates/*/src` — the shipped surfaces — so test fixtures
//! inventing their own event names do not have to be catalogued.

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

/// Every `ev = "…"` literal in shipped crate sources, with the file it came
/// from.
fn emitted_events() -> BTreeSet<(String, String)> {
    let root = workspace_root();
    let mut files = Vec::new();
    for crate_dir in std::fs::read_dir(root.join("crates"))
        .expect("crates/ exists")
        .flatten()
    {
        rust_sources(&crate_dir.path().join("src"), &mut files);
    }
    assert!(!files.is_empty(), "found no crate sources to scan");

    let mut found = BTreeSet::new();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let rel = file
            .strip_prefix(&root)
            .unwrap_or(&file)
            .display()
            .to_string();
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
// is now scanned the same way the event vocabulary is.

/// The macros this scanner reads, fully qualified.
///
/// Every emission in the workspace writes `tracing::warn!` rather than a bare
/// `warn!`; [`no_bare_tracing_macro_invocations`] is what keeps that true, and
/// with it the scan complete.
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

/// The field names one invocation declares.
///
/// `tracing`'s grammar puts fields before the message, so the first
/// string-literal argument ends the field list: everything after it is a
/// format argument, which is recorded under no name of its own.
fn fields_of(body: &str) -> Vec<String> {
    let mut names = Vec::new();
    for arg in split_args(body) {
        let arg = arg.trim();
        if arg.is_empty() {
            continue;
        }
        if arg.starts_with('"') || arg.starts_with("r\"") || arg.starts_with("r#") {
            break;
        }
        // Macro directives, not fields.
        if ["target:", "parent:", "name:"]
            .iter()
            .any(|d| arg.starts_with(d))
        {
            continue;
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
        if is_field_name(&name) {
            names.push(name);
        }
    }
    names
}

/// Every field name declared by a `tracing::*!` invocation in `text`.
fn fields_in_source(text: &str) -> Vec<String> {
    let cleaned = strip_noise(text);
    assert!(
        cleaned.is_ascii(),
        "non-ASCII survived comment and string blanking, so the scanner's byte \
         offsets would not line up with its char offsets"
    );
    let bytes = cleaned.as_bytes();
    let mut names = Vec::new();
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
                continue;
            };
            names.extend(fields_of(&cleaned[j + 1..end]));
        }
    }
    names
}

/// Every field name logged from shipped crate sources, with its file.
fn emitted_fields() -> BTreeSet<(String, String)> {
    let root = workspace_root();
    let mut files = Vec::new();
    for crate_dir in std::fs::read_dir(root.join("crates"))
        .expect("crates/ exists")
        .flatten()
    {
        rust_sources(&crate_dir.path().join("src"), &mut files);
    }
    assert!(!files.is_empty(), "found no crate sources to scan");

    let mut found = BTreeSet::new();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let rel = file
            .strip_prefix(&root)
            .unwrap_or(&file)
            .display()
            .to_string();
        for name in fields_in_source(&text) {
            found.insert((name, rel.clone()));
        }
    }
    found
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
fn no_bare_tracing_macro_invocations() {
    // The scan keys on the `tracing::` qualifier, so `use tracing::warn;` and a
    // bare `warn!(…)` would be invisible to it. Nothing does that today, and
    // this is what keeps the gate above complete.
    let root = workspace_root();
    let mut files = Vec::new();
    for crate_dir in std::fs::read_dir(root.join("crates"))
        .expect("crates/ exists")
        .flatten()
    {
        rust_sources(&crate_dir.path().join("src"), &mut files);
    }
    let mut bare = Vec::new();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let rel = file
            .strip_prefix(&root)
            .unwrap_or(&file)
            .display()
            .to_string();
        let cleaned = strip_noise(&text);
        for lvl in LEVELS {
            let needle = format!("{lvl}!");
            let mut from = 0;
            while let Some(hit) = cleaned[from..].find(&needle) {
                let at = from + hit;
                from = at + needle.len();
                if cleaned[..at].ends_with("tracing::") {
                    continue;
                }
                // Part of a longer path or identifier, not an invocation.
                if cleaned[..at]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
                {
                    continue;
                }
                bare.push(format!("{needle} in {rel}"));
            }
        }
    }
    assert!(
        bare.is_empty(),
        "these are bare tracing macro invocations, which the field scan cannot \
         see. Write them as `tracing::{{level}}!` instead:\n  {}",
        bare.join("\n  ")
    );
}

#[test]
fn the_field_scan_actually_finds_fields() {
    // The counterpart to `the_scan_actually_finds_events`: a scanner that
    // silently stopped matching would turn the gate above into a no-op.
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
    let found = fields_in_source(
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
        found,
        ["ev", "stream_h", "n_ops", "first_seen_ms", "err"],
        "the scanner must read the declared fields and stop at the message"
    );
}

#[test]
fn the_field_scanner_catches_a_planted_violation() {
    // Proof the gate has teeth: the shape a leak actually takes, seen by the
    // scanner and refused by the allowlist.
    let found = fields_in_source(r#"tracing::info!(ev = "srv.oops", task_title = %t, "x");"#);
    assert!(
        found.iter().any(|n| n == "task_title"),
        "the scanner missed a planted field: {found:?}"
    );
    assert!(
        !sunrise_log::is_allowed("task_title"),
        "and the allowlist must refuse it"
    );
}
