//! The event and field scanner: every `tracing::<level>!` invocation in the
//! file set, the fields it declares, and the event names read out of them.

use std::collections::BTreeSet;

use crate::file_set::shipped_sources;
use crate::source::{normalise, word_bang_hits, Source};
use crate::workspace_root;

/// Whether the text ending where a level name begins qualifies it as
/// `tracing::`.
///
/// One predicate, consumed by the field scan and by the path gate. They each
/// carried their own copy, and the copies disagreed: `tracing :: warn!` — legal,
/// fully qualified — was skipped by the scan *and* reported by the gate as a
/// path that is not `tracing::`. Whitespace is allowed on both sides of the
/// `::` because Rust allows it there.
pub(crate) fn is_tracing_qualified(before: &str) -> bool {
    let Some(head) = before.trim_end().strip_suffix("::") else {
        return false;
    };
    let head = head.trim_end();
    let Some(prefix) = head.strip_suffix("tracing") else {
        return false;
    };
    // Not the tail of a longer path or identifier: `foo::tracing::` is not
    // this crate's macro as far as this scanner can tell, so the path gate
    // reports it rather than the field scan reading it.
    !prefix
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == ':')
}

/// The level names, which are what both scans actually look for.
///
/// A fixed `"tracing::warn!"` needle would be wrong twice over: Rust allows
/// whitespace and a newline between a macro path and its `!`, and the path
/// itself is checked separately so an invocation reaching the macro under any
/// other name is reported rather than missed.
pub(crate) const LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error", "event"];

/// One field an invocation declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Field {
    pub(crate) name: String,
    /// The string literal on the right-hand side, when there is one. This is
    /// where an event name comes from, so the catalogue half and the allowlist
    /// half read one invocation exactly once and cannot disagree about it.
    pub(crate) value: Option<String>,
}

impl Field {
    pub(crate) fn named(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            value: None,
        }
    }

    pub(crate) fn with_value(name: &str, value: &str) -> Self {
        Self {
            name: name.to_owned(),
            value: Some(value.to_owned()),
        }
    }
}

/// What the scanner made of one invocation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Scanned {
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
pub(crate) fn split_args(structural: &str, span: (usize, usize)) -> Vec<(usize, usize)> {
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
pub(crate) fn after_string_literal(arg: &str) -> Option<&str> {
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
pub(crate) fn snippet(arg: &str) -> String {
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
            if is_assignment(rest) {
                return Scanned::Unanalysable(quoted_name(arg));
            }
            break;
        }
        match read_field(src, arg_start, to, arg) {
            Ok(Some(field)) => fields.push(field),
            Ok(None) => break,
            Err(why) => return Scanned::Unanalysable(why),
        }
    }
    Scanned::Fields(fields)
}

/// Whether the text after a string literal makes it a quoted field *name*.
pub(crate) fn is_assignment(rest: &str) -> bool {
    let rest = rest.trim_start();
    rest.starts_with('=') && !rest.starts_with("==")
}

pub(crate) fn quoted_name(arg: &str) -> String {
    format!(
        "a quoted field name, which this scanner cannot read: {}",
        snippet(arg)
    )
}

/// One argument read as a field, `None` for a `concat!(…)` with no `=`, or the
/// reason it cannot be read.
///
/// Shared by the event and span scans, so a field is the same shape to both:
/// what one of them refuses, the other refuses for the same reason. `arg` is
/// the trimmed argument, starting at `arg_start` and ending at `to`.
pub(crate) fn read_field(
    src: &Source,
    arg_start: usize,
    to: usize,
    arg: &str,
) -> Result<Option<Field>, String> {
    let bytes = arg.as_bytes();
    let mut depth = 0i32;
    let mut eq = None;
    for (k, &ch) in bytes.iter().enumerate() {
        match ch {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'=' if depth == 0 => {
                let prev = if k == 0 { b' ' } else { bytes[k - 1] };
                if bytes.get(k + 1) != Some(&b'=') && !matches!(prev, b'!' | b'<' | b'>' | b'=') {
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
        return Ok(None);
    }
    let name = match eq {
        Some(k) => arg[..k].trim().to_owned(),
        None => arg.trim_start_matches(['%', '?']).trim().to_owned(),
    };
    if name.starts_with("r#") {
        return Err(format!(
            "a raw-identifier field name, which `tracing` records with the `r#` stripped: {}",
            snippet(arg)
        ));
    }
    if !is_field_name(&name) {
        return Err(format!(
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
    Ok(Some(Field { name, value }))
}

/// The argument list of the invocation whose `!` is at `bang`, as the byte
/// range inside its parentheses.
///
/// `!` may be followed by `(`, `[` or `{` — all three are legal and all three
/// record their fields. Only the first is read, and the other two are
/// *refused*: skipping them is a gate that fails open on a shape the compiler
/// is perfectly happy with.
pub(crate) fn arg_list(src: &Source, bang: usize) -> Result<(usize, usize), String> {
    let bytes = src.structural.as_bytes();
    let mut j = bang + 1;
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    match bytes.get(j) {
        Some(b'(') => matching_paren(bytes, j)
            .map(|end| (j + 1, end))
            .ok_or_else(|| "an invocation whose argument list does not close".to_owned()),
        Some(&d @ (b'[' | b'{')) => Err(format!(
            "an invocation delimited by `{}…{}` rather than `(…)`, which this scanner does not read",
            d as char,
            if d == b'[' { ']' } else { '}' }
        )),
        _ => Err("an invocation with no argument list this scanner can find".to_owned()),
    }
}

/// The event invocation whose `!` is at `bang`.
fn analyse(src: &Source, bang: usize, takes_level: bool) -> Scanned {
    match arg_list(src, bang) {
        Ok(span) => fields_of(src, span, takes_level),
        Err(why) => Scanned::Unanalysable(why),
    }
}

/// Every fully-qualified `tracing::*!` invocation in `text`, analysed.
pub(crate) fn scan_source(text: &str) -> Vec<Scanned> {
    let src = normalise(text);
    let mut out = Vec::new();
    for lvl in LEVELS {
        for (at, bang) in word_bang_hits(&src.structural, lvl) {
            if !is_tracing_qualified(&src.structural[..at]) {
                continue;
            }
            out.push(analyse(&src, bang, *lvl == "event"));
        }
    }
    out
}

/// Every field logged from shipped code, as `(field, file)`.
pub(crate) fn emitted_fields() -> BTreeSet<(Field, String)> {
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
pub(crate) fn emitted_events() -> BTreeSet<(String, String)> {
    emitted_fields()
        .into_iter()
        .filter(|(f, _)| f.name == "ev")
        .filter_map(|(f, file)| f.value.map(|v| (v, file)))
        .collect()
}

/// Every event name appearing in a table row of the catalogue.
pub(crate) fn catalogued_events() -> BTreeSet<String> {
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
