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
