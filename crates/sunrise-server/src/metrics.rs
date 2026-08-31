//! Prometheus-compatible metrics.
//!
//! v1 self-host ships an in-process counter registry with a `/metrics`
//! exposition endpoint that emits the Prometheus text format
//! (<https://prometheus.io/docs/instrumenting/exposition_formats/>).
//! Production swaps in a full client (`prometheus`, `metrics`) without
//! changing the exposition contract.

use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Cheaply clonable metrics registry.
#[derive(Debug, Clone, Default)]
pub struct Metrics {
    inner: Arc<MetricsInner>,
}

#[derive(Debug, Default)]
struct MetricsInner {
    counters: Mutex<BTreeMap<String, AtomicU64>>,
}

impl Metrics {
    /// Construct.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Increment a labeled counter by 1.
    pub fn incr(&self, name: &str) {
        self.add(name, 1);
    }

    /// Add `n` to a labeled counter.
    pub fn add(&self, name: &str, n: u64) {
        let map = self.inner.counters.lock();
        if let Some(c) = map.get(name) {
            c.fetch_add(n, Ordering::Relaxed);
            return;
        }
        drop(map);
        let mut map = self.inner.counters.lock();
        map.entry(name.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(n, Ordering::Relaxed);
    }

    /// Render in Prometheus text format.
    #[must_use]
    pub fn render(&self) -> String {
        let map = self.inner.counters.lock();
        let mut out = String::with_capacity(map.len() * 64);
        for (name, c) in map.iter() {
            // counters with `{...}` label syntax pass through verbatim;
            // bare names get a TYPE line.
            if !name.contains('{') {
                out.push_str("# TYPE ");
                out.push_str(name);
                out.push_str(" counter\n");
            }
            out.push_str(name);
            out.push(' ');
            out.push_str(&c.load(Ordering::Relaxed).to_string());
            out.push('\n');
        }
        out
    }

    /// Snapshot a single counter (used by tests).
    #[must_use]
    pub fn get(&self, name: &str) -> u64 {
        self.inner
            .counters
            .lock()
            .get(name)
            .map_or(0, |c| c.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn increments_counter() {
        let m = Metrics::new();
        m.incr("sync_session_total");
        m.incr("sync_session_total");
        m.add("op_envelope_total", 5);
        assert_eq!(m.get("sync_session_total"), 2);
        assert_eq!(m.get("op_envelope_total"), 5);
    }

    #[test]
    fn renders_prometheus_format() {
        let m = Metrics::new();
        m.add("sunrise_sync_session_total", 3);
        let s = m.render();
        assert!(s.contains("# TYPE sunrise_sync_session_total counter"));
        assert!(s.contains("sunrise_sync_session_total 3"));
    }

    #[test]
    fn missing_counter_returns_zero() {
        let m = Metrics::new();
        assert_eq!(m.get("nope"), 0);
    }
}
