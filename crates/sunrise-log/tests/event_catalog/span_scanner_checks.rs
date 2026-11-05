//! The span scanner on the workspace and on the shapes it has to survive,
//! including the forms it must refuse rather than read.

use std::collections::BTreeSet;

use crate::events::Field;
use crate::spans::{scan_spans, span_fields, ScannedSpan};
use crate::vocabulary_gates::{tracing_import_problem, unqualified_macro_calls};

#[test]
fn the_scan_actually_finds_spans_and_their_fields() {
    // The span gate passes vacuously on a scanner that matches nothing, so it
    // is pinned to the sites that exist: `http.request`, built twice in
    // `api/observe.rs`. Named rather than counted, because a count cannot tell
    // a consolidated site from a scanner that stopped reading one.
    let observe = "crates/sunrise-server/src/api/observe.rs";
    let found: BTreeSet<(String, String)> = span_fields()
        .into_iter()
        .filter(|(file, _, _)| file == observe)
        .map(|(_, span, field)| (span, field))
        .collect();
    for field in ["method", "endpoint", "trace_id", "span_id"] {
        assert!(
            found.contains(&("http.request".to_owned(), field.to_owned())),
            "the span scan did not find `{field}` on `http.request` in {observe}, \
             so the span gate is reading less than ships: {found:?}"
        );
    }
}

#[test]
fn the_span_scanner_reads_the_shapes_it_must() {
    let scanned = scan_spans(
        r#"
        fn spans() {
            let a = tracing::info_span!("http.request", method = %method, endpoint = %endpoint);
            let b = tracing::debug_span!(
                target: "sunrise_core",
                parent: &a,
                "sync.session",
                // a comment naming task_title is not a field
                stream_h = ?h,
                n_ops,
                op_kind = tracing::field::Empty,
            );
            let c = tracing::span!(tracing::Level::WARN, "c.span", %err_code);
            let d = tracing::trace_span!("d.span");
        }
        "#,
    );
    let span = |name: &str, fields: &[&str]| ScannedSpan::Span {
        name: name.to_owned(),
        fields: fields.iter().map(|f| Field::named(f)).collect(),
    };
    assert_eq!(
        scanned,
        vec![
            span("d.span", &[]),
            span("sync.session", &["stream_h", "n_ops", "op_kind"]),
            span("http.request", &["method", "endpoint"]),
            span("c.span", &["err_code"]),
        ],
        "the span scanner must read each name and every field after it"
    );
}

#[test]
fn the_span_scanner_catches_a_planted_violation() {
    let scanned = scan_spans(r#"let s = tracing::info_span!("http.request", task_title = %t);"#);
    assert_eq!(
        scanned,
        vec![ScannedSpan::Span {
            name: "http.request".to_owned(),
            fields: vec![Field::named("task_title")],
        }]
    );
    assert!(
        !sunrise_log::is_allowed("task_title"),
        "and the allowlist must refuse it"
    );
}

#[test]
fn the_span_scanner_refuses_the_forms_it_cannot_read() {
    let why = |src: &str| match scan_spans(src).pop().expect("one span site") {
        ScannedSpan::Unanalysable(why) => why,
        ScannedSpan::Span { name, fields } => {
            panic!("expected a refusal, got span {name:?} with fields {fields:?}")
        }
    };
    for (src, expected) in [
        (
            r#"tracing::info_span!("s", "task_title" = %t);"#,
            "a quoted field name",
        ),
        (
            r#"tracing::info_span!("s", { task_title = %t });"#,
            "a braced field block",
        ),
        (
            r#"tracing::info_span!("s", r#type = 1);"#,
            "a raw-identifier field name",
        ),
        (
            "tracing::info_span!(SPAN_NAME, task_h = %t);",
            "name is not a string literal",
        ),
        (
            r#"tracing::info_span!("s", task_h = %t, "stray");"#,
            "a string literal after the span's name",
        ),
        (
            r#"tracing::info_span!["s", task_title = %t];"#,
            "rather than `(…)`",
        ),
        (
            "#[instrument]\nfn f(task_title: &str) {}",
            "an `#[instrument]` attribute",
        ),
        (
            "#[tracing::instrument(skip_all, fields(task_h))]\nfn f() {}",
            "an `#[instrument]` attribute",
        ),
        (
            "#[::tracing::instrument]\nfn f(task_title: &str) {}",
            "an `#[instrument]` attribute",
        ),
        (
            "#[tracing_attributes::instrument]\nfn f(task_title: &str) {}",
            "an `#[instrument]` attribute",
        ),
    ] {
        assert!(
            why(src).contains(expected),
            "{src:?} must be refused as {expected:?}"
        );
    }
    // `.instrument(span)` and a path through the `instrument` module are not
    // the attribute, and must not be refused as one.
    assert!(scan_spans(
        "use tracing::instrument::WithSubscriber as _;\nfut.instrument(log).await;"
    )
    .is_empty());
}

#[test]
fn an_aliased_span_macro_is_refused_by_the_path_gates() {
    for src in [
        "fn f() { t::info_span!(\"s\", task_title = %t); }",
        "fn f() { info_span!(\"s\", task_title = %t); }",
    ] {
        assert!(
            scan_spans(src).is_empty(),
            "the span scan cannot see {src:?} — which is why the path gates cover spans"
        );
        assert_eq!(
            unqualified_macro_calls(src).len(),
            1,
            "{src:?} must be refused by the path gate"
        );
    }
    assert!(
        unqualified_macro_calls(
            "fn f() { tracing::info_span!(\"s\"); tracing::span!(Level::INFO, \"s\"); }"
        )
        .is_empty(),
        "a fully-qualified span macro must pass the path gate"
    );
    for stmt in [
        "use tracing::info_span;",
        "use tracing::{span, Level};",
        "use tracing::span;",
        "use tracing::instrument as traced;",
        "use tracing::{instrument, Level};",
    ] {
        assert!(
            tracing_import_problem(stmt).is_some(),
            "{stmt:?} must be refused by the import gate"
        );
    }
    // A path through the `span` module imports no macro, and
    // `sunrise-log`'s own `test_util.rs` has exactly that import.
    assert!(tracing_import_problem("use tracing::span::{Attributes, Id, Record};").is_none());
    // Likewise a path through the `instrument` module, which `sunrise-core`'s
    // tests import, and the `Instrument` trait.
    assert!(tracing_import_problem("use tracing::instrument::WithSubscriber as _;").is_none());
    assert!(tracing_import_problem("use tracing::Instrument;").is_none());
}
