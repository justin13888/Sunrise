//! Device keychain: persistent local identity + Stream-key derivation.
//!
//! The keychain is created (first open) or loaded (subsequent opens) whenever a
//! [`crate::Core`] opens a vault. It owns:
//!
//! - the device id and device Ed25519 signing key (`D_S`),
//! - the self-issued [`DeviceCert`] bytes,
//! - a zeroizing copy of the [`VaultRootKey`] (Core drops its own after
//!   `Db::open`; the keychain keeps the only live copy), and
//! - an in-memory cache of per-Stream symmetric keys.
//!
//! Every op the engine emits is sealed through here: the inner-op CBOR is
//! encrypted under the Stream key and signed with `D_S` into a real
//! [`OpEnvelope`].
//!
//! ## Key derivation input layout
//!
//! - **device_id** = `BLAKE3.derive_key("sunrise.device_id.v1", D_S_pub)[..16]`.
//! - **signing_secret_wrapped** = `nonce(24) || XChaCha20-Poly1305_seal(`
//!   `key = vault_root, plaintext = D_S seed(32),`
//!   `aad = "sunrise.local_identity.v1" || device_id)`.
//! - **stream_key(stream_id, epoch)** =
//!   `BLAKE3.derive_key("sunrise.stream_key.v1", vault_root || stream_id || u32_be(epoch))`.
//!   This is device-independent: two devices sharing a vault root derive equal
//!   Stream keys for the same `(stream_id, epoch)` — the pairing model.

use crate::config::{Clock, Rng};
use parking_lot::Mutex;
use rusqlite::{params, OptionalExtension};
use std::collections::HashMap;
use sunrise_crypto::aead::{aead_open_xchacha, aead_seal_xchacha, AEAD_NONCE_LEN};
use sunrise_crypto::blake3_kdf::derive_key_32;
use sunrise_crypto::op_envelope::open_envelope;
use sunrise_crypto::{
    decode_envelope, derive_key, encode_envelope, identity_id_from_pub, wrap_stream_key, AeadAlgId,
    DeviceCert, DeviceCertInner, DeviceDhKeyPair, IdentitySigningKeyPair, OpEnvelopeError,
    StreamKey, VaultRootKey,
};
use sunrise_storage::Db;
use thiserror::Error;
use zeroize::Zeroize;

/// Stream-key epoch used for all v1 ops. Rotation (new epochs) is a later
/// slice; for now the derivation and the table write both pin epoch 1.
pub const EPOCH: u32 = 1;

/// AAD prefix binding the wrapped device signing secret to its device id.
const LOCAL_IDENTITY_AAD_PREFIX: &[u8] = b"sunrise.local_identity.v1";
/// Length of a wrapped signing secret: nonce (24) + ciphertext (32) + tag (16).
const WRAPPED_SECRET_LEN: usize = AEAD_NONCE_LEN + 32 + 16;

