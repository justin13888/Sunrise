//! Redaction property tests per `docs/10-cross-cutting/logging.md` §6.3.
//!
//! There are two independent defences and this file exercises both against
//! the *real* subscriber stack — an `EnvFilter` at `trace`, the
//! `RedactionLayer`, and the NDJSON formatter writing into a capture buffer.
//! Every assertion is on the bytes a sink would have received.
//!
//! 1. **Type-level.** A `Plain<T>` has no `Display`, no `Serialize`, and no
//!    `tracing::Value`, and its `Debug` is opaque, so no tracing path can
//!    render the payload. [`plain_never_reaches_the_sink`] pushes a random
//!    payload through every path that compiles — event field with `?`,
//!    `field::debug`, message interpolation, span field, error event, and a
//!    nested `Plain<Plain<_>>` — and asserts the sink saw `Plain<…>` and not
//!    one byte of the payload.
//!
//! 2. **Vocabulary.** A payload that was legitimately `.expose()`d and then
//!    logged is caught by the field-name allowlist:
//!    [`exposed_payload_under_an_unvetted_field_is_refused`] asserts the whole
//!    event is dropped, not merely stripped.
//!
//! The previous version of this test was tautological: it built a payload,
//! wrapped it, then emitted two events that referenced neither, and asserted
//! the unlogged string was absent. Every emission below names the payload.

use proptest::prelude::*;
use sunrise_log::{
    build_subscriber_with, Capture, LogConfig, LogFormat, LogTarget, Plain, RedactionLayer,
    ViolationPolicy,
};
use tracing::field;

/// Events must come from a `sunrise_*` target or `RedactionLayer` will not
/// claim them — the layer deliberately leaves third-party vocabularies alone.
const TARGET: &str = "sunrise_log_redaction_test";

/// A long, non-natural marker prefixed to every generated payload, so a
/// substring hit in the output is a real leak rather than a coincidental
/// collision with NDJSON punctuation.
const SENTINEL: &str = "SUNRISE_REDACTION_SENTINEL_DO_NOT_LEAK_";

fn capture_stack(cap: &Capture, policy: ViolationPolicy) -> tracing::Dispatch {
    build_subscriber_with(
        LogConfig {
            target: LogTarget::Capture(cap.clone()),
            // `trace` so nothing is dropped for being too verbose: this test
            // is about redaction, not filtering.
            filter: "trace".to_string(),
            format: LogFormat::Ndjson,
        },
        RedactionLayer::new().with_policy(policy),
    )
    .expect("capture subscriber builds")
}

/// Emit `payload` through every route a `Plain<T>` can take into a record.
///
/// Only field names on the allowlist are used, so what is under test here is
/// the *value* defence, not the vocabulary one.
fn emit_through_every_plain_path(payload: &str) {
    let plain = Plain::new(payload.to_string());

    // 1. Event field, `?` (Debug) sigil. `%` and a bare value do not compile.
    tracing::info!(target: TARGET, ev = "test.plain.debug_sigil", stream_h = ?plain, "debug sigil");

    // 2. Event field via the explicit `field::debug` adapter.
    tracing::info!(
        target: TARGET,
        ev = "test.plain.field_debug",
        cause = field::debug(&plain),
        "field adapter"
    );

    // 3. Interpolated into the message body itself.
    tracing::info!(target: TARGET, ev = "test.plain.message", "interpolated {plain:?}");

    // 4. Span field, rendered into every event emitted inside the span
    //    (`with_current_span(true)`).
    let span = tracing::info_span!(target: TARGET, "plain_span", op_kind = ?plain);
    span.in_scope(|| {
        tracing::info!(target: TARGET, ev = "test.plain.in_span", "inside span");
    });

    // 5. Recorded onto a span after creation.
    let late = tracing::info_span!(target: TARGET, "late_span", op_kind = tracing::field::Empty);
    late.record("op_kind", field::debug(&plain));
    late.in_scope(|| {
        tracing::info!(target: TARGET, ev = "test.plain.late_span", "after record");
    });

    // 6. Error event with an error envelope.
    tracing::error!(
        target: TARGET,
        ev = "test.plain.error",
        err_code = "TEST_REJECT",
        err_kind = "internal",
        retryable = false,
        cause = ?plain,
        "error envelope"
    );

    // 7. Nested wrapper — `map` and re-wrapping must not unwrap.
    let nested = Plain::new(Plain::new(payload.to_string()));
    tracing::warn!(target: TARGET, ev = "test.plain.nested", task_h = ?nested, "nested wrapper");

    // 8. Non-string payloads: the guarantee must not depend on `T`.
    let bytes = Plain::new(payload.as_bytes().to_vec());
    tracing::info!(target: TARGET, ev = "test.plain.bytes", n_bytes = 1u64, note_h = ?bytes, "byte payload");
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// No byte of a `Plain<T>` payload reaches the sink through any path, and
    /// the opaque marker reaches it instead — so the emissions really happened.
    #[test]
    fn plain_never_reaches_the_sink(suffix in "[a-zA-Z0-9]{1,64}") {
        let payload = format!("{SENTINEL}{suffix}");
        let cap = Capture::new();
        tracing::dispatcher::with_default(&capture_stack(&cap, ViolationPolicy::Panic), || {
            emit_through_every_plain_path(&payload);
        });

        let out = cap.contents();
        prop_assert!(
            !out.contains(SENTINEL),
            "payload leaked into the sink: {out}"
        );
        // The negative assertion above is only meaningful if the events were
        // actually emitted and actually mentioned the wrapper.
        prop_assert!(
            out.matches("Plain<").count() >= 7,
            "expected the opaque marker on every path, got: {out}"
        );
        prop_assert!(cap.lines().len() >= 7, "expected one record per path, got: {out}");
    }

    /// A payload that was `.expose()`d and then logged under a field name
    /// nobody vetted is refused outright — the record does not appear at all.
    #[test]
    fn exposed_payload_under_an_unvetted_field_is_refused(suffix in "[a-zA-Z0-9]{1,64}") {
        let payload = format!("{SENTINEL}{suffix}");
        let cap = Capture::new();
        tracing::dispatcher::with_default(&capture_stack(&cap, ViolationPolicy::Drop), || {
            let wrapper = Plain::new(payload.clone());
            let exposed = wrapper.expose();
            // Exactly the mistake the allowlist exists for: a real string,
            // a plausible-looking field name, no `Plain` in sight.
            tracing::info!(target: TARGET, ev = "test.redaction.leak", task_title = %exposed, "leak attempt");
        });

        prop_assert!(!cap.contents().contains(SENTINEL), "exposed payload leaked");
        prop_assert!(cap.lines().is_empty(), "the whole record must be dropped, got: {}", cap.contents());
    }
}

