//! An in-process client for the typed surface.
//!
//! `Service::call` is documented as the seam "so that a Kynos service can be
//! driven directly — by a test, or by an embedding that owns its own accept
//! loop". Driving it is all a test needs: no port is bound, no runtime task is
//! spawned, and the request travels the same routing, extraction and
//! interceptor path a real one does.
//!
//! kynos also ships a richer `TestClient` behind its `test-util` feature, and
//! this is deliberately not that. `test-util` pulls in `jsonschema`, which wants
//! a newer `regex-automata` than `criterion` pins and brings an `MIT-0`
//! dependency into a licence allowlist `deny.toml` curates by hand. Neither is
//! a fair price for assertion sugar.

use crate::metrics::Metrics;
use crate::state::ServerState;
use crate::ServerConfig;
use http_body_util::BodyExt as _;
use kynos::http::body::Body;
use kynos::http::{HeaderName, HeaderValue, Method, Request, StatusCode};
use kynos::router::service::Service;

/// The bearer tests present.
///
/// [`ServerState::new`] installs `NullVerifier`, which maps any bearer to one
/// synthetic self-host account, so the value means no more than "a credential
/// was presented".
pub(crate) const BEARER: &str = "Bearer test";

/// A client over a built typed surface.
pub(crate) struct Client {
    service: Service<ServerState>,
    /// The registry the handlers write to. `Metrics` is `Arc` inside, so this
    /// is that registry rather than a copy of it.
    pub(crate) metrics: Metrics,
    /// The same clock the server checks a `Date` against, so a test signs
    /// inside the replay window rather than near its edge.
    clock: std::sync::Arc<dyn crate::state::Clock>,
    /// The live sessions, so a test can assert what establishment filed.
    pub(crate) sessions: crate::sync_session::SessionStore,
}

impl Client {
    /// Build the whole typed surface over a fresh state.
    ///
    /// # Panics
    /// If the router cannot be described, which is the failure `build` exists
    /// to surface at startup rather than at documentation time.
    pub(crate) fn new(config: ServerConfig) -> Self {
        let state = ServerState::new(config);
        let metrics = state.metrics.clone();
        let clock = state.clock.clone();
        let sessions = state.sessions.clone();
        let service = state_service(state);

        Self {
            service,
            metrics,
            clock,
            sessions,
        }
    }

    /// A client over a state the caller assembled — a custom verifier, a test
    /// clock, tuned relay bounds.
    pub(crate) fn from_state(state: ServerState) -> Self {
        let metrics = state.metrics.clone();
        let clock = state.clock.clone();
        let sessions = state.sessions.clone();
        let service = state_service(state);
        Self {
            service,
            metrics,
            clock,
            sessions,
        }
    }

    /// A client whose blob root is a fresh temp directory.
    ///
    /// The guard comes back with it: dropping the `TempDir` deletes the tree,
    /// so a test that let it go out of scope would be writing chunks into a
    /// directory that no longer exists.
    pub(crate) fn with_blob_root() -> (Self, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let config = ServerConfig {
            blob_root: Some(dir.path().to_path_buf()),
            ..ServerConfig::default()
        };
        (Self::new(config), dir)
    }

    /// A client with a temp blob root and a verifier that checks bearers.
    ///
    /// For the tests that assert a credential is *required*: the default
    /// `NullVerifier` accepts an absent one on purpose.
    pub(crate) fn with_blob_root_and_verifier() -> (Self, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let state = ServerState::new(ServerConfig {
            blob_root: Some(dir.path().to_path_buf()),
            ..ServerConfig::default()
        })
        .with_verifier(std::sync::Arc::new(
            crate::StaticVerifier::default()
                .with("test", crate::Subject::new("https://idp.example", "alice")),
        ));
        (Self::from_state(state), dir)
    }

    /// The server's own notion of now, in milliseconds.
    pub(crate) fn clock_now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// Read an event stream for `window`, then stop.
    ///
    /// A live SSE body never ends, so reading it to completion would hang. This
    /// collects whatever arrived inside the window, which is what a test of the
    /// replay half needs: the frames and markers the stream emits before it
    /// settles into waiting for live traffic.
    pub(crate) async fn read_stream(
        &self,
        path: &str,
        headers: &[(&str, &str)],
        window: std::time::Duration,
    ) -> String {
        use http_body_util::BodyExt as _;

        let mut request = Request::new(Body::empty());
        *request.uri_mut() = path.parse().expect("a well-formed target");
        request.headers_mut().insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static(BEARER),
        );
        for (name, value) in headers {
            request.headers_mut().insert(
                HeaderName::from_bytes(name.as_bytes()).expect("a header name"),
                HeaderValue::from_str(value).expect("a header value"),
            );
        }

