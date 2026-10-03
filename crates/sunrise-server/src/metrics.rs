//! Prometheus-compatible metrics.
//!
//! An in-process registry of labelled counters, gauges and histograms, rendered
//! at `/metrics` in the Prometheus text format
//! (<https://prometheus.io/docs/instrumenting/exposition_formats/>). The
//! catalogue it serves, and the contract each metric answers to, is
//! `docs/06-server/metrics.md`.
//!
//! # Why it is shaped like this
//!
//! `metrics.md` §Rules asks for three things the counter-only `Mutex<BTreeMap>`
//! this replaces could not give:
//!
//! - **No lock on the hot path.** Series live in a fixed table of
//!   [`OnceLock`] slots, probed linearly from a hash of the metric name and its
//!   labels. A series is written once, the first time it is touched; every
//!   later observation is a hash, a probe that finds an initialised slot, and
//!   an atomic add. A request never waits on another request's metric.
//! - **Labels that cannot identify anyone.** A label *name* outside
//!   [`LABEL_ALLOWLIST`] is refused at the call, so a call site cannot invent
//!   `account_id` and have it reach a scrape. Label *values* are bounded by
//!   each call site passing a closed set — a route template, a status code, an
//!   enum's name — and `tests/metric-label-safety.rs` scrapes the real router
//!   to hold both properties.
//! - **A bounded series count.** The table has [`CAPACITY`] slots and never
//!   grows. An observation that would need a new series in a full table is
//!   dropped and counted in `sunrise_metrics_series_dropped_total`, rather than
//!   growing memory without bound because some label turned out to be open.
//!
//! Metric names are `&'static str`, so every name is a string literal at its
//! call site. That is what lets the `observability-catalog` gate find each one
//! with a grep.

use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

/// The only label names any metric may carry.
///
/// `docs/06-server/metrics.md` §Label allowlist is the record this mirrors, and
/// each name's value set is bounded there. Nothing here identifies an account,
/// a device, a stream, an op, a blob, an upload or an address, and nothing
/// may: a label value is a series, and an id-valued label is one series per
/// user.
pub const LABEL_ALLOWLIST: &[&str] = &[
    "endpoint",
    "method",
    "status",
    "kind",
    "provider",
    "result",
    "reason",
    "scope",
    "direction",
    "state",
    "version",
    "commit",
    "wire_proto",
    "crypto_suite",
];

/// Request and round-trip latency, in seconds.
///
/// Carries a bound at 0.5 s, the p99 sync target, so that SLO reads straight
/// off `_bucket{le="0.5"}`.
pub const LATENCY_BUCKETS: &[f64] = &[
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Whole-transfer durations, in seconds.
pub const TRANSFER_BUCKETS: &[f64] = &[0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0];

/// Item counts, such as ops per batch.
pub const COUNT_BUCKETS: &[f64] = &[1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0, 256.0, 512.0];

/// How many distinct series the registry holds before it starts dropping.
///
/// Well above what the catalogue can produce — the route table times its
/// methods and statuses is the largest family, at a few hundred — so reaching
/// it means a label value set was not closed after all.
pub const CAPACITY: usize = 4096;

/// A metric's label set: each name, and the value it takes in this series.
pub type Labels<'a> = &'a [(&'static str, &'a str)];

/// Which Prometheus type a series renders as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Counter,
    Gauge,
    Histogram,
}

impl Kind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
}

/// One histogram series: a count per bucket, plus the sum of every value.
#[derive(Debug)]
struct Hist {
    /// Upper bounds, ascending. The `+Inf` bucket is implicit.
    bounds: &'static [f64],
    /// Non-cumulative counts, one per bound plus one for `+Inf`. Rendering
    /// accumulates them, so an observation is one atomic add, not one per
    /// bucket it falls under.
    buckets: Box<[AtomicU64]>,
    /// The running sum, as `f64` bits.
    sum: AtomicU64,
}

