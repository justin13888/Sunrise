//! The generated client under a caller's bounds, with the raw reply kept.
//!
//! `sunrise_sync`'s SSE transport reaches the account and blob routes through
//! [`crate::api`] (ADR-0021, ADR-0023, #774), and it needs two things the
//! generated client does not give it on its own.
//!
//! **Bounds.** Every wait and read on that transport is bounded: a deadline on
//! the response head, a size cap and a per-frame idle deadline on the body, and
//! a refusal read as a document whatever the route. The generated client reads
//! a success body whole and has nowhere to state a head deadline, so the
//! bounds live in an [`api::HttpBackend`]: the seam the generated client
//! executes every request through. The backend buffers the body under the
//! bounds and hands the generated client a response that is already complete.
//!
//! **The reply as sent.** A refusal is classified by its status, its problem
//! document's `code`, its `Date` and its `Retry-After`, and a documented status
//! whose body does not decode reaches the caller as [`api::Error::Decode`],
//! which carries no status at all. So the backend records what came back
//! beside the typed result, and the caller reads the refusal from that record
//! rather than from whatever the generated error kept of it.
//!
//! Each [`BoundedRelay::exchange`] is one call: a generated client over a
//! backend whose record belongs to that call alone, so two calls in flight on
//! one transport never read each other's reply. The connection pool is shared:
//! every exchange executes on the one `reqwest::Client` the relay was built
//! with.

use crate::api;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// The size cap and idle deadline one body is read under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyBound {
    /// The most bytes the body may carry. One byte more abandons the read.
    pub cap: usize,
    /// The longest gap between two frames of the body. The timer is armed per
    /// frame, so a body that keeps arriving is never cut off by it.
    pub idle: Duration,
}

/// The bounds one call is made under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// The longest wait for the response head.
    pub head: Duration,
    /// The bound on a success body: the reply the caller asked for.
    pub success: BodyBound,
    /// The bound on any other body. A refusal is a problem document on every
    /// route, so a caller states it separately rather than letting a route
    /// whose success is an attachment read its `404` under an attachment's
    /// cap and patience.
    pub refusal: BodyBound,
}

/// What a call's relay answered, as it answered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawReply {
    /// The status code.
    pub status: u16,
    /// The relay's `Date` header, when it sent a readable one.
    pub date: Option<String>,
    /// The relay's `Retry-After` header as sent, when it sent a readable one.
    pub retry_after: Option<String>,
    /// The body of a reply that is not a success. A success body is the
    /// generated client's to decode and is not kept twice, so this is empty
    /// for one.
    pub refusal_body: Vec<u8>,
}

impl RawReply {
    /// Whether the status is a 2xx.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// How a call's exchange with the relay ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The relay answered, and its body arrived within the bounds.
    Replied(RawReply),
    /// The relay could not be reached, did not answer within the head
    /// deadline, or sent a body that broke off, stalled past the idle
    /// deadline or exceeded its cap. The text says which.
    Unreachable(String),
}

/// The relay at one origin, reached through the generated client under
/// per-call bounds.
#[derive(Clone)]
pub struct BoundedRelay {
    http: reqwest::Client,
    base_url: String,
    bearer: Option<String>,
}

// Hand-written: the bearer is a credential, and a derived `Debug` would print it.
impl std::fmt::Debug for BoundedRelay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundedRelay")
            .field("base_url", &self.base_url)
            .field("bearer", &self.bearer.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

impl BoundedRelay {
    /// The relay at `base_url` (the origin, no path), presenting `bearer`
    /// where there is one.
    ///
    /// The connection settings are the hand-written transport's, so moving a
    /// route onto the generated client changes nothing on the wire: HTTP/1.1,
    /// no redirect followed (a device binding signs one path, and a redirect
    /// would present it on another), and no proxy read from the environment.
    ///
    /// # Errors
    /// What reqwest reports when the client cannot be built, as text.
    pub fn new(base_url: &str, bearer: Option<&str>) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .http1_only()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
            bearer: bearer.map(ToOwned::to_owned),
        })
    }

    /// A generated client for one call under `bounds`, and the record of what
    /// that call's relay answered.
    ///
    /// Every account and blob operation requires the `AccountToken` scheme,
    /// and the generated client refuses to send one without a registered
    /// credential. A relay with no bearer is a real deployment (a self-host
    /// relay running `NullVerifier`), and the hand-written transport sent it
    /// no `Authorization` header at all. So a relay built without one
    /// registers an empty placeholder, and the backend removes the header it
    /// produces before the request leaves: the wire is what it was.
    ///
    /// # Errors
    /// What the generated client reports when the origin is not usable, as
    /// text.
    pub fn exchange(&self, bounds: Bounds) -> Result<Exchange, String> {
        let record = Arc::new(Mutex::new(None));
        let backend = BoundedBackend {
            http: self.http.clone(),
            bounds,
            record: Arc::clone(&record),
            strip_authorization: self.bearer.is_none(),
        };
        let client = api::Client::with_backend(Arc::new(backend), &self.base_url)
            .map_err(|e| e.to_string())?
            .with_credential(
                "AccountToken",
                api::Credential::Bearer(api::SecretString::from(
                    self.bearer.clone().unwrap_or_default(),
                )),
            );
        Ok(Exchange { client, record })
    }
}

/// One call's generated client, and what its relay answered.
#[derive(Debug)]
pub struct Exchange {
    client: api::Client,
    record: Arc<Mutex<Option<Outcome>>>,
}

impl Exchange {
    /// The generated client to make the call with.
    #[must_use]
    pub fn client(&self) -> &api::Client {
        &self.client
    }

