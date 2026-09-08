//! The server's bearer-token verification path: JWT header parse, JWKS
//! resolution, algorithm pinning, and the claim checks.
//!
//! Scope from `docs/10-cross-cutting/testing.md` §Continuous fuzz targets:
//! "OAuth/PKCE state-machine transitions in `sunrise-server::auth`".
//!
//! # What this target actually drives, and why it is not the PKCE dance
//!
//! The doc's phrase names two different things and only one of them lives in
//! `sunrise-server::auth`. The PKCE/`state` exchange is a *client* concern and
//! is implemented in `crates/sunrise-auth/src/login.rs`, where `parse_redirect`
//! is private and reachable only through a real loopback listener — nothing a
//! fuzzer can drive without inventing a socket. What `sunrise-server::auth`
//! owns is the other half of the same trust decision: it takes a bearer token
//! from an unauthenticated caller, and a discovery document and a JWKS from a
//! remote issuer, and decides whether they add up to a `Subject`. All three of
//! those are attacker-influenced bytes, and that is what is fuzzed here.
//!
//! # Input encoding
//!
//! Three NUL-separated sections: `<bearer>\0<discovery JSON>\0<JWKS JSON>`.
//! Deliberately not a derived `Arbitrary` struct — a NUL-separated corpus can
//! be written and read by hand, so a seed is a file someone can edit and a
//! minimized crash is a token and a key set someone can look at. Missing
//! sections are empty.
//!
//! # What is asserted
//!
//! That verification never panics, and never *succeeds* — the seed corpus
//! carries no private key, so no token in this corpus can carry a signature
//! over a JWKS key. An `Ok` here would mean the verifier accepted a token
//! nobody signed, which is the whole failure this surface exists to prevent,
//! and it is worth an abort rather than a shrug. (`alg: none` is not
//! representable in `jsonwebtoken`'s `Algorithm`, and `is_supported_alg`
//! refuses every `HS*`, so the two classic forgeries are both covered by this
//! one assertion.)

#![no_main]

use std::sync::Arc;

use async_trait::async_trait;
use libfuzzer_sys::fuzz_target;
use sunrise_server::auth::http::{HttpFetch, HttpResponse};
use sunrise_server::{AuthError, Clock, OidcConfig, OidcVerifier, TokenVerifier};

const ISSUER: &str = "https://issuer.example";
const AUDIENCE: &str = "sunrise-fuzz";

/// A clock that does not move. Wall time would make a crash irreproducible:
/// the same token is expired on one run and live on the next, and the artifact
/// libFuzzer writes would replay green.
#[derive(Debug)]
struct FrozenClock;

impl Clock for FrozenClock {
    fn now_ms(&self) -> u64 {
        // 2024-01-01T00:00:00Z. Any fixed instant works; a round one makes an
        // `exp`/`nbf` in a minimized artifact readable.
        1_704_067_200_000
    }
}

/// Serves the fuzzer's two documents, keyed on which URL is asked for.
#[derive(Debug)]
struct ScriptedFetch {
    discovery: Vec<u8>,
    jwks: Vec<u8>,
}

#[async_trait]
impl HttpFetch for ScriptedFetch {
    async fn get(&self, url: &str) -> Result<HttpResponse, AuthError> {
        let body = if url.contains("openid-configuration") {
            self.discovery.clone()
        } else {
            self.jwks.clone()
        };
        Ok(HttpResponse::ok(body))
    }
}

fn split3(data: &[u8]) -> (&[u8], &[u8], &[u8]) {
    let mut parts = data.splitn(3, |b| *b == 0);
    let a = parts.next().unwrap_or_default();
    let b = parts.next().unwrap_or_default();
    let c = parts.next().unwrap_or_default();
    (a, b, c)
}

fuzz_target!(|data: &[u8]| {
    let (bearer, discovery, jwks) = split3(data);
    let Ok(bearer) = std::str::from_utf8(bearer) else {
        return;
    };

    let verifier = OidcVerifier::new(
        OidcConfig::new(ISSUER, AUDIENCE),
        Arc::new(ScriptedFetch {
            discovery: discovery.to_vec(),
            jwks: jwks.to_vec(),
        }),
        Arc::new(FrozenClock),
    );

    let outcome = futures_executor::block_on(verifier.verify(bearer));
    assert!(
        outcome.is_err(),
        "the verifier accepted a token no key in this corpus could have signed: {outcome:?}"
    );
});
