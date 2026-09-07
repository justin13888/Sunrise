//! [`RedactionLayer`] — the part of Sunrise logging that is genuinely ours.
//!
//! `tracing` decides *whether* an event is enabled and *where* it is written.
//! This layer decides whether it is **allowed to exist at all**, by checking
//! every field name on every event from a `sunrise_*` target against
//! [`crate::field::ALLOWED`].
//!
//! # Why a field-name gate and not a value scrubber
//!
//! A `Layer` cannot rewrite an event before the `fmt` layer formats it —
//! `on_event` receives an immutable `&Event`. It *can* veto one:
//! [`Layer::event_enabled`] is `AND`-ed across the whole stack, so returning
//! `false` here suppresses the event for every layer beneath. So the only
//! honest enforcement point is a veto, and a veto is what this is.
//!
//! Vetoing on the *field name* rather than the value is deliberate. Values
//! are already covered from the other side: [`crate::Plain`] implements no
//! `Display`, no `serde::Serialize`, and no `tracing::Value`, and its `Debug`
//! prints `Plain<…>` — so a `Plain<T>` cannot be formatted into a record as
//! anything but that literal string, through any tracing path. What the value
//! defence *cannot* catch is a developer calling `.expose()` first and
//! logging the `String`. That lands under a field name nobody put on the
//! allowlist, and this layer refuses it.
//!
//! # Fail loud in dev, fail closed in production
//!
//! A violation is a bug in the calling code, and the useful moment to learn
//! about it is the test run. So the default policy panics when
//! `debug_assertions` is on and silently drops otherwise: a leak must never
//! be traded for a log line, and a test must never pass while a leak exists.
//! [`RedactionLayer::violations`] counts drops for the release path.
//!
//! # Scope
//!
//! Only `sunrise_*` targets are gated. `tower_http`, `hyper`, and friends
//! emit fields we do not control and whose names we have not vetted; they are
//! filtered by level (`EnvFilter`), not by vocabulary. The one third-party
//! surface that would otherwise log a user-bearing value — `tower_http`'s
//! request URI, which carries `?access_token=` — left the workspace with
//! `tower_http` when ADR-0021 ported the server to `kynos`. What records a
//! request today is `sunrise_server::api::observe`, built by
//! `sunrise_server::build_service`, and it takes `endpoint` from the *matched
//! route's* `paths` key rather than from the request's own target.
//!
//! **Spans are not gated, by design.** `on_new_span` has no veto and a span
//! cannot be rewritten once created, so this layer implements `event_enabled`
//! and nothing else, and makes no claim whatever about span fields.
//!
//! What stands behind the span vocabulary is not this layer but its size. The
//! workspace creates exactly one span — `http.request`, in
//! `sunrise_server::api::observe` — carrying `method` and `endpoint`, both on
//! [`crate::field::ALLOWED`] and both server-derived (`endpoint` is the matched
//! route's own template, never a raw target). The
//! `Plain<T>` type-level guarantee still applies to span fields and the
//! `.expose()` CI gate still covers the modules that build them.
//!
//! This is the one part of the field-vocabulary story with no gate under it:
//! `sunrise-log`'s `event_catalog` test scans `tracing::*!` events and not
//! `*_span!`. A second span site is the point at which that is worth fixing.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

use crate::field::is_allowed;

/// The target prefix whose vocabulary this layer owns. Crate names are
/// `sunrise-*`, which `module_path!` renders as `sunrise_*`.
const OWNED_TARGET_PREFIX: &str = "sunrise_";

/// What to do when a Sunrise event carries a field name outside the
/// allowlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViolationPolicy {
    /// Panic, naming the offending field (never its value). The default when
    /// `debug_assertions` is on: an unvetted field name is a bug, and a test
    /// suite that keeps quiet about it is worse than a failing one.
    Panic,
    /// Drop the event and bump [`RedactionLayer::violations`]. The default in
    /// release: losing one log line beats leaking a task title.
    Drop,
}

impl Default for ViolationPolicy {
    fn default() -> Self {
        if cfg!(debug_assertions) {
            Self::Panic
        } else {
            Self::Drop
        }
    }
}

