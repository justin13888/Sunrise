//! What the server is allowed to say about a request, checked against the real
//! router and the real subscriber stack.
//!
//! [ADR-0021](../../../docs/11-adr/0021-kynos-openapi-server.md) required this
//! file to survive the port to kynos, and
//! `docs/10-cross-cutting/logging.md` §11 lists it as the test that holds "no
//! `?access_token=`, no full entity ids, every field allowlisted, healthy
//! traffic silent at `info`". It did not survive the port — the axum router it
//! drove was deleted along with `build_router`, and the assertions went with it
//! — and it was **restored afterwards**, over `build_service`, the `kynos`
//! entry point that replaced that constructor. What follows describes the file
//! that runs today; the section below is why it is worth having as an
//! integration test rather than a record of its absence.
//!
//! # Why it stands as an integration test rather than folded into the unit tests
//!
//! `api/observe.rs` has unit tests over the same observer, and they are not the
//! same guarantee. They reach `crate::api::testing::Client`, which is
//! `pub(crate)` and builds the service from inside the crate. This file has
//! only the public surface — `ServerState::new` and
//! [`sunrise_server::build_service`] — which is what an operator's deployment
//! has. A redaction property that holds only through a test-only constructor
//! would be worth very little; the hazard is a bearer reaching a *production*
//! log, and this is the assembly that produces one.
//!
//! The hazard is concrete rather than hypothetical: a bearer reached this
//! server's log once already, from `?access_token=` in a URI a stock request
//! log records verbatim. `docs/06-server/auth.md` still describes that query
//! fallback as unbuilt and this test as the guard standing over it — "it MUST
//! survive any port … because the day the query fallback *is* added is the day
//! it starts mattering".
//!
//! Each test installs its own subscriber with
//! `tracing::dispatcher::with_default`, which is thread-local, so they run
//! concurrently without fighting over a global default.

use http_body_util::BodyExt as _;
use kynos::http::body::Body;
use kynos::http::{HeaderName, HeaderValue, Method, Request, StatusCode};
use kynos::router::service::Service;
use sunrise_log::{build_subscriber, Capture, LogConfig, LogFormat, LogTarget};
use sunrise_server::{ServerConfig, ServerState};

/// A bearer-shaped string that must never appear in a log line.
const QUERY_TOKEN: &str = "eyJhbGciOiJIUzI1NiJ9.SENTINELINQUERYSTRING.sig";
/// The same, presented the way a real client presents it.
const HEADER_TOKEN: &str = "eyJhbGciOiJIUzI1NiJ9.SENTINELINAUTHHEADER.sig";

/// The public surface, assembled exactly as [`sunrise_server::serve`] does.
struct Server {
    service: Service<ServerState>,
}

impl Server {
    /// Assemble from a state the caller has already configured.
    ///
    /// [`ServerState::with_verifier`] is the seam an operator's `main` uses to
    /// install a real identity provider, so a test that needs a bearer the
    /// server can *refuse* reaches for it rather than for a test-only hook.
    fn from_state(state: ServerState) -> Self {
        Self {
            service: sunrise_server::build_service(state).expect("the typed surface must build"),
        }
    }

    /// Drive one request through the router. `Service::call` is kynos's
    /// documented embedding seam, so this is the same routing, extraction and
    /// observer path a socket-borne request takes — minus the socket.
    async fn send(
        &self,
        method: Method,
        target: &str,
        body: Option<&serde_json::Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Vec<u8>) {
        let encoded = body.map(|v| serde_json::to_vec(v).expect("a serializable body"));
        let mut request = Request::new(match &encoded {
            Some(bytes) => Body::from_bytes(bytes.clone().into()),
            None => Body::empty(),
        });
        *request.method_mut() = method;
        *request.uri_mut() = target.parse().expect("a well-formed target");
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
        (status, bytes)
    }
}

/// Run `body` against a fresh server with logging captured, and hand back
/// everything that reached the sink.
///
/// A plain `#[test]` driving its own current-thread runtime rather than
/// `#[tokio::test]`: installing a dispatcher is synchronous and has to wrap the
/// whole request rather than sit inside it.
fn captured<F, Fut>(filter: &str, config: ServerConfig, body: F) -> String
where
    F: FnOnce(Server) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    captured_with(filter, ServerState::new(config), body)
}

