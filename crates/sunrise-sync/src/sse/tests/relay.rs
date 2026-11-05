//! A loopback HTTP/1.1 relay standing in for the real one.
//!
//! The transport's `events` field is a `hyper::body::Incoming`, and an
//! `Incoming` can only be produced by hyper from a real response — there is
//! nothing to fabricate and no seam to inject at. So the double is an
//! actual server on `127.0.0.1:0` that the transport dials, which also
//! makes every assertion against it an assertion about what went **on the
//! wire** rather than about what a private method returned.
//!
//! It costs no new dependency: the workspace already pins `hyper` with
//! `server` on and `tokio` with `net`, both for other crates.

use http_body_util::BodyExt as _;
use hyper::body::{Bytes, Frame};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::future::Future as _;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::net::TcpListener;

/// How long the relay waits between the chunks of one response.
///
/// A deterministic gap, so every chunk of a scripted response reaches
/// the client as a separate body frame no matter how the two ends are
/// scheduled, and reassembly is exercised across frames rather than
/// within one. The value is not load-bearing beyond being non-zero:
/// this relay writes one chunked-transfer chunk per [`Frame::data`] and
/// hyper's decoder yields one frame per chunk regardless of arrival
/// timing, so the gap removes a scheduling variable rather than a
/// coalescing one.
const CHUNK_GAP: Duration = Duration::from_millis(5);

/// How long a stalling response withholds the rest of its body.
///
/// Long enough that nothing in this suite can outwait it, so a test
/// asserting a client-side deadline is asserting the *client's* timer
/// and not this one. It is a real timer rather than a bare
/// `Poll::Pending` because a body that returns `Pending` without
/// registering a waker is a body the runtime is entitled never to poll
/// again, and the stall would then be an artefact of this double rather
/// than of the relay it stands in for.
const STALL: Duration = Duration::from_secs(3_600);

/// One canned response, in the order the relay will hand them out.
#[derive(Debug, Clone)]
pub(super) struct Canned {
    status: u16,
    content_type: Option<&'static str>,
    chunks: Vec<Bytes>,
    /// End the body with an error rather than with its last chunk.
    abort: bool,
    /// Never end the body at all: write `chunks`, then hold.
    stall: bool,
}

impl Canned {
    /// A whole JSON reply.
    pub(super) fn json(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: Some("application/json"),
            chunks: vec![Bytes::copy_from_slice(body.as_bytes())],
            abort: false,
            stall: false,
        }
    }

    /// A reply with a status and nothing else.
    pub(super) fn empty(status: u16) -> Self {
        Self {
            status,
            content_type: None,
            chunks: Vec::new(),
            abort: false,
            stall: false,
        }
    }

    /// A whole opaque byte reply, as the blob surface sends.
    pub(super) fn bytes(status: u16, body: &[u8]) -> Self {
        Self {
            status,
            content_type: Some("application/octet-stream"),
            chunks: vec![Bytes::copy_from_slice(body)],
            abort: false,
            stall: false,
        }
    }

    /// An event stream delivered one chunk at a time.
    pub(super) fn stream(chunks: Vec<Bytes>) -> Self {
        Self {
            status: 200,
            content_type: Some("text/event-stream"),
            chunks,
            abort: false,
            stall: false,
        }
    }

    /// A reply whose body starts and then never finishes.
    ///
    /// The status line and headers reach the client — a chunk has to
    /// be written for hyper to flush the head, which is why this takes
    /// a prefix rather than nothing — and the rest of the body never
    /// arrives. That is what a proxy, captive portal or relay holding a
    /// connection open looks like from here, and the only shape that
    /// exercises a *deadline* rather than an error: a stall is not a
    /// failure the body reports, so without a timer the read does not
    /// return at all.
    pub(super) fn json_then_stall(status: u16, prefix: &str) -> Self {
        Self {
            status,
            content_type: Some("application/json"),
            chunks: vec![Bytes::copy_from_slice(prefix.as_bytes())],
            abort: false,
            stall: true,
        }
    }

    /// A reply that fails part-way through its body.
    ///
    /// The relay answers `status`, writes `chunks`, and then errors
    /// instead of ending the message — which is what a connection
    /// broken mid-body looks like to the client, and the only way this
    /// double can reach a `Some(Err(e))` on a response body. A body
    /// whose `Error` is `Infallible` forecloses those arms by
    /// construction, which is why the error type here is not.
    ///
    /// The status is a parameter rather than a fixed `200` because two
    /// different arms need this shape and they need it on opposite
    /// sides of `is_success`: `recv_frame`'s mid-stream error arm on a
    /// `200`, and `open_events`' failed-refusal-read fallback on a
    /// `4xx`. Fixing the status at `200` left the second unreachable by
    /// any script, which is a limit of the double rather than a
    /// property of the code. The media type follows the status for the
    /// same reason: a refusal carries a problem document, not a stream.
    pub(super) fn stream_then_abort(status: u16, chunks: Vec<Bytes>) -> Self {
        Self {
            status,
            content_type: Some(if (200..300).contains(&status) {
                "text/event-stream"
            } else {
                "application/json"
            }),
            chunks,
            abort: true,
            stall: false,
        }
    }
}