/// The allowlist gate must not swallow legitimate records.
#[test]
fn allowlisted_fields_pass_through() {
    let cap = Capture::new();
    tracing::dispatcher::with_default(&capture_stack(&cap, ViolationPolicy::Panic), || {
        tracing::info!(
            target: TARGET,
            ev = "test.redaction.ok",
            stream_h = "abc12345",
            epoch = 4u64,
            lat_ms = 12u64,
            "allowlisted"
        );
    });
    let lines = cap.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains("\"ev\":\"test.redaction.ok\""),
        "{lines:?}"
    );
    assert!(lines[0].contains("\"stream_h\":\"abc12345\""), "{lines:?}");
}

/// Third-party targets keep their own vocabulary: the gate must not silently
/// delete `tower_http`'s request logs because it has never heard of `latency`.
#[test]
fn foreign_targets_are_not_gated() {
    let cap = Capture::new();
    tracing::dispatcher::with_default(&capture_stack(&cap, ViolationPolicy::Panic), || {
        tracing::info!(target: "tower_http::trace::on_response", latency = "3 ms", status = 200, "finished");
    });
    assert_eq!(cap.lines().len(), 1, "{:?}", cap.lines());
}

/// A refused event is dropped in isolation: the next, well-formed record on
/// the same target still gets through.
#[test]
fn one_bad_event_does_not_poison_the_stream() {
    let cap = Capture::new();
    tracing::dispatcher::with_default(&capture_stack(&cap, ViolationPolicy::Drop), || {
        tracing::info!(target: TARGET, ev = "test.redaction.bad", note_body = "x", "bad");
        tracing::info!(target: TARGET, ev = "test.redaction.good", n_ops = 1u64, "good");
    });
    let lines = cap.lines();
    assert_eq!(lines.len(), 1, "only the vetted record survives: {lines:?}");
    assert!(lines[0].contains("test.redaction.good"), "{lines:?}");
}

/// Level filtering still works — the redaction layer must not accidentally
/// re-enable events `EnvFilter` turned off.
#[test]
fn env_filter_still_applies() {
    let cap = Capture::new();
    let dispatch = build_subscriber_with(
        LogConfig {
            target: LogTarget::Capture(cap.clone()),
            filter: "warn".to_string(),
            format: LogFormat::Ndjson,
        },
        RedactionLayer::new(),
    )
    .expect("subscriber builds");
    tracing::dispatcher::with_default(&dispatch, || {
        tracing::info!(target: TARGET, ev = "test.filter.info", "below threshold");
        tracing::warn!(target: TARGET, ev = "test.filter.warn", "at threshold");
    });
    let lines = cap.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("test.filter.warn"), "{lines:?}");
}