/// Keychain errors.
#[derive(Debug, Error)]
pub enum KeychainError {
    /// Underlying storage failure.
    #[error("storage: {0}")]
    Storage(#[from] sunrise_storage::DbError),
    /// SQLite failure.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Device-cert build/encode failure.
    #[error("device cert: {0}")]
    Cert(#[from] sunrise_crypto::DeviceCertError),
    /// Envelope seal/open failure.
    #[error("op envelope: {0}")]
    Envelope(#[from] OpEnvelopeError),
    /// The vault root does not open the stored signing secret (wrong root).
    #[error("vault root does not match stored device identity")]
    VaultRootMismatch,
    /// Stored wrapped secret has an unexpected length.
    #[error("wrapped signing secret has wrong length")]
    WrappedLen,
    /// The stored device id does not match its unwrapped signing key.
    #[error("stored device_id does not match its signing key")]
    DeviceIdMismatch,
    /// An op envelope names a device this keychain cannot verify (v1: only the
    /// local device is known).
    #[error("op signed by an unknown device")]
    UnknownDevice,
}

/// Persistent per-device keychain.
pub struct Keychain {
    device_id: [u8; 16],
    signing: IdentitySigningKeyPair,
    cert_blob: Vec<u8>,
    vault_root: VaultRootKey,
    cache: Mutex<HashMap<[u8; 16], StreamKey>>,
}

impl std::fmt::Debug for Keychain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keychain")
            .field("device_id", &hex16(&self.device_id))
            .finish_non_exhaustive()
    }
}

impl Keychain {
    /// Load the device identity, or create it on first open.
    ///
    /// First open: generate a signing keypair from the injected RNG, derive the
    /// device id from its public key, self-issue a [`DeviceCert`], wrap the
    /// signing secret under `vault_root`, and persist both a `local_identity`
    /// row and a `devices` row. Subsequent opens: load, unwrap (fails cleanly if
    /// the vault root is wrong), and verify the device id.
    ///
    /// # Errors
    /// Storage/crypto failures, or a wrong vault root / corrupt identity row.
    pub fn open(
        db: &mut Db,
        vault_root: VaultRootKey,
        clock: &dyn Clock,
        rng: &dyn Rng,
    ) -> Result<Self, KeychainError> {
        if let Some((device_id, wrapped, cert_blob)) = load_identity_row(db)? {
            let mut secret = unwrap_signing_secret(&vault_root, &wrapped, &device_id)?;
            let signing = IdentitySigningKeyPair::from_secret_bytes(&secret);
            secret.zeroize();
            if device_id_from_pub(&signing.public_bytes()) != device_id {
                return Err(KeychainError::DeviceIdMismatch);
            }
            return Ok(Self {
                device_id,
                signing,
                cert_blob,
                vault_root,
                cache: Mutex::new(HashMap::new()),
            });
        }

        // First open: mint a fresh device identity from injected entropy only.
        let mut seed = [0u8; 32];
        rng.fill_bytes(&mut seed);
        let signing = IdentitySigningKeyPair::from_secret_bytes(&seed);
        seed.zeroize();
        let d_s_pub = signing.public_bytes();
        let device_id = device_id_from_pub(&d_s_pub);

        let mut dh_seed = [0u8; 32];
        rng.fill_bytes(&mut dh_seed);
        let dh = DeviceDhKeyPair::from_secret_bytes(dh_seed);
        dh_seed.zeroize();

        let now_ms = clock.now_ms();
        let nickname = "sunrise-device".to_string();
        let platform = std::env::consts::OS.to_string();
        // Self-signed v1 device cert: the device signing key is also the issuing
        // identity key (single-device bootstrap; real identity pairing is a
        // later slice).
        let body = DeviceCertInner {
            v: 1,
            device_id,
            d_s_pub,
            d_d_pub: dh.public_bytes(),
            identity_id: identity_id_from_pub(&d_s_pub),
            created_at_ms: now_ms,
            nickname: nickname.clone(),
            platform: platform.clone(),
        };
        let cert = DeviceCert::issue(body, &signing)?;
        let cert_blob = cert.to_cbor()?;

        let mut secret_bytes = signing.secret_bytes();
        let wrapped = wrap_signing_secret(&vault_root, &secret_bytes, &device_id, rng);
        secret_bytes.zeroize();

        db.with_tx(|tx| {
            tx.execute(
                "INSERT INTO local_identity
                 (id, device_id, signing_secret_wrapped, cert_blob, created_at_ms)
                 VALUES (1, ?, ?, ?, ?)",
                params![&device_id[..], wrapped, cert_blob, now_ms],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO devices
                 (device_id, cert_blob, nickname, platform, created_at_ms, revoked_at_ms)
                 VALUES (?, ?, ?, ?, ?, NULL)",
                params![&device_id[..], cert_blob, nickname, platform, now_ms],
            )?;
            Ok(())
        })?;

        Ok(Self {
            device_id,
            signing,
            cert_blob,
            vault_root,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// The 16-byte device id.
    #[must_use]
    pub const fn device_id(&self) -> [u8; 16] {
        self.device_id
    }

    /// The device signing public key (`D_S_pub`).
    #[must_use]
    pub fn device_signing_pub(&self) -> [u8; 32] {
        self.signing.public_bytes()
    }

    /// The self-issued device cert bytes (canonical CBOR).
    #[must_use]
    /// Copy the vault root out, for handing to a newly paired device.
    ///
    /// This is the one operation that deliberately breaks the rule the rest of
    /// this module exists to enforce — that the vault root never leaves the
    /// keychain. Pairing is the sole legitimate caller: a second device is
    /// useless without the root, since every stream key derives from it.
    ///
    /// The name is long and unpleasant on purpose. There is no other correct
    /// use, and anything else calling it is a bug worth noticing in review.
    /// Callers must send the result only through an authenticated encrypted
    /// channel (`sunrise_pairing::PairedChannel`) and drop it immediately
    /// after; it is returned as a `VaultRootKey` so it zeroizes on drop rather
    /// than as a bare array that would linger.
    #[must_use]
    pub fn export_vault_root_for_pairing(&self) -> VaultRootKey {
        VaultRootKey::from_bytes(*self.vault_root.as_bytes())
    }

    pub fn cert_blob(&self) -> &[u8] {
        &self.cert_blob
    }

    /// Derive (and cache) the Stream key for `stream_id` at the v1 epoch.
    #[must_use]
    pub fn stream_key(&self, stream_id: &[u8; 16]) -> StreamKey {
        let mut cache = self.cache.lock();
        if let Some(k) = cache.get(stream_id) {
            return k.clone();
        }
        let k = derive_stream_key(&self.vault_root, stream_id, EPOCH);
        cache.insert(*stream_id, k.clone());
        k
    }

    /// Seal an inner-op CBOR blob into a real [`OpEnvelope`] (encrypt under the
    /// Stream key + sign with `D_S`). Returns magic-prefixed envelope bytes.
    ///
    /// # Errors
    /// AEAD / CBOR failures from the envelope codec.
    pub fn seal_op(
        &self,
        stream_id: [u8; 16],
        seq: u64,
        ts_ms: u64,
        inner: &[u8],
        rng: &dyn Rng,
    ) -> Result<Vec<u8>, OpEnvelopeError> {
        let stream_key = self.stream_key(&stream_id);
        let mut nonce = [0u8; AEAD_NONCE_LEN];
        rng.fill_bytes(&mut nonce);
        encode_envelope(
            inner,
            stream_id,
            self.device_id,
            seq,
            ts_ms,
            AeadAlgId::XChaCha20Poly1305,
            EPOCH,
            nonce,
            Some(&stream_key),
            &self.signing,
        )
    }

    /// Decode, verify, and decrypt a stored envelope row back to inner-op CBOR.
    ///
    /// # Errors
    /// Bad magic / signature / AEAD failures, or an unknown signing device.
    pub fn open_op(&self, envelope_bytes: &[u8]) -> Result<Vec<u8>, KeychainError> {
        let env = decode_envelope(envelope_bytes)?;
        if env.device_id != self.device_id {
            return Err(KeychainError::UnknownDevice);
        }
        let stream_key = self.stream_key(&env.stream_id);
        let inner = open_envelope(&env, &self.device_signing_pub(), Some(&stream_key))?;
        Ok(inner)
    }

    /// Forward-compat: persist a wrapped copy of the Stream key into
    /// `stream_keys` on first use. Reads still derive; this table write only
    /// lets a later slice make rotation table-driven. Idempotent.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn persist_stream_key(
        &self,
        tx: &rusqlite::Transaction<'_>,
        stream_id: &[u8; 16],
        rng: &dyn Rng,
        now_ms: u64,
    ) -> rusqlite::Result<()> {
        let stream_key = self.stream_key(stream_id);
        let mut adapter = RngAdapter(rng);
        // wrap_stream_key can only fail on an AEAD size error, impossible for a
        // 32-byte key; treat as unreachable rather than surfacing.
        let Ok(wrapped) = wrap_stream_key(
            &self.vault_root,
            &stream_key,
            stream_id,
            EPOCH,
            &mut adapter,
        ) else {
            return Ok(());
        };
        tx.execute(
            "INSERT OR IGNORE INTO stream_keys (stream_id, epoch, wrapped, created_at_ms)
             VALUES (?, ?, ?, ?)",
            params![&stream_id[..], EPOCH, wrapped, now_ms],
        )?;
        Ok(())
    }

    /// Cheap in-memory keychain for engine unit tests: deterministic signing
    /// key, no DB row. Sealing/opening + Stream-key derivation all work; the
    /// `stream_keys` table write remains idempotent.
    #[cfg(test)]
    pub(crate) fn for_test(vault_root: VaultRootKey) -> Self {
        Self::for_test_seeded(vault_root, [7u8; 32])
    }

    /// Like [`Self::for_test`] but with a caller-chosen signing seed, so tests
    /// can construct two keychains with distinct device ids.
    #[cfg(test)]
    pub(crate) fn for_test_seeded(vault_root: VaultRootKey, signing_seed: [u8; 32]) -> Self {
        let signing = IdentitySigningKeyPair::from_secret_bytes(&signing_seed);
        let d_s_pub = signing.public_bytes();
        let device_id = device_id_from_pub(&d_s_pub);
        let dh = DeviceDhKeyPair::from_secret_bytes([8u8; 32]);
        let body = DeviceCertInner {
            v: 1,
            device_id,
            d_s_pub,
            d_d_pub: dh.public_bytes(),
            identity_id: identity_id_from_pub(&d_s_pub),
            created_at_ms: 0,
            nickname: "test-device".to_string(),
            platform: "test".to_string(),
        };
        let cert = DeviceCert::issue(body, &signing).expect("issue test cert");
        Self {
            device_id,
            signing,
            cert_blob: cert.to_cbor().expect("encode test cert"),
            vault_root,
            cache: Mutex::new(HashMap::new()),
        }
    }
}

/// `device_id = BLAKE3.derive_key("sunrise.device_id.v1", D_S_pub)[..16]`.
fn device_id_from_pub(d_s_pub: &[u8; 32]) -> [u8; 16] {
    let bytes = derive_key("sunrise.device_id.v1", d_s_pub, 16);
    let mut out = [0u8; 16];
    out.copy_from_slice(&bytes);
    out
}

fn derive_stream_key(vault_root: &VaultRootKey, stream_id: &[u8; 16], epoch: u32) -> StreamKey {
    let mut km = Vec::with_capacity(32 + 16 + 4);
    km.extend_from_slice(vault_root.as_bytes());
    km.extend_from_slice(stream_id);
    km.extend_from_slice(&epoch.to_be_bytes());
    let key = derive_key_32("sunrise.stream_key.v1", &km);
    km.zeroize();
    StreamKey::from_bytes(key)
}

fn local_identity_aad(device_id: &[u8; 16]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(LOCAL_IDENTITY_AAD_PREFIX.len() + 16);
    aad.extend_from_slice(LOCAL_IDENTITY_AAD_PREFIX);
    aad.extend_from_slice(device_id);
    aad
}

fn wrap_signing_secret(
    vault_root: &VaultRootKey,
    secret: &[u8; 32],
    device_id: &[u8; 16],
    rng: &dyn Rng,
) -> Vec<u8> {
    let mut nonce = [0u8; AEAD_NONCE_LEN];
    rng.fill_bytes(&mut nonce);
    let aad = local_identity_aad(device_id);
    // A 32-byte plaintext never trips the only AEAD error path.
    let ct = aead_seal_xchacha(vault_root.as_bytes(), &nonce, secret, &aad)
        .expect("aead seal of 32-byte secret");
    let mut out = Vec::with_capacity(AEAD_NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    out
}

fn unwrap_signing_secret(
    vault_root: &VaultRootKey,
    wrapped: &[u8],
    device_id: &[u8; 16],
) -> Result<[u8; 32], KeychainError> {
    if wrapped.len() != WRAPPED_SECRET_LEN {
        return Err(KeychainError::WrappedLen);
    }
    let (nonce, ct) = wrapped.split_at(AEAD_NONCE_LEN);
    let mut nonce_arr = [0u8; AEAD_NONCE_LEN];
    nonce_arr.copy_from_slice(nonce);
    let aad = local_identity_aad(device_id);
    let pt = aead_open_xchacha(vault_root.as_bytes(), &nonce_arr, ct, &aad)
        .map_err(|_| KeychainError::VaultRootMismatch)?;
    if pt.len() != 32 {
        return Err(KeychainError::WrappedLen);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&pt);
    Ok(out)
}

/// `(device_id, signing_secret_wrapped, cert_blob)` as read from `local_identity`.
type IdentityRow = ([u8; 16], Vec<u8>, Vec<u8>);

fn load_identity_row(db: &Db) -> Result<Option<IdentityRow>, KeychainError> {
    let row = db
        .conn()
        .query_row(
            "SELECT device_id, signing_secret_wrapped, cert_blob
             FROM local_identity WHERE id = 1",
            [],
            |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .optional()?;
    Ok(row.map(|(d, w, c)| {
        let mut id = [0u8; 16];
        let take = d.len().min(16);
        id[..take].copy_from_slice(&d[..take]);
        (id, w, c)
    }))
}

fn hex16(b: &[u8; 16]) -> String {
    let mut s = String::with_capacity(8);
    for byte in b.iter().take(4) {
        use core::fmt::Write;
        let _ = write!(s, "{byte:02x}");
    }
    s
}

/// Adapts the injected [`Rng`] (fill-only) to the `CryptoRngCore` interface the
/// crypto crate's `wrap_stream_key` requires.
struct RngAdapter<'a>(&'a dyn Rng);

impl rand_core::RngCore for RngAdapter<'_> {
    fn next_u32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        self.0.fill_bytes(&mut b);
        u32::from_le_bytes(b)
    }
    fn next_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        self.0.fill_bytes(&mut b);
        u64::from_le_bytes(b)
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.0.fill_bytes(dest);
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.0.fill_bytes(dest);
        Ok(())
    }
}

impl rand_core::CryptoRng for RngAdapter<'_> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SystemRng;
    use parking_lot::Mutex as PLMutex;
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

