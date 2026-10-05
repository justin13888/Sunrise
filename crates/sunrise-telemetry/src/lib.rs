//! Redacted OpenTelemetry tracing for the Sunrise relay.
//!
//! Off unless the operator configures `[observability]`
//! (`docs/06-server/observability.md` §Tracing). Disabled, a [`Telemetry`] is
//! `None` inside: no provider, no exporter, no background thread, and every
//! span call below returns at its first branch.
//!
//! # Shape
//!
//! - [`Telemetry`] is the handle the server holds. It starts the root span of
//!   each HTTP operation ([`Telemetry::server_span`]) and of work no request
//!   owns ([`Telemetry::root_span`]), and flushes on shutdown.
//! - Everything under a root is a child started by [`span`], which reads its
//!   parent *and its tracer* from the current OpenTelemetry [`Context`]. A root
//!   puts the tracer in the context it returns, so code that never sees the
//!   server's state — a store method, the relay hub — opens a child without
//!   being handed anything, and outside a traced request the same call is a
//!   no-op. No tracer is installed process-wide, which is what lets each test
//!   build its own.
//! - [`Attr`] is the closed set of attributes a span can carry; see [`attr`].
//! - [`CappedSampler`] follows a client's `traceparent` without letting it
//!   raise the sampling ratio; see [`sampler`].
//!
//! # What never becomes span data
//!
//! A span name is a `&'static str` everywhere except the HTTP root, whose name
//! is the matched route's template. An attribute is an [`Attr`]. An event name
//! is a `&'static str` and its attributes are [`Attr`]s. A status description
//! is a `&'static str`. There is no path by which a request's URI, a header
//! value, a bearer, a signature, an email, a push token or an id becomes any
//! of those. `crates/sunrise-server/tests/span-redaction.rs` drives every
//! route with sentinels in each and holds the exported spans to it.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod attr;
mod export;
pub mod sampler;

pub use attr::{Attr, Count};
pub use export::{ExportConfig, HyperExport, EXPORT_TIMEOUT};
/// Run a future under a [`SpanGuard::context`], so the spans it opens are
/// children of that span.
pub use opentelemetry::trace::FutureExt;
pub use sampler::CappedSampler;

/// An in-memory recorder for tests that read exported spans back.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    pub use opentelemetry::trace::{SpanId, Status};
    pub use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData};

    use crate::{CappedSampler, Telemetry};
    use opentelemetry_sdk::trace::SdkTracerProvider;

    /// Telemetry sampling `ratio` of traces through [`CappedSampler`], which
    /// exports each span to the returned recorder the moment it ends.
    ///
    /// The recorder is emptied when the last clone of the telemetry drops,
    /// which shuts its provider down, so a test reads the spans while it still
    /// holds one.
    #[must_use]
    pub fn recording(ratio: f64) -> (Telemetry, InMemorySpanExporter) {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_sampler(CappedSampler::new(ratio))
            .with_simple_exporter(exporter.clone())
            .build();
        (Telemetry::from_provider(provider), exporter)
    }
}

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use opentelemetry::propagation::TextMapPropagator as _;
use opentelemetry::trace::{
    Span as _, SpanKind, Status, TraceContextExt as _, Tracer as _, TracerProvider as _,
};
use opentelemetry::{Context, ContextGuard};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider};

/// The instrumentation scope every Sunrise span is recorded under.
const SCOPE: &str = "sunrise-server";

/// Why telemetry could not be started or stopped.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    /// The exporter could not be built from its configuration.
    #[error("the OTLP exporter could not be built: {0}")]
    Exporter(String),
    /// The final flush failed; spans still queued were lost.
    #[error("the tracer provider did not shut down cleanly: {0}")]
    Shutdown(String),
}

/// The relay's tracing handle. Cheap to clone; disabled by default.
#[derive(Debug, Clone, Default)]
pub struct Telemetry {
    inner: Option<Arc<Inner>>,
}

#[derive(Debug)]
struct Inner {
    provider: SdkTracerProvider,
    tracer: SdkTracer,
}

/// The tracer, as a context value: what lets [`span`] find it.
#[derive(Debug, Clone)]
struct Installed(SdkTracer);

