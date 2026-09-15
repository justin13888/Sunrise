//! A vault created by the Apple path has a recovery blob, where it previously
//! had none.
//!
//! This is the assertion #181 is about. `Core::seal_recovery_blob` and
//! `Core::holds_identity_key` existed and were not in `sunrise-core-bindings`,
//! and `apps/apple` reached `POST /api/v1/accounts` by no route at all — so a
//! vault founded on a Mac or an iPhone was the only place `ID_D_priv` would
//! ever exist. Losing that device destroyed the key permanently: every
//! `Recipient::Identity` copy in the op log becomes unopenable forever, and no
//! recovery feature shipped afterwards can retrieve it, because sealing a blob
//! needs the key it would carry.
//!
//! So this drives the seam the Swift app drives — `SunriseCore`, the same
//! handle `CoreBridge` wraps — against a real relay, and then fetches the blob
//! back out of that relay and opens it with the words the seam handed over. A
//! seam test in `sunrise-core-bindings` can assert that the call exists; only
//! this can assert that the account ends up with something to recover from.
//!
//! The relay runs the default self-host `NullVerifier`, which is the one
//! configuration exempt from the recovery step-up. That gate has its own
//! coverage against a real verifier in `recovery_blob_round_trip.rs`; what is
//! under test here is what the app path leaves behind it.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::time::Duration;

use sunrise_core_bindings::SunriseCore;
use sunrise_e2e::spawn_relay;

const BEARER: &str = "self-host";
const ROOT: [u8; 32] = [0x4d; 32];

fn client(base_url: &str) -> sunrise_relay_client::api::Client {
    sunrise_relay_client::api::Client::new(base_url)
        .expect("client")
        .with_credential(
            "AccountToken",
            sunrise_relay_client::api::Credential::Bearer(
                sunrise_relay_client::api::SecretString::from(BEARER.to_owned()),
            ),
        )
}

/// Fetch the blob the way a recovering client does.
async fn fetch_blob(base_url: &str) -> Result<String, String> {
    client(base_url)
        .get_recovery_blob(None)
        .await
        .map(|r| r.into_inner().recovery_blob)
        .map_err(|e| e.to_string())
}

/// The vault's `identity_id` — the blob's AAD — read back off the account
/// record, which is the only place a recovering client can get it.
async fn identity_id(base_url: &str) -> [u8; 16] {
    let account = client(base_url)
        .get_account(None)
        .await
        .expect("the account record")
        .into_inner();
    let key = sunrise_onboarding::decode_public_key(
        account
            .identity_signing_pub
            .as_deref()
            .expect("the app published its identity key"),
    )
    .expect("a 32-byte key");
    sunrise_crypto::identity_id_from_pub(&key)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_vault_created_through_the_app_seam_leaves_a_recovery_blob_on_the_relay() {
    let (addr, relay) = spawn_relay().await;
    let base_url = format!("http://{addr}");

    let dir = tempfile::tempdir().expect("a vault dir");
    let core = SunriseCore::open(
        dir.path().to_string_lossy().into_owned(),
        ROOT.to_vec(),
        "e2e".into(),
        None,
    )
    .await
    .expect("the app opens a vault");

    // The condition that makes this urgent, as the app can now ask it: until
    // the blob below exists, this vault is the only place `ID_D_priv` is.
    assert!(
        core.holds_identity_key(),
        "a vault the app just created founded its account and holds ID_D_priv"
    );

    // Before: nothing to recover from. This is the state every Apple-created
    // account used to stay in permanently, and asserting it is what stops the
    // assertion below passing vacuously against a relay that would have
    // answered with a blob whatever the client did.
    let before = fetch_blob(&base_url).await;
    assert!(
        before.is_err() || before.as_deref() == Ok(""),
        "the account must hold no blob before the app publishes one, or this test \
         proves nothing: {before:?}"
    );

    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        core.bootstrap_account(
            base_url.clone(),
            BEARER.to_owned(),
            "alice@example.com".to_owned(),
            "Alice's Mac".to_owned(),
        ),
    )
    .await
    .expect("bootstrap must not hang")
    .expect("the app publishes its vault");

    assert!(!outcome.device_id.is_empty());
    let code = outcome
        .recovery_code
        .expect("a founding device produces a code; only a paired one does not");

    // After: a blob, and the words the app showed open it. The second half is
    // what makes the first half worth anything — a blob the displayed code
    // does not open is a promise the user has taken and nobody can keep.
    let served = fetch_blob(&base_url)
        .await
        .expect("the relay serves it back");
    let blob = sunrise_onboarding::decode_recovery_blob(&served).expect("base64url");

    let restored =
        sunrise_onboarding::recover_identity_from_code(&blob, &code, &identity_id(&base_url).await)
            .expect("the code the app displayed opens the blob the app uploaded");

    // And the key inside is the one the vault's `key_envelope` ops are sealed
    // to, which is the whole point of sealing it: a blob carrying a mismatched
    // `ID_D_priv` would restore an identity that reads nothing.
    assert_eq!(
        sunrise_crypto::keys::IdentityDhKeyPair::from_secret_bytes(restored.id_d_priv)
            .public_bytes(),
        restored.id_d_pub,
    );

    core.shutdown().await;
    relay.abort();
}
