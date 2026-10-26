//! The production client's two bounds, against local servers: a body is
//! refused once it passes the cap, without reading the rest of it, and an
//! exchange that does not finish in time is refused.
//!
//! `exchange` is what [`HttpsClient::call`] runs after its HTTPS check, so the
//! servers here speak plain HTTP to it through a plain connector. A TLS
//! fixture would test rustls, not these bounds.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use hyper::body::{Bytes, Frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Far more than the cap: what a hostile issuer sends.
const LONG: usize = 64 * MAX_BODY_BYTES;

#[tokio::test]
async fn plaintext_endpoints_are_refused_before_the_socket_opens() {
    let c = HttpsClient::new();
    let req = http::Request::builder()
        .uri("http://idp.example/.well-known/openid-configuration")
        .body(Vec::new())
        .unwrap();
    let err = c.call(req).await.expect_err("http:// must be refused");
    assert!(matches!(err, LoginError::Provider(_)), "{err:?}");
}

/// A body of [`LONG`] bytes, counting the bytes it has handed out.
struct Long {
    frame: Bytes,
    served: Arc<AtomicUsize>,
}

impl hyper::body::Body for Long {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if self.served.load(Ordering::Relaxed) >= LONG {
            return Poll::Ready(None);
        }
        self.served.fetch_add(self.frame.len(), Ordering::Relaxed);
        Poll::Ready(Some(Ok(Frame::data(self.frame.clone()))))
    }
}

/// The bounded read itself: a body far past the cap is refused with the cap's
/// error, and no more than the cap plus the one frame that crossed it was
/// pulled.
#[tokio::test]
async fn a_long_body_is_read_only_to_just_past_the_cap() {
    const FRAME: usize = 64 * 1024;
    let served = Arc::new(AtomicUsize::new(0));
    let body = Long {
        frame: Bytes::from(vec![b'a'; FRAME]),
        served: served.clone(),
    };
    let err = read_capped(body, MAX_BODY_BYTES, "https://idp.example/")
        .await
        .expect_err("a long body must be refused");
    assert!(
        matches!(&err, LoginError::Transport(m)
            if *m == format!("response from https://idp.example/ exceeds {MAX_BODY_BYTES} bytes")),
        "{err:?}"
    );
    let pulled = served.load(Ordering::Relaxed);
    assert!(
        pulled <= MAX_BODY_BYTES + FRAME,
        "read {pulled} bytes for a cap of {MAX_BODY_BYTES}"
    );
}

/// A body exactly at the cap is accepted whole.
#[tokio::test]
async fn a_body_at_the_cap_is_accepted() {
    let body = http_body_util::Full::new(Bytes::from(vec![b'a'; MAX_BODY_BYTES]));
    let got = read_capped(body, MAX_BODY_BYTES, "https://idp.example/")
        .await
        .expect("a body at the cap is read");
    assert_eq!(got.len(), MAX_BODY_BYTES);
}

/// Read a request head off `sock`, up to its blank line.
async fn read_head(sock: &mut TcpStream) {
    let mut seen = Vec::new();
    let mut buf = [0u8; 1024];
    while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = sock.read(&mut buf).await.expect("read the request");
        if n == 0 {
            return;
        }
        seen.extend_from_slice(&buf[..n]);
    }
}

/// One connection on a local port, handed to `serve`: the URL to ask, and the
/// server's task.
async fn local_server<F, Fut>(serve: F) -> (String, tokio::task::JoinHandle<()>)
where
    F: FnOnce(TcpStream) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        read_head(&mut sock).await;
        serve(sock).await;
    });
    (
        format!("http://{addr}/.well-known/openid-configuration"),
        task,
    )
}

/// Ask `url` with `timeout`, and fail the test if the client has not given up
/// by itself well after it: a client with no deadline would hang the suite.
async fn ask(url: &str, timeout: Duration) -> Result<Response, LoginError> {
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http::<String>();
    let req = http::Request::builder().uri(url).body(Vec::new()).unwrap();
    tokio::time::timeout(
        timeout + Duration::from_secs(5),
        exchange(&client, req, timeout, MAX_BODY_BYTES),
    )
    .await
    .expect("the client gave up by itself")
}

/// A server that sends far past the cap is refused with the cap's error, and
/// the client hangs up rather than reading the rest: the server's writes fail
/// long before it has sent everything.
#[tokio::test]
async fn a_server_that_sends_past_the_cap_is_refused_without_reading_it_all() {
    let written = Arc::new(AtomicUsize::new(0));
    let w = written.clone();
    let (url, server) = local_server(move |mut sock| async move {
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {LONG}\r\n\r\n"
        );
        if sock.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        let chunk = vec![b'a'; 64 * 1024];
        while w.load(Ordering::Relaxed) < LONG {
            if sock.write_all(&chunk).await.is_err() {
                return; // The client hung up: the point of the test.
            }
            w.fetch_add(chunk.len(), Ordering::Relaxed);
        }
    })
    .await;

    let err = ask(&url, Duration::from_secs(60))
        .await
        .expect_err("a body past the cap must be refused");
    assert!(
        matches!(&err, LoginError::Transport(m)
            if m.contains(&format!("exceeds {MAX_BODY_BYTES} bytes"))),
        "{err:?}"
    );
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("the server stops once the client has gone")
        .unwrap();
    let sent = written.load(Ordering::Relaxed);
    assert!(
        sent < LONG / 2,
        "the client read {sent} of {LONG} bytes before refusing"
    );
}

/// A server that accepts and never answers is refused at the deadline.
#[tokio::test]
async fn a_server_that_never_answers_is_refused_at_the_timeout() {
    let (url, _server) = local_server(|sock| async move {
        let _held = sock;
        std::future::pending::<()>().await;
    })
    .await;
    let err = ask(&url, Duration::from_millis(200))
        .await
        .expect_err("a silent server must be refused");
    assert!(
        matches!(&err, LoginError::Transport(m) if m.contains("no complete response within 200ms")),
        "{err:?}"
    );
}

/// The deadline covers the body too: a server that sends its head and then
/// trickles is refused at the same deadline, not kept for as long as it likes.
#[tokio::test]
async fn a_server_that_trickles_its_body_is_refused_at_the_timeout() {
    let (url, _server) = local_server(|mut sock| async move {
        let head = b"HTTP/1.1 200 OK\r\ncontent-length: 1000\r\n\r\n";
        if sock.write_all(head).await.is_err() {
            return;
        }
        loop {
            if sock.write_all(b"a").await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    let err = ask(&url, Duration::from_millis(300))
        .await
        .expect_err("a trickled body must be refused");
    assert!(
        matches!(&err, LoginError::Transport(m) if m.contains("no complete response within 300ms")),
        "{err:?}"
    );
}
