//! One real vault, sealed by this build, committed, and opened by the
//! keychain on every `cargo test`.
//!
//! # What was missing
//!
//! Nothing pinned the wrapping side of the key hierarchy. The two "legacy
//! vault" tests in [`super`] synthesise their vault in-process with the same
//! [`wrap_secret`](super::crypto::wrap_secret) and
//! [`device_aad`](super::crypto::device_aad) they then read it back with, so
//! they are self-consistent by construction: rename an AAD prefix, change
//! `prefix || id` to `id || prefix`, or move `WRAPPED_SECRET_LEN`, and they go
//! on passing. `sunrise-storage`'s committed fixtures do go through a real
//! `Db::open`, but they assert *rows* — their wrapped blobs are invented byte
//! strings that no keychain ever unwraps.
//!
//! So the one thing a user actually depends on — that a build of Sunrise can
//! open a vault a *different* build of Sunrise wrote — had no test at all.
//! This is it.
//!
//! # What the fixture pins
//!
//! [`KEYCHAIN_FIXTURE`] is an encrypted `SQLCipher` vault written by this
//! build. Opening it exercises, in one call each and with no in-process
//! wrapping anywhere:
//!
//! | Constant | Where it is needed to open the file |
//! |---|---|
//! | `sunrise.sqlcipher_key.v1` | `Db::open` derives the page key |
//! | `sunrise.local_identity.v1` | AAD of `local_identity.signing_secret_wrapped` |
//! | `sunrise.local_identity.dh.v1` | AAD of `local_identity.dh_secret_wrapped` |
//! | `sunrise.local_identity.identity.sign.v2` | AAD of `identity.id_s_priv_wrapped` |
//! | `sunrise.local_identity.identity.dh.v2` | AAD of `identity.id_d_priv_wrapped` |
//! | `sunrise.device_id.v1` | the id recomputed from the unwrapped `D_S_pub` |
//! | `sunrise.identity_id.v1` | the id recomputed from the unwrapped `ID_S_pub` |
//! | `sunrise.device_cert.v1` | the committed cert verifies under `ID_S_pub` |
//! | `sunrise.wrap.stream_key.v1` | AAD of the `stream_keys.wrapped` column |
//! | `sunrise.meta_genesis_key.v1` | the vault-meta genesis key it must agree on |
//!
//! and the two lengths that have no other witness, `WRAPPED_SECRET_LEN` and
//! `WRAPPED_STREAM_KEY_LEN`, both asserted here against literals in
//! `sunrise-crypto-test-vectors` rather than against the constants themselves.
//! That separation is the point: a coordinated edit to a constant *and* to the
//! test beside it cannot pass, because the expectation lives in a crate that
//! depends on nothing and was written down once.
//!
//! # The key is committed, on purpose
//!
//! [`FIXTURE_VAULT_ROOT`] is thirty-two `0x6c` bytes and it is in this
//! repository in plain sight, for the reason `sunrise-storage`'s
//! `fixtures/README.md` gives at length: a checked-in encrypted fixture is
//! worthless without its key, and an unencrypted one would not go through the
//! `PRAGMA key` path that is half of what is under test. Nothing is behind it.
//! The account identity is the public one from
//! `sunrise-crypto-test-vectors`, the device keys are two constant seeds, and
//! no user has ever been keyed with any of it.
//!
//! # Regenerating
//!
//! `mise run keychain-fixture`, which runs
//! [`regenerate_the_keychain_vault_fixture`] — an `#[ignore]`d test, so an
//! ordinary `cargo test` reads the committed file and never rewrites it.
//!
//! **When it legitimately changes.** Exactly one case: a deliberate
//! `CRYPTO_SUITE_V` bump that moves one of the ten constants above, landing
//! with a rotation plan per `docs/03-crypto/key-rotation.md`. Then the fixture
//! is regenerated, renamed to the new suite version, and the old file is
//! *kept* if the build still claims to open vaults written under the old
//! suite — which is what its name is for. A fixture regenerated to make a red
//! test green is the one thing this module exists to prevent; if
//! [`the_committed_vault_opens_and_its_keys_are_the_frozen_ones`] fails and
//! no suite bump was intended, the change under review is the bug.
//!
//! The bytes are not reproducible run to run: `SQLCipher` writes a random salt
//! into the file header and every wrap draws a fresh nonce, so a regeneration
//! produces a different file with identical contents. Expect a whole-file
//! diff and review this module rather than the binary.

