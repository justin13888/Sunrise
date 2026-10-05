//! The only attributes a Sunrise span can carry.
//!
//! `docs/10-cross-cutting/logging.md` §6 lists what may never reach a log
//! surface, and a span is one: its name, its attributes and its events leave
//! the process for a collector the operator runs. The log path enforces that
//! rule twice over, with `Plain<T>` on the value and `RedactionLayer` on the
//! field name. A span has no layer to veto it, so the rule is held here by
//! construction instead: [`Attr`] has no public constructor that takes a key,
//! every key it can produce is one of [`KEYS`], and every one of those is on
//! `sunrise_log`'s allowlist (`every_key_is_on_the_log_allowlist` below).
//!
//! Values are held the same way. Most constructors take a number, a `bool` or
//! a `&'static str` — a literal chosen at the call site, never interpolated
//! from a request. The one runtime string, [`Attr::endpoint`], is documented
//! as the matched route's template and the server takes it from nowhere else.
//! There is deliberately no constructor for an id of any kind, hashed or not:
//! a trace already groups one request's work, so a correlation handle would
//! only add a way to follow one account across traces.

use opentelemetry::KeyValue;

/// Every attribute key a span can carry.
///
/// Each is a name `sunrise_log::field::ALLOWED` already admits, used with the
/// meaning it has in a log record, so a span attribute and a log field of the
/// same name can be joined.
pub const KEYS: &[&str] = &[
    METHOD, ENDPOINT, STATUS, RESULT, REASON, PROVIDER, RESUMED, ATTEMPT, N_OPS, N_BYTES, N_CHUNKS,
    N_STREAMS,
];

const METHOD: &str = "method";
const ENDPOINT: &str = "endpoint";
const STATUS: &str = "status";
const RESULT: &str = "result";
const REASON: &str = "reason";
const PROVIDER: &str = "provider";
const RESUMED: &str = "resumed";
const ATTEMPT: &str = "attempt";
const N_OPS: &str = "n_ops";
const N_BYTES: &str = "n_bytes";
const N_CHUNKS: &str = "n_chunks";
const N_STREAMS: &str = "n_streams";

/// What a [`Attr::count`] counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Count {
    /// Ops in a batch: `n_ops`.
    Ops,
    /// Bytes moved: `n_bytes`.
    Bytes,
    /// Blob chunks: `n_chunks`.
    Chunks,
    /// Live event streams, such as the subscribers a frame was fanned out to:
    /// `n_streams`, the meaning `srv.stop.draining` gives it in the log.
    Streams,
}

impl Count {
    const fn key(self) -> &'static str {
        match self {
            Self::Ops => N_OPS,
            Self::Bytes => N_BYTES,
            Self::Chunks => N_CHUNKS,
            Self::Streams => N_STREAMS,
        }
    }
}

/// One span attribute, from the closed set this module defines.
#[derive(Debug, Clone, PartialEq)]
pub struct Attr(KeyValue);

impl Attr {
    /// The HTTP method, as the route table spells it.
    #[must_use]
    pub fn method(method: &'static str) -> Self {
        Self(KeyValue::new(METHOD, method))
    }

    /// The matched route's template, such as `/api/v1/devices/:id`.
    ///
    /// Never a request's own path: a path carries ids, and its query string is
    /// where a bearer would sit.
    #[must_use]
    pub fn endpoint(template: &str) -> Self {
        Self(KeyValue::new(ENDPOINT, template.to_owned()))
    }

    /// The HTTP status returned.
    #[must_use]
    pub fn status(status: u16) -> Self {
        Self(KeyValue::new(STATUS, i64::from(status)))
    }

    /// How an operation ended, as a closed set of literals (`"ok"`, `"failed"`,
    /// `"fresh"`, `"duplicate"`, a push provider's result label).
    #[must_use]
    pub fn result(result: &'static str) -> Self {
        Self(KeyValue::new(RESULT, result))
    }

    /// Why something was refused or ended, as a literal chosen at the call
    /// site.
    #[must_use]
    pub fn reason(reason: &'static str) -> Self {
        Self(KeyValue::new(REASON, reason))
    }

    /// The push provider's metric label.
    #[must_use]
    pub fn provider(provider: &'static str) -> Self {
        Self(KeyValue::new(PROVIDER, provider))
    }

    /// Whether an event stream resumed from a `Last-Event-ID`. Derived from the
    /// header's presence, never its value.
    #[must_use]
    pub fn resumed(resumed: bool) -> Self {
        Self(KeyValue::new(RESUMED, resumed))
    }

    /// Which attempt this is, counting from 1.
    #[must_use]
    pub fn attempt(attempt: u32) -> Self {
        Self(KeyValue::new(ATTEMPT, i64::from(attempt)))
    }

    /// A count of `what`.
    #[must_use]
    pub fn count(what: Count, n: u64) -> Self {
        Self(KeyValue::new(
            what.key(),
            i64::try_from(n).unwrap_or(i64::MAX),
        ))
    }

    /// The key, for tests that read a span back.
    #[must_use]
    pub fn key(&self) -> &str {
        self.0.key.as_str()
    }

    pub(crate) fn into_inner(self) -> KeyValue {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guarantee this module exists for: a span can only carry a name the
    /// log allowlist already vets.
    #[test]
    fn every_key_is_on_the_log_allowlist() {
        for key in KEYS {
            assert!(
                sunrise_log::is_allowed(key),
                "span attribute key {key:?} is not on sunrise_log's allowlist"
            );
        }
    }

    /// Every constructor produces a key from [`KEYS`], so the test above covers
    /// what a caller can actually build.
    #[test]
    fn every_constructor_uses_a_listed_key() {
        let built = [
            Attr::method("GET"),
            Attr::endpoint("/api/v1/health"),
            Attr::status(200),
            Attr::result("ok"),
            Attr::reason("closed"),
            Attr::provider("apns"),
            Attr::resumed(true),
            Attr::attempt(1),
            Attr::count(Count::Ops, 1),
            Attr::count(Count::Bytes, 1),
            Attr::count(Count::Chunks, 1),
            Attr::count(Count::Streams, 1),
        ];
        for attr in &built {
            assert!(KEYS.contains(&attr.key()), "{attr:?}");
        }
        let distinct: std::collections::BTreeSet<&str> = built.iter().map(Attr::key).collect();
        assert_eq!(
            distinct.len(),
            KEYS.len(),
            "a listed key has no constructor"
        );
    }
}
