//! Pairing with nobody carrying the bytes: two devices meet at the relay's
//! rendezvous, agree a SAS, and the new one leaves holding the vault.
//!
//! Every message of the exchange — three Noise handshake messages, then the
//! offer, the cert request and the grant — travels through
//! `POST /api/v1/pairing/{send,receive}` on a live relay, driven by the same
//! `RelayPairing` handle the Apple apps hold. The test passes nothing between
//! the two sides except the QR text, which is what a camera carries.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use sunrise_core_bindings::{BindingError, RelayPairing, SunriseCore};
use sunrise_e2e::spawn_relay;

const ROOT: [u8; 32] = [0x5a; 32];
const BEARER: &str = "e2e";
const TIMEOUT: Duration = Duration::from_secs(60);
/// Long enough for a device to have polled the relay a few times.
const STILL_WAITING: Duration = Duration::from_secs(2);

fn seed(byte: u8) -> Vec<u8> {
    // Distinct, and fixed only because the test asserts nothing about them;
    // a client draws these from the platform CSPRNG.
    vec![byte; 32]
}

/// The QR with its `n_static_pub` replaced by another well-formed key: what a
/// code tampered with between the two screens looks like.
fn with_substituted_static(qr: &str) -> String {
    let mut v: serde_json::Value = serde_json::from_str(qr).expect("the QR is JSON");
    // Thirty-two `0x09` bytes, base64url without padding.
    let substitute = format!("{}CQk", "CQkJ".repeat(10));
    assert_ne!(v["n_static_pub"], substitute.as_str());
    v["n_static_pub"] = serde_json::Value::String(substitute);
    serde_json::to_string(&v).expect("re-encode")
}

#[tokio::test(flavor = "multi_thread")]
async fn two_devices_pair_over_a_live_relay_with_no_manual_transfer() {
    let (addr, relay) = spawn_relay().await;
    let base_url = format!("http://{addr}");

    let dir = tempfile::tempdir().expect("a vault dir");
    let sponsor_core = SunriseCore::open(
        dir.path().to_string_lossy().into_owned(),
        ROOT.to_vec(),
        "e2e".into(),
        None,
    )
    .await
    .expect("the existing device opens its vault");
    assert!(sponsor_core.can_sponsor_pairing());

    let joiner = RelayPairing::offer(base_url.clone(), BEARER.into(), "alice@example.com".into())
        .await
        .expect("the new device opens a session");
    let scanned = joiner.qr_payload().expect("the new device shows a code");
    let sponsor = RelayPairing::accept(scanned, base_url, BEARER.into())
        .expect("the existing device reads the code");

    let (sas_new, sas_existing) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(joiner.handshake(), sponsor.handshake())
    })
    .await
    .expect("the handshake completes");
    let sas_new = sas_new.expect("the new device reaches the SAS");
    let sas_existing = sas_existing.expect("the existing device reaches the SAS");
    assert_eq!(sas_new.len(), 6);
    assert_eq!(
        sas_new, sas_existing,
        "both screens show the same six digits"
    );

    let (bundle, sponsored) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(
            joiner.join("the new device".into(), "test".into(), seed(1), seed(2)),
            sponsor.sponsor(sponsor_core.clone())
        )
    })
    .await
    .expect("the three pairing messages cross");
    sponsored.expect("the existing device issues the grant");
    let bundle = bundle.expect("the new device opens the grant");

    assert_eq!(
        bundle.vault_root,
        ROOT.to_vec(),
        "the vault root arrived over the relay"
    );
    let payload =
        sunrise_pairing::decode_pairing_payload(&bundle.payload_bytes).expect("a pairing payload");
    assert_eq!(payload.vault_root, ROOT);

    let joined_dir = tempfile::tempdir().expect("a second vault dir");
    let joined = SunriseCore::open(
        joined_dir.path().to_string_lossy().into_owned(),
        bundle.vault_root.clone(),
        "e2e".into(),
        Some(bundle.payload_bytes.clone()),
    )
    .await
    .expect("the new device opens the vault it was handed");
    assert!(
        !joined.can_sponsor_pairing(),
        "a device added by pairing holds no identity signing key"
    );
    assert_ne!(joined.device_id(), sponsor_core.device_id());

    joined.shutdown().await;
    sponsor_core.shutdown().await;
    relay.abort();
}

