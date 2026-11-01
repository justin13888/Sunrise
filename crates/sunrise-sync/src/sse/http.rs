//! One request and its reply: the bounds on every wait and read, the device
//! binding, and the readers for what comes back.

use super::SseTransport;
use crate::transport::TransportError;
use http_body_util::{BodyExt as _, Full};
use hyper::body::Bytes;

/// The largest SSE event this client will buffer before giving up.
///
/// A relay that never emits a blank line would otherwise grow this buffer
/// without bound. The frame cap is the natural ceiling: an event carries one
/// frame, base64-encoded.
///
/// It has a second job, named here because it is not visible from the sentence
/// above: it is also the cap on every response body that is a **document** —
/// [`SseTransport::call`]'s replies, a refused stream's problem document, a
/// chunk upload's ack, and **a refused blob fetch's problem document**.
/// Nothing legitimate on those routes is anywhere near eight mebibytes, so
/// there it is not sizing the reply; it is denying a relay the choice of how
/// much memory this client holds. The one body it does not bound is an
/// attachment itself, which takes [`MAX_BLOB_BYTES`] — the blob route is
/// bounded by both constants, one per side of `is_success`.
pub(super) const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;

/// The largest *blob* this client will hold out of one reply.
///
/// The blob surface is the one route whose body is content rather than a
/// document: `blob_fetch` reads a whole committed attachment into a `Vec<u8>`.
/// Its *successful* body, that is — a refused blob fetch answers with a problem
/// document like every other route and is capped by [`MAX_EVENT_BYTES`] like
/// every other route, so this number applies to one side of `is_success` and
/// not to the route.
/// `crates/sunrise-server/src/api/blobs/mod.rs:339-343#finalize` refuses to commit one larger
/// than this, so the number is the relay's own ceiling restated on the side
/// that has to allocate it. Capping it at [`MAX_EVENT_BYTES`] instead would
/// refuse every attachment over eight mebibytes, and leaving it uncapped — as
/// this route was — lets any relay decide how much memory this client holds.
pub(super) const MAX_BLOB_BYTES: usize = 100 * 1000 * 1000;