impl Telemetry {
    /// Telemetry that records nothing.
    #[must_use]
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Telemetry over a provider the caller built — an in-memory exporter in a
    /// test, or the OTLP one [`Telemetry::otlp`] assembles.
    #[must_use]
    pub fn from_provider(provider: SdkTracerProvider) -> Self {
        let tracer = provider.tracer(SCOPE);
        Self {
            inner: Some(Arc::new(Inner { provider, tracer })),
        }
    }

    /// Export to the OTLP/HTTP collector `cfg` names, sending on `runtime`.
    ///
    /// # Errors
    /// [`TelemetryError::Exporter`] when the endpoint does not parse.
    pub fn otlp(
        cfg: &ExportConfig,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self, TelemetryError> {
        export::otlp(cfg, runtime)
    }

    /// Whether anything is recorded at all.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Flush what is queued and stop the exporter. Blocks until the flush ends
    /// or times out, so call it off the async runtime.
    ///
    /// # Errors
    /// [`TelemetryError::Shutdown`] when the flush failed.
    pub fn shutdown(&self) -> Result<(), TelemetryError> {
        match &self.inner {
            Some(inner) => inner
                .provider
                .shutdown()
                .map_err(|e| TelemetryError::Shutdown(e.to_string())),
            None => Ok(()),
        }
    }

    /// Stop the exporter without waiting for its flush: whatever is still
    /// queued is lost, and this returns at once whether or not the collector
    /// answers.
    ///
    /// For an exit the operator asked to be immediate. It also marks the
    /// provider shut down, which matters as much as the flush it skips: the
    /// SDK flushes, for up to five seconds, when the last clone of a provider
    /// that was never shut down drops, and on the way out of the process that
    /// drop is the runtime's teardown releasing every task's state.
    pub fn abandon(&self) {
        if let Some(inner) = &self.inner {
            // A zero bound is a timeout by construction, and an error here
            // says only that the queue was dropped, which is the point.
            let _ = inner.provider.shutdown_with_timeout(Duration::ZERO);
        }
    }

    /// The root span of one HTTP operation, named `"{method} {endpoint}"`.
    ///
    /// `endpoint` is the matched route's template. `headers` is read for the
    /// W3C `traceparent` and nothing else — no `tracestate` survives the
    /// sampler and no baggage is extracted — so a client can join its trace to
    /// the relay's, and can lower the relay's sampling for it but never raise
    /// it.
    pub fn server_span(
        &self,
        method: &'static str,
        endpoint: &str,
        headers: &http::HeaderMap,
    ) -> SpanGuard {
        let Some(inner) = &self.inner else {
            return SpanGuard::none();
        };
        let parent = TraceContextPropagator::new().extract_with_context(
            &Context::new(),
            &opentelemetry_http::HeaderExtractor(headers),
        );
        let name = format!("{method} {endpoint}");
        start(
            &inner.tracer,
            &parent,
            Cow::Owned(name),
            SpanKind::Server,
            [Attr::method(method), Attr::endpoint(endpoint)],
        )
    }

    /// The root of work no request owns — a push delivery, which the
    /// dispatcher coalesces from many requests and runs on its own task.
    pub fn root_span(
        &self,
        name: &'static str,
        attrs: impl IntoIterator<Item = Attr>,
    ) -> SpanGuard {
        let Some(inner) = &self.inner else {
            return SpanGuard::none();
        };
        start(
            &inner.tracer,
            &Context::new(),
            Cow::Borrowed(name),
            SpanKind::Internal,
            attrs,
        )
    }
}

fn start(
    tracer: &SdkTracer,
    parent: &Context,
    name: Cow<'static, str>,
    kind: SpanKind,
    attrs: impl IntoIterator<Item = Attr>,
) -> SpanGuard {
    let span = tracer
        .span_builder(name)
        .with_kind(kind)
        .with_attributes(attrs.into_iter().map(Attr::into_inner))
        .start_with_context(tracer, parent);
    if !span.span_context().is_sampled() {
        // Ended at once rather than carried: an unsampled root's children are
        // unsampled too, so nothing below it should pay for a span either.
        return SpanGuard::none();
    }
    SpanGuard {
        cx: Some(parent.with_span(span).with_value(Installed(tracer.clone()))),
    }
}

/// A child of the current span, or nothing when no traced work is current.
///
/// "Current" is the OpenTelemetry [`Context`]: a [`SpanGuard::enter`] on this
/// thread, or a future run under [`SpanGuard::context`] with
/// [`opentelemetry::trace::FutureExt::with_context`].
pub fn span(name: &'static str, attrs: impl IntoIterator<Item = Attr>) -> SpanGuard {
    let cx = Context::current();
    let Some(Installed(tracer)) = cx.get::<Installed>() else {
        return SpanGuard::none();
    };
    if !cx.span().span_context().is_sampled() {
        return SpanGuard::none();
    }
    start(tracer, &cx, Cow::Borrowed(name), SpanKind::Internal, attrs)
}

/// A span, ended when this is dropped. Empty when nothing is being traced, and
/// then every method is a no-op.
#[must_use = "a span ends when its guard is dropped"]
#[derive(Debug)]
pub struct SpanGuard {
    cx: Option<Context>,
}

/// The ids a log record carries to join the trace it was written in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceIds {
    /// 32 lowercase hex characters.
    pub trace_id: String,
    /// 16 lowercase hex characters.
    pub span_id: String,
}