/// [`captured`], from a state the caller has already configured.
fn captured_with<F, Fut>(filter: &str, state: ServerState, body: F) -> String
where
    F: FnOnce(Server) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let cap = Capture::new();
    let dispatch = build_subscriber(LogConfig {
        target: LogTarget::Capture(cap.clone()),
        filter: filter.to_owned(),
        format: LogFormat::Ndjson,
    })
    .expect("subscriber builds");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    tracing::dispatcher::with_default(&dispatch, || {
        rt.block_on(async move { body(Server::from_state(state)).await });
    });
    cap.contents()
}

/// The one record with this `ev`, parsed.
fn record(out: &str, ev: &str) -> serde_json::Value {
    let line = out
        .lines()
        .find(|l| l.contains(&format!("\"ev\":\"{ev}\"")))
        .unwrap_or_else(|| panic!("no {ev} record in:\n{out}"));
    serde_json::from_str(line).expect("record is JSON")
}

/// The hazard this whole module exists for, from both directions at once.
///
/// The query string is where a browser client puts its bearer and the header is
/// where every other client puts it. Neither reaches the log, and not by a
/// redaction pass over the URI: the observer is handed the matched route's
/// `paths` key and never reads the request's own target, so there is no step to
/// forget.
#[test]
fn a_bearer_in_the_query_string_and_in_the_header_both_stay_out_of_the_log() {
    let out = captured("debug", ServerConfig::default(), |server| async move {
        server
            .send(
                Method::GET,
                &format!("/api/v1/meta?access_token={QUERY_TOKEN}"),
                None,
                &[("authorization", &format!("Bearer {HEADER_TOKEN}"))],
            )
            .await;
    });

    assert!(!out.is_empty(), "nothing was logged at all");
    assert!(
        !out.contains("SENTINELINQUERYSTRING"),
        "a query-string bearer leaked: {out}"
    );
    assert!(
        !out.contains("SENTINELINAUTHHEADER"),
        "a header bearer leaked: {out}"
    );
    assert!(!out.contains("access_token"), "the query leaked: {out}");
    assert!(
        !out.contains("/api/v1/meta?"),
        "the raw target leaked: {out}"
    );
    // The request *was* logged, so the three absences above are redaction
    // rather than silence — the failure mode the old span-redaction test had.
    assert!(out.contains("srv.req.end"), "no request record: {out}");
    assert!(
        out.contains("\"endpoint\":\"/api/v1/meta\""),
        "the templated endpoint is still recorded: {out}"
    );
}

/// A refused signature is the one place raw key material is in scope, and the
/// tempting thing to log is exactly the thing that must not be: the signature
/// bytes and the device they name.
#[test]
fn a_signed_request_that_is_refused_logs_no_signature_bytes() {
    use base64::Engine as _;
    use ed25519_dalek::SigningKey;

    let out = captured("debug", ServerConfig::default(), |server| async move {
        let real = SigningKey::from_bytes(&[21u8; 32]);
        let (status, bytes) = server
            .send(
                Method::POST,
                "/api/v1/devices",
                Some(&serde_json::json!({
                    "device_pub_s": base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .encode(real.verifying_key().as_bytes()),
                    "nickname": "laptop",
                    "platform": "linux",
                })),
                &[("authorization", "Bearer test")],
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "device registration failed");
        let device_id = serde_json::from_slice::<serde_json::Value>(&bytes).expect("a JSON body")
            ["device_id"]
            .as_str()
            .expect("a device id")
            .to_owned();

        // A real, active device of this account, signed by a key that is not
        // its own: the case that reaches `srv.auth.device_sig_rejected`.
        let impostor = SigningKey::from_bytes(&[22u8; 32]);
        let date = jiff::Timestamp::now()
            .strftime("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();
        let signature = sunrise_http_sig::sign::<serde_json::Value>(
            &impostor,
            "GET",
            "/api/v1/accounts/me",
            &date,
            None,
        )
        .expect("the client half signs");

        let (status, _) = server
            .send(
                Method::GET,
                "/api/v1/accounts/me",
                None,
                &[
                    ("authorization", "Bearer test"),
                    ("x-sunrise-device", &device_id),
                    ("x-sunrise-device-sig", &signature),
                    ("date", &date),
                ],
            )
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "the signature was wrong");
    });

    let rejected = record(&out, "srv.auth.device_sig_rejected");
    assert_eq!(rejected["err_code"], "AUTH_DEVICE_SIG_INVALID");
    assert_eq!(rejected["err_kind"], "user");
    assert!(
        rejected["cause"].is_string(),
        "an operator needs to know which way it failed: {rejected}"
    );

    // The signature is 86 base64url characters; nothing that long should be in
    // the log at all, and a substring check would miss a truncated leak.
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        let record: serde_json::Value = serde_json::from_str(line).expect("record is JSON");
        let flat = record.to_string();
        assert!(
            !flat.contains("dev_"),
            "a device id reached the log: {line}"
        );
        for run in flat.split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_') {
            assert!(
                run.len() < 40,
                "a {}-character opaque run reached the log: {line}",
                run.len()
            );
        }
    }
}

