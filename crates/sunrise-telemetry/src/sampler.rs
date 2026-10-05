//! The sampler: parent-based, and capped at the configured ratio whatever a
//! client asks for.
//!
//! The stock `ParentBased(TraceIdRatioBased(r))` follows a remote parent's
//! `sampled` flag outright. On a relay that is a lever any client can pull: a
//! `traceparent` with `sampled=1` on every request turns a 1% sampler into a
//! 100% one, and every span it forces costs the operator's collector. So a
//! remote parent is respected only in the direction that costs nothing:
//!
//! | Parent | Decision |
//! |---|---|
//! | none (a root) | the ratio, over the trace id the server minted |
//! | local, sampled or not | the parent's decision, so a trace is whole or absent |
//! | remote, `sampled=0` | dropped, as the client asked |
//! | remote, `sampled=1` | the ratio, over a draw the server makes |
//!
//! The last row draws its own randomness rather than reading the trace id,
//! because a remote parent's trace id is the client's to choose:
//! `TraceIdRatioBased` compares the id's low bits against a threshold, and a
//! client that picks ids under it would be sampled every time. With the draw
//! the fraction of client-propagated traces sampled can never exceed the ratio.
//!
//! The client's `tracestate` is not carried either. It is free text the client
//! wrote, and a span's trace state is exported beside it.

use opentelemetry::trace::{Link, SpanKind, TraceContextExt as _, TraceId, TraceState};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::trace::{
    IdGenerator as _, RandomIdGenerator, Sampler, SamplingDecision, SamplingResult, ShouldSample,
};

/// The relay's sampler. See the module documentation for the table it applies.
#[derive(Debug, Clone)]
pub struct CappedSampler {
    ratio: f64,
    ids: RandomIdGenerator,
}

impl CappedSampler {
    /// Sample `ratio` of traces, clamped to `[0, 1]`; a NaN samples nothing.
    #[must_use]
    pub fn new(ratio: f64) -> Self {
        let ratio = if ratio.is_nan() {
            0.0
        } else {
            ratio.clamp(0.0, 1.0)
        };
        Self {
            ratio,
            ids: RandomIdGenerator::default(),
        }
    }

    /// The ratio in force.
    #[must_use]
    pub const fn ratio(&self) -> f64 {
        self.ratio
    }

    fn by_ratio(&self, trace_id: TraceId) -> SamplingDecision {
        Sampler::TraceIdRatioBased(self.ratio)
            .should_sample(None, trace_id, "", &SpanKind::Internal, &[], &[])
            .decision
    }
}

