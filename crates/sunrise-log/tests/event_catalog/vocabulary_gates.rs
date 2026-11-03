//! The gates over what shipped code logs: every invocation is analysable,
//! every event and span field is on the redaction allowlist, every event is
//! catalogued and grammatical, and every `tracing` macro is reached by its
//! full path.

use crate::events::{
    catalogued_events, emitted_events, emitted_fields, is_tracing_qualified, scan_source, snippet,
    Scanned, LEVELS,
};
use crate::file_set::shipped_sources;
use crate::source::{normalise, word_bang_hits, word_hits};
use crate::spans::{scan_spans, span_fields, ScannedSpan, SPAN_MACROS};

#[test]
fn every_tracing_invocation_is_in_the_analysable_form() {
    let mut refused = Vec::new();
    for (rel_path, text) in shipped_sources() {
        for scanned in scan_source(&text) {
            if let Scanned::Unanalysable(why) = scanned {
                refused.push(format!("{rel_path}: {why}"));
            }
        }
        for scanned in scan_spans(&text) {
            if let ScannedSpan::Unanalysable(why) = scanned {
                refused.push(format!("{rel_path}: span: {why}"));
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
        "these field names are logged from a file compiled into a workspace \
         library or binary, but are not on \
         the allowlist in crates/sunrise-log/src/field.rs. `RedactionLayer` \
         refuses the *whole event* on the first one it sees — a panic under \
         debug_assertions, a silent drop and a violations() bump in release.\n\n\
         The scan covers `#[cfg(test)]` unit tests inside these files too, \
         because they are not separable from the file that ships. If one of \
         these is a unit test, it is not a production defect — use an \
         allowlisted field name, or `_` the value out of the event:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn every_span_field_name_is_on_the_redaction_allowlist() {
    // `RedactionLayer` cannot veto a span — `on_new_span` has no veto, and a
    // span cannot be rewritten once created — so a span field off the
    // allowlist would reach every record written inside it. This is the gate
    // that stands where the layer cannot.
    let offenders: Vec<String> = span_fields()
        .into_iter()
        .filter(|(_, _, field)| !sunrise_log::is_allowed(field))
        .map(|(file, span, field)| format!("{field} on span {span:?} (built in {file})"))
        .collect();
    assert!(
        offenders.is_empty(),
        "these span field names are built in a file compiled into a workspace \
         library or binary, but are not on the allowlist in \
         crates/sunrise-log/src/field.rs. No layer refuses a span, so each \
         would ride every record written inside it, in every build. Use an \
         allowlisted name, or add this one to the allowlist with the argument \
         that it carries no user content:\n  {}",
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
        "these event names are emitted from a file compiled into a workspace \
         library or binary, but are not in \
         docs/10-cross-cutting/log-events.md.\n\n\
         The scan covers `#[cfg(test)]` unit tests inside those files. **Do not \
         add a test fixture's event name to the catalogue** — it is the record \
         of what an operator can see in production, and inventing entries for \
         it is the exact dishonesty this file exists to prevent. A unit test \
         that needs an event should reuse one already catalogued:\n  {}",
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

/// Every level or span macro invocation in `text` that is not reached as
/// `tracing::<macro>!`, described.
///
/// The gate and the test that proves the gate both call this, as they do
/// [`tracing_import_problem`], so dropping a macro family from it fails a test.
pub(crate) fn unqualified_macro_calls(text: &str) -> Vec<String> {
    let src = normalise(text);
    let mut aliased = Vec::new();
    for lvl in LEVELS.iter().chain(SPAN_MACROS) {
        for (at, _) in word_bang_hits(&src.structural, lvl) {
            let before = src.structural[..at].trim_end();
            if is_tracing_qualified(before) {
                continue;
            }
            if before.ends_with("::") {
                aliased.push(format!("a `…::{lvl}!` that is not `tracing::{lvl}!`"));
            } else {
                aliased.push(format!("a bare `{lvl}!`"));
            }
        }
    }
    aliased
}

#[test]
fn every_tracing_macro_is_called_by_its_full_path() {
    // The scan keys on the `tracing::` qualifier, so an invocation that reaches
    // the macro by any other name is invisible to it: `t::warn!` behind a
    // `use tracing as t`, or a `self::`/`crate::` re-export.
    let mut aliased = Vec::new();
    for (rel_path, text) in shipped_sources() {
        for why in unqualified_macro_calls(&text) {
            aliased.push(format!("{rel_path}: {why}"));
        }
    }
    assert!(
        aliased.is_empty(),
        "these reach a `tracing` macro by a name the field scan does not look \
         for, so its fields are checked by nothing. Call them as \
         `tracing::<level>!` or `tracing::<level>_span!`. This applies to `#[cfg(test)]` unit tests inside \
         a shipped file as well, which the scan cannot separate from the file \
         that ships:\n  {}",
        aliased.join("\n  ")
    );
}

/// Why this `use` statement would let an invocation escape the field scan, if
/// it would.
///
/// The gate and the test that proves the gate both call this. They each had
/// their own copy of the logic, which meant the gate had no test at all:
/// breaking it left the suite green.
pub(crate) fn tracing_import_problem(stmt: &str) -> Option<&'static str> {
    let rest = stmt
        .trim()
        .strip_prefix("use ")?
        .trim_start()
        .trim_start_matches("::");
    if !rest.starts_with("tracing") {
        return None;
    }
    if rest
        .trim_start_matches("tracing")
        .trim_start()
        .starts_with("as ")
    {
        return Some("renames the whole crate");
    }
    if rest
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|tok| LEVELS.contains(&tok))
    {
        return Some("imports a level macro");
    }
    // `tracing::span` is a module as well as a macro, and `use tracing::span;`
    // imports both. A path *through* it (`tracing::span::Id`) imports neither,
    // so `span` counts only where no `::` follows it.
    let imports_macro = |mac: &str| {
        word_hits(rest, mac)
            .into_iter()
            .any(|at| !rest[at + mac.len()..].trim_start().starts_with("::"))
    };
    if SPAN_MACROS.iter().any(|mac| imports_macro(mac)) {
        return Some("imports a span macro");
    }
    // `tracing::instrument` is likewise a module and an attribute. Imported,
    // the attribute can be renamed (`use tracing::instrument as traced;`), and
    // `#[traced]` is a shape the `#[instrument]` refusal cannot recognise.
    imports_macro("instrument").then_some("imports the `#[instrument]` attribute")
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
            if let Some(why) = tracing_import_problem(stmt) {
                imports.push(format!("{rel_path}: `{}` {why}", snippet(stmt)));
            }
        }
    }
    assert!(
        imports.is_empty(),
        "these import a `tracing` level macro (or the crate under another \
         name), which allows a bare or renamed invocation the field scan cannot \
         see. Call the macros by their full `tracing::<level>!` path instead — \
         including inside a `#[cfg(test)]` module, which the scan cannot \
         separate from the file that ships:\n  {}",
        imports.join("\n  ")
    );
}