/// How long this client will wait for the *next* piece of a **document** body.
///
/// An **idle** bound rather than a total one: every frame that arrives resets
/// it, so it bounds a peer that has stopped sending without putting a
/// throughput floor under a peer that is merely slow. Idle is the right shape
/// for every body this client reads; it is not the same *number* for every
/// body, and [`BLOB_IDLE_TIMEOUT`] is the other one.
///
/// Two seconds, measured against a connection that is already established and
/// a head that has already arrived. Every body this number bounds is a
/// document the relay writes from a `format!`-bounded string — a handshake
/// reply, an ack, a problem document — so two seconds of *no bytes at all* is
/// a stalled peer rather than a loaded one. It is deliberately not applied to
/// the one route whose body is content: see [`BLOB_IDLE_TIMEOUT`] for why two
/// seconds there refuses ordinary transfers rather than stalled ones.
///
/// The alternative to expiring is not a slower read; it is no return at all. A
/// stall is not a failure the body reports, and nothing above supplies a bound:
/// the client carries no timeout of its own (see
/// [`SseTransport::connect_with_bearer`]), [`HEAD_TIMEOUT`] ends when the head
/// arrives and so bounds none of the body, and
/// `crates/sunrise-core/src/sync_driver.rs:700#handshake` awaits the `Hello` send
/// bare, outside any `tokio::select!`. An unbounded read under that await parks
/// the driver task with no error, no `SessionEnd::Disconnected`, no backoff and
/// no log line. On the stream route the driver's `tokio::select!` *cancels*
/// rather than times out, which drops the refusal unreported and reopens the
/// stream, so a stall there becomes a silent retry loop instead of a session
/// that ends and backs off.
pub(super) const BODY_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// How long this client will wait for the *next* piece of an **attachment**.
///
/// Fifteen seconds, and the number is not this module's own. It is
/// `KEEP_ALIVE_SECS` (`crates/sunrise-server/src/api/sync/stream.rs:38#KEEP_ALIVE_SECS`), the
/// only value in this repository that says how long silence on a live
/// connection is *normal* rather than broken, and
/// `docs/05-sync/wire-protocol.md:360-366` states it in exactly this bound's
/// shape — "an **idle timer, not an interval**", restarted by every delivery,
/// with the promise being that a silent stream produces something "within
/// 15 s". So the magnitude transfers rather than being borrowed by analogy: a
/// client that treats fifteen seconds of silence as a live connection on one
/// route cannot call it a dead peer on another. Like the cap beside it, this
/// waits on an attachment and not on the route: a blob `GET` answered `404` or
/// `503` is a document, and waits [`BODY_IDLE_TIMEOUT`].
///
/// [`BODY_IDLE_TIMEOUT`]'s two seconds are right for a document and wrong
/// here, because an idle bound is a **minimum inter-frame arrival rate** and
/// this is the one route that carries content rather than a document. A TCP
/// retransmit backs off 1 s → 2 s → 4 s and a Wi-Fi roam is seconds, so two
/// seconds of application-level silence inside a hundred-megabyte transfer is
/// ordinary rather than stalled — and the cost of calling it stalled is not
/// one slow download. [`SseTransport::fetch_blob`] sends no `Range` header, so
/// every attempt restarts at byte 0, and
/// `crates/sunrise-core/src/blob_fetch.rs:104#MAX_FETCH_ATTEMPTS` allows ten of them before the
/// row is parked `partial` and the waiting client is told the attachment is
/// unavailable. One gap per hundred megabytes is then a permanently
/// unfetchable attachment.
///
/// What this number costs, stated because the cost is not one wait: a peer
/// that accepts the connection and then says nothing holds a fetch for fifteen
/// seconds, and `crates/sunrise-core/src/blob_fetch.rs:104#MAX_FETCH_ATTEMPTS` allows ten such
/// attempts before the row is parked.
///
/// Those ten are **not consecutive**, and nothing here should be read as
/// promising a total. A failed attempt reschedules nothing: `blob_fetch`'s
/// `count_failure` records it and returns, and that crate's only `poke_sync()`
/// fires once, when the request is made. The next attempt comes from the sync
/// driver's anti-entropy backstop, which drains blob transfers and only then
/// calls `deadlines.resynced()` (`crates/sunrise-core/src/sync_driver.rs:967#session`
/// and `:968`) — so the next attempt is `DEFAULT_RESYNC_INTERVAL`, thirty
/// seconds, after the last one *ended*. On a quiet vault the cycle is
/// therefore this bound plus thirty seconds, and the tenth attempt parks the
/// row between six and seven minutes after the request. On a busy one an
/// inbound batch or a local submit re-drives the drain sooner, so the span is
/// shorter, though never below the two minutes of delays `blob_fetch` keeps.
///
/// That span is longer than the 300 s `crates/sunrise-cli/src/cmd/files.rs#DOWNLOAD_DEADLINE`
/// gives one interactive download, so `sunrise attachment get` against a
/// silent relay returns before the attempts are exhausted, around the seventh
/// or eighth. That is neither the attachment being lost nor this bound's doing:
/// the CLI states its own deadline as a report rather than a failure, because
/// the request row is durable and the next `sunrise sync --once` finishes it,
/// and the dominant term in the span is the thirty-second resync cycle rather
/// than the fifteen. At two seconds the same ten attempts spanned roughly four
/// to five minutes, which was inside that deadline by as little as ten
/// seconds. A resumable fetch — a `Range` header, so an attempt continues
/// rather than restarting — is what would make the attempt budget describe
/// a bounded amount of work; until then it describes ten separate tries on a
/// timer this module does not own.
///
/// **Not for the live stream.** `recv_frame` reads its body with no idle bound
/// at all, deliberately, and this constant must not be the one that changes
/// that. On the blob route fifteen seconds is silence the peer never promised
/// to break; on the stream route it is the interval the relay promises to
/// break it *at*, so a bound of exactly this value races the beat it was
/// derived from. A stream bound would have to be strictly larger, and choosing
/// it is a separate decision from this one.
pub(super) const BLOB_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How long this client will wait for a response **head** on every route but
/// the chunk upload.
///
/// The body bounds above start only once a head has arrived, and until this
/// bound nothing started before it: `self.client.request(request).await`
/// resolves when the status line and headers arrive and not before, and the
/// pinned `hyper-util` legacy client has no request or response timeout to
/// set. A relay, proxy or captive portal that completes TLS and then sends
/// nothing therefore parked the awaiting task for good. On the `Hello` route
/// that task is the sync driver, which awaits the send bare
/// (`crates/sunrise-core/src/sync_driver.rs:700#handshake`), so the symptom was silence:
/// no error, no `SessionEnd::Disconnected`, no backoff and no log line.
///
/// A **total** bound, not an idle one, because a head is not a stream: it is
/// one short document the relay writes once it has decided the answer. What it
/// covers is everything before that — resolving the name, the TCP and TLS
/// handshakes, writing the request, and the relay's own work — which is why it
/// is not [`BODY_IDLE_TIMEOUT`]'s two seconds. A lost SYN alone is retried at
/// 1 s, 3 s and 7 s on a lossy link, and two seconds would turn an ordinary
/// mobile reconnect into a refused one.
///
/// Fifteen seconds for the reason [`BLOB_IDLE_TIMEOUT`] is: `KEEP_ALIVE_SECS`
/// (`crates/sunrise-server/src/api/sync/stream.rs:38#KEEP_ALIVE_SECS`) is the only value in
/// this repository that says how long silence from the relay is normal, and
/// a client that tolerates fifteen seconds of it on one route cannot call less
/// a dead peer on another. It applies to the event stream's open too: the
/// relay answers that head before it has anything to stream, so the head is a
/// document there as everywhere, and the stream's own silence — which this
/// bound never touches — begins after it.
///
/// A connect timeout on the connector is deliberately not added beside it.
/// This bound already contains the connect, and a connect bound alone would
/// not have fixed anything: the failure is a connection that succeeds and then
/// says nothing.
pub(super) const HEAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How long this client will wait for the head of a **chunk upload's** reply.
///
/// The one route where the request, not the relay, is most of the wait. The
/// head of a chunk `PUT` cannot arrive before the relay has read the whole
/// chunk — up to `SEALED_CHUNK_LEN` (`crates/sunrise-crypto/src/blob_chunk.rs`),
/// a quarter of a mebibyte of ciphertext — and stored it, so [`HEAD_TIMEOUT`]
/// here is a throughput floor of roughly 140 kbit/s on the uplink, which a
/// congested mobile connection does not always clear. Sixty seconds lowers
/// that floor to about 35 kbit/s while still ending a silent relay's hold on
/// the upload in a minute rather than never.
pub(super) const CHUNK_HEAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