use std::fs;
use std::path::{Path, PathBuf};

use parking_lot::Mutex as PLMutex;
use sunrise_crypto::keys::{IdentityDhKeyPair, IdentitySigningKeyPair};
use sunrise_crypto::recovery::RecoveryPayload;
use sunrise_crypto::{derive_key, identity_id_from_pub, DeviceCert, VaultRootKey};
use sunrise_crypto_test_vectors::at_rest::{keychain_aad, meta_genesis, wrapped_stream_key};
use sunrise_crypto_test_vectors::identity_transition as it;
use sunrise_storage::Db;

use super::crypto::{device_aad, device_dh_aad};
use super::{Keychain, GENESIS_EPOCH};
use crate::config::{Clock, Rng};
use crate::unlock::IdentitySeed;

/// The committed vault.
///
/// Named for `CRYPTO_SUITE_V`, not for `STORAGE_V`: what it pins is the key
/// hierarchy, and the storage schema it happens to carry is incidental — the
/// migration chain has its own fixtures in `sunrise-storage`.
const KEYCHAIN_FIXTURE: &str = "keychain_vault_suite_v5.db";

/// The vault root the fixture is keyed with. Public on purpose; see the
/// module header.
const FIXTURE_VAULT_ROOT: [u8; 32] = [0x6c; 32];

/// `D_S_priv` of the device inside the fixture — the same seed the frozen
/// device cert is issued for, so its `device_id` is already a frozen literal.
const FIXTURE_D_S_SEED: [u8; 32] = sunrise_crypto_test_vectors::DEVICE_SIGNING_SECRET;

/// `D_D_priv` of the same device.
const FIXTURE_D_D_SEED: [u8; 32] = sunrise_crypto_test_vectors::key_envelope::RECIPIENT_SECRET;

/// The one Stream key the fixture holds, beside the derived vault-meta one.
const FIXTURE_STREAM_KEY: [u8; 32] = wrapped_stream_key::STREAM_KEY;

/// The stream that key belongs to.
const FIXTURE_STREAM: [u8; 16] = wrapped_stream_key::STREAM_ID;

/// The stamp every row in the fixture carries.
const FIXTURE_NOW_MS: u64 = 1_700_000_000_000;

/// A clock that does not move, so the fixture's timestamps are stated rather
/// than observed.
#[derive(Debug)]
struct FixtureClock;

impl Clock for FixtureClock {
    fn now_ms(&self) -> u64 {
        FIXTURE_NOW_MS
    }
}

/// A scripted RNG, so the fixture's device keys and Stream key are constants
/// rather than draws.
///
/// The first two 32-byte requests are the device's two seeds, in the order
/// `mint_device_keys` asks for them. Every other request is filled from a
/// counter, so regeneration is deterministic without pretending an AEAD nonce
/// is meaningful. The fixture's Stream key is *not* drawn here — it is handed
/// to `absorb_stream_key` as a constant, so it does not depend on how many
/// times the create path happens to reach for randomness.
#[derive(Debug)]
struct ScriptedRng {
    calls: PLMutex<usize>,
}

