//! Rate limiting: the policy in `docs/06-server/api.md` §Rate limits, enforced.
//!
//! Two layers, because the two kinds of limit need two different things
//! before they can count:
//!
//! - **Per client address, before anything else runs.** [`Admission`] is an
//!   interceptor on the whole router. It charges the request to its route
//!   group's bucket for the client address, and refuses a request to an
//!   authenticated route outright while that address is over its failed-auth
//!   budget — before the bearer verifier spends anything on it. It is the
//!   outermost interceptor, so a flood is refused before `BodySize` reads a
//!   chunked body.
//! - **Per device and per account, after authentication.** An op batch's cost
//!   is its op count and a chunk's is its length, and the device is only known
//!   once its signature has verified. Those budgets are charged by the
//!   handlers through [`Limiter`], and refused with the same [`RateLimited`].
//!
//! A refusal is always `429` with `Retry-After` and code `RATE_LIMITED`, and it
//! increments `sunrise_ratelimit_rejected_total{endpoint,scope}`. The `429` is
//! in the description of every operation, because [`Admission`] covers every
//! operation and its refusal type describes itself.
//!
//! # Whose address
//!
//! The client address is kynos's [`Forwarded`] resolution under the router's
//! trusted-proxy list (`[server] trusted_proxies`). With the list empty — the
//! default — it is the socket peer and every forwarding header is ignored;
//! with proxies listed, it is the right-most address the chain gives that is
//! not one of them.

pub mod bucket;
pub mod policy;
pub mod store;

#[cfg(test)]
mod tests;

use crate::api::error::{codes, ApiError};
use crate::api::signed::Caller;
use crate::state::ServerState;
use bucket::Outcome;
use kynos::http::forwarded::Forwarded;
use kynos::http::{HeaderValue, Request, StatusCode};
use kynos::middleware::{Continued, Interceptor, Next};
use kynos::openapi::model::schema::types::SchemaType;
use kynos::openapi::{Header, RefOr, Schema};
use kynos::response::{IntoResponse, Responses, ShortCircuit};
use kynos::schema::registry::Registry;
use policy::{address_key, address_net, failed_auth_quota, route_group, Budget, UNLISTED};
use std::net::IpAddr;
use std::sync::Arc;
use store::{Gate, LimiterStore, MemoryStore, Permit, UploadLedger};

/// What a refusal was counted against: the metric's `scope` label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// A client address.
    Ip,
    /// An account: the per-account budgets, and the per-device ones when the
    /// caller signed with no device (single-tenant self-host).
    Account,
    /// A device.
    Device,
}

impl Scope {
    /// The label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ip => "ip",
            Self::Account => "account",
            Self::Device => "device",
        }
    }
}

/// The rate-limit state a relay holds.
///
/// Cheap to clone: everything inside is shared.
#[derive(Debug, Clone)]
pub struct Limiter {
    store: Arc<dyn LimiterStore>,
    streams: Gate,
    uploads: UploadLedger,
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new(Arc::new(MemoryStore::new()))
    }
}

/// A refusal's subject, for the log line.
enum Who<'a> {
    Address(Option<IpAddr>),
    Caller(&'a Caller),
}

impl Limiter {
    /// A limiter over `store`.
    #[must_use]
    pub fn new(store: Arc<dyn LimiterStore>) -> Self {
        Self {
            store,
            streams: Gate::new(),
            uploads: UploadLedger::new(),
        }
    }

    /// Charge `cost` units of `budget` to the caller's device, or to its
    /// account where it signed with no device.
    ///
    /// # Errors
    /// [`RateLimited`] when the budget is spent.
    pub async fn charge(
        &self,
        state: &ServerState,
        endpoint: &'static str,
        budget: Budget,
        caller: &Caller,
        cost: u64,
    ) -> Result<(), RateLimited> {
        let limits = &state.config.limits;
        if !limits.enabled {
            return Ok(());
        }
        let (scope, subject) = subject_of(caller);
        let key = format!("{}|{}:{subject}", budget.as_str(), scope.as_str());
        let outcome = self
            .store
            .take(&key, budget.quota(limits), cost, state.clock.now_ms())
            .await;
        if outcome.allowed {
            return Ok(());
        }
        Err(refuse(
            state,
            endpoint,
            scope,
            budget.as_str(),
            &Who::Caller(caller),
            outcome,
        ))
    }