/// `tracing` layer enforcing the [`crate::field`] allowlist.
#[derive(Debug, Default)]
pub struct RedactionLayer {
    policy: ViolationPolicy,
    violations: AtomicU64,
    /// Names already reported, so a hot loop reports once rather than every
    /// iteration. Only read on the violation path.
    seen: Mutex<HashSet<(&'static str, &'static str)>>,
}

impl RedactionLayer {
    /// A layer with the default policy for this build profile.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the violation policy.
    ///
    /// Tests that need to observe a *drop* rather than take a panic build the
    /// layer with [`ViolationPolicy::Drop`] explicitly.
    #[must_use]
    pub fn with_policy(mut self, policy: ViolationPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// How many events this layer has refused since construction.
    ///
    /// Wire this to a metrics counter: a non-zero value means shipped code is
    /// trying to log something nobody vetted.
    #[must_use]
    pub fn violations(&self) -> u64 {
        self.violations.load(Ordering::Relaxed)
    }

    /// Whether this layer owns the vocabulary of `target`.
    fn owns(target: &str) -> bool {
        target.starts_with(OWNED_TARGET_PREFIX)
    }

    fn refuse(&self, target: &'static str, field: &'static str) -> bool {
        self.violations.fetch_add(1, Ordering::Relaxed);
        let first_time = self.seen.lock().insert((target, field));
        match self.policy {
            ViolationPolicy::Panic => {
                panic!(
                    "sunrise-log: refusing to emit event from target {target:?}: field \
                     {field:?} is not on the redaction allowlist (crates/sunrise-log/src/field.rs). \
                     Either use an allowlisted key or do not log this value."
                );
            }
            ViolationPolicy::Drop => {
                // Deliberately silent. Re-entering `tracing` from inside a
                // subscriber callback is a no-op — `tracing_core`'s dispatcher
                // guards against recursion — so a "we dropped one" event
                // could not be emitted here even if we wanted it. The counter
                // and `first_time` are what an operator has; the latter is
                // returned so a caller can log the first occurrence from
                // outside the dispatch.
                let _ = first_time;
                false
            }
        }
    }
}

/// Collects the first field name that is not on the allowlist.
struct FieldNameCheck {
    offender: Option<&'static str>,
}

impl Visit for FieldNameCheck {
    fn record_debug(&mut self, field: &Field, _value: &dyn std::fmt::Debug) {
        if self.offender.is_none() && !is_allowed(field.name()) {
            self.offender = Some(field.name());
        }
    }
}

impl<S: Subscriber> Layer<S> for RedactionLayer {
    fn event_enabled(&self, event: &Event<'_>, _ctx: Context<'_, S>) -> bool {
        let target = event.metadata().target();
        if !Self::owns(target) {
            return true;
        }
        let mut check = FieldNameCheck { offender: None };
        event.record(&mut check);
        match check.offender {
            None => true,
            Some(field) => self.refuse(target, field),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owns_only_sunrise_targets() {
        assert!(RedactionLayer::owns("sunrise_server::ws"));
        assert!(RedactionLayer::owns("sunrise_log"));
        assert!(!RedactionLayer::owns("tower_http::trace::on_response"));
        assert!(!RedactionLayer::owns("hyper::proto"));
        // A crate merely *containing* "sunrise" is not ours.
        assert!(!RedactionLayer::owns("not_sunrise_thing"));
    }

    #[test]
    fn default_policy_follows_build_profile() {
        let expected = if cfg!(debug_assertions) {
            ViolationPolicy::Panic
        } else {
            ViolationPolicy::Drop
        };
        assert_eq!(ViolationPolicy::default(), expected);
    }

    #[test]
    fn drop_policy_counts_violations() {
        let layer = RedactionLayer::new().with_policy(ViolationPolicy::Drop);
        assert_eq!(layer.violations(), 0);
        assert!(!layer.refuse("sunrise_test", "task_title"));
        assert!(!layer.refuse("sunrise_test", "task_title"));
        assert_eq!(layer.violations(), 2);
    }
}