/// The record shape `docs/10-cross-cutting/log-events.md` catalogues and an
/// ingest pipeline parses. The `endpoint` is the description's `paths` key in
/// the log's own `:id` spelling, so a dashboard grouping by it sees one series
/// rather than one per device.
#[test]
fn the_request_records_carry_the_catalogued_shape() {
    let out = captured("debug", ServerConfig::default(), |server| async move {
        server
            .send(
                Method::DELETE,
                "/api/v1/devices/dev_01J8ZQ7X9K3M5N7P9R1T3V5W7Y",
                None,
                &[],
            )
            .await;
    });

    assert!(
        out.contains("\"ev\":\"srv.req.start\""),
        "no start record: {out}"
    );
    assert!(
        !out.contains("01J8ZQ7X9K3M5N7P9R1T3V5W7Y"),
        "a full device id reached the log: {out}"
    );

    let end = record(&out, "srv.req.end");
    assert!(end["status"].is_u64(), "{end}");
    assert!(end["lat_ms"].is_u64(), "{end}");
    assert!(end["result"].is_string(), "{end}");
    assert_eq!(end["span"]["method"], "DELETE");
    assert_eq!(end["span"]["endpoint"], "/api/v1/devices/:id");
}

/// `srv.req.end` is debug for a served request and warn for a 5xx, which is
/// what makes a default `info` deployment show failures and nothing else. A
/// server whose healthy traffic is chatty at `info` is one whose operator turns
/// the log off.
#[test]
fn healthy_traffic_is_silent_at_info() {
    let quiet = captured("info", ServerConfig::default(), |server| async move {
        let (status, _) = server.send(Method::GET, "/api/v1/health", None, &[]).await;
        assert_eq!(status, StatusCode::OK);
    });
    assert!(
        !quiet.contains("srv.req."),
        "a healthy 200 was logged at info: {quiet}"
    );

    // A blob root that is a *file* makes the pending-upload `create_dir_all`
    // fail, which is the surface's own path to a 500 — no test-only seam and
    // no panic.
    let dir = tempfile::tempdir().expect("a temp dir");
    let not_a_dir = dir.path().join("blobs");
    std::fs::write(&not_a_dir, b"this is a file").expect("write the obstruction");
    let config = ServerConfig {
        blob_root: Some(not_a_dir),
        ..ServerConfig::default()
    };

    let loud = captured("info", config, |server| async move {
        let (status, _) = server
            .send(
                Method::POST,
                "/api/v1/blobs/init",
                Some(&serde_json::json!({
                    "stream_id": "11111111111111111111111111111111",
                    "chunk_count": 1,
                    "size_bytes": 10,
                })),
                &[],
            )
            .await;
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "the obstruction must produce a 5xx, or this test proves nothing"
        );
    });

    let end = record(&loud, "srv.req.end");
    assert_eq!(end["status"], 500);
    assert_eq!(end["result"], "failed");
    assert_eq!(
        end["level"], "WARN",
        "a 5xx must clear an info filter: {end}"
    );
}

/// A bearer-shaped refresh token, so a leak is greppable rather than inferred.
const REFRESH_TOKEN: &str = "eyJhbGciOiJIUzI1NiJ9.SENTINELINREFRESHBODY.sig";