    /// Hold one of the caller's event-stream slots for as long as the
    /// returned permit lives. `None` when the limits are off.
    ///
    /// # Errors
    /// [`RateLimited`] when the device already holds `[limits] streams`.
    pub fn open_stream(
        &self,
        state: &ServerState,
        endpoint: &'static str,
        caller: &Caller,
    ) -> Result<Option<Permit>, RateLimited> {
        let limits = &state.config.limits;
        if !limits.enabled {
            return Ok(None);
        }
        let (scope, subject) = subject_of(caller);
        let key = format!("{}:{subject}", scope.as_str());
        match self.streams.acquire(&key, limits.streams) {
            Some(permit) => Ok(Some(permit)),
            None => Err(refuse(
                state,
                endpoint,
                scope,
                "streams",
                &Who::Caller(caller),
                // A slot frees the moment another stream ends, which nothing
                // here can predict; the keep-alive interval is how long a
                // client that left uncleanly takes to be noticed.
                Outcome {
                    allowed: false,
                    retry_after_ms: crate::api::sync::KEEP_ALIVE_SECS * 1_000,
                    remaining: 0,
                    first_refusal: true,
                },
            )),
        }
    }

    /// Count `upload` against the caller's account until it is finalized or
    /// goes an hour untouched.
    ///
    /// # Errors
    /// [`RateLimited`] when the account already holds `[limits] open_uploads`.
    pub fn open_upload(
        &self,
        state: &ServerState,
        endpoint: &'static str,
        caller: &Caller,
        upload: [u8; 16],
    ) -> Result<(), RateLimited> {
        let limits = &state.config.limits;
        if !limits.enabled {
            return Ok(());
        }
        let account = &caller.principal.account.account_id;
        let now_ms = state.clock.now_ms();
        self.uploads
            .open(account, upload, limits.open_uploads, now_ms)
            .map_err(|wait_ms| {
                refuse(
                    state,
                    endpoint,
                    Scope::Account,
                    "open_uploads",
                    &Who::Caller(caller),
                    Outcome {
                        allowed: false,
                        retry_after_ms: wait_ms,
                        remaining: 0,
                        first_refusal: true,
                    },
                )
            })
    }

    /// `upload` received a chunk, so it is still in use.
    pub fn touch_upload(&self, state: &ServerState, caller: &Caller, upload: [u8; 16]) {
        self.uploads.touch(
            &caller.principal.account.account_id,
            upload,
            state.clock.now_ms(),
        );
    }

    /// `upload` was finalized and no longer counts.
    pub fn close_upload(&self, caller: &Caller, upload: [u8; 16]) {
        self.uploads
            .close(&caller.principal.account.account_id, upload);
    }
}

/// The scope and the id a per-device budget is keyed on.
fn subject_of(caller: &Caller) -> (Scope, &str) {
    caller.device.as_ref().map_or(
        (Scope::Account, caller.principal.account.account_id.as_str()),
        |device| (Scope::Device, device.device_id.as_str()),
    )
}

/// Count a refusal, log the first of a run, and build the response.
fn refuse(
    state: &ServerState,
    endpoint: &str,
    scope: Scope,
    reason: &'static str,
    who: &Who<'_>,
    outcome: Outcome,
) -> RateLimited {
    state.metrics.incr_with(
        "sunrise_ratelimit_rejected_total",
        &[("endpoint", endpoint), ("scope", scope.as_str())],
    );
    let refusal = RateLimited::after_secs(outcome.retry_after_secs());
    // One line per run of refusals, not per request: a flood would otherwise
    // be a log flood, and the counter above already has the volume.
    if outcome.first_refusal {
        let endpoint = crate::api::observe::templated(endpoint);
        let delay_ms = outcome.retry_after_ms;
        match who {
            Who::Address(client) => tracing::info!(
                ev = "srv.ratelimit.rejected",
                endpoint = %endpoint,
                reason,
                client_net = %address_net(*client),
                delay_ms,
                "rate limit reached; refusing with 429"
            ),
            Who::Caller(caller) => tracing::info!(
                ev = "srv.ratelimit.rejected",
                endpoint = %endpoint,
                reason,
                account_h = %crate::logging::account_h(&caller.principal.account.account_id),
                delay_ms,
                "rate limit reached; refusing with 429"
            ),
        }
    }
    refusal
}

/// The per-address interceptor. See the module docs.
#[derive(Debug, Clone, Copy, Default)]
pub struct Admission;

/// The failed-auth bucket for a client address.
fn failed_auth_key(address: &str) -> String {
    format!("auth_failed|{address}")
}

impl Interceptor<ServerState> for Admission {
    type Reads = ();
    type Adds = ();
    type Short = RateLimited;

