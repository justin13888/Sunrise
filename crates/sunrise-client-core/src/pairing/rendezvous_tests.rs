//! [`Rendezvous`]'s waits, against a fake relay on a paused clock: what ends
//! one, what it retries, and what a retry keeps.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::time::Instant;

use super::{Rendezvous, RendezvousError, RendezvousRole, POLL_INTERVAL, WAIT_CAP};

/// One answer of the fake relay.
#[derive(Clone)]
enum Reply {
    /// `200` with the receive route's body, holding these messages.
    Messages(&'static [&'static str]),
    /// `200` with the send route's body.
    Accepted,
    /// A `problem+json` error with this status, of the problem type the API
    /// document gives it on both routes: the client decodes the body against
    /// that type, and only a decoded `404` means the session is gone.
    Problem(u16, &'static str),
}

const NOTHING_YET: Reply = Reply::Messages(&[]);
const OVERLOADED: Reply = Reply::Problem(503, "unavailable");
const NO_SESSION: Reply = Reply::Problem(404, "not-found");

/// A request as the relay saw it: the route's last segment and its body.
type Seen = Arc<Mutex<Vec<(String, serde_json::Value)>>>;

/// A relay on a loopback socket, in this test's runtime, answering from a
/// script and then with `then` for ever. It closes every connection after one
/// exchange, so a request it records is a request the client made.
///
/// The runtime's clock is paused and jumps when every task is idle. The
/// loopback delivers in the sending syscall, so a request or answer in flight
/// is already readable when the runtime looks, and the clock moves only when
/// both ends are really waiting on a timer.
struct FakeRelay {
    url: String,
    seen: Seen,
}

impl FakeRelay {
    async fn start(script: &[Reply], then: Reply) -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Seen::default();
        let record = Arc::clone(&seen);
        let mut script: VecDeque<Reply> = script.iter().cloned().collect();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let Some(request) = read_request(&mut socket).await else {
                    continue;
                };
                record.lock().unwrap().push(request);
                let reply = script.pop_front().unwrap_or_else(|| then.clone());
                answer(&mut socket, &reply).await;
            }
        });
        Self { url, seen }
    }

    fn rendezvous(&self, role: RendezvousRole) -> Rendezvous {
        Rendezvous::new(&self.url, "token", "session".into(), role).unwrap()
    }

    /// How many requests have reached the relay.
    fn requests(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    /// `field` of every request so far.
    fn field(&self, field: &str) -> Vec<serde_json::Value> {
        let seen = self.seen.lock().unwrap();
        seen.iter().map(|(_, body)| body[field].clone()).collect()
    }

    /// The route of every request so far.
    fn routes(&self) -> Vec<String> {
        let seen = self.seen.lock().unwrap();
        seen.iter().map(|(route, _)| route.clone()).collect()
    }
}

async fn read_request(socket: &mut TcpStream) -> Option<(String, serde_json::Value)> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
            let length: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .map_or(0, |value| value.trim().parse().unwrap());
            if bytes.len() >= end + 4 + length {
                let path = head.split_whitespace().nth(1)?;
                let route = path.rsplit('/').next()?.to_owned();
                let body = serde_json::from_slice(&bytes[end + 4..end + 4 + length]).ok()?;
                return Some((route, body));
            }
        }
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
}

