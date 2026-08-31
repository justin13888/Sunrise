//! `OidcVerifier` against an issuer this test controls end to end.
//!
//! Every token here is *minted locally*: the test owns the private keys, so it
//! can produce a token that is correct in every respect but one and assert that
//! the one defect is caught. A verifier tested only against tokens it accepts
//! is a verifier that has not been tested at all — the interesting cases are
//! the ones an attacker would construct.
//!
//! The issuer is a [`FakeIdp`] implementing [`HttpFetch`], which also counts
//! how many times its JWKS was fetched. That counter is the only way to observe
//! the caching contract in `docs/06-server/auth.md` ("cached per the discovery
//! doc's `Cache-Control`") as behaviour rather than as an implementation
//! detail.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use parking_lot::Mutex;
use sunrise_server::auth::http::{HttpFetch, HttpResponse};
use sunrise_server::auth::oidc::{OidcConfig, OidcVerifier};
use sunrise_server::auth::{AuthError, TokenVerifier};
use sunrise_server::state::Clock;

// ---------------------------------------------------------------------------
// Test keys
// ---------------------------------------------------------------------------

/// A 2048-bit RSA key, PKCS#1 DER, base64. Generated once for these tests and
/// used for nothing else; it signs the tokens the verifier is asked to accept.
const KEY_A_DER_B64: &str = "MIIEpAIBAAKCAQEAqNW2H9CspIxg8vnjg8mNXNJhG+7QZuD4lC2t0x2ZMUyz9jBPPn/+Z75TZEyg+ngNBlUB5g0t0Uu/AcD+yDzfCoHjIoXVosqzsKN6puXMUNdpSQPwm8QJ9OerZbv4sSQa5ybapO97ReoJzdmXNneYVnW7kG92YQpmQtruDMKtTSs3H/tPfGknZKu9pA7RKroFm7Irqjc6BpwT8hyZrPNNC7+8Llqqge27RBS9umqxghYAN6Rs1A9B4MVHNfazWtvtu+qrWY37mrMIYboaK9bJTxed2ZMRgwd62A2hzd7+B1QE43FoMIOm1M5kadzzL1o4MlWYmiasqh97Z9Mv1mDpLQIDAQABAoIBACoT9UuRmO17tQ/peqwWN/6Zyi0JhHQXfqyDg+55UnxIdxOU77MOeEvH0gXN2VMDR4+78PiycShX/fdEb9tc3GPEgmTQwTFM3qLX45Ij9JtzTGCvtDBGGAsrD/sPcYhIjNHuS5DOxMTkcuQUZkzjWpq1xfTV6sV9r4XbBXg1rrZr9rSB1gB92NY0nfZS3qrv6ZVkcHOfbAiAuspYoEFaAP6+o5uSyjpYX8puYh5WPo0Rm4ejZarW8rroD6BHvSy+zIIbcGtsQtQCzEBzZO/my4Gby3B033brPrpk/lyTrnvkBcHaWO7aardmO3iWfGyzin/df5EV4QDbGsR4KlLK0XsCgYEA0K6S5pp5esUsuh+/RVgcIabvvkPYV28rly9eFEpgUDOTeHG60dpwoXlHuVlUNXmoNpVMIjwq7F6z7nedYFLAQX3RlZcKLKj9o1LEmHUK5C+dpEI8UKoLIxSYnvA4PGb3TDTiM2k59CAQeGkKwgCczxbYLl4W8q30YuFy0plBlg8CgYEAzx4ehowIfuU0M9CodgQurh8PtwI0WmhIOdGel8ku6n6zIL4peAq2/OzNJkj1/thlQIEoZZQ8/S70me5ry8l0VzFdP2/z8GsIn644lxh8DHzsgcXKKDljkqFZNeekd5OkqNqRg3SRBrXC7nl9lF5BCirzn8f6WMKDK4cTLyrwaQMCgYEAkwedCwslskGAXPcHTbVhxLgYzKaCpD/4p4HBOGya5YchTUhcR4UvvCV2SnpM4YyA30xbovdfisDC566xXG+Rc9NROqN7kLHUWyFy0LQOY23FFTlxw6e7RxE44yr/hFdLwA62nWBza7S3xg7EfKHv2d0PncO/SWcU/CI6Q3WlhzMCgYBxdjCGyKPG0E0+rWn77OKdpIp5WQ3RERuwAPN+d0nqUCpVH5ecGVKRUDA6bvHEAEvHgHne28xlbpm00fXfl6bSNUq9+9iItjntMAX0UAd01+LAXNgYHQg9RYKXkyR4FTu4/LOGbg8cu+njtk5jPxcmOM1plKXChhxRdhe+WSmGfQKBgQCcAmWzfCgLu303AviCsgclVPotQeYRHWeuk8XJWQKfMmBETsencrq54kWtnCaNndn4lbOjL/bbm32CUUgfDgmOI5+hKyto2Mas56Mq5C8aiioBXgWPlrYghriy6kYKng6Xhd1x0obM+h2T2Mep6z5b37kVPo2RT5PzJ675rJ2mbQ==";
const KEY_A_N: &str = "qNW2H9CspIxg8vnjg8mNXNJhG-7QZuD4lC2t0x2ZMUyz9jBPPn_-Z75TZEyg-ngNBlUB5g0t0Uu_AcD-yDzfCoHjIoXVosqzsKN6puXMUNdpSQPwm8QJ9OerZbv4sSQa5ybapO97ReoJzdmXNneYVnW7kG92YQpmQtruDMKtTSs3H_tPfGknZKu9pA7RKroFm7Irqjc6BpwT8hyZrPNNC7-8Llqqge27RBS9umqxghYAN6Rs1A9B4MVHNfazWtvtu-qrWY37mrMIYboaK9bJTxed2ZMRgwd62A2hzd7-B1QE43FoMIOm1M5kadzzL1o4MlWYmiasqh97Z9Mv1mDpLQ";