impl SpanGuard {
    /// A guard over no span.
    pub const fn none() -> Self {
        Self { cx: None }
    }

    /// Whether this guard holds a sampled span.
    #[must_use]
    pub const fn is_recording(&self) -> bool {
        self.cx.is_some()
    }

    /// Add an attribute.
    pub fn set(&self, attr: Attr) {
        if let Some(cx) = &self.cx {
            cx.span().set_attribute(attr.into_inner());
        }
    }

    /// Record a point in the span's life, such as a frame an event stream
    /// delivered.
    pub fn event(&self, name: &'static str, attrs: impl IntoIterator<Item = Attr>) {
        if let Some(cx) = &self.cx {
            cx.span()
                .add_event(name, attrs.into_iter().map(Attr::into_inner).collect());
        }
    }

    /// Mark the span failed, with a description chosen at the call site.
    pub fn fail(&self, reason: &'static str) {
        if let Some(cx) = &self.cx {
            cx.span().set_status(Status::error(reason));
        }
    }

    /// The context this span is current in, to run a future or a spawned task
    /// under with [`opentelemetry::trace::FutureExt::with_context`]. The
    /// ambient context when this guard is empty, so the caller need not branch.
    #[must_use]
    pub fn context(&self) -> Context {
        self.cx.clone().unwrap_or_else(Context::current)
    }

    /// Make this span current on this thread until the returned guard drops,
    /// so synchronous calls below it open children. Never hold the result
    /// across an `.await`; use [`SpanGuard::context`] for a future.
    #[must_use = "the span is current only while the guard lives"]
    pub fn enter(&self) -> Option<ContextGuard> {
        self.cx.clone().map(Context::attach)
    }

    /// The ids to correlate a log record with this span, when it is sampled.
    #[must_use]
    pub fn ids(&self) -> Option<TraceIds> {
        let cx = self.cx.as_ref()?;
        let span = cx.span();
        let sc = span.span_context();
        Some(TraceIds {
            trace_id: sc.trace_id().to_string(),
            span_id: sc.span_id().to_string(),
        })
    }
}