/// A code whose `n_static_pub` was substituted is refused on the existing
/// device the moment the transcript completes — before any SAS exists — and
/// the new device learns the pairing is over through the relay.
#[tokio::test(flavor = "multi_thread")]
async fn a_tampered_static_key_in_the_code_is_refused_before_the_sas() {
    let (addr, relay) = spawn_relay().await;
    let base_url = format!("http://{addr}");

    let joiner = RelayPairing::offer(base_url.clone(), BEARER.into(), "alice@example.com".into())
        .await
        .expect("the new device opens a session");
    let tampered = with_substituted_static(&joiner.qr_payload().expect("a code"));
    let sponsor = RelayPairing::accept(tampered, base_url, BEARER.into())
        .expect("a well-formed code decodes; the substitution is only detectable in the handshake");

    let (_, refused) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(joiner.handshake(), sponsor.handshake())
    })
    .await
    .expect("both sides finish");
    match refused {
        Err(BindingError::Pairing(why)) => assert!(
            why.contains("not the one the QR published"),
            "refused for the wrong reason: {why}"
        ),
        other => panic!("a substituted static must be refused before the SAS, got {other:?}"),
    }

    let joined = tokio::time::timeout(
        TIMEOUT,
        joiner.join("the new device".into(), "test".into(), seed(1), seed(2)),
    )
    .await
    .expect("the new device stops waiting");
    assert!(
        matches!(joined, Err(BindingError::Pairing(_))),
        "the new device learns the session is over: {joined:?}"
    );

    relay.abort();
}

/// Both devices of a fresh session, showing the same SAS.
async fn devices_at_the_sas(base_url: String) -> (Arc<RelayPairing>, Arc<RelayPairing>) {
    let joiner = RelayPairing::offer(base_url.clone(), BEARER.into(), "alice@example.com".into())
        .await
        .expect("the new device opens a session");
    let scanned = joiner.qr_payload().expect("the new device shows a code");
    let sponsor = RelayPairing::accept(scanned, base_url, BEARER.into())
        .expect("the existing device reads the code");

    let (sas_new, sas_existing) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(joiner.handshake(), sponsor.handshake())
    })
    .await
    .expect("the handshake completes");
    assert_eq!(
        sas_new.expect("the new device reaches the SAS"),
        sas_existing.expect("the existing device reaches the SAS"),
    );
    (joiner, sponsor)
}

/// Prove `peer` is still waiting, have `rejecter` answer "don't match", and
/// require the wait to end in the relay's word that the session is gone: the
/// peer stops at a poll, well inside the session's 300 s lifetime.
async fn rejecting_ends_the_wait<T: std::fmt::Debug>(
    rejecter: &RelayPairing,
    peer: impl Future<Output = Result<T, BindingError>>,
) {
    tokio::pin!(peer);
    let early = tokio::time::timeout(STILL_WAITING, &mut peer).await;
    assert!(
        early.is_err(),
        "nothing has answered the peer yet, so it must still be waiting: {early:?}"
    );

    rejecter.reject().await;

    let ended = tokio::time::timeout(TIMEOUT, peer)
        .await
        .expect("the peer stops waiting once the other side rejects");
    match ended {
        Err(BindingError::Pairing(why)) => assert!(
            why.contains("the pairing ended"),
            "the peer ended for the wrong reason: {why}"
        ),
        other => panic!("a rejected pairing must fail on the peer, got {other:?}"),
    }
}

