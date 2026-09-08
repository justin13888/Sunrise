//! The whole recovery path, over HTTP, against a relay that demands a step-up.
//!
//! Three pieces landed for issue #56 item 8 — the BIP-39 codec, a
//! `seal_recovery_blob` caller on the CLI path, and
//! `GET /api/v1/accounts/me/recovery_blob` behind an OIDC step-up — and each
//! has unit coverage in its own crate. What none of them can assert alone is
//! the property the user actually depends on: **twenty-four words written down
//! on the day an account is created still open that account's identity after
//! every device is gone.** That crosses a vault, a CBOR seal, a base64url
//! transport encoding, two HTTP routes and a mnemonic a human retypes, and a
//! mistake in any one of them is invisible to the crate on either side.
//!
//! So this drives the real thing: the real `Core` produces the keys, the real
//! `sunrise_relay_client::bootstrap` uploads through the generated client, a
//! real `sunrise-server` serves on a loopback socket, and the code that comes
//! back is decoded from its *words* rather than from the seed those words were
//! made from.
//!
//! # Why the relay here runs a real verifier
//!
//! The rest of this harness runs the self-host `NullVerifier`, which is the one
//! configuration exempt from the step-up — so a test that used it would prove
//! the route serves blobs and nothing about the gate in front of it.
//! `StaticVerifier` lets a test state what the token claims, which is what
//! makes "an ordinary bearer is refused" assertable at all: the difference
//! between the two bearers below is one `auth_time` claim.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{Clock, SystemClock};
use sunrise_e2e::{open_core_offline, spawn_relay_with};
use sunrise_server::auth::StepUp;
use sunrise_server::{ServerConfig, StaticVerifier, Subject};

const ISSUER: &str = "https://idp.example";
/// A bearer whose holder authenticated moments ago — a completed step-up.
const FRESH: &str = "alice-just-authenticated";
/// A bearer that verifies and carries no `auth_time`: an ordinary session, and
/// what a stolen one looks like.
const STALE: &str = "alice-ordinary-session";
const ROOT: [u8; 32] = [0x7c; 32];

fn verifier(now_ms: u64) -> StaticVerifier {
    let subject = Subject::new(ISSUER, "alice");
    StaticVerifier::default()
        .with_step_up(
            FRESH,
            subject.clone(),
            StepUp {
                acr: None,
                amr: vec!["pwd".into()],
                auth_time_secs: Some(now_ms / 1000),
            },
        )
        .with(STALE, subject)
}

/// Fetch the blob the way a recovering client does, through the generated
/// client rather than through a hand-rolled request.
async fn fetch_blob(base_url: &str, bearer: &str) -> Result<String, String> {
    let client = sunrise_relay_client::api::Client::new(base_url)
        .map_err(|e| e.to_string())?
        .with_credential(
            "AccountToken",
            sunrise_relay_client::api::Credential::Bearer(
                sunrise_relay_client::api::SecretString::from(bearer.to_owned()),
            ),
        );
    client
        .get_recovery_blob(None)
        .await
        .map(|r| r.into_inner().recovery_blob)
        .map_err(|e| e.to_string())
}