/// One series: a name, its label values, and its value.
#[derive(Debug)]
struct Series {
    name: &'static str,
    labels: Box<[(&'static str, Box<str>)]>,
    kind: Kind,
    /// A counter's count, or a gauge's value as `f64` bits.
    value: AtomicU64,
    hist: Option<Hist>,
}

impl Series {
    fn new(name: &'static str, labels: Labels<'_>, kind: Kind, bounds: &'static [f64]) -> Self {
        let hist = (kind == Kind::Histogram).then(|| Hist {
            bounds,
            buckets: (0..=bounds.len()).map(|_| AtomicU64::new(0)).collect(),
            sum: AtomicU64::new(0f64.to_bits()),
        });
        Self {
            name,
            labels: labels.iter().map(|&(k, v)| (k, Box::from(v))).collect(),
            kind,
            value: AtomicU64::new(0),
            hist,
        }
    }

    fn is(&self, name: &str, labels: Labels<'_>) -> bool {
        self.name == name
            && self.labels.len() == labels.len()
            && self
                .labels
                .iter()
                .zip(labels)
                .all(|((k, v), (lk, lv))| k == lk && &**v == *lv)
    }
}

/// Cheaply clonable metrics registry.
#[derive(Debug, Clone)]
pub struct Metrics {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    slots: Box<[OnceLock<Series>]>,
    /// Observations refused: a full table, a label off the allowlist, or a
    /// name reused under a second type.
    dropped: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Where a series' probe starts.
fn home(name: &str, labels: Labels<'_>) -> usize {
    let mut hasher = std::hash::DefaultHasher::new();
    name.hash(&mut hasher);
    for (k, v) in labels {
        k.hash(&mut hasher);
        v.hash(&mut hasher);
    }
    // Truncation is the point: only the low bits pick a slot.
    #[allow(clippy::cast_possible_truncation)]
    let h = hasher.finish() as usize;
    h % CAPACITY
}

/// Add `delta` to an `f64` stored as bits.
fn add_f64(cell: &AtomicU64, delta: f64) {
    let _ = cell.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
        Some((f64::from_bits(bits) + delta).to_bits())
    });
}

/// A sample value as the exposition format spells it.
fn number(out: &mut String, v: f64) {
    if v.is_nan() {
        out.push_str("NaN");
    } else if v.is_infinite() {
        out.push_str(if v > 0.0 { "+Inf" } else { "-Inf" });
    } else {
        let _ = write!(out, "{v}");
    }
}

/// `{a="x",b="y"}`, with an optional trailing `le`, or nothing at all.
fn label_set(out: &mut String, labels: &[(&'static str, Box<str>)], le: Option<&str>) {
    if labels.is_empty() && le.is_none() {
        return;
    }
    out.push('{');
    let mut first = true;
    let extra = le.map(|v| ("le", v));
    for (k, v) in labels.iter().map(|(k, v)| (*k, &**v)).chain(extra) {
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(k);
        out.push_str("=\"");
        for ch in v.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                c => out.push(c),
            }
        }
        out.push('"');
    }
    out.push('}');
}

impl Metrics {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                slots: (0..CAPACITY).map(|_| OnceLock::new()).collect(),
                dropped: AtomicU64::new(0),
            }),
        }
    }

    /// The series for `name` and `labels`, created on first touch.
    ///
    /// `None` when the observation is refused, which also counts it.
    fn series(
        &self,
        name: &'static str,
        labels: Labels<'_>,
        kind: Kind,
        bounds: &'static [f64],
    ) -> Option<&Series> {
        if labels.iter().any(|(k, _)| !LABEL_ALLOWLIST.contains(k)) {
            debug_assert!(
                false,
                "{name} carries a label off the allowlist: {labels:?}"
            );
            self.inner.dropped.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let start = home(name, labels);
        for step in 0..CAPACITY {
            let slot = &self.inner.slots[(start + step) % CAPACITY];
            let series = slot.get_or_init(|| Series::new(name, labels, kind, bounds));
            if series.is(name, labels) {
                if series.kind == kind {
                    return Some(series);
                }
                break;
            }
        }
        self.inner.dropped.fetch_add(1, Ordering::Relaxed);
        None
    }

    /// The series for `name` and `labels` if it exists. Never creates one.
    fn find(&self, name: &str, labels: Labels<'_>) -> Option<&Series> {
        let start = home(name, labels);
        for step in 0..CAPACITY {
            // Linear probing with no removal leaves no empty slot between a
            // series and its home, so the first empty slot ends the search.
            let series = self.inner.slots[(start + step) % CAPACITY].get()?;
            if series.is(name, labels) {
                return Some(series);
            }
        }
        None
    }

    /// Increment an unlabelled counter by 1.
    pub fn incr(&self, name: &'static str) {
        self.add_with(name, &[], 1);
    }

    /// Add `n` to an unlabelled counter.
    pub fn add(&self, name: &'static str, n: u64) {
        self.add_with(name, &[], n);
    }

    /// Increment a labelled counter by 1.
    pub fn incr_with(&self, name: &'static str, labels: Labels<'_>) {
        self.add_with(name, labels, 1);
    }

    /// Add `n` to a labelled counter.
    pub fn add_with(&self, name: &'static str, labels: Labels<'_>, n: u64) {
        if let Some(s) = self.series(name, labels, Kind::Counter, &[]) {
            s.value.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// Set a gauge.
    ///
    /// `metrics.md` §Rules: a gauge is sampled from authoritative state at
    /// scrape time, or is a process constant. It is never moved up and down by
    /// paired calls that can drift apart, which is why there is no `inc`/`dec`.
    pub fn set_gauge(&self, name: &'static str, labels: Labels<'_>, value: f64) {
        if let Some(s) = self.series(name, labels, Kind::Gauge, &[]) {
            s.value.store(value.to_bits(), Ordering::Relaxed);
        }
    }

    /// Record one observation in a histogram with the bucket set `bounds`.
    ///
    /// `bounds` is one of [`LATENCY_BUCKETS`], [`TRANSFER_BUCKETS`] or
    /// [`COUNT_BUCKETS`]. The first observation fixes a series' buckets.
    pub fn observe(
        &self,
        name: &'static str,
        labels: Labels<'_>,
        bounds: &'static [f64],
        value: f64,
    ) {
        let Some(hist) = self
            .series(name, labels, Kind::Histogram, bounds)
            .and_then(|s| s.hist.as_ref())
        else {
            return;
        };
        let idx = hist
            .bounds
            .iter()
            .position(|&b| value <= b)
            .unwrap_or(hist.bounds.len());
        hist.buckets[idx].fetch_add(1, Ordering::Relaxed);
        add_f64(&hist.sum, value);
    }

    /// An unlabelled counter's value; 0 if it was never touched.
    #[must_use]
    pub fn get(&self, name: &str) -> u64 {
        self.get_with(name, &[])
    }

    /// A labelled counter's value; 0 if it was never touched.
    #[must_use]
    pub fn get_with(&self, name: &str, labels: Labels<'_>) -> u64 {
        self.find(name, labels)
            .filter(|s| s.kind == Kind::Counter)
            .map_or(0, |s| s.value.load(Ordering::Relaxed))
    }

    /// A histogram's observation count; 0 if it was never touched.
    #[must_use]
    pub fn histogram_count(&self, name: &str, labels: Labels<'_>) -> u64 {
        self.find(name, labels)
            .and_then(|s| s.hist.as_ref())
            .map_or(0, |h| {
                h.buckets.iter().map(|b| b.load(Ordering::Relaxed)).sum()
            })
    }

    /// Render in the Prometheus text format.
    ///
    /// One `# TYPE` line per family, then its series sorted by label values,
    /// so two scrapes of the same state are byte-identical.
    #[must_use]
    pub fn render(&self) -> String {
        let mut series: Vec<&Series> = self.inner.slots.iter().filter_map(OnceLock::get).collect();
        series.sort_by(|a, b| a.name.cmp(b.name).then_with(|| a.labels.cmp(&b.labels)));

        let mut out = String::with_capacity(series.len() * 96);
        let mut family = "";
        for s in series {
            if s.name != family {
                family = s.name;
                let _ = writeln!(out, "# TYPE {} {}", s.name, s.kind.as_str());
            }
            match (&s.hist, s.kind) {
                (Some(h), _) => render_histogram(&mut out, s, h),
                (None, Kind::Gauge) => {
                    out.push_str(s.name);
                    label_set(&mut out, &s.labels, None);
                    out.push(' ');
                    number(&mut out, f64::from_bits(s.value.load(Ordering::Relaxed)));
                    out.push('\n');
                }
                (None, _) => {
                    out.push_str(s.name);
                    label_set(&mut out, &s.labels, None);
                    let _ = writeln!(out, " {}", s.value.load(Ordering::Relaxed));
                }
            }
        }

        // Always present, so "nothing was dropped" is a reading rather than an
        // absence a dashboard cannot tell from a scrape that failed.
        let _ = writeln!(
            out,
            "# TYPE {name} counter\n{name} {}",
            self.inner.dropped.load(Ordering::Relaxed),
            name = "sunrise_metrics_series_dropped_total",
        );
        out
    }
}

/// `_bucket` (cumulative, ending at `+Inf`), `_sum` and `_count`.
fn render_histogram(out: &mut String, s: &Series, h: &Hist) {
    let mut cumulative = 0u64;
    let mut le = String::new();
    for (i, cell) in h.buckets.iter().enumerate() {
        cumulative += cell.load(Ordering::Relaxed);
        le.clear();
        number(&mut le, h.bounds.get(i).copied().unwrap_or(f64::INFINITY));
        out.push_str(s.name);
        out.push_str("_bucket");
        label_set(out, &s.labels, Some(&le));
        let _ = writeln!(out, " {cumulative}");
    }
    out.push_str(s.name);
    out.push_str("_sum");
    label_set(out, &s.labels, None);
    out.push(' ');
    number(out, f64::from_bits(h.sum.load(Ordering::Relaxed)));
    out.push('\n');
    // The `+Inf` bucket's own total rather than a separate counter, so `_count`
    // and `le="+Inf"` agree in every scrape even while observations race it.
    out.push_str(s.name);
    out.push_str("_count");
    label_set(out, &s.labels, None);
    let _ = writeln!(out, " {cumulative}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `# TYPE` line each family carries, keyed by family name.
    fn types(rendered: &str) -> Vec<(&str, &str)> {
        rendered
            .lines()
            .filter_map(|l| l.strip_prefix("# TYPE "))
            .filter_map(|l| l.split_once(' '))
            .collect()
    }

    #[test]
    fn increments_counter() {
        let m = Metrics::new();
        m.incr("test_session_total");
        m.incr("test_session_total");
        m.add("test_envelope_total", 5);
        assert_eq!(m.get("test_session_total"), 2);
        assert_eq!(m.get("test_envelope_total"), 5);
    }

    #[test]
    fn missing_counter_returns_zero() {
        let m = Metrics::new();
        assert_eq!(m.get("nope"), 0);
        assert_eq!(m.get_with("nope", &[("result", "ok")]), 0);
    }

    /// Two label values are two series of one family, under one `# TYPE`.
    #[test]
    fn labelled_counters_are_separate_series_of_one_family() {
        let m = Metrics::new();
        m.incr_with(
            "test_requests_total",
            &[("method", "GET"), ("status", "200")],
        );
        m.incr_with(
            "test_requests_total",
            &[("method", "GET"), ("status", "200")],
        );
        m.incr_with(
            "test_requests_total",
            &[("method", "GET"), ("status", "404")],
        );
        assert_eq!(
            m.get_with(
                "test_requests_total",
                &[("method", "GET"), ("status", "200")]
            ),
            2
        );

        let s = m.render();
        assert_eq!(
            s.matches("# TYPE test_requests_total counter").count(),
            1,
            "{s}"
        );
        assert!(
            s.contains("test_requests_total{method=\"GET\",status=\"200\"} 2\n"),
            "{s}"
        );
        assert!(
            s.contains("test_requests_total{method=\"GET\",status=\"404\"} 1\n"),
            "{s}"
        );
    }

    /// The exposition contract: every family typed, a histogram as cumulative
    /// `_bucket` lines ending at `+Inf`, then `_sum` and `_count`.
    #[test]
    fn renders_prometheus_exposition_format() {
        let m = Metrics::new();
        m.add("test_ops_total", 3);
        m.set_gauge("test_sessions_active", &[], 4.0);
        m.observe(
            "test_duration_seconds",
            &[("method", "GET")],
            LATENCY_BUCKETS,
            0.003,
        );
        m.observe(
            "test_duration_seconds",
            &[("method", "GET")],
            LATENCY_BUCKETS,
            0.2,
        );
        m.observe(
            "test_duration_seconds",
            &[("method", "GET")],
            LATENCY_BUCKETS,
            60.0,
        );
        let s = m.render();

        assert_eq!(
            types(&s),
            [
                ("test_duration_seconds", "histogram"),
                ("test_ops_total", "counter"),
                ("test_sessions_active", "gauge"),
                ("sunrise_metrics_series_dropped_total", "counter"),
            ],
            "{s}"
        );
        assert!(s.contains("test_ops_total 3\n"), "{s}");
        assert!(s.contains("test_sessions_active 4\n"), "{s}");

        let h = |le: &str| format!("test_duration_seconds_bucket{{method=\"GET\",le=\"{le}\"}}");
        assert!(s.contains(&format!("{} 0\n", h("0.001"))), "{s}");
        assert!(s.contains(&format!("{} 1\n", h("0.005"))), "{s}");
        assert!(s.contains(&format!("{} 2\n", h("0.25"))), "{s}");
        assert!(s.contains(&format!("{} 2\n", h("10"))), "{s}");
        assert!(s.contains(&format!("{} 3\n", h("+Inf"))), "{s}");
        assert!(
            s.contains("test_duration_seconds_sum{method=\"GET\"} 60.203\n"),
            "{s}"
        );
        assert!(
            s.contains("test_duration_seconds_count{method=\"GET\"} 3\n"),
            "{s}"
        );
        assert_eq!(
            m.histogram_count("test_duration_seconds", &[("method", "GET")]),
            3
        );

        // Every bucket line is in ascending `le` order, as the format requires.
        let bucket_lines = s.lines().filter(|l| l.contains("_bucket{")).count();
        assert_eq!(bucket_lines, LATENCY_BUCKETS.len() + 1, "{s}");
    }

    /// A label value cannot break out of its quotes.
    #[test]
    fn label_values_are_escaped() {
        let m = Metrics::new();
        m.incr_with("test_escape_total", &[("reason", "a\"b\\c\nd")]);
        let s = m.render();
        assert!(
            s.contains(r#"test_escape_total{reason="a\"b\\c\nd"} 1"#),
            "{s}"
        );
    }

    /// A label name off the allowlist never reaches a scrape. Release builds
    /// drop and count it; debug builds stop at the call, which is where a test
    /// that added one would want to find out.
    #[test]
    #[cfg(not(debug_assertions))]
    fn a_label_off_the_allowlist_is_dropped_and_counted() {
        let m = Metrics::new();
        m.incr_with("test_bad_total", &[("account_id", "x")]);
        let s = m.render();
        assert!(!s.contains("account_id"), "{s}");
        assert!(
            s.contains("sunrise_metrics_series_dropped_total 1\n"),
            "{s}"
        );
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "off the allowlist")]
    fn a_label_off_the_allowlist_is_refused() {
        Metrics::new().incr_with("test_bad_total", &[("account_id", "x")]);
    }

    /// A name reused under a second type would render as two `# TYPE` lines
    /// for one family, which a scraper rejects. The second use is dropped.
    #[test]
    fn a_name_is_one_type() {
        let m = Metrics::new();
        m.incr("test_mixed");
        m.set_gauge("test_mixed", &[], 9.0);
        let s = m.render();
        assert_eq!(s.matches("# TYPE test_mixed").count(), 1, "{s}");
        assert!(s.contains("test_mixed 1\n"), "{s}");
        assert!(
            s.contains("sunrise_metrics_series_dropped_total 1\n"),
            "{s}"
        );
    }

    /// The table never grows: past [`CAPACITY`], a new series is dropped and
    /// counted, and the existing ones keep counting.
    #[test]
    fn a_full_table_drops_new_series_and_keeps_the_old() {
        let m = Metrics::new();
        let values: Vec<String> = (0..=CAPACITY).map(|i| i.to_string()).collect();
        for v in &values {
            m.incr_with("test_wide_total", &[("status", v)]);
        }
        assert_eq!(m.get_with("test_wide_total", &[("status", "0")]), 1);
        m.incr_with("test_wide_total", &[("status", "0")]);
        assert_eq!(m.get_with("test_wide_total", &[("status", "0")]), 2);
        assert!(m
            .render()
            .contains("sunrise_metrics_series_dropped_total 1\n"));
    }

    /// Concurrent first touches of one series land in one slot.
    #[test]
    fn concurrent_increments_are_not_lost() {
        let m = Metrics::new();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..1000 {
                        m.incr_with("test_race_total", &[("result", "ok")]);
                    }
                });
            }
        });
        assert_eq!(m.get_with("test_race_total", &[("result", "ok")]), 8000);
        assert_eq!(m.render().matches("test_race_total{").count(), 1);
    }
}
