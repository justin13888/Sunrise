//! Trace and span propagation.
//!
//! Per `spec/10-cross-cutting/logging.md` §4, every log record carries:
//! - `trace`: the root trace id (ULID), started by the entry point that
//!   initiates user-visible work.
//! - `span`: the current span id (ULID), nested within a trace.
//!
//! For top-level events, `trace == span`. The propagation across the wire is
//! out-of-scope for this crate (the wire protocol carries `x-sunrise-trace` /
//! a `trace` frame field, see `sunrise-wire-protocol`).
//!
//! This module gives a minimal task-local stack of `(trace, span)` and a
//! helper `with_span` that pushes/pops a span for the duration of a closure.
//! It does NOT depend on `tracing`; downstream crates can adapt as needed.

use core::cell::RefCell;

/// Trace identifier (a ULID rendered as 26-char Crockford base32).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TraceId(pub [u8; 16]);

/// Span identifier (a ULID rendered as 26-char Crockford base32).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpanId(pub [u8; 16]);

impl TraceId {
    /// Render as 26-char Crockford base32 ULID string.
    #[must_use]
    pub fn to_ulid_string(self) -> String {
        crockford_base32_ulid(&self.0)
    }
}

impl SpanId {
    /// Render as 26-char Crockford base32 ULID string.
    #[must_use]
    pub fn to_ulid_string(self) -> String {
        crockford_base32_ulid(&self.0)
    }
}

/// Crockford base-32 ULID encoder (no checksum). Produces 26 chars.
///
/// This is duplicated here (rather than depending on `sunrise-id`) to keep
/// `sunrise-log` at the bottom of the dependency graph. The byte layout
/// matches the canonical ULID scheme.
fn crockford_base32_ulid(bytes: &[u8; 16]) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    // ULID is 128 bits, encoded as 26 chars where the high 2 bits of the first
    // char come from the top 6 bits left-padded by 2 zero bits.
    let mut out = String::with_capacity(26);
    let mut buf: u128 = 0;
    for &b in bytes {
        buf = (buf << 8) | u128::from(b);
    }
    // 26 * 5 = 130 bits; we pad two zero bits at the top.
    buf <<= 2;
    for i in (0..26).rev() {
        let idx = ((buf >> (i * 5)) & 0x1f) as usize;
        out.push(ALPHABET[idx] as char);
    }
    out
}

/// Active `(trace, span)` for the current thread.
#[derive(Default, Clone, Copy)]
struct ActiveContext {
    trace: Option<TraceId>,
    span: Option<SpanId>,
}

thread_local! {
    static ACTIVE: RefCell<ActiveContext> = const { RefCell::new(ActiveContext { trace: None, span: None }) };
}

/// Snapshot of the current `(trace, span)` for embedding in a record.
#[must_use]
pub fn current() -> (Option<TraceId>, Option<SpanId>) {
    ACTIVE.with(|c| {
        let c = c.borrow();
        (c.trace, c.span)
    })
}

/// Run `f` with the given `(trace, span)` installed; restores the prior pair on
/// completion (panic-safe via Drop).
pub fn with_span<F, R>(trace: TraceId, span: SpanId, f: F) -> R
where
    F: FnOnce() -> R,
{
    let _g = SpanGuard::enter(trace, span);
    f()
}

struct SpanGuard {
    prior: ActiveContext,
}

impl SpanGuard {
    fn enter(trace: TraceId, span: SpanId) -> Self {
        let prior = ACTIVE.with(|c| {
            let prior = *c.borrow();
            *c.borrow_mut() = ActiveContext {
                trace: Some(trace),
                span: Some(span),
            };
            prior
        });
        Self { prior }
    }
}

impl Drop for SpanGuard {
    fn drop(&mut self) {
        ACTIVE.with(|c| {
            *c.borrow_mut() = self.prior;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulid_roundtrip_length() {
        let bytes = [0u8; 16];
        let s = crockford_base32_ulid(&bytes);
        assert_eq!(s.len(), 26);
        assert!(s
            .chars()
            .all(|c| "0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(c)));
    }

    #[test]
    fn ulid_distinct() {
        let s1 = crockford_base32_ulid(&[0u8; 16]);
        let mut bytes = [0u8; 16];
        bytes[15] = 1;
        let s2 = crockford_base32_ulid(&bytes);
        assert_ne!(s1, s2);
    }

    #[test]
    fn span_guard_restores_on_drop() {
        assert_eq!(current(), (None, None));
        let t = TraceId([1u8; 16]);
        let s = SpanId([2u8; 16]);
        with_span(t, s, || {
            assert_eq!(current(), (Some(t), Some(s)));
        });
        assert_eq!(current(), (None, None));
    }

    #[test]
    fn span_guard_nests() {
        let t1 = TraceId([1u8; 16]);
        let s1 = SpanId([2u8; 16]);
        let t2 = TraceId([3u8; 16]);
        let s2 = SpanId([4u8; 16]);
        with_span(t1, s1, || {
            assert_eq!(current(), (Some(t1), Some(s1)));
            with_span(t2, s2, || {
                assert_eq!(current(), (Some(t2), Some(s2)));
            });
            assert_eq!(current(), (Some(t1), Some(s1)));
        });
    }
}