/// The new device says the digits differ while the existing one, having said
/// they match, waits for the cert request.
#[tokio::test(flavor = "multi_thread")]
async fn rejecting_on_the_new_device_ends_the_sponsors_wait() {
    let (addr, relay) = spawn_relay().await;
    let (joiner, sponsor) = devices_at_the_sas(format!("http://{addr}")).await;

    let dir = tempfile::tempdir().expect("a vault dir");
    let core = SunriseCore::open(
        dir.path().to_string_lossy().into_owned(),
        ROOT.to_vec(),
        "e2e".into(),
        None,
    )
    .await
    .expect("the existing device opens its vault");

    rejecting_ends_the_wait(&joiner, sponsor.sponsor(core.clone())).await;

    core.shutdown().await;
    relay.abort();
}

/// The existing device says the digits differ while the new one, having said
/// they match, waits for the offer.
#[tokio::test(flavor = "multi_thread")]
async fn rejecting_on_the_existing_device_ends_the_joiners_wait() {
    let (addr, relay) = spawn_relay().await;
    let (joiner, sponsor) = devices_at_the_sas(format!("http://{addr}")).await;

    rejecting_ends_the_wait(
        &sponsor,
        joiner.join("the new device".into(), "test".into(), seed(1), seed(2)),
    )
    .await;

    relay.abort();
}

/// A code that names a relay other than the existing device's own is refused
/// before any request carries the bearer anywhere, and so is one that names a
/// relay over plain `http://`.
#[tokio::test(flavor = "multi_thread")]
async fn a_code_naming_another_relay_never_receives_the_bearer() {
    let (addr, relay) = spawn_relay().await;
    let base_url = format!("http://{addr}");

    let joiner = RelayPairing::offer(base_url.clone(), BEARER.into(), "alice@example.com".into())
        .await
        .expect("the new device opens a session");
    let code = joiner.qr_payload().expect("a code");

    for (relay_url, why) in [
        ("https://relay.attacker.test", "another origin"),
        ("http://relay.attacker.test", "plain http"),
    ] {
        let mut v: serde_json::Value = serde_json::from_str(&code).expect("the QR is JSON");
        v["relay_url"] = serde_json::Value::String(relay_url.into());
        let redirected = serde_json::to_string(&v).expect("re-encode");
        let accepted = RelayPairing::accept(redirected, base_url.clone(), BEARER.into());
        assert!(
            matches!(accepted, Err(BindingError::Relay(_))),
            "a code naming {why} is refused: {accepted:?}"
        );
    }

    let insecure = RelayPairing::accept(code, "http://relay.example".into(), BEARER.into());
    assert!(
        matches!(insecure, Err(BindingError::Relay(_))),
        "a device configured for a non-loopback http:// relay sends no bearer: {insecure:?}"
    );

    joiner.cancel().await;
    relay.abort();
}

/// Past the account's hourly pair-attempt limit, `offer` returns the relay's
/// refusal at once rather than retrying it for the whole wait.
#[tokio::test(flavor = "multi_thread")]
async fn an_offer_over_the_pair_attempt_limit_is_refused_at_once() {
    let (addr, relay) = spawn_relay().await;
    let base_url = format!("http://{addr}");

    let mut open = Vec::new();
    for _ in 0..sunrise_pairing::RATE_LIMIT_HOURLY {
        open.push(
            RelayPairing::offer(base_url.clone(), BEARER.into(), "alice@example.com".into())
                .await
                .expect("an offer inside the limit opens a session"),
        );
    }

    let refused = tokio::time::timeout(
        Duration::from_secs(10),
        RelayPairing::offer(base_url, BEARER.into(), "alice@example.com".into()),
    )
    .await
    .expect("a refused offer returns without waiting out the cap");
    match refused {
        Err(BindingError::Relay(why)) => assert!(
            why.contains("Retry-After"),
            "the refusal carries the relay's Retry-After: {why}"
        ),
        other => panic!("an offer over the limit must be refused, got {other:?}"),
    }

    for pairing in open {
        pairing.cancel().await;
    }
    relay.abort();
}