async fn answer(socket: &mut TcpStream, reply: &Reply) {
    let (status, kind, body) = match reply {
        Reply::Messages(messages) => (
            200,
            "application/json",
            serde_json::json!({ "messages": messages, "expires_at_ms": 1 }),
        ),
        Reply::Accepted => (
            200,
            "application/json",
            serde_json::json!({ "sent": 1, "expires_at_ms": 1 }),
        ),
        Reply::Problem(status, problem) => (
            *status,
            "application/problem+json",
            serde_json::json!({
                "type": format!("https://sunrise.app/problems/{problem}"),
                "status": status,
            }),
        ),
    };
    let body = body.to_string();
    let response = format!(
        "HTTP/1.1 {status} Fake\r\ncontent-type: {kind}\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await.unwrap();
    socket.shutdown().await.unwrap();
}

/// How far past its target a wait may run: the [`POLL_INTERVAL`] sleep it was
/// in when the target passed, and the timer driver's rounding.
const SLACK: Duration = Duration::from_millis(450);

fn assert_ended_at(started: Instant, target: Duration) {
    let elapsed = started.elapsed();
    assert!(
        elapsed >= target && elapsed <= target + SLACK,
        "the wait ended after {elapsed:?}, aiming for {target:?}"
    );
}

/// The other side's abort, or the session's expiry, ends a wait at once and
/// for good: there is nothing left to poll.
#[tokio::test(start_paused = true)]
async fn a_404_ends_a_wait_at_once_as_gone() {
    let relay = FakeRelay::start(&[], NO_SESSION).await;
    let rendezvous = relay.rendezvous(RendezvousRole::ExistingDevice);
    let started = Instant::now();

    assert_eq!(rendezvous.next().await, Err(RendezvousError::Gone));
    assert_eq!(
        rendezvous.send("m".into()).await,
        Err(RendezvousError::Gone)
    );

    assert_eq!(started.elapsed(), Duration::ZERO, "neither call slept");
    assert_eq!(relay.routes(), ["receive", "send"], "and neither retried");
    assert!(RendezvousError::Gone.is_pairing());
}

/// A relay that keeps failing is retried for the whole bound, every
/// [`POLL_INTERVAL`], and then named as the relay's fault rather than the
/// other device's.
#[tokio::test(start_paused = true)]
async fn a_transient_failure_is_retried_to_the_cap_and_then_is_the_relays() {
    let relay = FakeRelay::start(&[], OVERLOADED).await;
    let rendezvous = relay.rendezvous(RendezvousRole::NewDevice);
    let polls = usize::try_from(WAIT_CAP.as_millis() / POLL_INTERVAL.as_millis()).unwrap();

    let started = Instant::now();
    let waiting = rendezvous.next().await.unwrap_err();
    assert_ended_at(started, WAIT_CAP);
    assert!(matches!(&waiting, RendezvousError::Relay(why) if why.contains("503")));
    assert!(!waiting.is_pairing());
    assert!(
        relay.requests() > polls / 2 && relay.requests() <= polls + 2,
        "{} requests in {polls} intervals",
        relay.requests()
    );

    let before = relay.requests();
    let started = Instant::now();
    let sending = rendezvous.send("m".into()).await.unwrap_err();
    assert_ended_at(started, WAIT_CAP);
    assert!(matches!(&sending, RendezvousError::Relay(why) if why.contains("503")));
    assert!(relay.requests() - before > polls / 2);
}

/// A relay that answers but holds nothing is the other device's silence:
/// [`RendezvousError::TimedOut`], not a relay fault.
#[tokio::test(start_paused = true)]
async fn silence_until_the_cap_times_out() {
    let relay = FakeRelay::start(&[], NOTHING_YET).await;
    let rendezvous = relay.rendezvous(RendezvousRole::ExistingDevice);

    let started = Instant::now();
    assert_eq!(rendezvous.next().await, Err(RendezvousError::TimedOut));
    assert_ended_at(started, WAIT_CAP);
    assert!(RendezvousError::TimedOut.is_pairing());
}

/// A cancel ends the wait at its next poll, without another request, and every
/// wait after it before it asks the relay anything.
#[tokio::test(start_paused = true)]
async fn a_cancel_ends_a_wait_at_its_next_poll_and_every_later_one() {
    let relay = FakeRelay::start(&[], NOTHING_YET).await;
    let rendezvous = relay.rendezvous(RendezvousRole::ExistingDevice);
    let cancel_at = POLL_INTERVAL * 2 + POLL_INTERVAL / 2;

    let started = Instant::now();
    let (waited, ()) = tokio::join!(rendezvous.next(), async {
        tokio::time::sleep(cancel_at).await;
        rendezvous.cancel();
    });

    assert_eq!(waited, Err(RendezvousError::Cancelled));
    assert_ended_at(started, cancel_at);
    assert_eq!(relay.requests(), 3, "polled at 0, 1 and 2 intervals only");

    assert_eq!(rendezvous.next().await, Err(RendezvousError::Cancelled));
    assert_eq!(
        rendezvous.send("m".into()).await,
        Err(RendezvousError::Cancelled)
    );
    assert_eq!(relay.requests(), 3, "a cancelled rendezvous asks nothing");
}

/// A cancel reaches a send that is retrying, too.
#[tokio::test(start_paused = true)]
async fn a_cancel_ends_a_retrying_send() {
    let relay = FakeRelay::start(&[], OVERLOADED).await;
    let rendezvous = relay.rendezvous(RendezvousRole::NewDevice);

    let (sent, ()) = tokio::join!(rendezvous.send("m".into()), async {
        tokio::time::sleep(POLL_INTERVAL / 2).await;
        rendezvous.cancel();
    });

    assert_eq!(sent, Err(RendezvousError::Cancelled));
    assert_eq!(relay.requests(), 1);
}

/// The relay acknowledges a resend by its index, so a retry that carried the
/// next one would hand the other side a duplicate. Only an accepted message
/// moves the index on.
#[tokio::test(start_paused = true)]
async fn a_retried_send_keeps_its_index_until_it_is_accepted() {
    let relay = FakeRelay::start(
        &[OVERLOADED, OVERLOADED, Reply::Accepted, OVERLOADED],
        Reply::Accepted,
    )
    .await;
    let rendezvous = relay.rendezvous(RendezvousRole::NewDevice);

    rendezvous.send("first".into()).await.unwrap();
    rendezvous.send("second".into()).await.unwrap();

    assert_eq!(relay.field("index"), [0, 0, 0, 1, 1]);
    assert_eq!(
        relay.field("message"),
        ["first", "first", "first", "second", "second"]
    );
}

/// A retried read asks from the same place, and only a message moves it.
#[tokio::test(start_paused = true)]
async fn a_retried_wait_keeps_its_place_until_a_message_arrives() {
    let relay = FakeRelay::start(
        &[
            OVERLOADED,
            NOTHING_YET,
            Reply::Messages(&["first"]),
            OVERLOADED,
        ],
        Reply::Messages(&["second"]),
    )
    .await;
    let rendezvous = relay.rendezvous(RendezvousRole::ExistingDevice);

    assert_eq!(rendezvous.next().await.as_deref(), Ok("first"));
    assert_eq!(rendezvous.next().await.as_deref(), Ok("second"));

    assert_eq!(relay.field("after"), [0, 0, 0, 1, 1]);
}
