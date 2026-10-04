//! Test-only capture of the `ev` names a closure emits.
//!
//! Other crates' tests assert that an event is emitted — a refusal that is
//! announced rather than silent, an unwind that is reported — and all they
//! need from the log line is its `ev` name, which is the only part of a record
//! this repository treats as a contract (`tests/event_catalog.rs` is the gate
//! on that name). [`events_emitted_by`] gives them that without each one
//! assembling a subscriber, and, more importantly, without each one
//! re-deriving the interest-cache pin a capture needs to see anything at all.
//!
//! Nothing in a shipped binary calls this module. It is public, and not behind
//! a feature, because it depends on `tracing` alone and a dev-dependency on
//! this crate is all a caller has to take.

use std::sync::Arc;

use parking_lot::Mutex;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Dispatch, Event, Metadata, Subscriber};

use crate::init::pin_interest_cache;

/// Reads the `ev` field of one event and ignores every other field.
struct EvVisitor(Option<String>);

impl Visit for EvVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "ev" {
            self.0 = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "ev" && self.0.is_none() {
            self.0 = Some(format!("{value:?}").trim_matches('"').to_owned());
        }
    }
}

/// A subscriber that keeps the `ev` name of every event it is shown.
///
/// Every level is enabled: a caller asserting that a `debug!` fired must not
/// depend on whatever filter the process happens to carry.
struct EvCapture(Arc<Mutex<Vec<String>>>);

impl Subscriber for EvCapture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut visitor = EvVisitor(None);
        event.record(&mut visitor);
        if let Some(ev) = visitor.0 {
            self.0.lock().push(ev);
        }
    }

    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

/// The `ev` names of every event emitted on this thread while `f` runs, in
/// order.
///
/// An event with no `ev` field is not recorded. Events emitted on other
/// threads are not seen: the capture is installed with
/// `tracing::dispatcher::with_default`, which is thread-local, so concurrent
/// tests in one binary cannot hear each other.
///
/// The interest-cache pin is registered first, for the reason documented on
/// `init::pin_interest_cache`: without it, a callsite a neighbouring test
/// thread reached first can be cached as `never`, and this capture would come
/// back empty. `tests/events_emitted_by.rs` is that shape, run
/// deterministically.
pub fn events_emitted_by(f: impl FnOnce()) -> Vec<String> {
    pin_interest_cache();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let dispatch = Dispatch::new(EvCapture(Arc::clone(&seen)));
    tracing::dispatcher::with_default(&dispatch, f);
    let taken = seen.lock().clone();
    taken
}

/// `tests/event_catalog.rs` scans unit tests in this file as shipped code, so
/// every name below is one already catalogued and every field is allowlisted.
#[cfg(test)]
mod tests {
    use super::events_emitted_by;

    #[test]
    fn names_are_kept_in_emission_order_at_every_level() {
        let seen = events_emitted_by(|| {
            tracing::trace!(ev = "core.open.start", "a");
            tracing::warn!(ev = "core.open.failed", "b");
            tracing::debug!(ev = "core.open.ok", "c");
        });
        assert_eq!(
            seen,
            ["core.open.start", "core.open.failed", "core.open.ok"]
        );
    }

    #[test]
    fn an_event_without_an_ev_field_is_not_recorded() {
        let seen = events_emitted_by(|| {
            tracing::info!(lat_ms = 3_u64, "no name");
            tracing::info!(ev = "core.shutdown.start", "named");
        });
        assert_eq!(seen, ["core.shutdown.start"]);
    }

    #[test]
    fn an_ev_recorded_by_debug_loses_its_quotes() {
        let seen = events_emitted_by(|| tracing::info!(ev = ?"core.shutdown.ok", "by debug"));
        assert_eq!(seen, ["core.shutdown.ok"]);
    }

    #[test]
    fn nothing_is_heard_from_another_thread() {
        let seen = events_emitted_by(|| {
            std::thread::spawn(|| tracing::info!(ev = "core.submit.applied", "x"))
                .join()
                .expect("the other thread");
        });
        assert!(seen.is_empty(), "{seen:?}");
    }
}
