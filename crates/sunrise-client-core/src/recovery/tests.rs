//! The recovery flow's pure halves: the code, the blob, the registration,
//! and the replay's progress.

use super::*;
use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;
use sunrise_crypto::seal_recovery_blob;

/// The registration a recovery sends names the vault id in Crockford
/// base-32 and carries the cert, so a sibling can revoke and inspect it.
#[test]
fn a_recovering_device_registers_its_vault_id_and_cert() {
    let id = recovering_identity(RecoveringDevice {
        device_pub_s: [7u8; 32],
        device_cert: vec![1, 2, 3],
        vault_device_id: [0u8; 16],
        nickname: "n".into(),
        platform: "linux".into(),
        app_version: Some("1".into()),
    });
    assert_eq!(id.device_pub_s, [7u8; 32]);
    assert_eq!(id.device_cert.as_deref(), Some("AQID"));
    assert_eq!(
        id.vault_device_id.as_deref(),
        Some("00000000000000000000000000")
    );
    assert!(id.device_pub_d.is_none());
    assert_eq!((id.nickname.as_str(), id.platform.as_str()), ("n", "linux"));
}

fn status(applied: u64, remaining: Option<u64>) -> SyncStatus {
    SyncStatus {
        state: sunrise_sync::SyncState::CatchingUp,
        outbox_pending: 0,
        peer_devices: 0,
        last_sync_ms: None,
        replay_applied: applied,
        replay_remaining: remaining,
    }
}

/// #459: n of N where the relay states what is left, n alone where it
/// does not.
#[test]
fn a_replay_reports_a_total_only_where_the_relay_states_one() {
    assert_eq!(
        replay_progress(&status(3, Some(7))),
        RecoveryProgress::Replaying {
            applied: 3,
            total: Some(10)
        }
    );
    assert_eq!(
        replay_progress(&status(10, Some(0))),
        RecoveryProgress::Replaying {
            applied: 10,
            total: Some(10)
        }
    );
    assert_eq!(
        replay_progress(&status(3, None)),
        RecoveryProgress::Replaying {
            applied: 3,
            total: None
        }
    );
}

#[test]
fn round_trip() {
    let mut rng = ChaCha20Rng::seed_from_u64(7);
    let seed = [9u8; 32];
    let payload = RecoveryPayload {
        id_s_priv: [1u8; 32],
        id_d_priv: [2u8; 32],
        id_s_pub: [3u8; 32],
        id_d_pub: [4u8; 32],
        identity_id: [5u8; 16],
        created_at_ms: 1,
        account_id_s_pub: None,
        meta_genesis_keys: Vec::new(),
    };
    let blob = seal_recovery_blob(&seed, &payload, &mut rng).unwrap();
    let back = recover_identity(&blob, &seed, &payload.identity_id).unwrap();
    assert_eq!(back, payload);
}

/// The flow a user actually runs: twenty-four words in, identity out.
#[test]
fn a_typed_recovery_code_restores_the_identity() {
    let mut rng = ChaCha20Rng::seed_from_u64(11);
    let seed = [0x2bu8; 32];
    let code = sunrise_crypto::bip39::encode_recovery_code(&seed);
    let payload = RecoveryPayload {
        id_s_priv: [1u8; 32],
        id_d_priv: [2u8; 32],
        id_s_pub: [3u8; 32],
        id_d_pub: [4u8; 32],
        identity_id: [5u8; 16],
        created_at_ms: 1,
        account_id_s_pub: None,
        meta_genesis_keys: Vec::new(),
    };
    let blob = seal_recovery_blob(&seed, &payload, &mut rng).unwrap();

    let back = recover_identity_from_code(&blob, code.reveal(), &payload.identity_id).unwrap();
    assert_eq!(back, payload);
}

/// A mistyped word is refused before Argon2id runs, and is reported as a
/// code failure rather than as a decryption failure — the user's next
/// action is "check what you typed", not "find another code".
#[test]
fn a_mistyped_code_is_refused_as_a_code() {
    let mut rng = ChaCha20Rng::seed_from_u64(12);
    let seed = [0x2cu8; 32];
    let code = sunrise_crypto::bip39::encode_recovery_code(&seed);
    let payload = RecoveryPayload {
        id_s_priv: [1u8; 32],
        id_d_priv: [2u8; 32],
        id_s_pub: [3u8; 32],
        id_d_pub: [4u8; 32],
        identity_id: [6u8; 16],
        created_at_ms: 1,
        account_id_s_pub: None,
        meta_genesis_keys: Vec::new(),
    };
    let blob = seal_recovery_blob(&seed, &payload, &mut rng).unwrap();

    let mut words: Vec<&str> = code.reveal().split(' ').collect();
    words.pop();
    assert!(matches!(
        recover_identity_from_code(&blob, &words.join(" "), &payload.identity_id),
        Err(RecoveryFlowError::Code(_))
    ));

    // And a well-formed code for another account fails the AEAD, which is
    // the other half: the two errors must not collapse.
    let other = sunrise_crypto::bip39::encode_recovery_code(&[0x2du8; 32]);
    assert!(matches!(
        recover_identity_from_code(&blob, other.reveal(), &payload.identity_id),
        Err(RecoveryFlowError::Crypto(_))
    ));
}

/// The replay wait ends on a session the relay refused, naming the state,
/// rather than spending its budget and reporting a timeout.
#[tokio::test]
async fn the_replay_wait_ends_on_a_refused_session() {
    use crate::sync::tests::{hello_ack, relay, terminal_close, vault_at, Stuck};
    let dir = tempfile::tempdir().unwrap();
    let core = vault_at(dir.path(), Arc::new(Stuck)).await;
    core.start_sync(relay(vec![hello_ack(), terminal_close()]))
        .unwrap();
    let detail = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        wait_until_live(&core, 3_600_000, &mut |_| {}),
    )
    .await
    .expect("a refused session ends the wait though the clock never moves")
    .expect_err("not live");
    assert!(detail.starts_with("ended Stopped, "), "{detail}");
    core.shutdown().await;
}

/// A `Degraded` session does not end the replay wait: a restored device
/// passes through it while a deferred stream waits for the account's key
/// bundle. The wait runs out its budget, and says it was still degraded.
#[tokio::test]
async fn the_replay_wait_rides_out_a_degraded_session_until_its_budget() {
    use crate::sync::tests::{cursor_gap, hello_ack, relay, vault_at, Stepping};
    use std::sync::atomic::AtomicU64;
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(Stepping(AtomicU64::new(0)));
    let core = vault_at(dir.path(), clock.clone()).await;
    core.start_sync(relay(vec![hello_ack(), cursor_gap()]))
        .unwrap();
    let started = core.now_ms();
    let detail = wait_until_live(&core, 120_000, &mut |_| {})
        .await
        .expect_err("not live");
    assert!(
        clock.0.load(std::sync::atomic::Ordering::Relaxed) >= started + 120_000,
        "the wait returned before its budget: {detail}"
    );
    assert!(
        detail.starts_with("still Degraded, ") && detail.ends_with(" pending after 120s"),
        "{detail}"
    );
    core.shutdown().await;
}
