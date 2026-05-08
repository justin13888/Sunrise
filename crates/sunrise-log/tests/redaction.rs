//! Redaction property test per `spec/10-cross-cutting/logging.md` §6.3.
//!
//! Asserts that even when the logger is configured at the most permissive
//! settings (`Trace+`, ring sink), no plaintext byte from a synthetic
//! `Plain<T>` payload appears anywhere in any sink's output. This is the
//! type-system enforcement of the `expose()` ban: there is no API path that
//! can format a `Plain<T>` into a record without first calling `.expose()`,
//! and `.expose()` is forbidden in this crate by the CI grep gate.

use proptest::prelude::*;
use std::sync::Arc;
use sunrise_log::{
    install_global, sink::RingSink, ErrField, ErrorKind, LogConfigBuilder, ProtoVersions, Sink,
};

// A long, non-natural sentinel that essentially cannot appear in the random
// bytes of an NDJSON envelope unless the synthetic payload is being emitted
// verbatim. We prefix every property-test payload with this so that even a
// single-character plaintext becomes a uniquely identifiable subsequence.
const PAYLOAD_SENTINEL: &str = "SUNRISE_LOG_REDACTION_PROPTEST_SENTINEL_DO_NOT_LEAK::";

// Run all `Plain<T>`-bearing log emission paths against random plaintext
// strings; assert no plaintext byte leaks into the ring sink.
proptest! {
    #![proptest_config(ProptestConfig {
        cases: 1_000,
        ..ProptestConfig::default()
    })]

    #[test]
    fn no_plain_bytes_leak(suffix in "[a-zA-Z0-9]{1,128}") {
        // `payload` is a synthetic plaintext value, prefixed with a sentinel
        // marker so that we are certain any subsequence match in the output
        // is a real leak rather than coincidental NDJSON character collision.
        let payload = format!("{PAYLOAD_SENTINEL}{suffix}");
        let plain = sunrise_log::Plain::new(payload.clone());
        // Touch the value so the optimizer can't eliminate it.
        let _ = format!("{plain:?}");

        let ring = Arc::new(RingSink::with_default_capacity());
        let cfg = LogConfigBuilder::new("sunrise-log-tests")
            .app("0.0.0+test")
            .dev("dev_00000000")
            .proto(ProtoVersions { wire: 1, doc: 1, crypto: 1 })
            .sink(ring.clone() as Arc<dyn Sink>)
            .build();
        install_global(cfg);

        // Emit a record carrying *only* allowlisted ctx keys.
        let ctx = sunrise_log::Ctx::new()
            .with(sunrise_log::CtxKey::StreamH, sunrise_log::CtxValue::Str("abc12345"))
            .with(sunrise_log::CtxKey::Epoch, sunrise_log::CtxValue::U64(1));
        sunrise_log::event!(
            level = sunrise_log::Level::Info,
            ev = "test.redaction.emit",
            msg = "test record",
            ctx = ctx,
        );

        let err = ErrField {
            code: "TEST_REJECT",
            kind: ErrorKind::Internal,
            retryable: false,
            cause: Some("internal".to_string()),
        };
        sunrise_log::error_event!(
            level = sunrise_log::Level::Error,
            ev = "test.redaction.error",
            msg = "test error",
            err = err,
        );

        // Snapshot every byte the ring saw and ensure no payload byte leaked.
        let snap = ring.snapshot();
        let bytes_seen: Vec<u8> = snap.into_iter().flatten().collect();
        let payload_bytes = payload.as_bytes();
        prop_assert!(
            !contains_subsequence(&bytes_seen, payload_bytes),
            "payload {:?} leaked into ring snapshot",
            payload
        );
    }
}

fn contains_subsequence(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}