impl SseTransport {
    /// The three `header_sig_v2` headers for one request, or none if this
    /// transport carries no binding.
    ///
    /// `body` is the value that will be sent, not the bytes: ADR-0022 signs the
    /// RFC 8785 canonical form of the request *value*, which is what lets the
    /// relay recompute the same string from what it parsed. Signing the octets
    /// this client happens to emit would break the moment either side changed
    /// its key order.
    pub(super) fn binding(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<Vec<(&'static str, String)>, TransportError> {
        let Some(signer) = &self.signer else {
            return Ok(Vec::new());
        };
        let date = sunrise_http_sig::date_header(signer.now_ms());
        // The `?` is unreachable from this module and stays for the signature's
        // sake. `sign_with` fails only with `SigError::NotCanonicalizable`,
        // which its own doc records as reachable for a map with non-string keys
        // — and `body` is a `serde_json::Value`, whose maps are
        // `Map<String, Value>` by construction. Non-finite floats do not reach
        // it either: `serde_jcs` follows `serde_json` and writes `null`. So a
        // mutant that deletes this error path survives because no input can
        // take it, not because nothing depends on it.
        let signature =
            sunrise_http_sig::sign_with(|msg| signer.sign(msg), method, path, &date, body)
                .map_err(|e| protocol(&e))?;
        Ok(vec![
            (sunrise_http_sig::DEVICE_HEADER, signer.device_id()),
            (sunrise_http_sig::DEVICE_SIG_HEADER, signature),
            ("date", date),
        ])
    }

    /// The three `header_sig_v2` headers for a request whose body is **not**
    /// JSON, or none if this transport carries no binding.
    ///
    /// Separate from [`Self::binding`] rather than a second parameter on it,
    /// because the two sign different things: that one canonicalizes a value,
    /// and here the bytes already are the canonical form. `sign_binary_with` is
    /// the encoder both sides of that split share with the relay's verifier.
    pub(super) fn binding_bytes(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> Vec<(&'static str, String)> {
        let Some(signer) = &self.signer else {
            return Vec::new();
        };
        let date = sunrise_http_sig::date_header(signer.now_ms());
        let signature =
            sunrise_http_sig::sign_binary_with(|msg| signer.sign(msg), method, path, &date, body);
        vec![
            (sunrise_http_sig::DEVICE_HEADER, signer.device_id()),
            (sunrise_http_sig::DEVICE_SIG_HEADER, signature),
            ("date", date),
        ]
    }

    /// Issue one request with a raw byte body (possibly empty) and read the
    /// whole reply under the bounds the caller states.
    ///
    /// The blob surface is the only place this is needed: a chunk `PUT` sends
    /// opaque ciphertext and a blob `GET` receives it, and neither is
    /// describable as a JSON value. `content_type` is `None` for a request
    /// with no body, which is what keeps a bodiless `GET` from advertising a
    /// media type it is not sending.
    ///
    /// `cap` and `idle` belong to the caller because this method's two callers
    /// do not read the same thing, and only one of them reads content. A blob
    /// `GET`'s reply is a whole attachment; a chunk `PUT`'s reply is an ack —
    /// an empty `204` or a short JSON document. Choosing the attachment's
    /// bounds here for both, as this route did, let a relay answer an upload
    /// with a hundred megabytes and be waited on per frame for as long as a
    /// download deserves. Same shape as [`read_body`]'s parameters one level
    /// down, and for the same reason.
    ///
    /// They are the bounds on the reply the caller **asked for**, and only on
    /// that one. A non-2xx reply is not content on any route: it is the problem
    /// document [`SseTransport::refuse`] parses, the same few hundred bytes the
    /// other five routes read, so it is read under the document bounds
    /// whatever the caller passed. Splitting by caller alone left the blob
    /// route's `404` — which [`SseTransport::fetch_blob`] handles as "not here,
    /// try later" — being read under a hundred-megabyte cap and fifteen seconds
    /// of patience per frame, which is the upload route's own defect one status
    /// along.
    ///
    /// `head` is the caller's for the same reason, one await earlier: the
    /// chunk upload's head waits on the chunk being sent and stored, and a
    /// bodiless fetch's does not, so the upload waits [`CHUNK_HEAD_TIMEOUT`]
    /// and the fetch waits [`HEAD_TIMEOUT`]. Unlike `cap` and `idle` it cannot
    /// be chosen by status, because until it arrives there is no status.
    pub(super) async fn call_bytes(
        &self,
        method: &str,
        path: &str,
        content_type: Option<&str>,
        body: &[u8],
        head: std::time::Duration,
        cap: usize,
        idle: std::time::Duration,
    ) -> Result<Reply, TransportError> {
        let mut request = hyper::Request::builder()
            .method(method)
            .uri(format!("{}{path}", self.base));
        if let Some(bearer) = &self.bearer {
            request = request.header(hyper::header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        for (name, value) in self.binding_bytes(method, path, body) {
            request = request.header(name, value);
        }
        if let Some(ct) = content_type {
            request = request.header(hyper::header::CONTENT_TYPE, ct);
        }
        let request = request
            .body(Full::new(Bytes::copy_from_slice(body)))
            .map_err(|e| protocol(&e))?;

        let response = await_head(self.client.request(request), head).await?;
        let status = response.status();
        let date = server_date(response.headers());
        // The status is already in hand, so the kind of body about to arrive is
        // known before a byte of it is read. The caller's bounds size the reply
        // it asked for; a refusal is a document on every route, and is read
        // like one.
        let (cap, idle) = if status.is_success() {
            (cap, idle)
        } else {
            (MAX_EVENT_BYTES, BODY_IDLE_TIMEOUT)
        };
        let bytes = read_body(response.into_body(), cap, idle).await?;
        Ok(Reply {
            status,
            date,
            bytes,
        })
    }

    /// Issue one JSON request and read the whole reply.
    pub(super) async fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<Reply, TransportError> {
        let mut request = hyper::Request::builder()
            .method(method)
            .uri(format!("{}{path}", self.base));
        if let Some(bearer) = &self.bearer {
            request = request.header(hyper::header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        if let Some(session) = &self.session {
            request = request.header("x-sunrise-session", session);
        }
        for (name, value) in self.binding(method, path, body.as_ref())? {
            request = request.header(name, value);
        }
        let request = match body {
            Some(value) => request
                .header(hyper::header::CONTENT_TYPE, "application/json")
                .body(Full::new(Bytes::from(
                    serde_json::to_vec(&value).map_err(|e| protocol(&e))?,
                ))),
            None => request.body(Full::new(Bytes::new())),
        }
        .map_err(|e| protocol(&e))?;

        // Bounded for the reason the body read below is, one await earlier: a
        // relay that completes TLS and never sends a status line would
        // otherwise park the `Hello` send, and the driver with it.
        let response = await_head(self.client.request(request), HEAD_TIMEOUT).await?;
        let status = response.status();
        let date = server_date(response.headers());
        // Every reply on this route is a small JSON document, so the event cap
        // is far above anything legitimate — it is here so a relay cannot
        // choose how much memory this client holds, not to size the reply.
        //
        // The deadline is the load-bearing half. This is the route the
        // handshake rides: `send_frame`'s `Hello` arm calls it, and
        // `crates/sunrise-core/src/sync_driver.rs:700#handshake` awaits that send bare,
        // outside any `tokio::select!`. An unbounded read here was not a retry
        // loop the driver eventually notices — it was the driver task parked
        // for good, with no error, no backoff and nothing in the log.
        let bytes = read_body(response.into_body(), MAX_EVENT_BYTES, BODY_IDLE_TIMEOUT).await?;
        Ok(Reply {
            status,
            date,
            bytes,
        })
    }

    /// Turn a non-2xx into the typed error the driver branches on.
    ///
    /// The relay's own code wins wherever it is one this build knows. It was
    /// parsed out of the problem document and then thrown away for a status
    /// map, which flattened every `401` to `AUTH_TOKEN_INVALID` — so a client
    /// told "your signature is stale, fix your clock" heard "your bearer is
    /// bad" and refreshed a token that was never the problem. The status map
    /// stays as the fallback for a code this build does not know and for a body
    /// that carries none, which is what keeps an older client working against a
    /// newer relay.
    ///
    /// A refused *binding* additionally gets [`binding_advice`] appended, since
    /// the code alone still leaves the user with nothing to do.
    pub(super) fn refuse(&self, reply: &Reply) -> TransportError {
        // The problem document carries the stable code as an extension member,
        // which is exactly what a client is meant to switch on.
        let reported = serde_json::from_slice::<serde_json::Value>(&reply.bytes)
            .ok()
            .and_then(|v| {
                v.get("code")
                    .and_then(|c| c.as_str())
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_else(|| reply.status.as_str().to_owned());
        let by_status = match reply.status.as_u16() {
            401 | 403 => "AUTH_TOKEN_INVALID",
            503 => "RELAY_STORAGE_UNAVAILABLE",
            _ => "SYNC_OP_INVALID",
        };
        let code = sunrise_error::ErrorCode::from_wire_str(&reported)
            .map_or(by_status, sunrise_error::ErrorCode::as_str);
        let mut message = format!("{}: {reported}", reply.status);
        if code == sunrise_error::ErrorCode::AuthDeviceSigInvalid.as_str() {
            message.push_str(&binding_advice(
                reply.date.as_deref(),
                self.signer.as_ref().map(|s| s.now_ms()),
            ));
        }
        TransportError::Server { code, message }
    }
}

/// One reply, already read.
pub(super) struct Reply {
    /// The status line.
    pub(super) status: hyper::StatusCode,
    /// The relay's own `Date`, when it sent one.
    ///
    /// The only statement of server time a *refused* request carries, and
    /// therefore the only way this side can tell "your clock is wrong" from
    /// "your key is not registered" without a second round trip. hyper's
    /// server writes one on every HTTP/1 response, but nothing in the scheme
    /// requires it, so it is an `Option` and its absence degrades the advice
    /// rather than the diagnosis.
    pub(super) date: Option<String>,
    /// The whole body.
    pub(super) bytes: Vec<u8>,
}

/// Read a response body whole, under a size cap and an idle deadline.
///
/// The one place this client reads a body it did not write. Three routes need
/// it — the JSON control plane ([`SseTransport::call`]), the blob surface
/// ([`SseTransport::call_bytes`]) and a refused stream open
/// ([`SseTransport::open_events`]) — and each of them used to hand-roll the
/// read. That is how *one* of them came to have a cap and a deadline and the
/// other two neither: a bound nobody can see is missing is a bound that goes
/// missing, and stating it once per route is what made the omission invisible.
/// (One, counted against the tree rather than remembered: before the reads
/// were shared, `open_events` held both bounds and `call` and `call_bytes`
/// each collected without either. Further back still, on `master`,
/// `open_events` did not read the refusal body at all, so the count there is
/// none of three.)
///
/// `cap` is a parameter because the routes carry different things — a problem
/// document and an attachment are not bounded by the same number — and `idle`
/// is one for the same reason and not merely as a courtesy: a fetched
/// attachment is read under [`BLOB_IDLE_TIMEOUT`] where every document is read
/// under [`BODY_IDLE_TIMEOUT`], because an idle bound is a minimum arrival
/// rate and content and documents do not arrive alike. Both are now stated at
/// the call rather than assembled out of a `timeout` and a `Limited` at each
/// site. The split is by *body*, not by route: the blob route reaches this
/// function with both pairs, one per side of `is_success`, which
/// [`SseTransport::call_bytes`] chooses between.
///
/// What a failure *means* is deliberately not decided here, because it differs:
/// a refusal whose problem document did not arrive is still a refusal and
/// degrades to the status map, while a success reply whose body did not arrive
/// is not a reply at all. So this reports the failure and the caller reads it.
///
/// Both bounds discard what already arrived. Half a document is not a document:
/// parsing a truncated body would turn a bounded read into a source of
/// arbitrary misreadings, which is a worse answer than the one the caller's
/// fallback already gives.
pub(super) async fn read_body<B>(
    body: B,
    cap: usize,
    idle: std::time::Duration,
) -> Result<Vec<u8>, TransportError>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let mut body = http_body_util::Limited::new(body, cap);
    let mut out = Vec::new();
    loop {
        // The timer is armed per frame rather than once around the whole read,
        // which is the difference between bounding a stall and bounding a
        // download: a body that keeps arriving keeps resetting it.
        let Ok(next) = tokio::time::timeout(idle, body.frame()).await else {
            return Err(TransportError::Unavailable(format!(
                "response body stalled: no data for {}s",
                idle.as_secs()
            )));
        };
        let Some(frame) = next else { return Ok(out) };
        let frame = frame.map_err(|e| TransportError::Unavailable(e.to_string()))?;
        // Trailers carry nothing this client reads. Dropping them here rather
        // than treating the frame as data is what keeps a trailing frame from
        // appending its encoding to a body somebody is about to parse.
        if let Ok(data) = frame.into_data() {
            out.extend_from_slice(&data);
        }
    }
}

/// Await a response head, under a total deadline.
///
/// The one place this client waits for a head, for the reason [`read_body`] is
/// the one place it reads a body: three routes dial, and a bound stated once
/// per route is a bound one of them goes without. `bound` is a parameter
/// because the chunk upload waits [`CHUNK_HEAD_TIMEOUT`] where every other
/// route waits [`HEAD_TIMEOUT`].
///
/// Both failures are [`TransportError::Unavailable`], which is what the sync
/// driver turns into a disconnect and a backoff: a relay that never answered
/// is as unreachable as one that refused the connection. Dropping the pending
/// request on expiry also drops its connection rather than returning it to
/// the pool, so the next attempt dials afresh instead of inheriting the peer
/// that went quiet.
pub(super) async fn await_head<F, B, E>(
    pending: F,
    bound: std::time::Duration,
) -> Result<hyper::Response<B>, TransportError>
where
    F: std::future::Future<Output = Result<hyper::Response<B>, E>>,
    E: std::fmt::Display,
{
    match tokio::time::timeout(bound, pending).await {
        Ok(response) => response.map_err(|e| TransportError::Unavailable(e.to_string())),
        Err(_) => Err(TransportError::Unavailable(format!(
            "no response head within {bound:?}"
        ))),
    }
}

/// The `Date` a response carried, if it carried a readable one.
pub(super) fn server_date(headers: &hyper::HeaderMap) -> Option<String> {
    headers
        .get(hyper::header::DATE)?
        .to_str()
        .ok()
        .map(ToOwned::to_owned)
}

/// What to tell a user whose device binding was refused.
///
/// `AUTH_DEVICE_SIG_INVALID` covers four different situations — a clock outside
/// the replay window, an unregistered key, a revoked device, and a binding that
/// was not sent at all — and only one of them is the user's to fix. A bare 401
/// sent them to `sunrise login`, which cannot help with any of the four.
///
/// The clock is separated out because it is both the commonest cause and the
/// only one this side can *measure*: the relay's `Date` on the very response
/// that refused the request is server time, so the skew is arithmetic rather
/// than a guess. Where it can be measured and exceeds the window, that is
/// stated as the cause; otherwise the remaining possibilities are listed, since
/// naming a clock that is fine would send the user to fix the wrong thing.
///
/// `now_ms` is `None` for a transport carrying no signer, which is the fourth
/// case and is named directly.
pub(super) fn binding_advice(server_date: Option<&str>, now_ms: Option<u64>) -> String {
    let Some(now_ms) = now_ms else {
        return " — this client sent no device binding and the relay requires one; register                 this device and give its transport a DeviceSigner"
            .to_owned();
    };
    match server_date.and_then(|d| sunrise_http_sig::skew_secs(d, now_ms)) {
        Some(skew) if skew.abs() > sunrise_http_sig::MAX_CLOCK_SKEW_SECS => format!(
            " — this device's clock is {skew}s from the relay's, outside the {}s the              signature allows; set the system clock and retry",
            sunrise_http_sig::MAX_CLOCK_SKEW_SECS
        ),
        _ => " — the relay refused this device's signature; the device may not be registered,               or may have been revoked"
            .to_owned(),
    }
}

/// Everything that is a malformed exchange rather than an unreachable one.
pub(super) fn protocol<E: std::fmt::Display>(e: &E) -> TransportError {
    TransportError::Protocol(e.to_string())
}

pub(super) fn field_str(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|f| f.as_str())
        .unwrap_or_default()
        .to_owned()
}

pub(super) fn field_u64(v: &serde_json::Value, key: &str) -> u64 {
    v.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0)
}

/// A negotiated version that does not fit a `u32` is a malformed reply rather
/// than a number to quietly truncate: the whole point of negotiation is that
/// both sides agree on the value.
///
/// Only an out-of-range value is refused. An absent or non-integer key reads
/// as 0, as [`field_u64`] reads it, so this reader is total over a missing
/// field; negotiation then sees a 0 rather than an error.
pub(super) fn field_u32(v: &serde_json::Value, key: &str) -> Result<u32, TransportError> {
    u32::try_from(field_u64(v, key))
        .map_err(|_| TransportError::Protocol(format!("{key} does not fit a u32")))
}