impl ShouldSample for CappedSampler {
    fn should_sample(
        &self,
        parent_context: Option<&Context>,
        trace_id: TraceId,
        _name: &str,
        _span_kind: &SpanKind,
        _attributes: &[KeyValue],
        _links: &[Link],
    ) -> SamplingResult {
        let parent = parent_context
            .filter(|cx| cx.has_active_span())
            .map(|cx| cx.span().span_context().clone())
            .filter(opentelemetry::trace::SpanContext::is_valid);
        let decision = match parent {
            None => self.by_ratio(trace_id),
            Some(parent) if !parent.is_sampled() => SamplingDecision::Drop,
            Some(parent) if !parent.is_remote() => SamplingDecision::RecordAndSample,
            Some(_) => self.by_ratio(self.ids.new_trace_id()),
        };
        SamplingResult {
            decision,
            attributes: Vec::new(),
            trace_state: TraceState::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{SpanContext, SpanId, TraceFlags};

    fn decide(sampler: &CappedSampler, parent: Option<SpanContext>, trace_id: TraceId) -> bool {
        let cx = parent.map(|sc| Context::new().with_remote_span_context(sc));
        let result = sampler.should_sample(cx.as_ref(), trace_id, "t", &SpanKind::Server, &[], &[]);
        result.decision == SamplingDecision::RecordAndSample
    }

    fn remote(trace_id: TraceId, sampled: bool) -> SpanContext {
        SpanContext::new(
            trace_id,
            SpanId::from(1),
            if sampled {
                TraceFlags::SAMPLED
            } else {
                TraceFlags::default()
            },
            true,
            TraceState::default(),
        )
    }

    /// A trace id whose low bits sit at the very bottom of the range, which
    /// `TraceIdRatioBased` samples at any ratio above zero. A client that
    /// wants to be sampled sends exactly this.
    const FAVOURED: TraceId = TraceId::from_bytes([
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0, 0, 0, 0, 0,
    ]);

    #[test]
    fn a_ratio_outside_the_unit_interval_is_clamped() {
        assert!((CappedSampler::new(7.0).ratio() - 1.0).abs() < f64::EPSILON);
        assert!(CappedSampler::new(-1.0).ratio().abs() < f64::EPSILON);
        assert!(CappedSampler::new(f64::NAN).ratio().abs() < f64::EPSILON);
    }

    #[test]
    fn a_remote_sampled_flag_cannot_force_sampling_past_a_zero_ratio() {
        let sampler = CappedSampler::new(0.0);
        for _ in 0..1000 {
            assert!(!decide(&sampler, Some(remote(FAVOURED, true)), FAVOURED));
        }
    }

    #[test]
    fn a_remote_unsampled_flag_is_respected_even_at_full_ratio() {
        let sampler = CappedSampler::new(1.0);
        assert!(!decide(&sampler, Some(remote(FAVOURED, false)), FAVOURED));
        assert!(decide(&sampler, Some(remote(FAVOURED, true)), FAVOURED));
    }

    /// The draw ignores the client's trace id, so a favoured id is sampled at
    /// the ratio and not every time.
    #[test]
    fn a_remote_sampled_flag_is_capped_at_the_ratio() {
        let sampler = CappedSampler::new(0.1);
        let sampled = (0..4000)
            .filter(|_| decide(&sampler, Some(remote(FAVOURED, true)), FAVOURED))
            .count();
        // Binomial(4000, 0.1): mean 400, standard deviation 19. Ten deviations
        // either way, so the bound fails on a broken sampler and not by chance.
        assert!((210..=590).contains(&sampled), "{sampled} of 4000");
    }

    #[test]
    fn a_root_is_sampled_at_the_ratio() {
        let sampler = CappedSampler::new(0.25);
        let ids = RandomIdGenerator::default();
        let sampled = (0..4000)
            .filter(|_| decide(&sampler, None, ids.new_trace_id()))
            .count();
        // Binomial(4000, 0.25): mean 1000, standard deviation 27.
        assert!((730..=1270).contains(&sampled), "{sampled} of 4000");
        assert!(decide(&CappedSampler::new(1.0), None, ids.new_trace_id()));
        assert!(!decide(&CappedSampler::new(0.0), None, ids.new_trace_id()));
    }

    #[test]
    fn a_local_parent_decides_for_its_children() {
        let sampler = CappedSampler::new(0.0);
        let local = |sampled| {
            let sc = SpanContext::new(
                FAVOURED,
                SpanId::from(2),
                if sampled {
                    TraceFlags::SAMPLED
                } else {
                    TraceFlags::default()
                },
                false,
                TraceState::default(),
            );
            Context::new().with_remote_span_context(sc)
        };
        // `with_remote_span_context` keeps the context's own `is_remote`, so a
        // local parent is one built with `false` above.
        let yes = sampler.should_sample(
            Some(&local(true)),
            FAVOURED,
            "t",
            &SpanKind::Internal,
            &[],
            &[],
        );
        let no = sampler.should_sample(
            Some(&local(false)),
            FAVOURED,
            "t",
            &SpanKind::Internal,
            &[],
            &[],
        );
        assert_eq!(yes.decision, SamplingDecision::RecordAndSample);
        assert_eq!(no.decision, SamplingDecision::Drop);
    }

    #[test]
    fn the_clients_trace_state_is_not_carried() {
        let sc = SpanContext::new(
            FAVOURED,
            SpanId::from(1),
            TraceFlags::SAMPLED,
            true,
            TraceState::from_key_value([("vendor", "bearer-SENTINEL")]).expect("valid state"),
        );
        let cx = Context::new().with_remote_span_context(sc);
        let result = CappedSampler::new(1.0).should_sample(
            Some(&cx),
            FAVOURED,
            "t",
            &SpanKind::Server,
            &[],
            &[],
        );
        assert_eq!(result.trace_state, TraceState::default());
    }
}
