//! The keychain's unit tests.
//!
//! Each theme is a module of its own, declared below. This file holds the
//! imports they share through `use super::*`, and the helpers more than one
//! theme draws on.

mod envelopes;
mod legacy;
mod open;
mod pairing;
mod recovery;
mod refusals;
mod stream_keys;
mod succession;

use super::*;
use crate::config::SystemRng;
use crypto::wrap_secret;
use device::mint_device_keys;
use parking_lot::Mutex as PLMutex;
use std::collections::BTreeSet;
use sunrise_crypto::aead::AEAD_NONCE_LEN;
use sunrise_crypto::op_envelope::open_envelope;
use sunrise_crypto::{decode_envelope, stream_key_id, AeadAlgId, DeviceCert, DeviceCertInner};
use sunrise_domain::PRIVATE_DOMAIN_BYTES;
use sunrise_pairing::PairingPayload;
use sunrise_storage::Db;

#[derive(Debug)]
struct FakeClock(PLMutex<u64>);
impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        *self.0.lock()
    }
}

fn clock() -> FakeClock {
    FakeClock(PLMutex::new(1_700_000_000_000))
}

fn db(root: &VaultRootKey) -> Db {
    Db::open_memory(root).unwrap()
}

/// Open the blob `founder` seals under `seed`, the way a recovering device
/// does: through the real format, rather than by copying fields across.
fn recovered_payload(
    founder: &Keychain,
    seed: &[u8; 32],
) -> sunrise_crypto::recovery::RecoveryPayload {
    let blob = founder.seal_recovery_blob(seed, &SystemRng).unwrap();
    sunrise_crypto::unseal_recovery_blob(&blob, seed, &founder.identity_id()).unwrap()
}

/// The device a test's `key_envelope` keys arrive from, when which device
/// it is does not matter to the test.
const PEER: [u8; 16] = [0xee; 16];

fn open(d: &mut Db, root: &VaultRootKey) -> Keychain {
    Keychain::open(d, root.clone(), &clock(), &SystemRng, &IdentitySeed::Own).unwrap()
}

/// A pairing payload as [`Keychain::open`] now takes it. Clones because
/// [`IdentitySeed`] owns its payload and these tests keep reading theirs.
fn seed_of(payload: &PairingPayload) -> IdentitySeed {
    IdentitySeed::Paired(Box::new(payload.clone()))
}

/// The public halves of a successor, as the signed body would state them.
fn publics(successor: &Identity) -> SuccessorPublics {
    SuccessorPublics {
        identity_id: successor.identity_id,
        id_s_pub: successor.signing.public_bytes(),
        id_d_pub: successor.dh_pub,
    }
}

/// The roster entry a rotation would carry for `subject`.
///
/// `adopt_successor_identity` takes this rather than re-issuing, because
/// since `#105` the device doing the adopting usually holds no `ID_S_priv`
/// to issue with. Signed by `successor`, which only the emitter holds — so
/// every test that adopts has to go through the emitter to get one, exactly
/// as the real path does.
fn roster_cert(successor: &Identity, subject: &Keychain, now_ms: u64) -> Vec<u8> {
    Keychain::issue_roster_cert(
        successor,
        subject.device_id(),
        subject.device_signing_pub(),
        subject.device_dh_pub(),
        "a",
        "test",
        now_ms,
    )
    .expect("issue a roster cert")
}

/// A founding vault and a device paired from it — the two halves every
/// rotation has to serve, since only the founder holds `ID_D_priv`.
fn paired_pair() -> (VaultRootKey, Db, Keychain, Db, Keychain) {
    let root = VaultRootKey::from_bytes([0x71; 32]);
    let mut da = db(&root);
    let ka = open(&mut da, &root);
    let payload = ka
        .pair_device_in_process(
            &da,
            "joiner".into(),
            "test".into(),
            [0x71; 32],
            [0x72; 32],
            1_700_000_000_000,
        )
        .unwrap();
    let mut db2 = db(&root);
    let kb = Keychain::open(
        &mut db2,
        root.clone(),
        &clock(),
        &SystemRng,
        &seed_of(&payload),
    )
    .unwrap();
    (root, da, ka, db2, kb)
}
