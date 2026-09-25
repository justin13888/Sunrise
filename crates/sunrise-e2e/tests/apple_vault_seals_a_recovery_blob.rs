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

use std::sync::Arc;
use std::time::Duration;

use sunrise_core_bindings::SunriseCore;
use sunrise_e2e::spawn_relay_with;
use sunrise_server::{ServerConfig, Store, Subject};

const BEARER: &str = "self-host";
const ROOT: [u8; 32] = [0x4d; 32];
/// A value no clock would produce, so the relay recording it proves the seam
/// forwarded the caller's acceptance rather than reading its own clock (#183).
const TERMS_ACCEPTED_AT_MS: u64 = 1_234_567;

/// The account every self-host bearer resolves to.
fn self_host_account(store: &Store) -> sunrise_server::store::Account {
    store
        .resolve_account(
            &Subject::new(sunrise_server::auth::SELF_HOST_ISSUER, "self-host"),
            true,
            0,
        )
        .expect("the self-host account")
}

/// The vault device id as the relay records it: the seam reports it as hex,
/// the relay's `vault_device_id` column holds Crockford base-32.
fn vault_device_id(core: &SunriseCore) -> String {
    let hex = core.device_id();
    let mut bytes = [0u8; 16];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex device id");
    }
    sunrise_id::crockford::encode_bytes(&bytes)
}

/// A relay, and a handle on its own store to read what it recorded.
async fn relay_with_store() -> (String, tokio::task::JoinHandle<()>, Arc<Store>) {
    let mut captured: Option<Arc<Store>> = None;
    let (addr, relay) = spawn_relay_with(ServerConfig::default(), |state| {
        captured = Some(state.store.clone());
        state
    })
    .await;
    let store = captured.expect("the harness hands back the relay's own store");
    (format!("http://{addr}"), relay, store)
}

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
    let (base_url, relay, store) = relay_with_store().await;

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
            TERMS_ACCEPTED_AT_MS,
        ),
    )
    .await
    .expect("bootstrap must not hang")
    .expect("the app publishes its vault");

    // The id handed back names a row the relay holds for *this* device — the
    // one the app now records and every later request presents (#183).
    let account = self_host_account(&store);
    let rows = store
        .list_devices(&account.account_id)
        .expect("list devices");
    let row = rows
        .iter()
        .find(|d| d.device_id == outcome.device_id)
        .expect("the returned device id names a registered row");
    assert_eq!(
        row.vault_device_id.as_deref(),
        Some(vault_device_id(&core).as_str()),
    );
    // The acceptance the caller supplied, not a reading of the core's clock.
    assert_eq!(account.terms_at_ms, Some(TERMS_ACCEPTED_AT_MS));

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

/// The route a device admitted by pairing takes (#183): register this device
/// and nothing else.
///
/// Before `register_relay_device` existed, the Apple app reached the relay
/// only through `bootstrap_account`, and never for a paired device — so such a
/// device held no relay id and a relay with `require_device_sig` refused it.
/// What is asserted is both halves: the id names a row for this device, and
/// the account record is left as it was — no identity keys, and above all no
/// terms acceptance asserted on the holder's behalf.
#[tokio::test(flavor = "multi_thread")]
async fn registering_a_device_alone_binds_it_and_publishes_nothing_else() {
    let (base_url, relay, store) = relay_with_store().await;

    let dir = tempfile::tempdir().expect("a vault dir");
    let core = SunriseCore::open(
        dir.path().to_string_lossy().into_owned(),
        ROOT.to_vec(),
        "e2e".into(),
        None,
    )
    .await
    .expect("the app opens a vault");

    let device_id = tokio::time::timeout(
        Duration::from_secs(30),
        core.register_relay_device(base_url, BEARER.to_owned(), "Alice's iPhone".to_owned()),
    )
    .await
    .expect("registration must not hang")
    .expect("the relay registers the device");

    let account = self_host_account(&store);
    let rows = store
        .list_devices(&account.account_id)
        .expect("list devices");
    let row = rows
        .iter()
        .find(|d| d.device_id == device_id)
        .expect("the returned id names a registered row");
    assert_eq!(
        row.vault_device_id.as_deref(),
        Some(vault_device_id(&core).as_str())
    );
    assert_eq!(row.nickname, "Alice's iPhone");

    assert_eq!(
        account.identity_pub_s, None,
        "registering publishes no identity"
    );
    assert_eq!(
        account.terms_at_ms, None,
        "registering asserts no terms acceptance"
    );

    core.shutdown().await;
    relay.abort();
}
