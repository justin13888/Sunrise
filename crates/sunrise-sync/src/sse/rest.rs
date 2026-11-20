//! The account and blob routes' way onto the wire: the generated relay client.
//!
//! ADR-0021 generates the REST client from the description the handlers
//! produce, and ADR-0023 put this transport on that description. The account
//! and blob routes go through `sunrise_relay_client::api` (#774): their request
//! bodies are the description's types, and so are the replies they read. What
//! stays hand-written in this module tree is the event stream and the four
//! `/sync` operations the frame table maps, in `events` and `session`.
//!
//! This module is what those routes share: the bounds each call is made
//! under, the device binding in the form the generated parameters take it,
//! and the reading of a call's result back into this transport's errors.

use super::http::{
    protocol, retry_after_value, Reply, BLOB_IDLE_TIMEOUT, BODY_IDLE_TIMEOUT, CHUNK_HEAD_TIMEOUT,
    HEAD_TIMEOUT, MAX_BLOB_BYTES, MAX_EVENT_BYTES,
};
use super::SseTransport;
use crate::transport::TransportError;
use sunrise_relay_client::api;
use sunrise_relay_client::bounded::{BodyBound, Bounds, Exchange, Outcome};

/// A document: every reply on these routes but a fetched attachment, and
/// every refusal on all of them.
const DOCUMENT: BodyBound = BodyBound {
    cap: MAX_EVENT_BYTES,
    idle: BODY_IDLE_TIMEOUT,
};

/// The bounds on a call whose reply is a document.
pub(super) const DOCUMENT_CALL: Bounds = Bounds {
    head: HEAD_TIMEOUT,
    success: DOCUMENT,
    refusal: DOCUMENT,
};

/// The bounds on a chunk upload. The head waits on the chunk arriving at the
/// relay, so it gets the upload's own patience rather than a document's; the
/// reply is an ack, an empty `204` or a short document, and is read as one.
pub(super) const CHUNK_UPLOAD: Bounds = Bounds {
    head: CHUNK_HEAD_TIMEOUT,
    success: DOCUMENT,
    refusal: DOCUMENT,
};

/// The bounds on a blob fetch: the one reply on this transport that is content
/// rather than a document. [`MAX_BLOB_BYTES`] is the relay's own ceiling, and
/// [`BLOB_IDLE_TIMEOUT`] keeps an ordinary gap in a hundred-megabyte transfer
/// from being read as a dead peer. They apply to a 2xx and nothing else: the
/// `404` the fetch handles and whatever `refuse` parses are problem
/// documents, read under the document bounds, so a relay cannot answer a
/// fetch `404` with a hundred megabytes.
pub(super) const BLOB_FETCH: Bounds = Bounds {
    head: HEAD_TIMEOUT,
    success: BodyBound {
        cap: MAX_BLOB_BYTES,
        idle: BLOB_IDLE_TIMEOUT,
    },
    refusal: DOCUMENT,
};

/// The three `header_sig_v2` values, in the form the generated parameters
/// take them.
pub(super) struct Binding {
    pub(super) device: String,
    pub(super) signature: String,
    pub(super) date: String,
}

/// Put `binding`'s three headers on a generated operation's parameters, or
/// leave them unset for a transport that carries no binding.
macro_rules! bind {
    ($params:expr, $binding:expr) => {
        match $binding {
            Some(b) => $params
                .x_sunrise_device(b.device)
                .x_sunrise_device_sig(b.signature)
                .date(b.date),
            None => $params,
        }
    };
}
pub(super) use bind;

/// What a generated call came to, once its outcome is read.
pub(super) enum Settled<T> {
    /// The relay answered with a success the description reads.
    Value(T),
    /// The relay answered with something else, as it sent it.
    Refused(Reply),
}

impl SseTransport {
    /// One call's generated client and the record of its reply.
    pub(super) fn exchange(&self, bounds: Bounds) -> Result<Exchange, TransportError> {
        self.rest
            .as_ref()
            .map_err(|e| TransportError::Unavailable(format!("no relay client: {e}")))?
            .exchange(bounds)
            .map_err(|e| protocol(&e))
    }

    /// The binding for a request whose body is a JSON value: the canonical
    /// form of `body`, the value the generated client serializes and the relay
    /// parses back (ADR-0022).
    pub(super) fn sign_json<T: serde::Serialize>(
        &self,
        method: &str,
        path: &str,
        body: Option<&T>,
    ) -> Result<Option<Binding>, TransportError> {
        let Some(signer) = &self.signer else {
            return Ok(None);
        };
        let date = sunrise_http_sig::date_header(signer.now_ms());
        let signature =
            sunrise_http_sig::sign_with(|msg| signer.sign(msg), method, path, &date, body)
                .map_err(|e| protocol(&e))?;
        Ok(Some(Binding {
            device: signer.device_id(),
            signature,
            date,
        }))
    }

    /// The binding for a request with no body.
    pub(super) fn sign_empty(
        &self,
        method: &str,
        path: &str,
    ) -> Result<Option<Binding>, TransportError> {
        self.sign_json(method, path, None::<&serde_json::Value>)
    }

    /// The binding for a request whose body is opaque bytes, which already are
    /// their canonical form.
    pub(super) fn sign_bytes(&self, method: &str, path: &str, body: &[u8]) -> Option<Binding> {
        let signer = self.signer.as_ref()?;
        let date = sunrise_http_sig::date_header(signer.now_ms());
        let signature =
            sunrise_http_sig::sign_binary_with(|msg| signer.sign(msg), method, path, &date, body);
        Some(Binding {
            device: signer.device_id(),
            signature,
            date,
        })
    }
}

/// Read a generated call's result against what its relay sent.
///
/// The record speaks first. A relay that was unreachable, stalled or sent too
/// much is [`TransportError::Unavailable`] whatever the generated client made
/// of it. A reply that is not a success is handed back whole for the route to
/// classify, because a documented status whose body did not decode reaches
/// the generated client's caller with no status at all. A success the
/// description cannot read, and a request the generated client would not
/// build, are malformed exchanges.
pub(super) fn settle<T, E: std::fmt::Display>(
    exchange: &Exchange,
    result: Result<api::ResponseValue<T>, api::Error<E>>,
) -> Result<Settled<T>, TransportError> {
    let error = match result {
        Ok(value) => return Ok(Settled::Value(value.into_inner())),
        Err(error) => error,
    };
    match exchange.outcome() {
        Some(Outcome::Unreachable(message)) => Err(TransportError::Unavailable(message)),
        Some(Outcome::Replied(raw)) if !raw.is_success() => Ok(Settled::Refused(Reply {
            status: hyper::StatusCode::from_u16(raw.status).map_err(|e| protocol(&e))?,
            date: raw.date,
            retry_after: raw.retry_after.as_deref().and_then(retry_after_value),
            bytes: raw.refusal_body,
        })),
        Some(Outcome::Replied(_)) | None => Err(protocol(&error)),
    }
}