    async fn intercept(
        &self,
        request: Request,
        reads: (),
        state: &ServerState,
        next: Next<'_, ServerState>,
    ) -> Result<Continued<()>, RateLimited> {
        let () = reads;
        let limits = &state.config.limits;
        if !limits.enabled {
            return Ok(next.run(request).await);
        }

        let route = next.route();
        let endpoint = route.path().to_owned();
        let group = route_group(route.method().as_wire_str(), &endpoint).unwrap_or(UNLISTED);
        let client = request
            .extensions()
            .get::<Forwarded>()
            .and_then(Forwarded::client);
        let address = address_key(client);
        let limiter = &state.limiter;

        // Counted before anything verifies a credential: a client guessing
        // bearers is refused without costing the verifier a signature check.
        if group.authenticates() {
            let outcome = limiter
                .store
                .peek(
                    &failed_auth_key(&address),
                    failed_auth_quota(limits),
                    state.clock.now_ms(),
                )
                .await;
            if !outcome.allowed {
                return Err(refuse(
                    state,
                    &endpoint,
                    Scope::Ip,
                    "failed_auth",
                    &Who::Address(client),
                    outcome,
                ));
            }
        }

        let outcome = limiter
            .store
            .take(
                &format!("{}|{address}", group.as_str()),
                group.quota(limits),
                1,
                state.clock.now_ms(),
            )
            .await;
        if !outcome.allowed {
            return Err(refuse(
                state,
                &endpoint,
                Scope::Ip,
                group.as_str(),
                &Who::Address(client),
                outcome,
            ));
        }

        let continued = next.run(request).await;
        if group.authenticates() && continued.status() == StatusCode::UNAUTHORIZED {
            limiter
                .store
                .take(
                    &failed_auth_key(&address),
                    failed_auth_quota(limits),
                    1,
                    state.clock.now_ms(),
                )
                .await;
        }
        Ok(continued)
    }
}

/// The problem document a `429` carries.
///
/// A struct of its own rather than a variant of [`ApiError`], because a
/// refusal also carries a `Retry-After` header, and the derive has no way to
/// put a header on a response. [`RateLimited`] wraps it and adds one.
#[derive(Debug, thiserror::Error, kynos::ApiError)]
#[error("rate limit reached; retry in {retry_after_secs} s")]
#[problem(
    status = 429,
    title = "Too many requests",
    type = "https://sunrise.app/problems/rate-limited"
)]
struct RateLimitedProblem {
    /// Always [`codes::RATE_LIMITED`].
    #[problem(extension)]
    code: &'static str,
    /// The same number `Retry-After` carries, for the detail sentence.
    retry_after_secs: u64,
}

/// A `429 RATE_LIMITED` with `Retry-After`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimited {
    retry_after_secs: u64,
}

impl RateLimited {
    /// A refusal telling the client to wait `secs` seconds, at least one.
    #[must_use]
    pub fn after_secs(secs: u64) -> Self {
        Self {
            retry_after_secs: secs.max(1),
        }
    }

    /// The `Retry-After` this refusal sends.
    #[must_use]
    pub const fn retry_after_secs(&self) -> u64 {
        self.retry_after_secs
    }
}

impl IntoResponse for RateLimited {
    fn into_response(self) -> kynos::http::Response {
        let mut response = RateLimitedProblem {
            code: codes::RATE_LIMITED,
            retry_after_secs: self.retry_after_secs,
        }
        .into_response();
        response.headers_mut().insert(
            kynos::http::header::RETRY_AFTER,
            HeaderValue::from(self.retry_after_secs),
        );
        response
    }
}

impl Responses for RateLimited {
    fn responses(registry: &mut Registry) -> kynos::openapi::Responses {
        let mut responses = <RateLimitedProblem as Responses>::responses(registry);
        if let Some(RefOr::Item(response)) = responses.responses.get_mut("429") {
            response.headers.insert(
                "Retry-After".to_owned(),
                RefOr::Item(
                    Header::new(Schema::of_type(SchemaType::Integer))
                        .with_description(
                            "Whole seconds to wait before retrying; at least 1. A retry \
                             sooner is refused again.",
                        )
                        .required(true),
                ),
            );
        }
        responses
    }
}

impl ShortCircuit for RateLimited {
    const STATUSES: &'static [u16] = &[429];
}

/// A handler failure that may also be a rate-limit refusal.
///
/// What the handlers that charge a per-device budget return in place of
/// [`ApiError`]. `?` converts either, so a handler body reads the same.
#[derive(Debug)]
pub enum Throttled {
    /// Any other failure.
    Api(ApiError),
    /// A budget was spent.
    Limited(RateLimited),
}

impl From<ApiError> for Throttled {
    fn from(e: ApiError) -> Self {
        Self::Api(e)
    }
}

impl From<RateLimited> for Throttled {
    fn from(e: RateLimited) -> Self {
        Self::Limited(e)
    }
}

impl IntoResponse for Throttled {
    fn into_response(self) -> kynos::http::Response {
        match self {
            Self::Api(e) => e.into_response(),
            Self::Limited(e) => e.into_response(),
        }
    }
}

impl Responses for Throttled {
    fn responses(registry: &mut Registry) -> kynos::openapi::Responses {
        let mut responses = <ApiError as Responses>::responses(registry);
        responses.merge_from(&<RateLimited as Responses>::responses(registry));
        responses
    }
}
