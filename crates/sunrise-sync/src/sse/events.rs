//! The live event stream: opening it, cutting it into events, and turning each
//! event back into the frame the driver already knows how to handle.

use super::http::{
    await_head, protocol, read_body, server_date, Reply, BODY_IDLE_TIMEOUT, HEAD_TIMEOUT,
    MAX_EVENT_BYTES,
};
use super::SseTransport;
use crate::transport::TransportError;
use base64::Engine as _;
use http_body_util::Full;
use hyper::body::Bytes;
use sunrise_wire_protocol::{
    encode_frame, BacklogPayload, CaughtUpPayload, ClosePayload, ErrorPayload, FrameFlags, MsgKind,
};

impl SseTransport {
    /// Open the event stream, resuming from the last id if there is one.
    pub(super) async fn open_events(&mut self) -> Result<(), TransportError> {
        let Some(session) = self.session.clone() else {
            return Err(TransportError::Protocol(
                "no sync session: send Hello first".to_owned(),
            ));
        };
        // `SignedParts`: the stream is a bound route like every other, and
        // hashes the empty string because it carries no body.
        let target = "/api/v1/sync/events";
        let mut request = hyper::Request::builder()
            .method("GET")
            .uri(format!("{}{target}", self.base))
            .header("x-sunrise-session", session)
            .header(hyper::header::ACCEPT, "text/event-stream");
        if let Some(bearer) = &self.bearer {
            request = request.header(hyper::header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        if let Some(id) = &self.last_event_id {
            request = request.header("last-event-id", id);
        }
        for (name, value) in self.binding("GET", target, None)? {
            request = request.header(name, value);
        }
        let request = request
            .body(Full::new(Bytes::new()))
            .map_err(|e| protocol(&e))?;

        // The head, not the stream: once it arrives the body is handed to
        // `self.events` and read with no idle bound, deliberately (see
        // [`BLOB_IDLE_TIMEOUT`]'s last paragraph). Without this a silent open
        // was only ever cancelled by the driver's `tokio::select!`, never
        // failed, so nothing reported it and no backoff engaged.
        let response = await_head(self.client.request(request), HEAD_TIMEOUT).await?;
        if !response.status().is_success() {
            // A stream refusal is diagnosed from its status, its code and the
            // relay's `Date`, so the body is read rather than dropped: the
            // problem document is where the relay puts the typed code, and
            // leaving it unread flattened every `401` on this one route back
            // onto the status map — the exact defect [`SseTransport::refuse`]
            // exists to prevent on the other six.
            //
            // This is the one body read in this module that *degrades* instead
            // of propagating, and the asymmetry is the point rather than an
            // oversight. On this route the 2xx body is the event stream and is
            // never collected at all — it is handed to `self.events` below — so
            // the only body [`read_body`] ever reads here is a refusal's, and a
            // refusal whose document did not arrive is still a refusal. Every
            // way of not getting the document therefore lands on the status
            // map, which is where this route was before it read the body at
            // all. [`SseTransport::call`] collects on both sides of
            // `is_success`, where the same degrade would turn a success reply
            // that never arrived into an empty one that did, so it propagates.
            let status = response.status();
            let date = server_date(response.headers());
            let bytes = read_body(response.into_body(), MAX_EVENT_BYTES, BODY_IDLE_TIMEOUT)
                .await
                .unwrap_or_default();
            return Err(self.refuse(&Reply {
                status,
                date,
                bytes,
            }));
        }
        self.events = Some(response.into_body());
        Ok(())
    }

    /// Pull the next complete event out of the buffer, if one is there.
    ///
    /// This reads any conformant `text/event-stream`, as the WHATWG HTML
    /// standard's "interpreting an event stream" defines it — not only what
    /// this relay's encoder happens to write. That encoder (kynos's
    /// `response::stream::sse::encode`) emits `\n` line endings and a single
    /// `data:` line per event, so today the two readings agree; the parser
    /// does not lean on that, so a proxy that rewrites line endings, a second
    /// relay implementation, or a value that spans lines reads the same.
    ///
    /// - A line ends at `\r\n`, `\n`, or a lone `\r`.
    /// - An event ends at the first blank line.
    /// - Successive `data:` fields are joined with `\n` between them.
    /// - One space after a field's colon is dropped; any further whitespace
    ///   belongs to the value.
    /// - An `id` holding a NUL is ignored, as the standard requires.
    ///
    /// Comments — the keep-alives that replaced `Ping` — start with `:` and
    /// are dropped here rather than reaching the driver, which has no frame
    /// for them. An event with no data comes back with an empty string, which
    /// the driver skips. Fields the driver has no use for (`event`, `retry`)
    /// are ignored.
    pub(super) fn take_event(&mut self) -> Option<(Option<String>, String)> {
        // A blank line before any field dispatches nothing, so leading line
        // terminators are no-ops and are consumed. That is also what swallows
        // the `\n` of a `\r\n` whose `\r` ended the previous event exactly at
        // a chunk boundary.
        let lead = self
            .buf
            .iter()
            .take_while(|b| matches!(b, b'\r' | b'\n'))
            .count();
        self.buf.drain(..lead);

        let end = event_end(&self.buf)?;
        let raw = self.buf.drain(..end).collect::<Vec<u8>>();
        let text = String::from_utf8_lossy(&raw);

        let mut id = None;
        let mut data = String::new();
        // `event_end` stops at the first blank line, so every empty piece of
        // this split is either that line or the gap inside a `\r\n`. Both, and
        // a comment (`:` then text), have an empty field name, which no arm
        // below matches: they fall through without a test of their own.
        for line in text.split(['\r', '\n']) {
            let (field, value) = match line.split_once(':') {
                Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
                None => (line, ""),
            };
            match field {
                "data" => {
                    data.push_str(value);
                    data.push('\n');
                }
                "id" if !value.contains('\0') => id = Some(value.to_owned()),
                _ => {}
            }
        }
        // Every `data` field appended a `\n`; the last one is not part of the
        // value. With no `data` field this pops nothing and the event is empty.
        data.pop();
        Some((id, data))
    }

    /// Convert one decoded event into the frame the driver expects.
    pub(super) fn frame_for(event: &serde_json::Value) -> Result<Option<Vec<u8>>, TransportError> {
        let kind = event
            .get("kind")
            .and_then(|k| k.as_str())
            .unwrap_or_default();
        match kind {
            // Verbatim: the relay stored the frame the publisher sent, and this
            // is that frame. Nothing is re-encoded, so nothing can drift.
            "ops" => {
                let raw = event
                    .get("frame")
                    .and_then(|f| f.as_str())
                    .ok_or_else(|| TransportError::Protocol("ops event has no frame".to_owned()))?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(raw)
                    .map_err(|e| protocol(&e))?;
                Ok(Some(bytes))
            }
            "caught_up" => {
                let stream_id = parse_id(event.get("stream_id"))?;
                let payload = CaughtUpPayload { stream_id }
                    .encode()
                    .map_err(|e| protocol(&e))?;
                Ok(Some(
                    encode_frame(MsgKind::StreamUpdate, FrameFlags::EMPTY, &payload)
                        .map_err(|e| protocol(&e))?,
                ))
            }
            // How much the replay that follows will deliver (#459). Rides the
            // same kind as `caught_up`; the driver reads it first because a
            // `CaughtUpPayload` reader would accept its bytes too. A relay
            // too old to send it sends nothing, and the driver reports no
            // total.
            "backlog" => {
                let stream_id = parse_id(event.get("stream_id"))?;
                let ops = event
                    .get("ops")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| {
                        TransportError::Protocol("backlog event has no op count".to_owned())
                    })?;
                let payload = BacklogPayload { ops, stream_id }
                    .encode()
                    .map_err(|e| protocol(&e))?;
                Ok(Some(
                    encode_frame(MsgKind::StreamUpdate, FrameFlags::EMPTY, &payload)
                        .map_err(|e| protocol(&e))?,
                ))
            }
            // The stream rides along so the driver can catch that stream up
            // from its snapshot record (ADR-0068). A gap event without one is
            // still a gap; the driver then has no stream to recover.
            "gap" => Ok(Some(error_frame(
                sunrise_error::ErrorCode::SyncCursorGap,
                event
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("cursor gap"),
                parse_id(event.get("stream_id")).ok(),
            )?)),
            "closed" => {
                // The relay's own code wins, exactly as [`SseTransport::refuse`]
                // keeps it for a refused request. `ClosePayload::is_recoverable`
                // is `matches!(self.code, AuthTokenExpired)`, and the stream
                // emits three distinct close reasons — `AUTH_TOKEN_EXPIRED`,
                // `AUTH_DEVICE_REVOKED` and `RELAY_STORAGE_UNAVAILABLE`
                // (`docs/05-sync/wire-protocol.md:379-383`). Collapsing them
                // onto the first is what `docs/05-sync/wire-protocol.md:203-206`
                // names as the defect: a client that cannot tell them apart
                // "retries forever against a revoked device".
                //
                // A close this build cannot read is **not** recoverable, and
                // the direction of that fallback is the whole point of it.
                // `docs/05-sync/wire-protocol.md:214-216` records adding a
                // close code as non-breaking, so a relay one version ahead
                // closing for a fourth reason is sanctioned rather than
                // malformed. Defaulting such a close onto `AUTH_TOKEN_EXPIRED`
                // — the single member `is_recoverable` admits — would put a
                // client into exactly the loop the paragraph above names: it
                // refreshes, reconnects, is closed again for the same unread
                // reason, and never stops. `INTERNAL_UNKNOWN_CODE` is the code
                // `docs/10-cross-cutting/error-handling.md:36` reserves for
                // this — "an older client receiving an unknown code maps it to
                // `INTERNAL_UNKNOWN_CODE` and preserves the original wire
                // string in `diagnostic`" — so the unparsed spelling is carried
                // into `reason`, which is `ClosePayload`'s diagnostic field.
                // This is the same direction [`SseTransport::refuse`] takes at
                // its own fallback, and for the same stated reason.
                //
                // An **absent** code is a different condition and gets a
                // different answer. `INTERNAL_UNKNOWN_CODE` is reserved for an
                // unknown code, and "the relay sent a code this build has
                // never heard of" is not "the relay sent no code": the first
                // is a newer peer doing something the protocol permits, the
                // second is a malformed event.
                // `crates/sunrise-server/src/api/sync/stream.rs:107-109#SyncEvent`
                // declares `SyncEvent::Closed.code` as a `String` rather than
                // an `Option<String>`, so no relay produces the second — which
                // is exactly the argument `send_frame`'s `Hello` arm makes from
                // `crates/sunrise-server/src/api/sync/credential.rs:59#SessionResponse`
                // about a missing `session_id`, and it is made here for the
                // same reason. Reporting it as a protocol error says what
                // happened; folding it into the forward-compat code would file
                // a broken relay under "newer than us" and hide it.
                let Some(code) = event.get("code").filter(|c| !c.is_null()) else {
                    return Err(TransportError::Protocol(
                        "closed event carries no code".to_owned(),
                    ));
                };
                // Present but not a string is still present, and its spelling
                // is still the only evidence of which close this was. Rendering
                // the value as it arrived keeps the diagnostic
                // `error-handling.md:36` asks for on the one shape where
                // reading it as a string yields nothing.
                let reported = match code {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let parsed = sunrise_error::ErrorCode::from_wire_str(&reported);
                let reason = event
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("session closed");
                let payload = ClosePayload {
                    code: parsed.unwrap_or(sunrise_error::ErrorCode::InternalUnknownCode),
                    reason: match parsed {
                        Some(_) => reason.to_owned(),
                        None => format!("{reason} (unrecognised close code {reported})"),
                    },
                }
                .encode()
                .map_err(|e| protocol(&e))?;
                Ok(Some(
                    encode_frame(MsgKind::Close, FrameFlags::EMPTY, &payload)
                        .map_err(|e| protocol(&e))?,
                ))
            }
            _ => Ok(None),
        }
    }
}

/// How many bytes at the head of `buf` make up its first complete event: every
/// line through the blank line that ends it, or `None` while that blank line
/// has not arrived.
///
/// A line ends at `\r\n`, `\n`, or a lone `\r`. A `\r` that is the last byte
/// in `buf` may be the first half of a `\r\n`, but it still ends its line; only
/// the byte after it can say whether the next line is blank, so the answer
/// waits for that byte either way. A blank line ended by a lone `\r` at the
/// end of `buf` does end the event now, and a `\n` that arrives after it is
/// consumed by [`SseTransport::take_event`] as a no-op blank line.
fn event_end(buf: &[u8]) -> Option<usize> {
    let terminator_end = |at: usize| match buf.get(at) {
        Some(b'\r') if buf.get(at + 1) == Some(&b'\n') => Some(at + 2),
        Some(b'\n' | b'\r') => Some(at + 1),
        _ => None,
    };
    let mut line_start = 0;
    loop {
        let line_end = line_start
            + buf[line_start..]
                .iter()
                .position(|b| matches!(b, b'\r' | b'\n'))?;
        let next_line = terminator_end(line_end)?;
        // `next_line` starts a line; it is blank when it opens on a terminator.
        if let Some(end) = terminator_end(next_line) {
            return Some(end);
        }
        line_start = next_line;
    }
}

pub(super) fn parse_id(v: Option<&serde_json::Value>) -> Result<[u8; 16], TransportError> {
    let s = v
        .and_then(|f| f.as_str())
        .ok_or_else(|| TransportError::Protocol("event has no stream_id".to_owned()))?;
    let mut out = [0u8; 16];
    hex::decode_to_slice(s, &mut out).map_err(|e| protocol(&e))?;
    Ok(out)
}

fn error_frame(
    code: sunrise_error::ErrorCode,
    reason: &str,
    stream_id: Option<[u8; 16]>,
) -> Result<Vec<u8>, TransportError> {
    let payload = ErrorPayload {
        code,
        reason: reason.to_owned(),
        stream_id,
    }
    .encode()
    .map_err(|e| protocol(&e))?;
    encode_frame(MsgKind::Error, FrameFlags::EMPTY, &payload).map_err(|e| protocol(&e))
}