/// The other refusal records that carried their diagnostic under `reason`.
///
/// `reason` is not on `sunrise-log`'s allowlist, so `RedactionLayer` refused
/// the whole event: a panic under `debug_assertions`, a silent drop and a
/// `violations()` bump in release. Every one of these warnings was invisible to
/// the operator it was written for.
/// [`a_signed_request_that_is_refused_logs_no_signature_bytes`] pins
/// `srv.auth.device_sig_rejected`; these are the two others a client can reach
/// through the public surface.
///
/// `srv.store.failed` is the fourth and is deliberately not here: it fires only
/// when `SQLite` itself fails, which no request can provoke through this surface
/// without a test-only seam into the store. `sunrise-log`'s
/// `every_emitted_field_name_is_on_the_redaction_allowlist` is what covers it —
/// statically, at every site, which is the gate this class of defect needed.
#[test]
fn the_refusal_records_survive_redaction_and_carry_their_cause() {
    use sunrise_server::{StaticVerifier, Subject};
    use sunrise_wire_protocol::{Capability, CapabilityBits, REQUIRED_CLIENT_BITS};

    // `NullVerifier`, the self-host default, accepts every bearer it is shown,
    // so under it a refresh can never be refused. `StaticVerifier` is the same
    // public seam a multi-tenant deployment installs its own provider through.
    let state = ServerState::new(ServerConfig::default()).with_verifier(std::sync::Arc::new(
        StaticVerifier::default().with("test", Subject::new("https://idp.test", "user-1")),
    ));

    let out = captured_with("debug", state, |server| async move {
        let caps =
            REQUIRED_CLIENT_BITS.0 | CapabilityBits::EMPTY.with(Capability::SrvTokenRefresh).0;
        let doc_v = u32::from(sunrise_cbor::version::DOC_SCHEMA_V);
        let crypto_v = u32::from(sunrise_cbor::version::CRYPTO_SUITE_V);

        // A wire version no server can agree to: `srv.sync.negotiate_refused`.
        let (status, _) = server
            .send(
                Method::POST,
                "/api/v1/sync/session",
                Some(&serde_json::json!({
                    "client_app_v": "0.1.0",
                    "client_platform": "test",
                    "wire_proto_supported": [9999u32],
                    "doc_schema_min": doc_v,
                    "doc_schema_max": doc_v,
                    "crypto_suite_supported": [crypto_v],
                    "capabilities": caps,
                    "trace": "01J000000000000000000000000",
                })),
                &[("authorization", "Bearer test")],
            )
            .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "the negotiation must be refused, or this test proves nothing"
        );

        // Then a real session, refreshed with a token the verifier refuses:
        // `srv.sync.refresh_rejected`.
        let (status, bytes) = server
            .send(
                Method::POST,
                "/api/v1/sync/session",
                Some(&serde_json::json!({
                    "client_app_v": "0.1.0",
                    "client_platform": "test",
                    "wire_proto_supported": [u32::from(sunrise_cbor::WIRE_PROTO_V)],
                    "doc_schema_min": doc_v,
                    "doc_schema_max": doc_v,
                    "crypto_suite_supported": [crypto_v],
                    "capabilities": caps,
                    "trace": "01J000000000000000000000000",
                })),
                &[("authorization", "Bearer test")],
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "the session must establish");
        let session_id = serde_json::from_slice::<serde_json::Value>(&bytes).expect("a JSON body")
            ["session_id"]
            .as_str()
            .expect("a session id")
            .to_owned();

        let (status, _) = server
            .send(
                Method::POST,
                "/api/v1/sync/session/refresh",
                Some(&serde_json::json!({ "token": REFRESH_TOKEN })),
                &[
                    ("authorization", "Bearer test"),
                    ("x-sunrise-session", &session_id),
                ],
            )
            .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "the replacement token must be refused"
        );
    });

    let refused = record(&out, "srv.sync.negotiate_refused");
    // `permanent`, not `user`: the four negotiation codes are all `permanent`
    // in `crates/sunrise-error/codes.toml`, and the record now carries the
    // code itself — a record naming `SYNC_PROTOCOL_VERSION_MISMATCH` beside
    // `err_kind: user` would contradict the registry on the same line.
    assert_eq!(refused["err_kind"], "permanent");
    assert_eq!(
        refused["err_code"], "SYNC_PROTOCOL_VERSION_MISMATCH",
        "the refusal's own code, not a shared one: {refused}"
    );
    assert!(
        refused["cause"].is_string(),
        "an operator needs to know what could not be agreed: {refused}"
    );

    let rejected = record(&out, "srv.sync.refresh_rejected");
    assert_eq!(rejected["err_kind"], "user");
    assert!(
        rejected["cause"].is_string(),
        "an operator needs to know why the token failed: {rejected}"
    );
    assert!(
        rejected["account_h"].is_string(),
        "the record is scoped to an account by hash: {rejected}"
    );

    // The rejected credential is still a credential.
    assert!(
        !out.contains("SENTINELINREFRESHBODY"),
        "the refused refresh token reached the log: {out}"
    );
}