impl Rng for ScriptedRng {
    fn fill_bytes(&self, dest: &mut [u8]) {
        let mut calls = self.calls.lock();
        let n = *calls;
        *calls += 1;
        drop(calls);
        if dest.len() == 32 {
            match n {
                0 => return dest.copy_from_slice(&FIXTURE_D_S_SEED),
                1 => return dest.copy_from_slice(&FIXTURE_D_D_SEED),
                _ => {}
            }
        }
        let stream = derive_key(
            "sunrise.keychain_fixture.nonces",
            &(n as u64).to_be_bytes(),
            dest.len(),
        );
        dest.copy_from_slice(&stream);
    }
}

/// The directory the committed fixture lives in.
fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn fixture_key() -> VaultRootKey {
    VaultRootKey::from_bytes(FIXTURE_VAULT_ROOT)
}

/// The account identity inside the fixture, as a recovery payload.
///
/// Recovered rather than freshly minted so the account keys are the frozen
/// ones: a founding vault would draw `ID_S`/`ID_D` at random and the fixture's
/// `identity_id` would not be a literal anybody can check.
fn fixture_identity_seed() -> IdentitySeed {
    let id_s = IdentitySigningKeyPair::from_secret_bytes(&it::IDENTITY_SIGNING_SECRET);
    let id_d = IdentityDhKeyPair::from_secret_bytes(it::IDENTITY_DH_SECRET);
    IdentitySeed::Recovered(Box::new(RecoveryPayload {
        id_s_priv: it::IDENTITY_SIGNING_SECRET,
        id_d_priv: it::IDENTITY_DH_SECRET,
        id_s_pub: id_s.public_bytes(),
        id_d_pub: id_d.public_bytes(),
        identity_id: identity_id_from_pub(&id_s.public_bytes()),
        created_at_ms: FIXTURE_NOW_MS,
    }))
}

/// Copy the committed fixture out and open the copy through the ordinary
/// public entry points.
///
/// The copy is not politeness: `Db::open` migrates and the keychain writes, so
/// a test that opened the committed file would rewrite it and every later run
/// would be reading a vault this build had just produced.
fn open_committed() -> (tempfile::TempDir, Db, Keychain) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(KEYCHAIN_FIXTURE);
    let source = fixture_dir().join(KEYCHAIN_FIXTURE);
    fs::copy(&source, &path).unwrap_or_else(|e| {
        panic!(
            "copy {}: {e} — run `mise run keychain-fixture`",
            source.display()
        )
    });
    let mut db = Db::open(&path, &fixture_key()).unwrap_or_else(|e| {
        panic!(
            "the committed vault must open: {e}. `Db::open` derives its page \
             key with `sunrise.sqlcipher_key.v1`, so this failing means that \
             derivation moved and no vault on any disk opens any more"
        )
    });
    let keychain = Keychain::open(
        &mut db,
        fixture_key(),
        &FixtureClock,
        &ScriptedRng {
            calls: PLMutex::new(0),
        },
        &IdentitySeed::Own,
    )
    .unwrap_or_else(|e| {
        panic!(
            "the keychain must open the committed vault: {e}. Every wrapped \
             secret in it is bound to an AAD that is nowhere in the file, so \
             this failing means a wrapping domain, an AAD construction or a \
             secret length changed — a CRYPTO_SUITE_V bump, not a test fix"
        )
    });
    (dir, db, keychain)
}