        let mut body = self.service.call(request).await.into_body();
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + window;
        loop {
            let frame = tokio::time::timeout_at(deadline, body.frame()).await;
            match frame {
                // The window closed: whatever has arrived is the answer.
                Err(_) => break,
                Ok(None) => break,
                Ok(Some(Err(_))) => break,
                Ok(Some(Ok(frame))) => {
                    if let Some(data) = frame.data_ref() {
                        out.extend_from_slice(data);
                    }
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Send a raw body under an explicit media type.
    pub(crate) async fn send_bytes(
        &self,
        method: Method,
        path: &str,
        media_type: &str,
        body: &[u8],
        headers: &[(&str, &str)],
    ) -> Res {
        let mut request = Request::new(Body::from_bytes(body.to_vec().into()));
        *request.method_mut() = method;
        *request.uri_mut() = path.parse().expect("a well-formed target");
        request.headers_mut().insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static(BEARER),
        );
        request.headers_mut().insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_str(media_type).expect("a media type"),
        );
        for (name, value) in headers {
            request.headers_mut().insert(
                HeaderName::from_bytes(name.as_bytes()).expect("a header name"),
                HeaderValue::from_str(value).expect("a header value"),
            );
        }

        let response = self.service.call(request).await;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("the response body must collect")
            .to_bytes()
            .to_vec();
        Res { status, bytes }
    }

    /// Send a request carrying [`BEARER`].
    pub(crate) async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Res {
        self.send_as(method, path, Some(BEARER), body).await
    }

    /// Send a request with an explicit credential, or none.
    pub(crate) async fn send_as(
        &self,
        method: Method,
        path: &str,
        bearer: Option<&str>,
        body: Option<&serde_json::Value>,
    ) -> Res {
        self.send_with(method, path, bearer, body, &[]).await
    }

    /// Send a request carrying extra headers — the device binding, chiefly.
    pub(crate) async fn send_with(
        &self,
        method: Method,
        path: &str,
        bearer: Option<&str>,
        body: Option<&serde_json::Value>,
        headers: &[(&str, &str)],
    ) -> Res {
        // Built field by field rather than with `Request::builder`, which the
        // `http` crate puts on `Request<()>` only — and `kynos::http::Request`
        // is already `Request<Body>`.
        let encoded = body.map(|value| serde_json::to_vec(value).expect("a serializable body"));
        let mut request = Request::new(match &encoded {
            Some(bytes) => Body::from_bytes(bytes.clone().into()),
            None => Body::empty(),
        });
        *request.method_mut() = method;
        *request.uri_mut() = path.parse().expect("a well-formed target");
        if let Some(bearer) = bearer {
            request.headers_mut().insert(
                HeaderName::from_static("authorization"),
                HeaderValue::from_str(bearer).expect("a header-safe credential"),
            );
        }
        if encoded.is_some() {
            request.headers_mut().insert(
                HeaderName::from_static("content-type"),
                HeaderValue::from_static("application/json"),
            );
        }
        for (name, value) in headers {
            request.headers_mut().insert(
                HeaderName::from_bytes(name.as_bytes()).expect("a header name"),
                HeaderValue::from_str(value).expect("a header value"),
            );
        }

        let response = self.service.call(request).await;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("the response body must collect")
            .to_bytes()
            .to_vec();
        Res { status, bytes }
    }
}

/// Build the router over `state`.
fn state_service(state: ServerState) -> Service<ServerState> {
    let config = ServerConfig::clone(&state.config);
    super::router(&config)
        .build(state)
        .expect("the typed surface must build")
}

/// One response, already read.
pub(crate) struct Res {
    /// The status line.
    pub(crate) status: StatusCode,
    /// The whole body.
    pub(crate) bytes: Vec<u8>,
}

impl Res {
    /// The body as JSON.
    ///
    /// # Panics
    /// If the body is not JSON, which for this surface means the response was
    /// not the one the test expected — so panicking here reports the real
    /// failure rather than deferring it to a confusing field access.
    pub(crate) fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.bytes).unwrap_or_else(|e| {
            panic!(
                "expected a JSON body, got {e}: {}",
                String::from_utf8_lossy(&self.bytes)
            )
        })
    }

    /// Assert the status, reporting the body when it does not match — a problem
    /// document says why, and a bare status comparison throws that away.
    #[track_caller]
    pub(crate) fn assert_status(&self, expected: StatusCode) -> &Self {
        assert_eq!(
            self.status,
            expected,
            "unexpected status; body was: {}",
            String::from_utf8_lossy(&self.bytes)
        );
        self
    }
}
