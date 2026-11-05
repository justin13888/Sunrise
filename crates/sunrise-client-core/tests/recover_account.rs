#![cfg(feature = "relay")]

//! The account-level recovery flow against an injected relay.
//!
//! No network: [`FakeRelay`] serves a blob a real founding vault sealed, and
//! refuses where a test needs it to. What is under test is the order of the
//! flow and what each failure leaves behind, which is what a client branches
//! on: a mistyped code never reaches the relay, a refusal before the vault
//! exists writes nothing, and every failure after it says so.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use sunrise_client_core::recovery::{
    check_recovery_code, identity_id_from_account, is_recovery_word, recover_account,
    rejoin_account, restore_identity, DeviceLabel, RecoveringDevice, RecoveryMaterial,
    RecoveryProgress, RecoveryRelay, RelayRefusal, RestoreError, RetiredCode,
};
use sunrise_core::{Core, CoreConfig, Unlock};
use sunrise_crypto::keys::VaultRootKey;

const FOUNDING_ROOT: [u8; 32] = [0x41; 32];
const RECOVERED_ROOT: [u8; 32] = [0x42; 32];
const SEED: [u8; 32] = [0x5a; 32];

/// What the relay answers, one field per route.
struct FakeRelay {
    identity_key: Option<String>,
    blob: Result<String, RelayRefusal>,
    retired: Vec<RetiredCode>,
    register: Result<String, RelayRefusal>,
    calls: AtomicUsize,
    registered: Mutex<Option<RecoveringDevice>>,
}

impl FakeRelay {
    fn serving(identity_key: String, blob: String) -> Self {
        Self {
            identity_key: Some(identity_key),
            blob: Ok(blob),
            retired: Vec::new(),
            register: Ok("01RELAYDEVICE".to_owned()),
            calls: AtomicUsize::new(0),
            registered: Mutex::new(None),
        }
    }
}

impl RecoveryRelay for FakeRelay {
    fn account_identity_key(
        &self,
    ) -> impl Future<Output = Result<Option<String>, RelayRefusal>> + Send {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let key = self.identity_key.clone();
        async move { Ok(key) }
    }

    fn recovery_blob(&self) -> impl Future<Output = Result<RecoveryMaterial, RelayRefusal>> + Send {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let served = self.blob.clone().map(|blob| RecoveryMaterial {
            blob,
            identity_chain: None,
            retired: self.retired.clone(),
        });
        async move { served }
    }

    fn register_device(
        &self,
        device: RecoveringDevice,
    ) -> impl Future<Output = Result<String, RelayRefusal>> + Send {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut g) = self.registered.lock() {
            *g = Some(device);
        }
        let answer = self.register.clone();
        async move { answer }
    }
}

/// A founding vault's published key and sealed blob, and the code that opens
/// it. The vault itself is shut and dropped: the words are all that is left.
async fn founded() -> (String, String, String, [u8; 16]) {
    let dir = tempfile::tempdir().expect("a vault dir");
    let core = Core::open(
        CoreConfig::production(dir.path().to_path_buf(), "test".to_owned()),
        Unlock::DevicePaired {
            root: VaultRootKey::from_bytes(FOUNDING_ROOT),
            paired: None,
        },
    )
    .await
    .expect("open the founding vault");
    let blob = core.seal_recovery_blob(&SEED).expect("the creator seals");
    let key = core.identity_signing_pub();
    core.shutdown().await;
    let code = sunrise_crypto::bip39::encode_recovery_code(&SEED);
    (
        sunrise_relay_client::encode_public_key(&key),
        sunrise_relay_client::encode_recovery_blob(&blob),
        code.reveal().to_owned(),
        sunrise_crypto::identity_id_from_pub(&key),
    )
}

fn label() -> DeviceLabel {
    DeviceLabel {
        nickname: "test".to_owned(),
        platform: "linux".to_owned(),
        app_version: None,
    }
}

fn cfg(dir: &std::path::Path) -> CoreConfig {
    CoreConfig::production(dir.to_path_buf(), "test".to_owned())
}

/// A sync starter that starts nothing, so the replay never catches up.
#[allow(clippy::unnecessary_wraps)] // The `SyncStarter` signature.
fn no_sync(_: &Arc<Core>, _: &str) -> Result<(), String> {
    Ok(())
}

#[tokio::test]
async fn a_mistyped_code_never_reaches_the_relay() {
    let (key, blob, code, _) = founded().await;
    let relay = FakeRelay::serving(key, blob);
    let mut words: Vec<&str> = code.split(' ').collect();
    words.pop();

    let mut seen = Vec::new();
    let err = restore_identity(&relay, &words.join(" "), &mut |p| seen.push(p))
        .await
        .expect_err("23 words are not a code");
    assert!(matches!(err, RestoreError::Code(_)), "{err:?}");
    assert!(!err.vault_written());
    assert_eq!(
        relay.calls.load(Ordering::SeqCst),
        0,
        "no round trip for a typo"
    );
    assert!(seen.is_empty());
}