/// Write the fixture from the constants above.
///
/// `#[ignore]`d: an ordinary `cargo test` must read the committed file, not
/// replace it. Run it through `mise run keychain-fixture`.
#[test]
#[ignore = "rewrites the committed vault fixture; run it through `mise run keychain-fixture`"]
fn regenerate_the_keychain_vault_fixture() {
    let dir = fixture_dir();
    fs::create_dir_all(&dir).expect("create the fixture directory");
    let path = dir.join(KEYCHAIN_FIXTURE);
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(dir.join(format!("{KEYCHAIN_FIXTURE}{suffix}")));
    }

    let mut db = Db::open(&path, &fixture_key()).expect("create the fixture vault");
    let rng = ScriptedRng {
        calls: PLMutex::new(0),
    };
    let keychain = Keychain::open(
        &mut db,
        fixture_key(),
        &FixtureClock,
        &rng,
        &fixture_identity_seed(),
    )
    .expect("create the keychain");
    db.with_tx(|tx| {
        keychain.absorb_stream_key(
            tx,
            &FIXTURE_STREAM,
            GENESIS_EPOCH,
            &sunrise_crypto::keys::StreamKey::from_bytes(FIXTURE_STREAM_KEY),
            super::KeySource::Local,
            &rng,
            FIXTURE_NOW_MS,
        )
    })
    .expect("store the fixture's one Stream key");

    drop(keychain);
    // WAL leaves `-wal`/`-shm` beside the database. A fixture is one file or
    // it is not a fixture: half of it would be uncommitted and the committed
    // half would be missing every row.
    db.conn()
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .expect("checkpoint");
    drop(db);
    for suffix in ["-wal", "-shm"] {
        let sidecar = dir.join(format!("{KEYCHAIN_FIXTURE}{suffix}"));
        assert!(
            !sidecar.exists() || fs::metadata(&sidecar).expect("stat").len() == 0,
            "`{}` still holds data; the fixture is incomplete",
            sidecar.display()
        );
        let _ = fs::remove_file(&sidecar);
    }
}

/// The committed file really is a sealed vault, asserted before anything else.
///
/// A fixture that had somehow been written in the clear would let every test
/// below pass while exercising no `PRAGMA key` path at all.
#[test]
fn the_committed_vault_is_encrypted_at_rest() {
    let bytes = fs::read(fixture_dir().join(KEYCHAIN_FIXTURE)).expect("read the fixture");
    assert!(
        !bytes.starts_with(b"SQLite format 3\0"),
        "the fixture must be encrypted, or it is not a vault"
    );
    assert!(
        !bytes
            .windows(FIXTURE_D_S_SEED.len())
            .any(|w| w == FIXTURE_D_S_SEED),
        "the device signing seed is in the file in the clear"
    );
    assert!(
        !bytes
            .windows(FIXTURE_STREAM_KEY.len())
            .any(|w| w == FIXTURE_STREAM_KEY),
        "the Stream key is in the file in the clear"
    );
}

/// The whole point: a vault this process did not write opens, and every key
/// that comes out of it is a literal written down elsewhere.
///
/// Nothing here wraps anything. The expectations are in
/// `sunrise-crypto-test-vectors`, which depends on nothing, so a coordinated
/// rename of a wrapping domain and of the code that reads it still fails — the
/// file on disk does not move when the constants do.
#[test]
fn the_committed_vault_opens_and_its_keys_are_the_frozen_ones() {
    let (_dir, db, kc) = open_committed();

    // `sunrise.device_id.v1` and the device's two wrapped secrets: the id is
    // recomputed from the unwrapped `D_S_pub` and refused if it disagrees with
    // the stored column, so reaching here means both.
    assert_eq!(
        kc.device_id,
        it::device_cert::DEVICE_ID,
        "the committed vault's device id is not the frozen one"
    );
    assert_eq!(
        kc.device_signing_pub(),
        sunrise_crypto_test_vectors::DEVICE_SIGNING_PUBLIC
    );
    assert_eq!(kc.device_dh_pub(), it::device_cert::D_D_PUB);

    // `sunrise.identity_id.v1` and the identity's two wrapped halves, which
    // are wrapped under *different* domains since `CRYPTO_SUITE_V = 4`.
    assert_eq!(kc.identity_id(), it::IDENTITY_ID);
    assert_eq!(kc.identity_signing_pub(), it::IDENTITY_SIGNING_PUBLIC);
    assert_eq!(kc.identity_dh_pub(), it::IDENTITY_DH_PUBLIC);
    assert!(
        kc.holds_only_copy_of_identity_key(),
        "the fixture was written as a recovered vault, so it holds ID_D_priv"
    );

    // `sunrise.device_cert.v1`: the committed cert verifies under the
    // committed identity.
    let cert = DeviceCert::from_cbor(&kc.cert_blob()).expect("the committed cert decodes");
    cert.verify(&it::IDENTITY_SIGNING_PUBLIC)
        .expect("the committed cert verifies under the committed identity");
    assert_eq!(cert.body.device_id, it::device_cert::DEVICE_ID);

    // `sunrise.wrap.stream_key.v1`: the one stored Stream key unwraps to the
    // key it was minted from.
    let held = kc.held_stream_keys();
    let epochs = held
        .get(&FIXTURE_STREAM)
        .expect("the fixture holds a key for its one stream");
    assert_eq!(
        epochs.get(&GENESIS_EPOCH),
        Some(&FIXTURE_STREAM_KEY),
        "the stored Stream key did not unwrap to the frozen key — the \
         sunrise.wrap.stream_key.v1 AAD moved"
    );

    // `sunrise.meta_genesis_key.v1`: the derived key a recovered device has to
    // agree on before it can be *given* any other key.
    assert_eq!(
        kc.meta_genesis_key(&meta_genesis::META_STREAM, meta_genesis::GENESIS_EPOCH)
            .expect("the fixture holds ID_D_priv")
            .as_bytes(),
        &meta_genesis::KEY
    );

    drop(db);
}