impl Drop for SpanGuard {
    fn drop(&mut self) {
        if let Some(cx) = &self.cx {
            cx.span().end();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{recording, InMemorySpanExporter, SpanData};
    use super::*;

    fn finished(exporter: &InMemorySpanExporter) -> Vec<SpanData> {
        exporter.get_finished_spans().expect("spans")
    }

    #[test]
    fn disabled_telemetry_records_nothing_and_children_are_no_ops() {
        let telemetry = Telemetry::disabled();
        assert!(!telemetry.is_enabled());
        let root = telemetry.server_span("GET", "/api/v1/health", &http::HeaderMap::new());
        assert!(!root.is_recording());
        assert!(root.enter().is_none());
        assert!(!span("child", []).is_recording());
        telemetry.shutdown().expect("nothing to flush");
    }

    #[test]
    fn a_child_outside_any_root_is_a_no_op() {
        let (_telemetry, exporter) = recording(1.0);
        drop(span("orphan", []));
        assert!(finished(&exporter).is_empty());
    }

    #[test]
    fn a_child_entered_under_a_root_is_parented_to_it() {
        let (telemetry, exporter) = recording(1.0);
        {
            let root = telemetry.server_span("POST", "/api/v1/sync/ops", &http::HeaderMap::new());
            let _in = root.enter();
            let child = span("store.relay_append", []);
            assert!(child.is_recording());
        }
        let spans = finished(&exporter);
        let root = spans
            .iter()
            .find(|s| s.name == "POST /api/v1/sync/ops")
            .expect("root");
        let child = spans
            .iter()
            .find(|s| s.name == "store.relay_append")
            .expect("child");
        assert_eq!(child.parent_span_id, root.span_context.span_id());
        assert_eq!(child.span_context.trace_id(), root.span_context.trace_id());
        assert_eq!(root.span_kind, SpanKind::Server);
    }

    #[tokio::test]
    async fn a_future_under_a_roots_context_parents_its_children() {
        let (telemetry, exporter) = recording(1.0);
        let root = telemetry.server_span("GET", "/api/v1/meta", &http::HeaderMap::new());
        async {
            drop(span("inside", []));
        }
        .with_context(root.context())
        .await;
        drop(root);
        let spans = finished(&exporter);
        assert_eq!(spans.len(), 2, "{spans:?}");
        let inside = spans.iter().find(|s| s.name == "inside").expect("child");
        assert_ne!(inside.parent_span_id, opentelemetry::trace::SpanId::INVALID);
    }

    #[test]
    fn an_unsampled_root_has_no_children_and_exports_nothing() {
        let (telemetry, exporter) = recording(0.0);
        {
            let root = telemetry.server_span("GET", "/api/v1/meta", &http::HeaderMap::new());
            assert!(!root.is_recording());
            let _in = root.enter();
            drop(span("child", []));
        }
        assert!(finished(&exporter).is_empty());
    }

    #[test]
    fn a_traceparent_joins_the_clients_trace() {
        let (telemetry, exporter) = recording(1.0);
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "traceparent",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
                .parse()
                .expect("a header"),
        );
        drop(telemetry.server_span("GET", "/api/v1/meta", &headers));
        let spans = finished(&exporter);
        assert_eq!(
            spans[0].span_context.trace_id().to_string(),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
        assert_eq!(spans[0].parent_span_id.to_string(), "00f067aa0ba902b7");
    }

    #[test]
    fn events_status_and_ids_reach_the_span() {
        let (telemetry, exporter) = recording(1.0);
        let ids;
        {
            let root = telemetry.root_span("push.dispatch", [Attr::provider("apns")]);
            root.event("attempt", [Attr::attempt(1)]);
            root.set(Attr::result("ok"));
            root.fail("gave up");
            ids = root.ids().expect("sampled");
        }
        let spans = finished(&exporter);
        let span = &spans[0];
        assert_eq!(span.name, "push.dispatch");
        assert_eq!(span.events.len(), 1);
        assert_eq!(span.status, Status::error("gave up"));
        assert_eq!(ids.trace_id, span.span_context.trace_id().to_string());
        assert_eq!(ids.span_id, span.span_context.span_id().to_string());
        assert_eq!(ids.trace_id.len(), 32);
        assert_eq!(ids.span_id.len(), 16);
    }

    /// A collector that never answers in time: each export holds the batch
    /// thread for longer than any test should wait.
    #[derive(Debug)]
    struct Unanswering;

    impl opentelemetry_sdk::trace::SpanExporter for Unanswering {
        async fn export(&self, _batch: Vec<SpanData>) -> opentelemetry_sdk::error::OTelSdkResult {
            std::thread::sleep(Duration::from_secs(3));
            Ok(())
        }
    }

    /// An abandoned exporter neither waits for its flush nor lets the last
    /// clone's drop wait for one, however long the collector takes.
    #[test]
    fn abandoning_returns_at_once_and_disarms_the_flush_on_drop() {
        let provider = SdkTracerProvider::builder()
            .with_batch_exporter(Unanswering)
            .build();
        let telemetry = Telemetry::from_provider(provider);
        let held = telemetry.clone();
        drop(telemetry.root_span("push.dispatch", [Attr::provider("apns")]));

        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            telemetry.abandon();
            drop(telemetry);
            drop(held);
            let _ = done.send(());
        });
        assert!(
            finished.recv_timeout(Duration::from_secs(1)).is_ok(),
            "abandon, or the drop after it, waited on the collector"
        );
    }
}
