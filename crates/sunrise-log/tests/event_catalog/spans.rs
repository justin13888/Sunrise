//! The span scanner: every `tracing::*_span!` and `tracing::span!` invocation
//! in the file set, and the fields each one declares.

use std::collections::BTreeSet;

use crate::events::{
    after_string_literal, arg_list, is_assignment, is_tracing_qualified, quoted_name, read_field,
    snippet, split_args, Field,
};
use crate::file_set::shipped_sources;
use crate::source::{is_ident_byte, normalise, word_bang_hits, word_hits, Source};

/// The span macros. Each names its span *before* its fields, where an event
/// ends its fields with a message, so they are read by [`span_of`] rather than
/// the event scan's `fields_of`. `span!` takes a `Level` first, as `event!`
/// does.
pub(crate) const SPAN_MACROS: &[&str] = &[
    "trace_span",
    "debug_span",
    "info_span",
    "warn_span",
    "error_span",
    "span",
];

/// What the scanner made of one span site.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ScannedSpan {
    Span {
        name: String,
        fields: Vec<Field>,
    },
    /// A form the scanner will not guess at, and why.
    Unanalysable(String),
}

/// The name and fields of one span macro's argument list.
///
/// The grammar is `[target: …,] [parent: …,] [Level,] "name" [, field]*`. The
/// first string literal is the name; every argument after it is a field, read
/// by the same [`read_field`] the event scan uses. There is no message, so a
/// second bare literal is not a place the field list ends, as it is for an
/// event — it is a shape this scanner refuses rather than reads past.
fn span_of(src: &Source, span: (usize, usize), takes_level: bool) -> ScannedSpan {
    let refuse = ScannedSpan::Unanalysable;
    let mut name: Option<String> = None;
    let mut fields = Vec::new();
    let mut level_pending = takes_level;
    for (from, to) in split_args(&src.structural, span) {
        let raw = &src.structural[from..to];
        let arg = raw.trim();
        if arg.is_empty() {
            continue;
        }
        let arg_start = from + (raw.len() - raw.trim_start().len());
        if ["target:", "parent:"].iter().any(|d| arg.starts_with(d)) {
            continue;
        }
        if level_pending {
            level_pending = false;
            continue;
        }
        if arg.starts_with('{') {
            return refuse(format!(
                "a braced field block, whose fields this scanner cannot see: {}",
                snippet(arg)
            ));
        }
        if let Some(rest) = after_string_literal(arg) {
            if is_assignment(rest) {
                return refuse(quoted_name(arg));
            }
            if name.is_some() {
                return refuse(format!(
                    "a string literal after the span's name, which is not a field: {}",
                    snippet(arg)
                ));
            }
            let literal = src.literal[arg_start..to].trim();
            let inner = literal
                .strip_prefix('"')
                .and_then(|l| l.strip_suffix('"'))
                .unwrap_or(literal);
            name = Some(inner.to_owned());
            continue;
        }
        if name.is_none() {
            return refuse(format!(
                "a span whose name is not a string literal, so which span these \
                 fields belong to cannot be read: {}",
                snippet(arg)
            ));
        }
        match read_field(src, arg_start, to, arg) {
            Ok(Some(field)) => fields.push(field),
            Ok(None) => {
                return refuse(format!(
                    "a `concat!` where a span field goes, which declares no field: {}",
                    snippet(arg)
                ));
            }
            Err(why) => return refuse(why),
        }
    }
    match name {
        Some(name) => ScannedSpan::Span { name, fields },
        None => refuse("a span macro with no name".to_owned()),
    }
}

/// Offsets of every `#[instrument]`, `#[tracing::instrument]` or
/// `#[tracing_attributes::instrument]` attribute, with or without a leading `::`.
///
/// Found, not read. An `#[instrument]` records every argument of the function
/// it decorates as a span field unless `skip`/`skip_all` says otherwise, names
/// the span after the function, and emits events of its own under `err` and
/// `ret` — so its field names are mostly not written in the attribute at all.
/// Reading `fields(…)` alone would report a vocabulary the span does not have.
/// None exists in shipped code, so each one found is refused, and the first
/// author to need one decides how this scanner should read it.
fn instrument_attributes(structural: &str) -> Vec<usize> {
    word_hits(structural, "instrument")
        .into_iter()
        .filter(|&at| {
            let mut before = structural[..at].trim_end();
            // The attribute is reached as `instrument`, `tracing::instrument`
            // or `tracing_attributes::instrument`, each with or without a
            // leading `::`. A renamed import (`use tracing::instrument as
            // traced;`) is refused by the import gate instead, because a
            // renamed attribute cannot be recognised by shape.
            if let Some(head) = before.strip_suffix("::") {
                let head = head.trim_end();
                let Some(prefix) = ["tracing_attributes", "tracing"]
                    .iter()
                    .find_map(|krate| head.strip_suffix(krate))
                    .filter(|p| !p.as_bytes().last().copied().is_some_and(is_ident_byte))
                else {
                    return false;
                };
                let prefix = prefix.trim_end();
                before = prefix.strip_suffix("::").unwrap_or(prefix).trim_end();
            }
            before
                .strip_suffix('[')
                .is_some_and(|b| b.trim_end().ends_with('#'))
        })
        .collect()
}

/// Every fully-qualified span macro invocation and `#[instrument]` attribute
/// in `text`, analysed.
pub(crate) fn scan_spans(text: &str) -> Vec<ScannedSpan> {
    let src = normalise(text);
    let mut out = Vec::new();
    for mac in SPAN_MACROS {
        for (at, bang) in word_bang_hits(&src.structural, mac) {
            if !is_tracing_qualified(&src.structural[..at]) {
                continue;
            }
            out.push(match arg_list(&src, bang) {
                Ok(span) => span_of(&src, span, *mac == "span"),
                Err(why) => ScannedSpan::Unanalysable(why),
            });
        }
    }
    for _ in instrument_attributes(&src.structural) {
        out.push(ScannedSpan::Unanalysable(
            "an `#[instrument]` attribute, which records the function's arguments \
             as span fields without naming them, so this scanner cannot read its \
             vocabulary"
                .to_owned(),
        ));
    }
    out
}

/// Every span field built in shipped code, as `(file, span name, field name)`.
pub(crate) fn span_fields() -> BTreeSet<(String, String, String)> {
    let mut found = BTreeSet::new();
    for (rel_path, text) in shipped_sources() {
        for scanned in scan_spans(&text) {
            if let ScannedSpan::Span { name, fields } = scanned {
                for f in fields {
                    found.insert((rel_path.clone(), name.clone(), f.name));
                }
            }
        }
    }
    found
}