    #[test]
    fn first_open_creates_identity_and_rows() {
        let root = VaultRootKey::from_bytes([0xab; 32]);
        let mut d = db(&root);
        let kc = Keychain::open(&mut d, root.clone(), &clock(), &SystemRng).unwrap();

        // local_identity row exists and matches the device id.
        let (dev, cert): (Vec<u8>, Vec<u8>) = d
            .conn()
            .query_row(
                "SELECT device_id, cert_blob FROM local_identity WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(&dev[..], &kc.device_id()[..]);
        assert_eq!(cert, kc.cert_blob());

        // A devices row was written for the local device.
        let n: i64 = d
            .conn()
            .query_row(
                "SELECT count(*) FROM devices WHERE device_id = ?",
                params![&kc.device_id()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);

        // The stored cert verifies under the device signing key (self-signed).
        let parsed = DeviceCert::from_cbor(kc.cert_blob()).unwrap();
        parsed.verify(&kc.device_signing_pub()).unwrap();
        assert_eq!(parsed.body.device_id, kc.device_id());
    }

    #[test]
    fn second_open_loads_same_identity() {
        let root = VaultRootKey::from_bytes([0xcd; 32]);
        let mut d = db(&root);
        let first = Keychain::open(&mut d, root.clone(), &clock(), &SystemRng).unwrap();
        let first_id = first.device_id();
        let first_cert = first.cert_blob().to_vec();
        drop(first);

        let second = Keychain::open(&mut d, root.clone(), &clock(), &SystemRng).unwrap();
        assert_eq!(second.device_id(), first_id);
        assert_eq!(second.cert_blob(), &first_cert[..]);
    }

    #[test]
    fn wrong_vault_root_fails_cleanly() {
        // Seed an identity with one root, but reopen the *same DB* with a
        // different keychain root. Use a raw connection so SQLCipher (keyed by a
        // KDF of the root) does not itself block the reopen — we want to prove
        // the keychain unwrap fails, not the DB open.
        let root = VaultRootKey::from_bytes([1u8; 32]);
        let mut d = db(&root);
        let _ = Keychain::open(&mut d, root, &clock(), &SystemRng).unwrap();

        let bad = VaultRootKey::from_bytes([2u8; 32]);
        let err = Keychain::open(&mut d, bad, &clock(), &SystemRng).unwrap_err();
        assert!(matches!(err, KeychainError::VaultRootMismatch));
    }

    #[test]
    fn same_root_two_keychains_derive_equal_stream_keys() {
        // The pairing model: two devices (distinct DBs) that share a vault root
        // derive identical Stream keys for the same stream id.
        let root = VaultRootKey::from_bytes([9u8; 32]);
        let mut da = db(&root);
        let mut db2 = db(&root);
        let ka = Keychain::open(&mut da, root.clone(), &clock(), &SystemRng).unwrap();
        let kb = Keychain::open(&mut db2, root.clone(), &clock(), &SystemRng).unwrap();
        // Distinct device identities...
        assert_ne!(ka.device_id(), kb.device_id());
        // ...but equal Stream keys for the same stream id.
        let sid = [5u8; 16];
        assert_eq!(ka.stream_key(&sid), kb.stream_key(&sid));
        // Different stream ids diverge.
        assert_ne!(ka.stream_key(&[5u8; 16]), ka.stream_key(&[6u8; 16]));
    }

    #[test]
    fn seal_open_round_trip() {
        let root = VaultRootKey::from_bytes([0x11; 32]);
        let mut d = db(&root);
        let kc = Keychain::open(&mut d, root, &clock(), &SystemRng).unwrap();
        let inner = b"inner op cbor bytes";
        let env = kc.seal_op([0u8; 16], 1, 42, inner, &SystemRng).unwrap();
        let back = kc.open_op(&env).unwrap();
        assert_eq!(&back, inner);
    }
}