/// One request exactly as it reached the relay.
#[derive(Debug, Clone)]
pub(super) struct Seen {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) body: Vec<u8>,
    headers: Vec<(String, String)>,
}

impl Seen {
    /// The value of `name`, which hyper has already lower-cased.
    pub(super) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// The body of a canned response: one HTTP chunk per element, spaced so
/// the client sees them as separate body frames.
#[derive(Debug)]
struct Chunks {
    rest: VecDeque<Bytes>,
    gap: Option<Pin<Box<tokio::time::Sleep>>>,
    abort: bool,
    stall: bool,
}

impl hyper::body::Body for Chunks {
    type Data = Bytes;
    /// Not `Infallible`: a body that cannot fail forecloses the client's
    /// mid-stream error arm by construction, and that arm is the one
    /// that decides whether a broken connection is reported or read as
    /// a graceful end of stream.
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        let this = self.get_mut();
        if let Some(gap) = this.gap.as_mut() {
            match gap.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(()) => this.gap = None,
            }
        }
        let Some(next) = this.rest.pop_front() else {
            if this.stall {
                let held = this
                    .gap
                    .get_or_insert_with(|| Box::pin(tokio::time::sleep(STALL)));
                return match held.as_mut().poll(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(()) => Poll::Ready(None),
                };
            }
            if this.abort {
                this.abort = false;
                return Poll::Ready(Some(Err(std::io::Error::other(
                    "the relay dropped the body mid-stream",
                ))));
            }
            return Poll::Ready(None);
        };
        // A gap after the last chunk too when an abort follows it: hyper
        // polls a response body ahead of writing it, so an error offered
        // in the same pass as the final chunk kills the connection
        // before the head is flushed, and the client reports a failed
        // *request* rather than a failed body. The gap makes the server
        // flush what it has first.
        if !this.rest.is_empty() || this.abort {
            this.gap = Some(Box::pin(tokio::time::sleep(CHUNK_GAP)));
        }
        Poll::Ready(Some(Ok(Frame::data(next))))
    }
}

/// A running relay: where to dial it, and what it has been asked.
#[derive(Debug)]
pub(super) struct Relay {
    pub(super) base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Relay {
    /// Every request the relay has answered, in order.
    pub(super) fn seen(&self) -> Vec<Seen> {
        self.seen
            .lock()
            .expect("the relay's record outlives every request")
            .clone()
    }
}

/// Start a relay that answers `script` in order and records what it was
/// asked. A request past the end of the script gets a `500`, which
/// makes an unscripted call a visible failure rather than a hang.
pub(super) async fn start(script: Vec<Canned>) -> Relay {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let base = format!(
        "http://{}",
        listener.local_addr().expect("the bound address")
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let queue = Arc::new(Mutex::new(script.into_iter().collect::<VecDeque<_>>()));

    let accepted = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let seen = Arc::clone(&accepted);
            let queue = Arc::clone(&queue);
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let seen = Arc::clone(&seen);
                    let queue = Arc::clone(&queue);
                    async move {
                        let method = req.method().to_string();
                        let path = req.uri().path().to_owned();
                        let headers = req
                            .headers()
                            .iter()
                            .map(|(n, v)| {
                                (
                                    n.as_str().to_owned(),
                                    v.to_str().unwrap_or_default().to_owned(),
                                )
                            })
                            .collect();
                        let body = req
                            .into_body()
                            .collect()
                            .await
                            .map(|b| b.to_bytes().to_vec())
                            .unwrap_or_default();
                        seen.lock().expect("the relay's record").push(Seen {
                            method,
                            path,
                            body,
                            headers,
                        });
                        let canned = queue
                            .lock()
                            .expect("the relay's script")
                            .pop_front()
                            .unwrap_or_else(|| Canned::empty(500));
                        let mut reply = Response::builder()
                            .status(StatusCode::from_u16(canned.status).expect("a status"));
                        if let Some(ct) = canned.content_type {
                            reply = reply.header(hyper::header::CONTENT_TYPE, ct);
                        }
                        Ok::<_, Infallible>(
                            reply
                                .body(Chunks {
                                    rest: canned.chunks.into_iter().collect(),
                                    gap: None,
                                    abort: canned.abort,
                                    stall: canned.stall,
                                })
                                .expect("a response"),
                        )
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });

    Relay { base, seen }
}
