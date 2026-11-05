//! The event and field scanner on the workspace and on the shapes it has to
//! survive: a scanner that silently stopped matching would turn every gate
//! into a no-op.

use std::collections::BTreeSet;

use crate::events::{emitted_events, emitted_fields, scan_source, Field, Scanned};
use crate::vocabulary_gates::tracing_import_problem;

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
    // Through the gate's own predicate, not a second copy of it. The copies
    // were what let the gate be broken with the suite still green.
    for stmt in [
        "use tracing::warn;",
        "use tracing::{info, warn};",
        "use tracing::warn as wlog;",
        "use tracing as t;",
    ] {
        assert!(
            tracing_import_problem(stmt).is_some(),
            "{stmt:?} must be refused by the import gate"
        );
    }
    // And it must not refuse the imports this workspace legitimately has.
    for stmt in [
        "use tracing::{Dispatch, Subscriber};",
        "use tracing::field::{Field, Visit};",
        "use tracing::Span;",
    ] {
        assert!(
            tracing_import_problem(stmt).is_none(),
            "{stmt:?} is not a level-macro import"
        );
    }
}
