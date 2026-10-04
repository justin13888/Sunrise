//! The limiter through the real router: the per-address groups, the
//! failed-auth budget, the trusted-proxy resolution, and the wire contract.
//!
//! Every test drives an injected clock, so a refill is a value and nothing
//! sleeps.

use crate::api::error::codes;
use crate::api::testing::{code_of, Client, Res, BEARER};
use crate::config::LimitsConfig;
use crate::state::{Clock, ServerState};
use crate::{ServerConfig, StaticVerifier, Subject};
use kynos::http::{Method, StatusCode};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const T0_MS: u64 = 1_704_067_200_000;

#[derive(Debug)]
struct TestClock(AtomicU64);

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl TestClock {
    fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

/// A socket peer, in the `Option` every send helper takes: `None` is a request
/// that arrived on no socket.
#[allow(clippy::unnecessary_wraps)]
fn peer(addr: &str) -> Option<SocketAddr> {
    Some(SocketAddr::new(addr.parse().expect("an address"), 40_000))
}

/// A client with a hand-moved clock, the given proxies, and `limits`.
fn client_with(proxies: &[&str], limits: LimitsConfig) -> (Client, Arc<TestClock>) {
    let clock = Arc::new(TestClock(AtomicU64::new(T0_MS)));
    let config = ServerConfig {
        trusted_proxies: proxies.iter().map(|p| (*p).to_owned()).collect(),
        limits,
        ..ServerConfig::default()
    };
    let state = ServerState::with_clock(config, clock.clone()).with_verifier(Arc::new(
        StaticVerifier::default().with("test", Subject::new("https://idp.example", "alice")),
    ));
    (Client::from_state(state), clock)
}

fn tight() -> LimitsConfig {
    LimitsConfig {
        meta_per_min: 3,
        failed_auth_per_5min: 2,
        ..LimitsConfig::default()
    }
}

async fn meta_from(client: &Client, from: Option<SocketAddr>, headers: &[(&str, &str)]) -> Res {
    client
        .send_from(from, Method::GET, "/api/v1/meta", None, None, headers)
        .await
}

/// Admissions until the first refusal, from `from`.
async fn admitted_from(client: &Client, from: Option<SocketAddr>, headers: &[(&str, &str)]) -> u32 {
    let mut n = 0;
    for _ in 0..10 {
        if meta_from(client, from, headers).await.status == StatusCode::TOO_MANY_REQUESTS {
            return n;
        }
        n += 1;
    }
    n
}

fn retry_after(res: &Res) -> u64 {
    res.headers
        .get("retry-after")
        .expect("a 429 carries Retry-After")
        .to_str()
        .unwrap()
        .parse()
        .expect("whole seconds")
}

/// A group trips at its documented threshold, and the refusal is the wire
/// contract: `429`, `RATE_LIMITED`, and a `Retry-After` in whole seconds.
#[tokio::test]
async fn a_group_trips_at_its_threshold_with_the_documented_refusal() {
    let (client, _) = client_with(&[], tight());
    let a = peer("198.51.100.1");
    for _ in 0..3 {
        meta_from(&client, a, &[])
            .await
            .assert_status(StatusCode::OK);
    }
    let refused = meta_from(&client, a, &[]).await;
    refused.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(code_of(&refused), codes::RATE_LIMITED);
    assert_eq!(
        refused.json()["type"],
        "https://sunrise.app/problems/rate-limited"
    );
    assert_eq!(
        retry_after(&refused),
        20,
        "one unit of 3/min is back in 20 s"
    );
}

/// Waiting exactly `Retry-After` is admitted; a moment less is not.
#[tokio::test]
async fn retry_after_is_honest() {
    let (client, clock) = client_with(&[], tight());
    let a = peer("198.51.100.1");
    for _ in 0..3 {
        meta_from(&client, a, &[]).await;
    }
    let refused = meta_from(&client, a, &[]).await;
    let wait = retry_after(&refused) * 1_000;

    clock.advance(wait - 1);
    meta_from(&client, a, &[])
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);
    clock.advance(1);
    meta_from(&client, a, &[])
        .await
        .assert_status(StatusCode::OK);
}

/// Two clients, two buckets; two groups, two buckets.
#[tokio::test]
async fn addresses_and_groups_are_independent() {
    let (client, _) = client_with(&[], tight());
    assert_eq!(admitted_from(&client, peer("198.51.100.1"), &[]).await, 3);
    assert_eq!(
        admitted_from(&client, peer("198.51.100.2"), &[]).await,
        3,
        "a second address has its own bucket"
    );
    client
        .send_from(
            peer("198.51.100.1"),
            Method::GET,
            "/api/v1/health",
            None,
            None,
            &[],
        )
        .await
        .assert_status(StatusCode::OK);
}