/// The one that matters: a vault's identity survives the loss of the vault, and
/// comes back from twenty-four words.
#[tokio::test]
async fn a_recovery_code_restores_the_identity_across_the_relay() {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let now_ms = clock.now_ms();
    let (addr, relay) = spawn_relay_with(ServerConfig::default(), move |s| {
        s.with_verifier(Arc::new(verifier(now_ms)))
    })
    .await;
    let base_url = format!("http://{addr}");

    let dir = tempfile::tempdir().expect("a vault dir");
    let core = open_core_offline(dir.path(), ROOT, addr, Arc::clone(&clock)).await;
    assert!(
        core.holds_identity_key(),
        "the founding device is the one that can seal a blob"
    );

    // Exactly what `sunrise bootstrap` does: draw a seed, seal, encode, upload.
    let seed = [0x33u8; sunrise_crypto::bip39::RECOVERY_ENTROPY_LEN];
    let blob = core.seal_recovery_blob(&seed).expect("the creator seals");
    let code = sunrise_crypto::bip39::encode_recovery_code(&seed);

    let account = sunrise_onboarding::AccountCreateRequest {
        email: "alice@example.com".into(),
        identity_signing_pub: core.identity_signing_pub(),
        identity_dh_pub: core.identity_dh_pub(),
        recovery_blob: Some(blob.clone()),
        terms_at_ms: now_ms,
    };
    let device = sunrise_relay_client::DeviceIdentity {
        device_pub_s: core.device_signing_pub(),
        device_pub_d: None,
        device_cert: None,
        vault_device_id: None,
        nickname: "e2e".into(),
        platform: "linux".into(),
        app_version: None,
    };
    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        sunrise_relay_client::bootstrap(&base_url, FRESH, account, device),
    )
    .await
    .expect("bootstrap must not hang")
    .expect("the account and the device register");
    assert!(!outcome.identity_id.is_empty());

    // Every device is now gone. All that is left is the account login and the
    // words on paper.
    let identity_id = core.identity_id();
    let identity_dh_pub = core.identity_dh_pub();
    let typed = code.reveal().to_uppercase();
    drop(core);
    drop(dir);

    let served = fetch_blob(&base_url, FRESH)
        .await
        .expect("the blob comes back");
    assert_eq!(
        sunrise_onboarding::decode_recovery_blob(&served).expect("base64url"),
        blob,
        "the relay must serve back exactly the ciphertext it was given"
    );

    let restored = sunrise_onboarding::recover_identity_from_code(
        &sunrise_onboarding::decode_recovery_blob(&served).expect("base64url"),
        &typed,
        &identity_id,
    )
    .expect("the code that was shown at creation opens the blob");

    // The restored key is the one the vault's `key_envelope` ops were sealed
    // to. A blob that round-trips but carries a mismatched `ID_D_priv` would
    // restore an identity that reads nothing, which is the failure this
    // assertion exists to catch rather than the encoding one above.
    assert_eq!(
        sunrise_crypto::keys::IdentityDhKeyPair::from_secret_bytes(restored.id_d_priv)
            .public_bytes(),
        identity_dh_pub
    );
    assert_eq!(restored.identity_id, identity_id);

    relay.abort();
}

/// And the guard that stops the test above passing vacuously: the same account,
/// the same blob, a bearer that verifies — and no blob, because the token
/// carries no fresh authentication.
///
/// Without this, a relay that had quietly stopped checking the step-up would
/// still be green above.
#[tokio::test]
async fn an_ordinary_bearer_is_refused_by_the_live_relay() {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let now_ms = clock.now_ms();
    let (addr, relay) = spawn_relay_with(ServerConfig::default(), move |s| {
        s.with_verifier(Arc::new(verifier(now_ms)))
    })
    .await;
    let base_url = format!("http://{addr}");

    let dir = tempfile::tempdir().expect("a vault dir");
    let core = open_core_offline(dir.path(), ROOT, addr, Arc::clone(&clock)).await;
    let seed = [0x34u8; sunrise_crypto::bip39::RECOVERY_ENTROPY_LEN];
    let blob = core.seal_recovery_blob(&seed).expect("the creator seals");

    sunrise_relay_client::bootstrap(
        &base_url,
        FRESH,
        sunrise_onboarding::AccountCreateRequest {
            email: "alice@example.com".into(),
            identity_signing_pub: core.identity_signing_pub(),
            identity_dh_pub: core.identity_dh_pub(),
            recovery_blob: Some(blob),
            terms_at_ms: now_ms,
        },
        sunrise_relay_client::DeviceIdentity {
            device_pub_s: core.device_signing_pub(),
            device_pub_d: None,
            device_cert: None,
            vault_device_id: None,
            nickname: "e2e".into(),
            platform: "linux".into(),
            app_version: None,
        },
    )
    .await
    .expect("the account registers");

    assert!(
        fetch_blob(&base_url, FRESH).await.is_ok(),
        "the stepped-up bearer is the control: it must succeed here"
    );
    let refused = fetch_blob(&base_url, STALE)
        .await
        .expect_err("an ordinary bearer must not receive the blob");
    assert!(
        refused.contains("403"),
        "the refusal must be the step-up's 403 — a transport failure or a 401 here \
         would mean the test is not exercising the gate: {refused}"
    );

    relay.abort();
}