#[tokio::test]
async fn a_relay_that_wants_a_step_up_is_reported_as_such_and_writes_nothing() {
    let (key, _, code, _) = founded().await;
    let mut relay = FakeRelay::serving(key, String::new());
    relay.blob = Err(RelayRefusal::StepUpRequired("403".to_owned()));

    let err = restore_identity(&relay, &code, &mut |_| {})
        .await
        .expect_err("the blob was refused");
    assert!(matches!(err, RestoreError::StepUpRequired(_)), "{err:?}");
    assert!(!err.vault_written());
}

#[tokio::test]
async fn another_accounts_code_is_refused_by_the_aead() {
    let (key, blob, _, _) = founded().await;
    let relay = FakeRelay::serving(key, blob);
    let other = sunrise_crypto::bip39::encode_recovery_code(&[0x01; 32]);

    let err = restore_identity(&relay, other.reveal(), &mut |_| {})
        .await
        .expect_err("not this account's code");
    assert!(
        matches!(
            err,
            RestoreError::Code(sunrise_client_core::recovery::RecoveryFlowError::Crypto(_))
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_malformed_blob_is_named_as_the_relays_fault() {
    let (key, _, code, _) = founded().await;
    let relay = FakeRelay::serving(key, "not base64url!!".to_owned());
    let err = restore_identity(&relay, &code, &mut |_| {})
        .await
        .expect_err("malformed");
    assert!(matches!(err, RestoreError::MalformedBlob(_)), "{err:?}");
}

#[tokio::test]
async fn the_words_restore_the_identity_and_say_so_in_order() {
    let (key, blob, code, identity_id) = founded().await;
    let relay = FakeRelay::serving(key, blob);
    let mut seen = Vec::new();
    let identity = restore_identity(&relay, &code, &mut |p| seen.push(p))
        .await
        .expect("the code opens the blob");
    assert_eq!(identity.payload.identity_id, identity_id);
    assert!(identity.identity_chain.is_none());
    assert_eq!(
        seen,
        vec![
            RecoveryProgress::BlobFetched,
            RecoveryProgress::IdentityOpened
        ]
    );
}

/// A replay that never finishes is a failure, and one that has written the
/// vault and registered the device: the caller must keep the root and record
/// the id, and both are in the error.
#[tokio::test]
async fn a_replay_that_never_catches_up_is_not_reported_as_success() {
    let (key, blob, code, identity_id) = founded().await;
    let relay = FakeRelay::serving(key, blob);
    let dir = tempfile::tempdir().expect("a vault dir");

    let mut seen = Vec::new();
    let err = recover_account(
        &relay,
        &code,
        cfg(dir.path()),
        VaultRootKey::from_bytes(RECOVERED_ROOT),
        label(),
        &no_sync,
        0,
        &mut |p| seen.push(p),
    )
    .await
    .expect_err("no sync, no catch-up");

    match &err {
        RestoreError::NeverCaughtUp {
            relay_device_id, ..
        } => assert_eq!(relay_device_id, "01RELAYDEVICE"),
        other => panic!("expected NeverCaughtUp, got {other:?}"),
    }
    assert!(err.vault_written());
    assert!(seen.contains(&RecoveryProgress::DeviceRegistered {
        relay_device_id: "01RELAYDEVICE".to_owned()
    }));
    assert!(!seen.contains(&RecoveryProgress::CaughtUp));

    // What the relay was told is this device, under the restored identity.
    let device = relay
        .registered
        .lock()
        .expect("lock")
        .clone()
        .expect("the device registered");
    assert!(!device.device_cert.is_empty());

    // The vault the flow left reopens under the root it was given, holding
    // the restored `ID_D_priv`: the core was shut down, so it opens again in
    // this process.
    let reopened = Core::open(
        cfg(dir.path()),
        Unlock::DevicePaired {
            root: VaultRootKey::from_bytes(RECOVERED_ROOT),
            paired: None,
        },
    )
    .await
    .expect("the restored vault reopens");
    assert!(reopened.holds_identity_key());
    assert_eq!(
        sunrise_crypto::identity_id_from_pub(&reopened.identity_signing_pub()),
        identity_id,
        "the vault belongs to the recovered account, not a fresh one"
    );
    assert_eq!(reopened.device_id(), device.vault_device_id);
    reopened.shutdown().await;
}

#[tokio::test]
async fn a_refused_registration_has_still_written_the_vault() {
    let (key, blob, code, _) = founded().await;
    let mut relay = FakeRelay::serving(key, blob);
    relay.register = Err(RelayRefusal::Other("503".to_owned()));
    let dir = tempfile::tempdir().expect("a vault dir");

    let identity = restore_identity(&relay, &code, &mut |_| {})
        .await
        .expect("restored");
    let err = rejoin_account(
        &relay,
        cfg(dir.path()),
        VaultRootKey::from_bytes(RECOVERED_ROOT),
        identity,
        label(),
        &no_sync,
        0,
        &mut |_| {},
    )
    .await
    .expect_err("refused");
    assert!(matches!(err, RestoreError::Register(_)), "{err:?}");
    assert!(err.vault_written());
}

#[tokio::test]
async fn a_sync_that_will_not_start_is_reported_with_the_device_id() {
    let (key, blob, code, _) = founded().await;
    let relay = FakeRelay::serving(key, blob);
    let dir = tempfile::tempdir().expect("a vault dir");
    let refuse = |_: &Arc<Core>, _: &str| Err("no transport".to_owned());

    let err = recover_account(
        &relay,
        &code,
        cfg(dir.path()),
        VaultRootKey::from_bytes(RECOVERED_ROOT),
        label(),
        &refuse,
        60_000,
        &mut |_| {},
    )
    .await
    .expect_err("sync never started");
    assert!(
        matches!(&err, RestoreError::NeverCaughtUp { detail, .. } if detail.contains("no transport")),
        "{err:?}"
    );
}

#[test]
fn the_identity_id_is_derived_from_the_accounts_published_key() {
    let key = [0x11u8; 32];
    let encoded = sunrise_relay_client::encode_public_key(&key);
    assert_eq!(
        identity_id_from_account(Some(&encoded)).expect("a key"),
        sunrise_crypto::identity_id_from_pub(&key),
        "the AAD a recovering client computes must be the one the founder sealed under"
    );
}

/// The two ways an account can fail to be recoverable, kept apart because the
/// advice differs: nobody registered the account, or the relay answered with
/// something that is not a key.
#[test]
fn an_account_with_no_usable_identity_key_is_refused_as_such() {
    assert!(matches!(
        identity_id_from_account(None),
        Err(RestoreError::NoIdentityKey)
    ));
    assert!(matches!(
        identity_id_from_account(Some("   ")),
        Err(RestoreError::NoIdentityKey)
    ));
    assert!(matches!(
        identity_id_from_account(Some("not base64url!!")),
        Err(RestoreError::IdentityKey(_))
    ));
    // Right alphabet, wrong length: the case a padding length check would turn
    // into a silently wrong AAD.
    let short = sunrise_relay_client::encode_recovery_blob(&[0u8; 16]);
    assert!(matches!(
        identity_id_from_account(Some(&short)),
        Err(RestoreError::IdentityKey(
            sunrise_relay_client::PublicKeyError::Length(16)
        ))
    ));
}

#[test]
fn words_are_checked_one_at_a_time_and_codes_whole() {
    assert!(is_recovery_word("abandon"));
    assert!(is_recovery_word(" Zoo "));
    assert!(!is_recovery_word("abandonn"));
    assert!(!is_recovery_word(""));

    let code = sunrise_crypto::bip39::encode_recovery_code(&SEED);
    check_recovery_code(code.reveal()).expect("a real code");
    assert!(check_recovery_code("abandon abandon").is_err());
}

/// A code whose blob a rotation replaced fails as retired, with the date the
/// relay replaced it, and not as a mistyped code; any other well-formed code
/// still fails as a code (ADR-0070).
#[tokio::test]
async fn a_retired_code_is_told_so_and_no_other_code_is() {
    let (key, blob, code, _) = founded().await;
    let old_seed = [0x5b; 32];
    let old_code = sunrise_crypto::bip39::encode_recovery_code(&old_seed);
    let payload = sunrise_crypto::recovery::RecoveryPayload {
        id_s_priv: [1; 32],
        id_d_priv: [2; 32],
        id_s_pub: [3; 32],
        id_d_pub: [4; 32],
        identity_id: [5; 16],
        created_at_ms: 1,
        account_id_s_pub: None,
        meta_genesis_keys: Vec::new(),
    };
    let mut rng = <rand_chacha::ChaCha20Rng as rand_core::SeedableRng>::seed_from_u64(3);
    let old_blob = sunrise_crypto::seal_recovery_blob(&old_seed, &payload, &mut rng).unwrap();
    let mut relay = FakeRelay::serving(key, blob);
    relay.retired = vec![RetiredCode {
        check: sunrise_relay_client::encode_recovery_blob(
            &old_blob[..sunrise_crypto::RETIRED_CHECK_LEN],
        ),
        retired_at_ms: 1_793_000_000_000,
    }];

    let err = restore_identity(&relay, old_code.reveal(), &mut |_| {})
        .await
        .expect_err("a retired code opens nothing");
    assert!(
        matches!(
            err,
            RestoreError::CodeRetired {
                retired_at_ms: Some(1_793_000_000_000)
            }
        ),
        "{err:?}"
    );
    assert!(err.to_string().contains("retired on 2026-10-26"), "{err}");
    assert!(!err.vault_written());

    let stranger = sunrise_crypto::bip39::encode_recovery_code(&[0x5c; 32]);
    let err = restore_identity(&relay, stranger.reveal(), &mut |_| {})
        .await
        .expect_err("a stranger's code opens nothing");
    assert!(matches!(err, RestoreError::Code(_)), "{err:?}");

    restore_identity(&relay, &code, &mut |_| {})
        .await
        .expect("the current code still opens the blob");
}