/// Every refusal is counted under bounded labels: the route template and the
/// scope, never the address.
#[tokio::test]
async fn a_refusal_is_counted_by_endpoint_and_scope() {
    let (client, _) = client_with(&[], tight());
    admitted_from(&client, peer("198.51.100.1"), &[]).await;
    assert_eq!(
        client.metrics.get_with(
            "sunrise_ratelimit_rejected_total",
            &[("endpoint", "/api/v1/meta"), ("scope", "ip")]
        ),
        1
    );
    assert!(
        !client.metrics.render().contains("198.51.100"),
        "no address reaches a label"
    );
}

/// **A forwarding header from an untrusted peer is ignored.** With no proxy
/// trusted, a client rotating `X-Forwarded-For` still counts against its own
/// socket address.
#[tokio::test]
async fn a_spoofed_forwarding_header_from_an_untrusted_peer_is_ignored() {
    let (client, _) = client_with(&[], tight());
    let a = peer("198.51.100.1");
    for i in 0..3 {
        let xff = format!("203.0.113.{i}");
        meta_from(&client, a, &[("x-forwarded-for", &xff)])
            .await
            .assert_status(StatusCode::OK);
    }
    meta_from(&client, a, &[("x-forwarded-for", "203.0.113.99")])
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);
    meta_from(&client, a, &[("forwarded", "for=203.0.113.98")])
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);
}

/// **A trusted chain resolves to the client.** Two clients behind one trusted
/// proxy get two buckets, and a client prepending its own forged hop does not
/// move to a third: the right-most untrusted address is the client.
#[tokio::test]
async fn a_trusted_proxy_chain_resolves_to_the_client() {
    let (client, _) = client_with(&["127.0.0.1/32", "10.0.0.0/8"], tight());
    let proxy = peer("127.0.0.1");

    assert_eq!(
        admitted_from(&client, proxy, &[("x-forwarded-for", "198.51.100.1")]).await,
        3
    );
    assert_eq!(
        admitted_from(&client, proxy, &[("x-forwarded-for", "198.51.100.2")]).await,
        3,
        "the second client is not the proxy's bucket"
    );
    // Two trusted hops deep, with a forged hop in front.
    meta_from(
        &client,
        proxy,
        &[("x-forwarded-for", "203.0.113.7, 198.51.100.1, 10.1.2.3")],
    )
    .await
    .assert_status(StatusCode::TOO_MANY_REQUESTS);
    // RFC 7239's own field resolves the same way.
    meta_from(&client, proxy, &[("forwarded", "for=198.51.100.2")])
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);
}

/// Past its failed-auth budget an address is refused on authenticated routes
/// before its bearer is verified — a good bearer included — while other
/// addresses and the unauthenticated routes are untouched.
#[tokio::test]
async fn failed_auth_is_refused_before_the_verifier_runs() {
    let (client, _) = client_with(&[], tight());
    let a = peer("198.51.100.1");
    let me = |from, bearer| {
        let client = &client;
        async move {
            client
                .send_from(from, Method::GET, "/api/v1/accounts/me", bearer, None, &[])
                .await
        }
    };

    for _ in 0..2 {
        me(a, Some("Bearer forged"))
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }
    let refused = me(a, Some(BEARER)).await;
    refused.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(code_of(&refused), codes::RATE_LIMITED);
    assert!(retry_after(&refused) > 0);

    assert_ne!(
        me(peer("198.51.100.2"), Some(BEARER)).await.status,
        StatusCode::TOO_MANY_REQUESTS,
        "another address is unaffected"
    );
    meta_from(&client, a, &[])
        .await
        .assert_status(StatusCode::OK);
    assert_eq!(
        client.metrics.get_with(
            "sunrise_ratelimit_rejected_total",
            &[("endpoint", "/api/v1/accounts/me"), ("scope", "ip")]
        ),
        1
    );
}

/// `enabled = false` turns every limit off.
#[tokio::test]
async fn disabled_limits_refuse_nothing() {
    let (client, _) = client_with(
        &[],
        LimitsConfig {
            enabled: false,
            ..tight()
        },
    );
    assert_eq!(admitted_from(&client, peer("198.51.100.1"), &[]).await, 10);
}

/// **The `429` is in the description of every operation**, with the
/// `Retry-After` it promises — so a generated client knows to expect it
/// everywhere the limiter can send it, which is everywhere.
#[test]
fn every_operation_describes_the_refusal() {
    let doc = crate::api::document().expect("the router must describe");
    let v: serde_json::Value =
        serde_json::from_str(&doc.to_json().expect("serializes")).expect("valid JSON");
    for (path, item) in v["paths"].as_object().expect("paths") {
        for (method, op) in item.as_object().expect("a path item") {
            let refusal = &op["responses"]["429"];
            assert!(refusal.is_object(), "{method} {path} must describe 429");
            assert_eq!(
                refusal["headers"]["Retry-After"]["required"],
                serde_json::json!(true),
                "{method} {path} must describe Retry-After"
            );
        }
    }
}