/// A second, unrelated RSA key. Used to sign tokens the issuer never signed.
const KEY_B_DER_B64: &str = "MIIEowIBAAKCAQEAoOTrVjjffdsOpLdY9H9EzzRblKKxxA1+YN3PoylYBw4LMCAj0wsvVZI921FV7xdz3k4Ds6AiLY3PY2qJjCmzSbUhQ+bgObR5X0NFCPJ/WisfYPryjwGjIUaXk5qYzoqmx5pfcx+ZdHUvplBi4X1daHOTD8sEW0AYmRMkebL03va6mZNFmsUJyVVxuqMrzSK8ZL/77STlOUE87WwX+rCr1Rl4MOqC4Lp5d8PbcEIBsiCzHihrUsLx/kD1f4kxH3XwuLr2aCxkFxYZ62qaFSjaH9iiQRf+DKr9s2nGdlMGJDpWubf+SbXDJuwpTVk8EyYD77ZfTY2r55x4BSiX/t99UwIDAQABAoIBAAQHp9NZilODLJs4kmxRUb5k39RZvN0dv2gatiwuiWtn0STr8SnEknNwvcbkAySBcGAFkTcrECAW+LZTQU2276wtcr9aJYycdhvKOgzu0fzGrrsFnhSx5E2dkIdcbG5j76h5N+HQzU2q774ZLljahH/swSa4nYvRj6wp3BSGRHbfKfsxA9E4k+qCwt1pIdWOP1o9zgvXkwAgMhVOyNWzgIRvxC7+7MOpQcd1O5n7ounVD0xM5DR8HNfe6eRTDvKtjogfPvws03Q7a2h0wEWYIt95JxVBu2ErJWbff5c+aAB3ZQlYif4hAJTmtFuZ7XD/pH0MY+Cvx6LXfu49JxrwIVUCgYEA2/0cZ3lIHrfA9x2pAx2pbjMFdaoh7z2xUTHBeLUhN3UuP2qtCCRSYsvPWWGUVs7y4nCSSVkpJdV12UO0PiamLcwaD1fszAMHqF3DvGx+xE0ViuFgNzRQ6eSJG4SlVVeb2M7VmVQAs5FR8blXGIx6Ltx47SOdX1vhBF2bKW6+0LUCgYEAuztiC93l2sHv90W4iqx/euCHj3CwBHSNeI9RZMp1nCcbZKKCcuy/0pBZZs8+4LJeh95y4tUiwLMB4zHvuxprSzN3etfSe8Nyi3YT67gKUIXzRcjcP7nH29E7hGKrFD2L7sqXA+39AFDcEVJOjM6hnooExJLzIkNZPFT2GhlowucCgYAdF7UY7g4emdh4FcETO2n7u92d+Pjx5au8fCME7pdM+T87fcUSTZNjo2ZxgJkYfdfbIF4IOzVY3ojuSajdi0jwx4wuuUcEl+X8WyIWmhaNqVAPBM0vn8iPlfyX2gvvZF2k532SAGzzUmWO3R7qjTFfXyLS4aHfSYxRgnuRmCa9/QKBgGw4Dpu9TjX7ErBh5CCDQ8vKK5CFGbf5hivA6tLPEtuG3xZzt+KlZNpYBNSfxUAq2Oi/crgZaVToIpcnLeF/i7STsuOWC1rtxS9GuIzue0e/pLUZO/S5dQNhFH2YajnwuQj0oATtcebU1d5NLInGhTQVolvcdBvBwbpVgUnkleDRAoGBAM5oCq4rYtw1beaCiTlTw/gTbw26Pd2JyaAhag4NhgcHEjcFPHSfY/Z35kQc7OAYSavwe8ixTBsPf/hn0i9XfIYn7AkSPGfZpWAVQZ5phJ2BPqIRODcHmRXHBqnGC+3Iv1pP0zVFAk4yy7di3/AjVKdTuQLBAUunyCFJs5vJtJw6";

