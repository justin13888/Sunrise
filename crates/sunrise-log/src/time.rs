//! RFC 3339 millisecond timestamps for log records.
//!
//! `tracing-subscriber`'s stock timers either need its `time` feature (a
//! second datetime crate in the tree) or print a `SystemTime` debug blob.
//! Sunrise already ships `jiff` with a bundled tzdb (ADR-0011), so the
//! formatter here is a thin adapter over it and keeps the `ts` shape
//! `docs/10-cross-cutting/logging.md` §3 specifies:
//! `YYYY-MM-DDTHH:MM:SS.mmmZ`.

use std::fmt;

use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;

/// UTC RFC 3339 timestamp with fixed millisecond precision.
#[derive(Debug, Clone, Copy, Default)]
pub struct Rfc3339Millis;

impl FormatTime for Rfc3339Millis {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        // ---------------------------------------------------------------
        // Sanctioned wall-clock read.
        //
        // The workspace determinism rule (docs/01-architecture/shared-core.md)
        // routes every time read through the injected `Clock` so domain state
        // is reproducible. Log timestamps are the deliberate exception, and
        // the exemption is scoped to this one function:
        //
        //   * a log line records *when the process observed something*, which
        //     is observational data, not domain state — nothing merges on it
        //     and no test asserts against it;
        //   * a core under a fixture clock still needs its logs stamped with
        //     real time, or a debugging session reads a wall of 1970;
        //   * threading a `Clock` into the subscriber would put the injected
        //     clock on the path of every `warn!` in the tree, including the
        //     ones reporting that the clock itself is wrong.
        //
        // `jiff::Timestamp::now` rather than `SystemTime::now` because jiff is
        // the workspace's datetime library (ADR-0011) and already formats to
        // RFC 3339. No other crate may follow this pattern.
        // ---------------------------------------------------------------
        let now = jiff::Timestamp::now();
        write!(w, "{}", now.strftime("%Y-%m-%dT%H:%M:%S%.3fZ"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render a fixed instant through the same `strftime` pattern the timer
    /// uses, so the *shape* is asserted without asserting on wall time.
    fn render(ts: jiff::Timestamp) -> String {
        ts.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
    }

    #[test]
    fn shape_matches_logging_spec() {
        let ts: jiff::Timestamp = "2026-05-08T12:34:56.789Z".parse().unwrap();
        assert_eq!(render(ts), "2026-05-08T12:34:56.789Z");
    }

    #[test]
    fn millis_are_zero_padded_not_elided() {
        // A `%.3f` that collapsed to `%f` would print `.7` here and break the
        // schema pattern in schemas/log-record.v1.json.
        let ts: jiff::Timestamp = "2026-05-08T12:34:56.700Z".parse().unwrap();
        assert_eq!(render(ts), "2026-05-08T12:34:56.700Z");
        let ts: jiff::Timestamp = "2026-05-08T12:34:56Z".parse().unwrap();
        assert_eq!(render(ts), "2026-05-08T12:34:56.000Z");
    }

    #[test]
    fn output_is_utc_regardless_of_host_zone() {
        let ts: jiff::Timestamp = "2026-05-08T12:34:56.001+09:00".parse().unwrap();
        assert_eq!(render(ts), "2026-05-08T03:34:56.001Z");
    }

    #[test]
    fn live_timer_produces_a_parsable_timestamp() {
        let mut buf = String::new();
        let mut w = Writer::new(&mut buf);
        Rfc3339Millis.format_time(&mut w).unwrap();
        assert_eq!(
            buf.len(),
            24,
            "expected YYYY-MM-DDTHH:MM:SS.mmmZ, got {buf:?}"
        );
        assert!(buf.ends_with('Z'));
        buf.parse::<jiff::Timestamp>()
            .expect("re-parses as RFC 3339");
    }
}