    /// How the exchange ended, or `None` when nothing was sent: the generated
    /// client refused the request while building it.
    #[must_use]
    pub fn outcome(&self) -> Option<Outcome> {
        self.record
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The backend that executes one exchange's request under its bounds.
#[derive(Debug)]
struct BoundedBackend {
    http: reqwest::Client,
    bounds: Bounds,
    record: Arc<Mutex<Option<Outcome>>>,
    strip_authorization: bool,
}

impl api::HttpBackend for BoundedBackend {
    fn execute(&self, mut request: reqwest::Request) -> api::ExecuteFuture<'_> {
        if self.strip_authorization {
            request.headers_mut().remove(reqwest::header::AUTHORIZATION);
        }
        Box::pin(async move {
            let ended = run(&self.http, request, self.bounds).await;
            let (outcome, response) = match ended {
                Ok((reply, response)) => (Outcome::Replied(reply), Ok(response)),
                Err(Ended::Failed(e)) => (
                    Outcome::Unreachable(e.to_string()),
                    Err(api::TransportError::new(e)),
                ),
                Err(Ended::Bound(message)) => (Outcome::Unreachable(message), Ok(breach())),
            };
            *self.record.lock().unwrap_or_else(PoisonError::into_inner) = Some(outcome);
            response
        })
    }
}

/// Why an exchange ended without a whole reply.
enum Ended {
    /// reqwest's own failure: the dial, the send, or a body that broke off.
    Failed(reqwest::Error),
    /// A bound this backend holds expired or was exceeded.
    Bound(String),
}

/// Send `request`, and read its whole reply under `bounds`.
async fn run(
    http: &reqwest::Client,
    request: reqwest::Request,
    bounds: Bounds,
) -> Result<(RawReply, reqwest::Response), Ended> {
    // Dropping the pending request on expiry drops its connection rather than
    // returning it to the pool, so the next call dials afresh instead of
    // inheriting the peer that went quiet.
    let mut response = tokio::time::timeout(bounds.head, http.execute(request))
        .await
        .map_err(|_| Ended::Bound(format!("no response head within {:?}", bounds.head)))?
        .map_err(Ended::Failed)?;
    let status = response.status();
    let headers = response.headers().clone();
    let version = response.version();
    let bound = if status.is_success() {
        bounds.success
    } else {
        bounds.refusal
    };
    let mut body = Vec::new();
    loop {
        let Ok(next) = tokio::time::timeout(bound.idle, response.chunk()).await else {
            return Err(Ended::Bound(format!(
                "response body stalled: no data for {}s",
                bound.idle.as_secs()
            )));
        };
        let Some(chunk) = next.map_err(Ended::Failed)? else {
            break;
        };
        // Half a document is not a document, so a body past its cap is
        // abandoned whole rather than parsed short.
        if body.len() + chunk.len() > bound.cap {
            return Err(Ended::Bound(format!(
                "response body exceeds {} bytes",
                bound.cap
            )));
        }
        body.extend_from_slice(&chunk);
    }
    let text = |name: reqwest::header::HeaderName| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(ToOwned::to_owned)
    };
    let reply = RawReply {
        status: status.as_u16(),
        date: text(reqwest::header::DATE),
        retry_after: text(reqwest::header::RETRY_AFTER),
        refusal_body: if status.is_success() {
            Vec::new()
        } else {
            body.clone()
        },
    };
    let mut rebuilt = http::Response::new(body);
    *rebuilt.status_mut() = status;
    *rebuilt.version_mut() = version;
    *rebuilt.headers_mut() = headers;
    Ok((reply, reqwest::Response::from(rebuilt)))
}

/// What the generated client is handed when a bound ended the exchange.
///
/// The backend's error type wraps a `reqwest::Error`, which nothing outside
/// reqwest can make, so a bound cannot be reported through it. The generated
/// client reads this as an undocumented status and fails the call; the caller
/// reads the cause from [`Exchange::outcome`], which says
/// [`Outcome::Unreachable`] before it looks at the call's result at all.
fn breach() -> reqwest::Response {
    let mut response = http::Response::new(Vec::new());
    *response.status_mut() = reqwest::StatusCode::GATEWAY_TIMEOUT;
    reqwest::Response::from(response)
}

#[cfg(test)]
mod tests {
    use super::{BodyBound, BoundedRelay, Bounds, Outcome};
    use std::time::Duration;

    fn bounds() -> Bounds {
        let document = BodyBound {
            cap: 64,
            idle: Duration::from_secs(2),
        };
        Bounds {
            head: Duration::from_secs(2),
            success: document,
            refusal: document,
        }
    }

    /// Nothing is sent before the call is made, so nothing is recorded.
    #[test]
    fn an_exchange_records_nothing_until_it_is_used() {
        let relay = BoundedRelay::new("http://127.0.0.1:9", Some("t")).expect("a client");
        let exchange = relay.exchange(bounds()).expect("an origin");
        assert_eq!(exchange.outcome(), None);
    }

    #[test]
    fn the_bearer_never_reaches_debug() {
        let relay =
            BoundedRelay::new("http://127.0.0.1:9", Some("secret-bearer")).expect("a client");
        let shown = format!("{relay:?}");
        assert!(!shown.contains("secret-bearer"), "{shown}");
    }

    /// A refused dial is recorded as unreachable, in reqwest's words.
    #[tokio::test]
    async fn a_dial_that_fails_is_recorded_as_unreachable() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        let addr = listener.local_addr().expect("an address");
        drop(listener);
        let relay = BoundedRelay::new(&format!("http://{addr}"), Some("t")).expect("a client");
        let exchange = relay.exchange(bounds()).expect("an origin");
        let result = exchange.client().fetch_key_bundle(None).await;
        assert!(result.is_err());
        assert!(
            matches!(exchange.outcome(), Some(Outcome::Unreachable(_))),
            "{:?}",
            exchange.outcome()
        );
    }
}