/// The AAD prefixes and the two wrapped lengths, as literals, against the
/// blobs the committed file actually holds.
///
/// The open above proves the AADs still work. This proves they are still the
/// *same* ones, which is a different claim: a build that renamed a prefix and
/// regenerated the fixture would pass the open and fail here.
#[test]
fn the_committed_vaults_blobs_have_the_frozen_aads_and_lengths() {
    let (_dir, db, _kc) = open_committed();

    assert_eq!(
        device_aad(&it::device_cert::DEVICE_ID),
        keychain_aad::DEVICE_SIGNING,
        "the sunrise.local_identity.v1 AAD is not the one the committed \
         vault's signing blob was sealed under"
    );
    assert_eq!(
        device_dh_aad(&it::device_cert::DEVICE_ID),
        keychain_aad::DEVICE_DH,
        "the sunrise.local_identity.dh.v1 AAD is not the one the committed \
         vault's DH blob was sealed under"
    );

    let (signing, dh): (Vec<u8>, Vec<u8>) = db
        .conn()
        .query_row(
            "SELECT signing_secret_wrapped, dh_secret_wrapped FROM local_identity WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("read the committed local_identity row");
    let (id_s, id_d): (Vec<u8>, Vec<u8>) = db
        .conn()
        .query_row(
            "SELECT id_s_priv_wrapped, id_d_priv_wrapped FROM identity",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("read the committed identity row");
    for (name, blob) in [
        ("signing_secret_wrapped", &signing),
        ("dh_secret_wrapped", &dh),
        ("id_s_priv_wrapped", &id_s),
        ("id_d_priv_wrapped", &id_d),
    ] {
        assert_eq!(
            blob.len(),
            keychain_aad::WRAPPED_SECRET_LEN,
            "`{name}` in the committed vault is not WRAPPED_SECRET_LEN bytes; \
             a length change orphans every vault ever written"
        );
    }

    let wrapped: Vec<u8> = db
        .conn()
        .query_row(
            "SELECT wrapped FROM stream_keys WHERE stream_id = ?",
            rusqlite::params![&FIXTURE_STREAM[..]],
            |r| r.get(0),
        )
        .expect("read the committed stream_keys row");
    assert_eq!(
        wrapped.len(),
        wrapped_stream_key::WRAPPED_STREAM_KEY_LEN,
        "the committed wrap is not WRAPPED_STREAM_KEY_LEN bytes"
    );
}