const ISSUER: &str = "https://idp.example";
const CLIENT_ID: &str = "sunrise-relay";
/// 2024-01-01T00:00:00Z.
const T0_MS: u64 = 1_704_067_200_000;
const T0_SECS: i64 = 1_704_067_200;

fn der(b64: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .unwrap()
}

fn b64u(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Clock the test drives by hand.
#[derive(Debug)]
struct TestClock(AtomicU64);

impl TestClock {
    fn new(ms: u64) -> Arc<Self> {
        Arc::new(Self(AtomicU64::new(ms)))
    }
    fn advance_secs(&self, secs: u64) {
        self.0.fetch_add(secs * 1000, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug)]
struct IdpState {
    jwks: String,
    discovery_cache_control: Option<String>,
    jwks_fetches: usize,
}

/// An OIDC issuer served entirely out of memory.
#[derive(Debug)]
struct FakeIdp {
    issuer: String,
    state: Mutex<IdpState>,
}

impl FakeIdp {
    fn new(jwks: String) -> Arc<Self> {
        Arc::new(Self {
            issuer: ISSUER.to_string(),
            state: Mutex::new(IdpState {
                jwks,
                discovery_cache_control: None,
                jwks_fetches: 0,
            }),
        })
    }

    fn with_cache_control(self: Arc<Self>, v: &str) -> Arc<Self> {
        self.state.lock().discovery_cache_control = Some(v.to_string());
        self
    }

    fn jwks_fetches(&self) -> usize {
        self.state.lock().jwks_fetches
    }

    fn rotate_to(&self, jwks: String) {
        self.state.lock().jwks = jwks;
    }
}

#[async_trait]
impl HttpFetch for FakeIdp {
    async fn get(&self, url: &str) -> Result<HttpResponse, AuthError> {
        let s = self.state.lock();
        if url.ends_with("/.well-known/openid-configuration") {
            let body = serde_json::json!({
                "issuer": self.issuer,
                "jwks_uri": format!("{}/jwks", self.issuer),
            })
            .to_string();
            let mut res = HttpResponse::ok(body);
            res.cache_control.clone_from(&s.discovery_cache_control);
            return Ok(res);
        }
        if url == format!("{}/jwks", self.issuer) {
            drop(s);
            let mut s = self.state.lock();
            s.jwks_fetches += 1;
            return Ok(HttpResponse::ok(s.jwks.clone()));
        }
        Err(AuthError::Transport(format!("no route to {url}")))
    }
}

/// A JWKS advertising one RSA key under `kid`.
fn rsa_jwks(kid: &str, n: &str) -> String {
    serde_json::json!({
        "keys": [{ "kty": "RSA", "use": "sig", "alg": "RS256", "kid": kid, "n": n, "e": "AQAB" }]
    })
    .to_string()
}

fn verifier(idp: Arc<FakeIdp>, clock: Arc<TestClock>) -> OidcVerifier {
    OidcVerifier::new(OidcConfig::new(ISSUER, CLIENT_ID), idp, clock)
}

/// Claims a well-behaved IdP would issue.
fn good_claims() -> serde_json::Value {
    serde_json::json!({
        "iss": ISSUER,
        "sub": "user-123",
        "aud": CLIENT_ID,
        "exp": T0_SECS + 3600,
        "iat": T0_SECS,
        "email": "user@example.com",
    })
}

fn mint_rs256(key_der_b64: &str, kid: &str, claims: &serde_json::Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(kid.to_string());
    encode(
        &header,
        claims,
        &EncodingKey::from_rsa_der(&der(key_der_b64)),
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// Accepting a good token
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_valid_token_is_accepted_and_yields_the_iss_sub_pair() {
    let idp = FakeIdp::new(rsa_jwks("k1", KEY_A_N));
    let v = verifier(idp, TestClock::new(T0_MS));
    let verified = v
        .verify(&mint_rs256(KEY_A_DER_B64, "k1", &good_claims()))
        .await
        .expect("a correctly signed, in-date, correctly audienced token must verify");
    assert_eq!(verified.subject.issuer, ISSUER);
    assert_eq!(verified.subject.subject, "user-123");
    assert_eq!(verified.subject.email.as_deref(), Some("user@example.com"));
}

/// The deadline the whole of mid-session expiry rests on. `exp` used to be
/// checked and thrown away, so a session had no way to know when its
/// credential died; it now comes back with the subject, in wall-clock ms.
#[tokio::test]
async fn verification_surfaces_the_tokens_expiry() {
    let idp = FakeIdp::new(rsa_jwks("k1", KEY_A_N));
    let v = verifier(idp, TestClock::new(T0_MS));
    let verified = v
        .verify(&mint_rs256(KEY_A_DER_B64, "k1", &good_claims()))
        .await
        .unwrap();
    assert_eq!(
        verified.expires_at_ms,
        Some((T0_SECS as u64 + 3600) * 1000),
        "the deadline is the token's own exp, in milliseconds"
    );
    assert!(!verified.is_expired_at(T0_MS));
    assert!(verified.is_expired_at(T0_MS + 3_600_000));
}

/// The account key is `(iss, sub)`, so the verifier must surface both — a
/// subject that dropped the issuer would collapse two IdPs' users together.
#[tokio::test]
async fn the_device_id_claim_is_surfaced_when_present() {
    let idp = FakeIdp::new(rsa_jwks("k1", KEY_A_N));
    let v = verifier(idp, TestClock::new(T0_MS));
    let mut claims = good_claims();
    claims["https://sunrise.app/device_id"] = serde_json::json!("DEV123");
    let verified = v
        .verify(&mint_rs256(KEY_A_DER_B64, "k1", &claims))
        .await
        .unwrap();
    assert_eq!(verified.subject.device_id.as_deref(), Some("DEV123"));
}

/// `aud` may be an array; the server's client id only has to be a member.
#[tokio::test]
async fn an_audience_array_containing_the_client_id_is_accepted() {
    let idp = FakeIdp::new(rsa_jwks("k1", KEY_A_N));
    let v = verifier(idp, TestClock::new(T0_MS));
    let mut claims = good_claims();
    claims["aud"] = serde_json::json!(["some-other-rp", CLIENT_ID]);
    assert!(v
        .verify(&mint_rs256(KEY_A_DER_B64, "k1", &claims))
        .await
        .is_ok());
}

// ---------------------------------------------------------------------------
// Rejecting bad tokens
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_tampered_signature_is_rejected() {
    let idp = FakeIdp::new(rsa_jwks("k1", KEY_A_N));
    let v = verifier(idp, TestClock::new(T0_MS));
    let token = mint_rs256(KEY_A_DER_B64, "k1", &good_claims());
    // Flip one character of the signature segment, leaving the claims intact.
    let (msg, sig) = token.rsplit_once('.').unwrap();
    let flipped: String = sig
        .chars()
        .enumerate()
        .map(|(i, c)| if i == 0 && c != 'A' { 'A' } else { c })
        .collect();
    let tampered = format!("{msg}.{flipped}");
    assert!(matches!(
        v.verify(&tampered).await,
        Err(AuthError::Invalid(_))
    ));
}

/// The headline forgery: the attacker signs a *perfectly formed* token with a
/// key of their own and publishes it under the issuer's `kid`.
#[tokio::test]
async fn a_token_signed_by_another_key_is_rejected() {
    let idp = FakeIdp::new(rsa_jwks("k1", KEY_A_N));
    let v = verifier(idp, TestClock::new(T0_MS));
    let forged = mint_rs256(KEY_B_DER_B64, "k1", &good_claims());
    assert!(matches!(
        v.verify(&forged).await,
        Err(AuthError::Invalid(_))
    ));
}

/// A token the IdP legitimately issued to a *different* relying party must not
/// authenticate here. Without the `aud` check, any app sharing the IdP could
/// replay its users' tokens against Sunrise.
#[tokio::test]
async fn a_token_for_another_audience_is_rejected() {
    let idp = FakeIdp::new(rsa_jwks("k1", KEY_A_N));
    let v = verifier(idp, TestClock::new(T0_MS));
    let mut claims = good_claims();
    claims["aud"] = serde_json::json!("some-other-app");
    assert!(matches!(
        v.verify(&mint_rs256(KEY_A_DER_B64, "k1", &claims)).await,
        Err(AuthError::Invalid(_))
    ));
    // …and an absent `aud` is not a pass either.
    let mut claims = good_claims();
    claims.as_object_mut().unwrap().remove("aud");
    assert!(matches!(
        v.verify(&mint_rs256(KEY_A_DER_B64, "k1", &claims)).await,
        Err(AuthError::Invalid(_))
    ));
}

/// The signature is valid — the token was minted by this very key — but it
/// claims a different issuer. Accepting it would let one federated issuer mint
/// principals inside another's namespace.
#[tokio::test]
async fn a_token_claiming_another_issuer_is_rejected() {
    let idp = FakeIdp::new(rsa_jwks("k1", KEY_A_N));
    let v = verifier(idp, TestClock::new(T0_MS));
    let mut claims = good_claims();
    claims["iss"] = serde_json::json!("https://evil.example");
    assert!(matches!(
        v.verify(&mint_rs256(KEY_A_DER_B64, "k1", &claims)).await,
        Err(AuthError::Invalid(_))
    ));
}

#[tokio::test]
async fn an_expired_token_is_rejected_as_expired() {
    let idp = FakeIdp::new(rsa_jwks("k1", KEY_A_N));
    let clock = TestClock::new(T0_MS);
    let v = verifier(idp, clock.clone());
    let token = mint_rs256(KEY_A_DER_B64, "k1", &good_claims());
    assert!(v.verify(&token).await.is_ok());

    // Past `exp` plus the leeway. The distinct `Expired` variant is what lets
    // the client tell "refresh and retry" from "your token is not valid here".
    clock.advance_secs(3600 + 120);
    assert!(matches!(v.verify(&token).await, Err(AuthError::Expired)));
}

#[tokio::test]
async fn a_token_with_no_exp_is_rejected() {
    let idp = FakeIdp::new(rsa_jwks("k1", KEY_A_N));
    let v = verifier(idp, TestClock::new(T0_MS));
    let mut claims = good_claims();
    claims.as_object_mut().unwrap().remove("exp");
    assert!(
        matches!(
            v.verify(&mint_rs256(KEY_A_DER_B64, "k1", &claims)).await,
            Err(AuthError::Invalid(_))
        ),
        "a token with no expiry is a permanent credential"
    );
}

#[tokio::test]
async fn a_not_yet_valid_token_is_rejected() {
    let idp = FakeIdp::new(rsa_jwks("k1", KEY_A_N));
    let clock = TestClock::new(T0_MS);
    let v = verifier(idp, clock.clone());
    let mut claims = good_claims();
    claims["nbf"] = serde_json::json!(T0_SECS + 3600);
    claims["exp"] = serde_json::json!(T0_SECS + 7200);
    let token = mint_rs256(KEY_A_DER_B64, "k1", &claims);
    assert!(matches!(v.verify(&token).await, Err(AuthError::Invalid(_))));

    // Once `nbf` passes, the same token verifies.
    clock.advance_secs(3600);
    assert!(v.verify(&token).await.is_ok());
}

#[tokio::test]
async fn a_token_naming_an_unknown_kid_is_rejected() {
    let idp = FakeIdp::new(rsa_jwks("k1", KEY_A_N));
    let v = verifier(idp.clone(), TestClock::new(T0_MS));
    let token = mint_rs256(KEY_A_DER_B64, "not-a-published-kid", &good_claims());
    assert!(matches!(v.verify(&token).await, Err(AuthError::Invalid(_))));
}

/// An unknown `kid` must not become a way to make the server issue one outbound
/// JWKS request per forged token.
#[tokio::test]
async fn repeated_unknown_kids_do_not_refetch_the_jwks_each_time() {
    let idp = FakeIdp::new(rsa_jwks("k1", KEY_A_N));
    let v = verifier(idp.clone(), TestClock::new(T0_MS));
    let token = mint_rs256(KEY_A_DER_B64, "nope", &good_claims());
    for _ in 0..10 {
        assert!(v.verify(&token).await.is_err());
    }
    assert!(
        idp.jwks_fetches() <= 2,
        "ten forged tokens caused {} JWKS fetches",
        idp.jwks_fetches()
    );
}

#[tokio::test]
async fn an_empty_bearer_is_rejected() {
    let idp = FakeIdp::new(rsa_jwks("k1", KEY_A_N));
    let v = verifier(idp, TestClock::new(T0_MS));
    assert!(matches!(v.verify("").await, Err(AuthError::Missing)));
    assert!(matches!(
        v.verify("not.a.jwt").await,
        Err(AuthError::Invalid(_))
    ));
}

/// The classic algorithm-confusion attack: the JWKS is public, so an attacker
/// takes the RSA modulus and HMACs a token with it. `alg: HS256` must never
/// even reach a key lookup.
#[tokio::test]
async fn an_hs256_token_is_refused_outright() {
    let idp = FakeIdp::new(rsa_jwks("k1", KEY_A_N));
    let v = verifier(idp.clone(), TestClock::new(T0_MS));
    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some("k1".into());
    let token = encode(
        &header,
        &good_claims(),
        &EncodingKey::from_secret(KEY_A_N.as_bytes()),
    )
    .unwrap();
    assert!(matches!(v.verify(&token).await, Err(AuthError::Invalid(_))));
    assert_eq!(
        idp.jwks_fetches(),
        0,
        "a symmetric token must be refused before any key material is consulted"
    );
}

/// A discovery document that redirects to somebody else's keys is a full
/// server compromise if followed.
#[tokio::test]
async fn a_discovery_document_naming_a_different_issuer_is_refused() {
    #[derive(Debug)]
    struct WrongIssuerIdp;
    #[async_trait]
    impl HttpFetch for WrongIssuerIdp {
        async fn get(&self, _url: &str) -> Result<HttpResponse, AuthError> {
            Ok(HttpResponse::ok(
                serde_json::json!({
                    "issuer": "https://evil.example",
                    "jwks_uri": "https://evil.example/jwks",
                })
                .to_string(),
            ))
        }
    }
    let v = OidcVerifier::new(
        OidcConfig::new(ISSUER, CLIENT_ID),
        Arc::new(WrongIssuerIdp),
        TestClock::new(T0_MS),
    );
    let err = v
        .verify(&mint_rs256(KEY_A_DER_B64, "k1", &good_claims()))
        .await
        .expect_err("metadata for another issuer must not be trusted");
    assert!(matches!(err, AuthError::Transport(_)));
}

// ---------------------------------------------------------------------------
// Caching
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_second_verify_reuses_the_cached_jwks() {
    let idp = FakeIdp::new(rsa_jwks("k1", KEY_A_N)).with_cache_control("public, max-age=600");
    let clock = TestClock::new(T0_MS);
    let v = verifier(idp.clone(), clock.clone());
    let token = mint_rs256(KEY_A_DER_B64, "k1", &good_claims());

    assert!(v.verify(&token).await.is_ok());
    assert_eq!(idp.jwks_fetches(), 1);

    // Several more verifies, well inside the advertised lifetime.
    clock.advance_secs(300);
    for _ in 0..5 {
        assert!(v.verify(&token).await.is_ok());
    }
    assert_eq!(
        idp.jwks_fetches(),
        1,
        "the JWKS must not be refetched inside its max-age"
    );
}

#[tokio::test]
async fn the_cache_expires_and_refetches_after_max_age() {
    let idp = FakeIdp::new(rsa_jwks("k1", KEY_A_N)).with_cache_control("max-age=60");
    let clock = TestClock::new(T0_MS);
    let v = verifier(idp.clone(), clock.clone());
    let token = mint_rs256(KEY_A_DER_B64, "k1", &good_claims());

    assert!(v.verify(&token).await.is_ok());
    assert_eq!(idp.jwks_fetches(), 1);

    clock.advance_secs(61);
    assert!(v.verify(&token).await.is_ok());
    assert_eq!(
        idp.jwks_fetches(),
        2,
        "past max-age the key set must be fetched again"
    );
}

/// The operational reason the cache expires at all: an issuer that rotates its
/// signing key must become usable again without a restart, and the key it
/// retired must stop working.
#[tokio::test]
async fn a_rotated_signing_key_is_picked_up_after_the_cache_lapses() {
    let idp = FakeIdp::new(rsa_jwks("k1", KEY_A_N)).with_cache_control("max-age=60");
    let clock = TestClock::new(T0_MS);
    let v = verifier(idp.clone(), clock.clone());

    let old = mint_rs256(KEY_A_DER_B64, "k1", &good_claims());
    assert!(v.verify(&old).await.is_ok());

    // The IdP retires k1 and publishes k2, backed by a different key pair.
    let key_b_n = "oOTrVjjffdsOpLdY9H9EzzRblKKxxA1-YN3PoylYBw4LMCAj0wsvVZI921FV7xdz3k4Ds6AiLY3PY2qJjCmzSbUhQ-bgObR5X0NFCPJ_WisfYPryjwGjIUaXk5qYzoqmx5pfcx-ZdHUvplBi4X1daHOTD8sEW0AYmRMkebL03va6mZNFmsUJyVVxuqMrzSK8ZL_77STlOUE87WwX-rCr1Rl4MOqC4Lp5d8PbcEIBsiCzHihrUsLx_kD1f4kxH3XwuLr2aCxkFxYZ62qaFSjaH9iiQRf-DKr9s2nGdlMGJDpWubf-SbXDJuwpTVk8EyYD77ZfTY2r55x4BSiX_t99Uw";
    idp.rotate_to(rsa_jwks("k2", key_b_n));
    clock.advance_secs(61);

    let new = mint_rs256(KEY_B_DER_B64, "k2", &good_claims());
    assert!(
        v.verify(&new).await.is_ok(),
        "a token signed by the issuer's current key must verify"
    );
    assert!(
        v.verify(&old).await.is_err(),
        "a token signed by the retired key must stop verifying"
    );
}

/// `no-store` on the discovery document means exactly that.
#[tokio::test]
async fn a_no_store_issuer_is_refetched_every_time() {
    let idp = FakeIdp::new(rsa_jwks("k1", KEY_A_N)).with_cache_control("no-store");
    let clock = TestClock::new(T0_MS);
    let v = verifier(idp.clone(), clock);
    let token = mint_rs256(KEY_A_DER_B64, "k1", &good_claims());
    assert!(v.verify(&token).await.is_ok());
    assert!(v.verify(&token).await.is_ok());
    assert_eq!(idp.jwks_fetches(), 2);
}

// ---------------------------------------------------------------------------
// A second key family, to prove the JWK path is not RSA-only
// ---------------------------------------------------------------------------

/// Wrap a raw Ed25519 seed in the PKCS#8 v2 envelope (RFC 8410) that the JWT
/// signer expects. Fixed-shape, so the whole thing is a template.
fn ed25519_pkcs8(seed: &[u8; 32], public: &[u8; 32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(85);
    out.extend_from_slice(&[
        0x30, 0x53, // SEQUENCE (83 bytes)
        0x02, 0x01, 0x01, // INTEGER version = 1 (v2, public key present)
        0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, // AlgorithmIdentifier: id-Ed25519
        0x04, 0x22, 0x04, 0x20, // OCTET STRING { OCTET STRING (32) }
    ]);
    out.extend_from_slice(seed);
    out.extend_from_slice(&[0xa1, 0x23, 0x03, 0x21, 0x00]); // [1] BIT STRING (33)
    out.extend_from_slice(public);
    out
}

#[tokio::test]
async fn an_eddsa_issuer_verifies_through_the_same_path() {
    let sk = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
    let pk = sk.verifying_key();
    let jwks = serde_json::json!({
        "keys": [{
            "kty": "OKP", "crv": "Ed25519", "use": "sig", "alg": "EdDSA",
            "kid": "ed1", "x": b64u(pk.as_bytes()),
        }]
    })
    .to_string();
    let idp = FakeIdp::new(jwks);
    let v = verifier(idp, TestClock::new(T0_MS));

    let mut header = Header::new(Algorithm::EdDSA);
    header.kid = Some("ed1".into());
    let pkcs8 = ed25519_pkcs8(&sk.to_bytes(), pk.as_bytes());
    let token = encode(&header, &good_claims(), &EncodingKey::from_ed_der(&pkcs8)).unwrap();

    let verified = v.verify(&token).await.expect("an Ed25519 JWKS must work");
    assert_eq!(verified.subject.subject, "user-123");

    // And a token from a different Ed25519 key still fails.
    let other = ed25519_dalek::SigningKey::from_bytes(&[43u8; 32]);
    let forged = encode(
        &header,
        &good_claims(),
        &EncodingKey::from_ed_der(&ed25519_pkcs8(
            &other.to_bytes(),
            other.verifying_key().as_bytes(),
        )),
    )
    .unwrap();
    assert!(v.verify(&forged).await.is_err());
}
