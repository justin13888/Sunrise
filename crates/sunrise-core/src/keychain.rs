//! Device keychain: the account identity, this device's keys, and the
//! `stream_keys` table.
//!
//! The keychain is created (first open) or loaded (subsequent opens) whenever a
//! [`crate::Core`] opens a vault. It owns:
//!
//! - the **account identity** — `ID_S` (Ed25519) and `ID_D` (X25519), generated
//!   once per account and independent of every device key,
//! - this device's id, signing key `D_S` and DH key `D_D`,
//! - the identity-signed [`DeviceCert`] bytes,
//! - a zeroizing copy of the [`VaultRootKey`] (Core drops its own after
//!   `Db::open`; the keychain keeps the only live copy), and
//! - every Stream key this device holds, keyed by `(stream_id, epoch)`.
//!
//! Every op the engine emits is sealed through here: the inner-op CBOR is
//! encrypted under the Stream key for the stream's live epoch and signed with
//! `D_S` into a real [`OpEnvelope`].
//!
//! ## What ADR-0024 changed
//!
//! Stream keys used to be *derived*:
//! `BLAKE3.derive_key("sunrise.stream_key.v1", vault_root ‖ stream_id ‖ epoch)`,
//! with `EPOCH` pinned at 1. Everything followed from that one line — rotation
//! rotated nothing (anyone with the root computes any epoch), revocation was
//! impossible (every paired device holds the root), and recovery restored keys
//! that decrypted nothing.
//!
//! They are now **independently random per `(stream_id, epoch)`**, wrapped
//! under the vault root at rest and distributed between devices by HPKE
//! `key_envelope` ops. The derivation survives in exactly one place — the
//! private `legacy_derived_stream_key` below — because a vault written before
//! this change has ops sealed under those derived keys, and adopting it means
//! recomputing them once and storing them like any other key.
//!
//! ## Key derivation input layout
//!
//! - **device_id** = `BLAKE3.derive_key("sunrise.device_id.v1", D_S_pub)[..16]`.
//! - **identity_id** = `BLAKE3.derive_key("sunrise.identity_id.v1", ID_S_pub)[..16]`.
//! - **signing_secret_wrapped** = `nonce(24) || XChaCha20-Poly1305_seal(`
//!   `key = vault_root, plaintext = D_S seed(32),`
//!   `aad = "sunrise.local_identity.v1" || device_id)`.
//! - **dh_secret_wrapped** — the same, with AAD
//!   `"sunrise.local_identity.dh.v1" || device_id`.
//! - **id_s_priv_wrapped / id_d_priv_wrapped** — the same, with AAD
//!   `"sunrise.local_identity.identity.v1" || identity_id`.
//! - **stream key** — 32 random bytes; stored as
//!   `wrap_stream_key(vault_root, key, stream_id, epoch)`.

use crate::config::{Clock, Rng};
use parking_lot::Mutex;
use rusqlite::{params, OptionalExtension};
use std::collections::{BTreeMap, HashMap};
use sunrise_crypto::aead::{aead_open_xchacha, aead_seal_xchacha, AEAD_NONCE_LEN};
use sunrise_crypto::blake3_kdf::derive_key_32;
use sunrise_crypto::op_envelope::open_envelope;
use sunrise_crypto::{
    decode_envelope, derive_key, encode_envelope, hpke_open, hpke_open_identity, hpke_seal,
    identity_id_from_pub, key_envelope_info, stream_key_id, unwrap_stream_key, wrap_stream_key,
    AeadAlgId, DeviceCert, DeviceCertInner, DeviceDhKeyPair, DeviceSigningKeyPair, HpkeError,
    IdentityDhKeyPair, IdentitySigningKeyPair, OpEnvelopeError, StreamKey, VaultRootKey,
};
use sunrise_domain::INBOX_STREAM_BYTES;
use sunrise_pairing::PairingPayload;
use sunrise_storage::Db;
use thiserror::Error;
use zeroize::Zeroize;

/// AAD prefix binding the wrapped device signing secret to its device id.
const LOCAL_IDENTITY_AAD_PREFIX: &[u8] = b"sunrise.local_identity.v1";
/// AAD prefix binding the wrapped device DH secret to its device id.
const LOCAL_DH_AAD_PREFIX: &[u8] = b"sunrise.local_identity.dh.v1";
/// AAD prefix binding the wrapped identity secrets to the identity id.
const IDENTITY_AAD_PREFIX: &[u8] = b"sunrise.local_identity.identity.v1";
/// Length of a wrapped 32-byte secret: nonce (24) + ciphertext (32) + tag (16).
const WRAPPED_SECRET_LEN: usize = AEAD_NONCE_LEN + 32 + 16;

/// Where a Stream key came from. Recorded for forensics, never for policy: a
/// key opens an op or it does not, whichever route delivered it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// Minted on this device.
    Local,
    /// Arrived in a `key_envelope` op.
    Envelope,
    /// Arrived in a pairing payload.
    Pairing,
    /// Recomputed from the pre-ADR-0024 derivation while adopting a legacy
    /// vault.
    Legacy,
}

impl KeySource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Envelope => "envelope",
            Self::Pairing => "pairing",
            Self::Legacy => "legacy",
        }
    }
}

/// Who a `key_envelope` was sealed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeRecipient {
    /// This device's `D_D`.
    Device,
    /// The account identity's `ID_D`.
    Identity,
}

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
    /// HPKE seal/open failure.
    #[error("key envelope: {0}")]
    Hpke(#[from] HpkeError),
    /// Pairing payload codec failure.
    #[error("pairing payload: {0}")]
    Pairing(#[from] sunrise_pairing::PairingPayloadError),
    /// The vault root does not open the stored secrets (wrong root).
    #[error("vault root does not match stored device identity")]
    VaultRootMismatch,
    /// Stored wrapped secret has an unexpected length.
    #[error("wrapped secret has wrong length")]
    WrappedLen,
    /// The stored device id does not match its unwrapped signing key.
    #[error("stored device_id does not match its signing key")]
    DeviceIdMismatch,
    /// The stored identity row is inconsistent with its own keys.
    #[error("stored identity_id does not match its signing key")]
    IdentityIdMismatch,
    /// A vault with no identity row was opened with a pairing payload for a
    /// different account.
    #[error("pairing payload belongs to a different identity than this vault")]
    IdentityConflict,
    /// The vault has no identity and none can be minted: it has a
    /// `local_identity` row from a build that predates ADR-0024 and the
    /// adoption path was not taken.
    #[error("vault has no account identity")]
    NoIdentity,
    /// An op envelope names a device this keychain cannot verify.
    #[error("op signed by an unknown device")]
    UnknownDevice,
    /// No key at the envelope's `(stream_id, epoch)` opened it.
    #[error("no stream key opens this envelope")]
    NoStreamKey,
}

/// The account identity: one keypair pair per account, not per device.
///
/// `identity_id` keeps its full name rather than shortening to `id`: it is the
/// name the format carries — `identity_id_from_pub`, the `identity` table's
/// column, field 5 of `PairingPayload`, the `identity_id` in every `DeviceCert`
/// — and one struct renaming it would be the only place in the tree where the
/// value is called something else.
#[allow(clippy::struct_field_names)]
pub struct Identity {
    identity_id: [u8; 16],
    signing: IdentitySigningKeyPair,
    dh: IdentityDhKeyPair,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("identity_id", &hex16(&self.identity_id))
            .finish_non_exhaustive()
    }
}

/// Every Stream key one device holds, indexed by `(stream_id, epoch)`.
///
/// A `Vec` per slot rather than one key: two devices can mint the same epoch
/// concurrently and both keys are kept, with the AEAD tag deciding which one
/// opens a given op.
type StreamKeyCache = Mutex<HashMap<([u8; 16], u32), Vec<StreamKey>>>;

/// Persistent per-device keychain.
pub struct Keychain {
    device_id: [u8; 16],
    signing: DeviceSigningKeyPair,
    device_dh: DeviceDhKeyPair,
    identity: Identity,
    cert_blob: Vec<u8>,
    vault_root: VaultRootKey,
    /// Every Stream key this device holds, `(stream_id, epoch) -> keys`.
    ///
    /// Loaded in full at open and kept in step by [`Keychain::mint_epoch`] and
    /// [`Keychain::absorb_stream_key`]. It is a **decrypt-side** cache and is
    /// deliberately allowed to be a superset of the table: a key that a rolled
    /// back transaction never committed simply fails to open anything. The
    /// *live epoch* is never read from here — that comes from the table inside
    /// the caller's transaction, where it is consistent by construction.
    cache: StreamKeyCache,
    /// Engine unit tests only: mint and look up Stream keys by the
    /// pre-ADR-0024 derivation instead of drawing them at random.
    ///
    /// Those tests hand an envelope from one in-memory engine to another with
    /// no relay in between, so the `key_envelope` ops that carry a random key
    /// have nothing to travel over — every cross-device assertion would fail on
    /// a missing key rather than on the behaviour it is testing. Deriving makes
    /// two keychains sharing a vault root agree again, exactly as they did
    /// before this ADR.
    ///
    /// The real path is covered where it can be: `sunrise-e2e` runs a live
    /// relay and carries the envelope ops for real, and
    /// [`Self::for_test_random_keys`] opts one engine test back into random
    /// keys so the deferral path has coverage here too.
    #[cfg(test)]
    test_derived_keys: bool,
}

impl std::fmt::Debug for Keychain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keychain")
            .field("device_id", &hex16(&self.device_id))
            .field("identity_id", &hex16(&self.identity.identity_id))
            .finish_non_exhaustive()
    }
}

impl Keychain {
    /// Load the device identity, or create it on first open.
    ///
    /// Four cases, and the third is the one that carries the migration:
    ///
    /// 1. **Fresh vault** — mint `ID_S`/`ID_D`, mint `D_S`/`D_D`, issue a cert
    ///    signed by `ID_S_priv`, persist both rows.
    /// 2. **Fresh vault + `paired`** — adopt the identity out of the pairing
    ///    payload, mint this device's own keys, self-issue a cert *under the
    ///    account identity* (the payload carries `ID_S_priv`, which is what
    ///    makes that legitimate), and import every Stream key it carries.
    /// 3. **Pre-ADR-0024 vault** — a `local_identity` row with no `identity`
    ///    row. [`Self::adopt_legacy_vault`] runs, once and idempotently.
    /// 4. **Known vault** — load and unwrap, failing cleanly on a wrong root.
    ///
    /// # Errors
    /// Storage/crypto failures, or a wrong vault root / corrupt identity row.
    pub fn open(
        db: &mut Db,
        vault_root: VaultRootKey,
        clock: &dyn Clock,
        rng: &dyn Rng,
        paired: Option<&PairingPayload>,
    ) -> Result<Self, KeychainError> {
        let local = load_identity_row(db)?;
        let identity_row = load_account_identity_row(db)?;

        let kc = match (local, identity_row) {
            (Some(local), Some(row)) => Self::load(db, vault_root, &local, &row)?,
            (Some(local), None) => Self::adopt_legacy_vault(db, vault_root, clock, rng, &local)?,
            (None, _) => Self::create(db, vault_root, clock, rng, paired)?,
        };
        kc.load_cache(db)?;
        Ok(kc)
    }

    /// Case 4: both rows present.
    fn load(
        db: &Db,
        vault_root: VaultRootKey,
        local: &IdentityRow,
        row: &AccountIdentityRow,
    ) -> Result<Self, KeychainError> {
        let _ = db;
        let mut secret = unwrap_secret(
            &vault_root,
            &local.signing_wrapped,
            &device_aad(&local.device_id),
        )?;
        let signing = DeviceSigningKeyPair::from_secret_bytes(&secret);
        secret.zeroize();
        if device_id_from_pub(&signing.public_bytes()) != local.device_id {
            return Err(KeychainError::DeviceIdMismatch);
        }
        let dh_wrapped = local.dh_wrapped.as_ref().ok_or(KeychainError::WrappedLen)?;
        let mut dh_secret =
            unwrap_secret(&vault_root, dh_wrapped, &device_dh_aad(&local.device_id))?;
        let device_dh = DeviceDhKeyPair::from_secret_bytes(dh_secret);
        dh_secret.zeroize();

        let identity = unwrap_identity(&vault_root, row)?;
        Ok(Self {
            device_id: local.device_id,
            signing,
            device_dh,
            identity,
            cert_blob: local.cert_blob.clone(),
            vault_root,
            cache: Mutex::new(HashMap::new()),
            #[cfg(test)]
            test_derived_keys: false,
        })
    }

    /// Cases 1 and 2: no `local_identity` row at all.
    fn create(
        db: &mut Db,
        vault_root: VaultRootKey,
        clock: &dyn Clock,
        rng: &dyn Rng,
        paired: Option<&PairingPayload>,
    ) -> Result<Self, KeychainError> {
        let now_ms = clock.now_ms();
        let existing = load_account_identity_row(db)?;

        let identity = match (paired, existing.as_ref()) {
            // A device being paired adopts the account's identity wholesale.
            (Some(p), existing) => {
                let identity = Identity {
                    identity_id: p.identity_id,
                    signing: IdentitySigningKeyPair::from_secret_bytes(&p.id_s_priv),
                    dh: IdentityDhKeyPair::from_secret_bytes(p.id_d_priv),
                };
                if identity_id_from_pub(&identity.signing.public_bytes()) != identity.identity_id {
                    return Err(KeychainError::IdentityIdMismatch);
                }
                if let Some(row) = existing {
                    if row.identity_id != identity.identity_id {
                        return Err(KeychainError::IdentityConflict);
                    }
                }
                identity
            }
            (None, Some(row)) => unwrap_identity(&vault_root, row)?,
            (None, None) => {
                let mut seed = [0u8; 32];
                rng.fill_bytes(&mut seed);
                let signing = IdentitySigningKeyPair::from_secret_bytes(&seed);
                seed.zeroize();
                let mut dh_seed = [0u8; 32];
                rng.fill_bytes(&mut dh_seed);
                let dh = IdentityDhKeyPair::from_secret_bytes(dh_seed);
                dh_seed.zeroize();
                let identity_id = identity_id_from_pub(&signing.public_bytes());
                Identity {
                    identity_id,
                    signing,
                    dh,
                }
            }
        };

        let (signing, device_dh, device_id) = mint_device_keys(rng);
        let nickname = "sunrise-device".to_string();
        let platform = std::env::consts::OS.to_string();
        let cert_blob = issue_cert(
            &identity, device_id, &signing, &device_dh, now_ms, &nickname, &platform,
        )?;

        let wrapped_identity = wrap_identity(&vault_root, &identity, rng);
        let (wrapped_signing, wrapped_dh) =
            wrap_device_secrets(&vault_root, &signing, &device_dh, &device_id, rng);
        let imported = paired.map(|p| p.stream_keys.clone()).unwrap_or_default();
        let write_identity = existing.is_none();

        db.with_tx(|tx| {
            if write_identity {
                insert_identity_row(tx, &identity, &wrapped_identity, now_ms)?;
            }
            insert_local_identity_row(
                tx,
                &device_id,
                &wrapped_signing,
                &wrapped_dh,
                &cert_blob,
                now_ms,
            )?;
            insert_device_row(
                tx,
                &device_id,
                &cert_blob,
                &nickname,
                &platform,
                &identity.identity_id,
                &device_dh.public_bytes(),
                now_ms,
            )?;
            for (stream_id, epochs) in &imported {
                for (epoch, key) in epochs {
                    insert_stream_key_row(
                        tx,
                        &vault_root,
                        stream_id,
                        *epoch,
                        &StreamKey::from_bytes(*key),
                        KeySource::Pairing,
                        rng,
                        now_ms,
                    )?;
                }
            }
            Ok(())
        })?;

        Ok(Self {
            device_id,
            signing,
            device_dh,
            identity,
            cert_blob,
            vault_root,
            cache: Mutex::new(HashMap::new()),
            #[cfg(test)]
            test_derived_keys: false,
        })
    }

    /// Case 3: a vault written before ADR-0024.
    ///
    /// Such a vault has a device, a self-signed cert whose `identity_id` was
    /// derived from the *device* key, ops sealed under keys derived from the
    /// vault root at epoch 1, and no account identity at all. Refusing it
    /// would wipe every developer and operator vault in existence for a change
    /// that loses nothing: the derived keys are recomputable here and now, and
    /// re-inserting them as ordinary `stream_keys` rows is pure bookkeeping —
    /// the ops were sealed under exactly those bytes.
    ///
    /// Idempotent by construction: it runs only when `identity` is absent, and
    /// its last act is to write that row. A second open takes case 4.
    ///
    /// The device keeps its id (which is a derivation of `D_S_pub`, unchanged)
    /// but gets a **new** cert, signed by the freshly minted `ID_S` — the old
    /// one was signed by the device itself and asserts an identity that no
    /// longer exists. It also gets a new `D_D`, because the old one was
    /// generated at first open and immediately dropped; only its public half
    /// survived, and `key_envelope` needs the private half.
    fn adopt_legacy_vault(
        db: &mut Db,
        vault_root: VaultRootKey,
        clock: &dyn Clock,
        rng: &dyn Rng,
        local: &IdentityRow,
    ) -> Result<Self, KeychainError> {
        let now_ms = clock.now_ms();
        let mut secret = unwrap_secret(
            &vault_root,
            &local.signing_wrapped,
            &device_aad(&local.device_id),
        )?;
        let signing = DeviceSigningKeyPair::from_secret_bytes(&secret);
        secret.zeroize();
        if device_id_from_pub(&signing.public_bytes()) != local.device_id {
            return Err(KeychainError::DeviceIdMismatch);
        }
        let device_id = local.device_id;

        // Mint the account identity this vault never had.
        let mut seed = [0u8; 32];
        rng.fill_bytes(&mut seed);
        let id_signing = IdentitySigningKeyPair::from_secret_bytes(&seed);
        seed.zeroize();
        let mut dh_seed = [0u8; 32];
        rng.fill_bytes(&mut dh_seed);
        let id_dh = IdentityDhKeyPair::from_secret_bytes(dh_seed);
        dh_seed.zeroize();
        let identity = Identity {
            identity_id: identity_id_from_pub(&id_signing.public_bytes()),
            signing: id_signing,
            dh: id_dh,
        };

        // A fresh D_D, since the old private half was never stored.
        let mut d_dh_seed = [0u8; 32];
        rng.fill_bytes(&mut d_dh_seed);
        let device_dh = DeviceDhKeyPair::from_secret_bytes(d_dh_seed);
        d_dh_seed.zeroize();

        let (nickname, platform) = legacy_cert_labels(&local.cert_blob);
        let cert_blob = issue_cert(
            &identity, device_id, &signing, &device_dh, now_ms, &nickname, &platform,
        )?;

        let wrapped_identity = wrap_identity(&vault_root, &identity, rng);
        let (wrapped_signing, wrapped_dh) =
            wrap_device_secrets(&vault_root, &signing, &device_dh, &device_id, rng);

        let streams = legacy_stream_set(db)?;

        db.with_tx(|tx| {
            insert_identity_row(tx, &identity, &wrapped_identity, now_ms)?;
            tx.execute(
                "UPDATE local_identity
                 SET signing_secret_wrapped = ?, dh_secret_wrapped = ?, cert_blob = ?
                 WHERE id = 1",
                params![wrapped_signing, wrapped_dh, cert_blob],
            )?;
            insert_device_row(
                tx,
                &device_id,
                &cert_blob,
                &nickname,
                &platform,
                &identity.identity_id,
                &device_dh.public_bytes(),
                now_ms,
            )?;
            tx.execute(
                "UPDATE devices SET cert_blob = ?, identity_id = ?, d_d_pub = ?
                 WHERE device_id = ?",
                params![
                    cert_blob,
                    &identity.identity_id[..],
                    &device_dh.public_bytes()[..],
                    &device_id[..]
                ],
            )?;
            for stream_id in &streams {
                let key = legacy_derived_stream_key(&vault_root, stream_id, LEGACY_EPOCH);
                insert_stream_key_row(
                    tx,
                    &vault_root,
                    stream_id,
                    LEGACY_EPOCH,
                    &key,
                    KeySource::Legacy,
                    rng,
                    now_ms,
                )?;
            }
            Ok(())
        })?;

        Ok(Self {
            device_id,
            signing,
            device_dh,
            identity,
            cert_blob,
            vault_root,
            cache: Mutex::new(HashMap::new()),
            #[cfg(test)]
            test_derived_keys: false,
        })
    }

    fn load_cache(&self, db: &Db) -> Result<(), KeychainError> {
        let mut stmt = db
            .conn()
            .prepare("SELECT stream_id, epoch, wrapped FROM stream_keys")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut cache = self.cache.lock();
        for (stream_id, epoch, wrapped) in rows {
            let stream_id = to16(&stream_id);
            let epoch = u32::try_from(epoch).unwrap_or(0);
            // A row whose wrap does not open under this root is not fatal: the
            // rest of the vault still works, and refusing to open the whole
            // keychain over one unreadable row would turn a corrupt byte into
            // a lost vault.
            if let Ok(key) = unwrap_stream_key(&self.vault_root, &wrapped, &stream_id, epoch) {
                cache.entry((stream_id, epoch)).or_default().push(key);
            }
        }
        Ok(())
    }

    // ---- accessors ----

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

    /// The device DH public key (`D_D_pub`) — what a `key_envelope` seals to.
    #[must_use]
    pub fn device_dh_pub(&self) -> [u8; 32] {
        self.device_dh.public_bytes()
    }

    /// The account identity id.
    #[must_use]
    pub const fn identity_id(&self) -> [u8; 16] {
        self.identity.identity_id
    }

    /// The account identity signing public key (`ID_S_pub`) — every cert in
    /// this vault must verify under it.
    #[must_use]
    pub fn identity_signing_pub(&self) -> [u8; 32] {
        self.identity.signing.public_bytes()
    }

    /// The account identity DH public key (`ID_D_pub`) — the recovery
    /// recipient of every `key_envelope`.
    #[must_use]
    pub fn identity_dh_pub(&self) -> [u8; 32] {
        self.identity.dh.public_bytes()
    }

    /// The identity-signed device cert bytes (canonical CBOR).
    #[must_use]
    pub fn cert_blob(&self) -> &[u8] {
        &self.cert_blob
    }

    /// Copy the vault root out, for handing to a newly paired device.
    ///
    /// This is the one operation that deliberately breaks the rule the rest of
    /// this module exists to enforce — that the vault root never leaves the
    /// keychain. It is no longer sufficient on its own: since ADR-0024 a device
    /// also needs the identity and the Stream keys, which is what
    /// [`Self::export_pairing_payload`] assembles. The root is still what keys
    /// the local database, so it still has to travel.
    ///
    /// The name is long and unpleasant on purpose. Callers must send the result
    /// only through an authenticated encrypted channel
    /// (`sunrise_pairing::PairedChannel`) and drop it immediately after.
    #[must_use]
    pub fn export_vault_root_for_pairing(&self) -> VaultRootKey {
        VaultRootKey::from_bytes(*self.vault_root.as_bytes())
    }

    /// Assemble everything a device being paired needs.
    ///
    /// # Errors
    /// SQLite failures reading the device labels.
    pub fn export_pairing_payload(&self, db: &Db) -> Result<PairingPayload, KeychainError> {
        let (nickname, platform) = self.local_labels(db)?;
        let mut stream_keys: BTreeMap<[u8; 16], BTreeMap<u32, [u8; 32]>> = BTreeMap::new();
        for ((stream_id, epoch), keys) in self.cache.lock().iter() {
            // One key per (stream, epoch) in the payload. Where two devices
            // minted the same epoch concurrently the receiver still learns the
            // other through the `key_envelope` op that distributed it; sending
            // both here would double a payload that is already the value most
            // likely to hit the transport limit.
            if let Some(first) = keys.first() {
                stream_keys
                    .entry(*stream_id)
                    .or_default()
                    .insert(*epoch, *first.as_bytes());
            }
        }
        Ok(PairingPayload {
            id_s_priv: self.identity.signing.secret_bytes(),
            id_d_priv: self.identity.dh.secret_bytes(),
            id_s_pub: self.identity.signing.public_bytes(),
            id_d_pub: self.identity.dh.public_bytes(),
            identity_id: self.identity.identity_id,
            vault_root: *self.vault_root.as_bytes(),
            stream_keys,
            nickname,
            platform,
        })
    }

    fn local_labels(&self, db: &Db) -> Result<(String, String), KeychainError> {
        let row: Option<(String, String)> = db
            .conn()
            .query_row(
                "SELECT nickname, platform FROM devices WHERE device_id = ?",
                params![&self.device_id[..]],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row.unwrap_or_else(|| {
            (
                "sunrise-device".to_string(),
                std::env::consts::OS.to_string(),
            )
        }))
    }

    // ---- Stream keys ----

    /// Every key this device holds at `(stream_id, epoch)`.
    ///
    /// Plural because two devices can mint the same epoch concurrently and both
    /// keys are retained; the AEAD tag decides which one actually opens an op.
    /// Losing one to a last-writer-wins on the minting op would lose every op
    /// sealed under it.
    #[must_use]
    pub fn stream_keys_at(&self, stream_id: &[u8; 16], epoch: u32) -> Vec<StreamKey> {
        let cached: Vec<StreamKey> = self
            .cache
            .lock()
            .get(&(*stream_id, epoch))
            .cloned()
            .unwrap_or_default();
        #[cfg(test)]
        if self.test_derived_keys && cached.is_empty() {
            return vec![legacy_derived_stream_key(
                &self.vault_root,
                stream_id,
                epoch,
            )];
        }
        cached
    }

    /// The live epoch for `stream_id`, read from the table inside `tx`.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn current_epoch_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        stream_id: &[u8; 16],
    ) -> rusqlite::Result<Option<u32>> {
        let epoch: Option<i64> = tx
            .query_row(
                "SELECT MAX(epoch) FROM stream_keys WHERE stream_id = ?",
                params![&stream_id[..]],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(epoch.map(|e| u32::try_from(e).unwrap_or(1)))
    }

    /// The live `(epoch, key)` for `stream_id`, or `None` if this device holds
    /// no key for it at all.
    ///
    /// When two devices minted the same epoch this returns the lowest `key_id`,
    /// so a device's choice of which to seal under is deterministic rather
    /// than dependent on row order.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn current_stream_key_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        stream_id: &[u8; 16],
    ) -> rusqlite::Result<Option<(u32, StreamKey)>> {
        // A derived-key test keychain always "has" every key, so an engine unit
        // test never mints one and never emits the `key_envelope` ops it has no
        // relay to carry. Without this the first vault-meta op of every test
        // would mint that stream's key mid-transaction and emit envelope ops
        // into the very stream whose sequence number the caller had already
        // read.
        #[cfg(test)]
        if self.test_derived_keys {
            return Ok(Some((
                LEGACY_EPOCH,
                legacy_derived_stream_key(&self.vault_root, stream_id, LEGACY_EPOCH),
            )));
        }
        let Some(epoch) = self.current_epoch_tx(tx, stream_id)? else {
            return Ok(None);
        };
        let wrapped: Option<Vec<u8>> = tx
            .query_row(
                "SELECT wrapped FROM stream_keys
                 WHERE stream_id = ? AND epoch = ? ORDER BY key_id LIMIT 1",
                params![&stream_id[..], epoch],
                |r| r.get(0),
            )
            .optional()?;
        let Some(wrapped) = wrapped else {
            return Ok(None);
        };
        match unwrap_stream_key(&self.vault_root, &wrapped, stream_id, epoch) {
            Ok(key) => Ok(Some((epoch, key))),
            Err(_) => Ok(None),
        }
    }

    /// Mint the next epoch for `stream_id`: 32 fresh random bytes, wrapped and
    /// persisted, returned so the caller can seal `key_envelope` ops with it.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn mint_epoch(
        &self,
        tx: &rusqlite::Transaction<'_>,
        stream_id: &[u8; 16],
        rng: &dyn Rng,
        now_ms: u64,
    ) -> rusqlite::Result<(u32, StreamKey)> {
        let epoch = self
            .current_epoch_tx(tx, stream_id)?
            .map_or(1, |e| e.saturating_add(1));
        let key = self.fresh_key(stream_id, epoch, rng);
        insert_stream_key_row(
            tx,
            &self.vault_root,
            stream_id,
            epoch,
            &key,
            KeySource::Local,
            rng,
            now_ms,
        )?;
        self.cache_insert(stream_id, epoch, &key);
        Ok((epoch, key))
    }

    /// Record a Stream key this device learned from somewhere other than its
    /// own minting. Idempotent on `(stream_id, epoch, key_id)`.
    ///
    /// Returns `true` if the key was new to this device, which is what tells
    /// the caller to drain [`deferred_ops`](crate::engine) for that key.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn absorb_stream_key(
        &self,
        tx: &rusqlite::Transaction<'_>,
        stream_id: &[u8; 16],
        epoch: u32,
        key: &StreamKey,
        source: KeySource,
        rng: &dyn Rng,
        now_ms: u64,
    ) -> rusqlite::Result<bool> {
        let changed = insert_stream_key_row(
            tx,
            &self.vault_root,
            stream_id,
            epoch,
            key,
            source,
            rng,
            now_ms,
        )?;
        if changed {
            self.cache_insert(stream_id, epoch, key);
        }
        Ok(changed)
    }

    /// 32 fresh random bytes — the whole point of ADR-0024, and the reason a
    /// Stream key can be rotated at all.
    // `self` is read only by the `cfg(test)` branch below, which is the whole
    // point of it being a method: a derived-key test keychain has to mint the
    // key the other replica would derive. Taking `&self` unconditionally keeps
    // one call site rather than two.
    #[allow(clippy::unused_self)]
    fn fresh_key(&self, stream_id: &[u8; 16], epoch: u32, rng: &dyn Rng) -> StreamKey {
        #[cfg(test)]
        if self.test_derived_keys {
            return legacy_derived_stream_key(&self.vault_root, stream_id, epoch);
        }
        let _ = (stream_id, epoch);
        let mut bytes = [0u8; 32];
        rng.fill_bytes(&mut bytes);
        let key = StreamKey::from_bytes(bytes);
        bytes.zeroize();
        key
    }

    fn cache_insert(&self, stream_id: &[u8; 16], epoch: u32, key: &StreamKey) {
        let mut cache = self.cache.lock();
        let slot = cache.entry((*stream_id, epoch)).or_default();
        if !slot.iter().any(|k| k == key) {
            slot.push(key.clone());
        }
    }

    /// Every `(stream_id, live epoch)` this device holds a key for.
    ///
    /// This is the rotation set: revoking a device mints a new epoch for every
    /// one of them, the vault-meta stream and the Inbox included.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn rotation_set(&self, tx: &rusqlite::Transaction<'_>) -> rusqlite::Result<Vec<[u8; 16]>> {
        let mut streams: std::collections::BTreeSet<[u8; 16]> = std::collections::BTreeSet::new();
        streams.insert(crate::engine::META_STREAM);
        streams.insert(INBOX_STREAM_BYTES);
        for sql in [
            "SELECT DISTINCT stream_id FROM stream_keys",
            "SELECT stream_id FROM streams",
            "SELECT DISTINCT stream_id FROM ops",
        ] {
            let mut stmt = tx.prepare(sql)?;
            let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
            for row in rows {
                streams.insert(to16(&row?));
            }
        }
        Ok(streams.into_iter().collect())
    }

    // ---- key envelopes ----

    /// Seal `key` to `recipient_pub` as a `key_envelope` payload.
    ///
    /// # Errors
    /// [`KeychainError::Hpke`] for a malformed recipient key.
    pub fn seal_key_envelope(
        &self,
        recipient_pub: &[u8; 32],
        stream_id: &[u8; 16],
        epoch: u32,
        key: &StreamKey,
        rng: &dyn Rng,
    ) -> Result<Vec<u8>, KeychainError> {
        let info = key_envelope_info(stream_id, epoch);
        let mut adapter = RngAdapter(rng);
        Ok(hpke_seal(
            recipient_pub,
            &info,
            key.as_bytes(),
            b"",
            &mut adapter,
        )?)
    }

    /// Open a `key_envelope` payload addressed to this device or to the
    /// account identity.
    ///
    /// # Errors
    /// [`KeychainError::Hpke`] when the blob is not for us, is truncated, or
    /// was sealed for another `(stream, epoch)`.
    pub fn open_key_envelope(
        &self,
        recipient: EnvelopeRecipient,
        stream_id: &[u8; 16],
        epoch: u32,
        sealed: &[u8],
    ) -> Result<StreamKey, KeychainError> {
        let info = key_envelope_info(stream_id, epoch);
        let bytes = match recipient {
            EnvelopeRecipient::Device => hpke_open(&self.device_dh, &info, sealed, b"")?,
            EnvelopeRecipient::Identity => {
                hpke_open_identity(&self.identity.dh, &info, sealed, b"")?
            }
        };
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| KeychainError::WrappedLen)?;
        Ok(StreamKey::from_bytes(arr))
    }

    // ---- op envelopes ----

    /// Seal an inner-op CBOR blob into a real [`OpEnvelope`] under an explicit
    /// `(epoch, key)`, and sign it with `D_S`.
    ///
    /// The epoch is passed in rather than looked up because a rotation has to
    /// seal the ops that *carry* the new keys under the **old** epoch — a
    /// device that does not yet hold the new key could not otherwise read the
    /// op that gives it one.
    ///
    /// # Errors
    /// AEAD / CBOR failures from the envelope codec.
    pub fn seal_op_at(
        &self,
        stream_id: [u8; 16],
        seq: u64,
        hlc: sunrise_cbor::hlc::Hlc,
        inner: &[u8],
        rng: &dyn Rng,
        epoch: u32,
        stream_key: &StreamKey,
    ) -> Result<Vec<u8>, OpEnvelopeError> {
        let mut nonce = [0u8; AEAD_NONCE_LEN];
        rng.fill_bytes(&mut nonce);
        encode_envelope(
            inner,
            stream_id,
            self.device_id,
            seq,
            hlc,
            AeadAlgId::XChaCha20Poly1305,
            epoch,
            nonce,
            Some(stream_key),
            &self.signing,
        )
    }

    /// Decode, verify, and decrypt a stored envelope back to inner-op CBOR.
    ///
    /// Every key at the envelope's `(stream_id, epoch)` is tried, because two
    /// devices can have minted that epoch concurrently.
    ///
    /// # Errors
    /// Bad magic / signature / AEAD failures, an unknown signing device, or
    /// [`KeychainError::NoStreamKey`] when this device holds no key that opens
    /// it.
    pub fn open_op(&self, envelope_bytes: &[u8]) -> Result<Vec<u8>, KeychainError> {
        let env = decode_envelope(envelope_bytes)?;
        if env.device_id != self.device_id {
            return Err(KeychainError::UnknownDevice);
        }
        for key in self.stream_keys_at(&env.stream_id, env.epoch) {
            if let Ok(inner) = open_envelope(&env, &self.device_signing_pub(), Some(&key)) {
                return Ok(inner);
            }
        }
        Err(KeychainError::NoStreamKey)
    }

    /// Cheap in-memory keychain for engine unit tests: deterministic keys, no
    /// DB row.
    #[cfg(test)]
    pub(crate) fn for_test(vault_root: VaultRootKey) -> Self {
        Self::for_test_seeded(vault_root, [7u8; 32])
    }

    /// Like [`Self::for_test`] but with a caller-chosen signing seed, so tests
    /// can construct two keychains with distinct device ids. The *identity* is
    /// fixed, so two test keychains belong to one account the way two paired
    /// devices do.
    #[cfg(test)]
    pub(crate) fn for_test_seeded(vault_root: VaultRootKey, signing_seed: [u8; 32]) -> Self {
        Self::for_test_inner(vault_root, signing_seed, true)
    }

    /// A test keychain that mints **real random** keys, for the one engine test
    /// that has to observe an op arriving before the key that opens it.
    #[cfg(test)]
    pub(crate) fn for_test_random_keys(vault_root: VaultRootKey, signing_seed: [u8; 32]) -> Self {
        Self::for_test_inner(vault_root, signing_seed, false)
    }

    #[cfg(test)]
    fn for_test_inner(
        vault_root: VaultRootKey,
        signing_seed: [u8; 32],
        test_derived_keys: bool,
    ) -> Self {
        let signing = DeviceSigningKeyPair::from_secret_bytes(&signing_seed);
        let device_id = device_id_from_pub(&signing.public_bytes());
        let mut dh_seed = [0u8; 32];
        dh_seed[..32].copy_from_slice(&signing_seed);
        dh_seed[0] ^= 0xff;
        let device_dh = DeviceDhKeyPair::from_secret_bytes(dh_seed);
        let id_signing = IdentitySigningKeyPair::from_secret_bytes(&[0x5a; 32]);
        let identity = Identity {
            identity_id: identity_id_from_pub(&id_signing.public_bytes()),
            signing: id_signing,
            dh: IdentityDhKeyPair::from_secret_bytes([0x5b; 32]),
        };
        let cert_blob = issue_cert(
            &identity,
            device_id,
            &signing,
            &device_dh,
            0,
            "test-device",
            "test",
        )
        .expect("issue test cert");
        Self {
            device_id,
            signing,
            device_dh,
            identity,
            cert_blob,
            vault_root,
            cache: Mutex::new(HashMap::new()),
            test_derived_keys,
        }
    }
}

/// The epoch every pre-ADR-0024 op was sealed under.
const LEGACY_EPOCH: u32 = 1;

/// The pre-ADR-0024 Stream-key derivation.
///
/// **Do not call this for anything but legacy adoption.** It is the line
/// ADR-0024 deletes: because every device holds the vault root, every device
/// can compute every epoch, so bumping the epoch rotates the ciphertext without
/// rotating the secret. It survives only because a vault written before the
/// change has ops sealed under these exact bytes, and adopting it means
/// recomputing them once.
///
/// Every stream a pre-ADR-0024 vault could hold ops for.
///
/// The two fixed ids are in the set unconditionally: the vault-meta stream
/// never has a `streams` row, and the Inbox's only appears once a task has
/// landed in it.
///
/// Delete at 1.0, along with [`Keychain::adopt_legacy_vault`].
fn legacy_stream_set(db: &Db) -> rusqlite::Result<std::collections::BTreeSet<[u8; 16]>> {
    let mut streams: std::collections::BTreeSet<[u8; 16]> = std::collections::BTreeSet::new();
    streams.insert(crate::engine::META_STREAM);
    streams.insert(INBOX_STREAM_BYTES);
    let conn = db.conn();
    for sql in [
        "SELECT DISTINCT stream_id FROM ops",
        "SELECT stream_id FROM streams",
    ] {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
        for row in rows {
            streams.insert(to16(&row?));
        }
    }
    Ok(streams)
}

/// Delete at 1.0, along with [`Keychain::adopt_legacy_vault`].
fn legacy_derived_stream_key(
    vault_root: &VaultRootKey,
    stream_id: &[u8; 16],
    epoch: u32,
) -> StreamKey {
    let mut km = Vec::with_capacity(32 + 16 + 4);
    km.extend_from_slice(vault_root.as_bytes());
    km.extend_from_slice(stream_id);
    km.extend_from_slice(&epoch.to_be_bytes());
    let key = derive_key_32("sunrise.stream_key.v1", &km);
    km.zeroize();
    StreamKey::from_bytes(key)
}

/// `device_id = BLAKE3.derive_key("sunrise.device_id.v1", D_S_pub)[..16]`.
fn device_id_from_pub(d_s_pub: &[u8; 32]) -> [u8; 16] {
    let bytes = derive_key("sunrise.device_id.v1", d_s_pub, 16);
    let mut out = [0u8; 16];
    out.copy_from_slice(&bytes);
    out
}

fn mint_device_keys(rng: &dyn Rng) -> (DeviceSigningKeyPair, DeviceDhKeyPair, [u8; 16]) {
    let mut seed = [0u8; 32];
    rng.fill_bytes(&mut seed);
    let signing = DeviceSigningKeyPair::from_secret_bytes(&seed);
    seed.zeroize();
    let mut dh_seed = [0u8; 32];
    rng.fill_bytes(&mut dh_seed);
    let dh = DeviceDhKeyPair::from_secret_bytes(dh_seed);
    dh_seed.zeroize();
    let device_id = device_id_from_pub(&signing.public_bytes());
    (signing, dh, device_id)
}

fn issue_cert(
    identity: &Identity,
    device_id: [u8; 16],
    signing: &DeviceSigningKeyPair,
    dh: &DeviceDhKeyPair,
    now_ms: u64,
    nickname: &str,
    platform: &str,
) -> Result<Vec<u8>, KeychainError> {
    let body = DeviceCertInner {
        v: 1,
        device_id,
        d_s_pub: signing.public_bytes(),
        d_d_pub: dh.public_bytes(),
        identity_id: identity.identity_id,
        created_at_ms: now_ms,
        nickname: nickname.to_string(),
        platform: platform.to_string(),
    };
    let cert = DeviceCert::issue(body, &identity.signing)?;
    Ok(cert.to_cbor()?)
}

/// Read the nickname/platform out of a legacy self-signed cert so adoption
/// keeps the device's name rather than renaming it behind the user's back.
fn legacy_cert_labels(cert_blob: &[u8]) -> (String, String) {
    DeviceCert::from_cbor(cert_blob).map_or_else(
        |_| {
            (
                "sunrise-device".to_string(),
                std::env::consts::OS.to_string(),
            )
        },
        |cert| (cert.body.nickname, cert.body.platform),
    )
}

fn device_aad(device_id: &[u8; 16]) -> Vec<u8> {
    prefixed_aad(LOCAL_IDENTITY_AAD_PREFIX, device_id)
}

fn device_dh_aad(device_id: &[u8; 16]) -> Vec<u8> {
    prefixed_aad(LOCAL_DH_AAD_PREFIX, device_id)
}

fn identity_aad(identity_id: &[u8; 16]) -> Vec<u8> {
    prefixed_aad(IDENTITY_AAD_PREFIX, identity_id)
}

fn prefixed_aad(prefix: &[u8], id: &[u8; 16]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(prefix.len() + 16);
    aad.extend_from_slice(prefix);
    aad.extend_from_slice(id);
    aad
}

fn wrap_secret(vault_root: &VaultRootKey, secret: &[u8; 32], aad: &[u8], rng: &dyn Rng) -> Vec<u8> {
    let mut nonce = [0u8; AEAD_NONCE_LEN];
    rng.fill_bytes(&mut nonce);
    // A 32-byte plaintext never trips the only AEAD error path.
    let ct = aead_seal_xchacha(vault_root.as_bytes(), &nonce, secret, aad)
        .expect("aead seal of 32-byte secret");
    let mut out = Vec::with_capacity(AEAD_NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    out
}

fn unwrap_secret(
    vault_root: &VaultRootKey,
    wrapped: &[u8],
    aad: &[u8],
) -> Result<[u8; 32], KeychainError> {
    if wrapped.len() != WRAPPED_SECRET_LEN {
        return Err(KeychainError::WrappedLen);
    }
    let (nonce, ct) = wrapped.split_at(AEAD_NONCE_LEN);
    let mut nonce_arr = [0u8; AEAD_NONCE_LEN];
    nonce_arr.copy_from_slice(nonce);
    let pt = aead_open_xchacha(vault_root.as_bytes(), &nonce_arr, ct, aad)
        .map_err(|_| KeychainError::VaultRootMismatch)?;
    pt.as_slice()
        .try_into()
        .map_err(|_| KeychainError::WrappedLen)
}

fn wrap_device_secrets(
    vault_root: &VaultRootKey,
    signing: &DeviceSigningKeyPair,
    dh: &DeviceDhKeyPair,
    device_id: &[u8; 16],
    rng: &dyn Rng,
) -> (Vec<u8>, Vec<u8>) {
    let mut s = signing.secret_bytes();
    let wrapped_signing = wrap_secret(vault_root, &s, &device_aad(device_id), rng);
    s.zeroize();
    let mut d = dh.secret_bytes();
    let wrapped_dh = wrap_secret(vault_root, &d, &device_dh_aad(device_id), rng);
    d.zeroize();
    (wrapped_signing, wrapped_dh)
}

fn wrap_identity(
    vault_root: &VaultRootKey,
    identity: &Identity,
    rng: &dyn Rng,
) -> (Vec<u8>, Vec<u8>) {
    let aad = identity_aad(&identity.identity_id);
    let mut s = identity.signing.secret_bytes();
    let wrapped_s = wrap_secret(vault_root, &s, &aad, rng);
    s.zeroize();
    let mut d = identity.dh.secret_bytes();
    let wrapped_d = wrap_secret(vault_root, &d, &aad, rng);
    d.zeroize();
    (wrapped_s, wrapped_d)
}

fn unwrap_identity(
    vault_root: &VaultRootKey,
    row: &AccountIdentityRow,
) -> Result<Identity, KeychainError> {
    let aad = identity_aad(&row.identity_id);
    let mut s = unwrap_secret(vault_root, &row.id_s_priv_wrapped, &aad)?;
    let signing = IdentitySigningKeyPair::from_secret_bytes(&s);
    s.zeroize();
    let mut d = unwrap_secret(vault_root, &row.id_d_priv_wrapped, &aad)?;
    let dh = IdentityDhKeyPair::from_secret_bytes(d);
    d.zeroize();
    if signing.public_bytes() != row.id_s_pub
        || dh.public_bytes() != row.id_d_pub
        || identity_id_from_pub(&signing.public_bytes()) != row.identity_id
    {
        return Err(KeychainError::IdentityIdMismatch);
    }
    Ok(Identity {
        identity_id: row.identity_id,
        signing,
        dh,
    })
}

fn insert_identity_row(
    tx: &rusqlite::Transaction<'_>,
    identity: &Identity,
    wrapped: &(Vec<u8>, Vec<u8>),
    now_ms: u64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO identity
         (id, identity_id, id_s_pub, id_d_pub, id_s_priv_wrapped, id_d_priv_wrapped, created_at_ms)
         VALUES (1, ?, ?, ?, ?, ?, ?)",
        params![
            &identity.identity_id[..],
            &identity.signing.public_bytes()[..],
            &identity.dh.public_bytes()[..],
            wrapped.0,
            wrapped.1,
            now_ms,
        ],
    )?;
    Ok(())
}

fn insert_local_identity_row(
    tx: &rusqlite::Transaction<'_>,
    device_id: &[u8; 16],
    wrapped_signing: &[u8],
    wrapped_dh: &[u8],
    cert_blob: &[u8],
    now_ms: u64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO local_identity
         (id, device_id, signing_secret_wrapped, dh_secret_wrapped, cert_blob, created_at_ms)
         VALUES (1, ?, ?, ?, ?, ?)",
        params![
            &device_id[..],
            wrapped_signing,
            wrapped_dh,
            cert_blob,
            now_ms
        ],
    )?;
    Ok(())
}

fn insert_device_row(
    tx: &rusqlite::Transaction<'_>,
    device_id: &[u8; 16],
    cert_blob: &[u8],
    nickname: &str,
    platform: &str,
    identity_id: &[u8; 16],
    d_d_pub: &[u8; 32],
    now_ms: u64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO devices
         (device_id, cert_blob, nickname, platform, created_at_ms, revoked_at_ms,
          identity_id, d_d_pub)
         VALUES (?, ?, ?, ?, ?, NULL, ?, ?)",
        params![
            &device_id[..],
            cert_blob,
            nickname,
            platform,
            now_ms,
            &identity_id[..],
            &d_d_pub[..],
        ],
    )?;
    Ok(())
}

/// Wrap and store one Stream key. Returns `true` if the row was new.
fn insert_stream_key_row(
    tx: &rusqlite::Transaction<'_>,
    vault_root: &VaultRootKey,
    stream_id: &[u8; 16],
    epoch: u32,
    key: &StreamKey,
    source: KeySource,
    rng: &dyn Rng,
    now_ms: u64,
) -> rusqlite::Result<bool> {
    let mut adapter = RngAdapter(rng);
    // `wrap_stream_key` can only fail on an AEAD size error, impossible for a
    // 32-byte key.
    let Ok(wrapped) = wrap_stream_key(vault_root, key, stream_id, epoch, &mut adapter) else {
        return Ok(false);
    };
    let key_id = stream_key_id(key);
    tx.execute(
        "INSERT OR IGNORE INTO stream_keys
         (stream_id, epoch, key_id, wrapped, source, created_at_ms)
         VALUES (?, ?, ?, ?, ?, ?)",
        params![
            &stream_id[..],
            epoch,
            &key_id[..],
            wrapped,
            source.as_str(),
            now_ms
        ],
    )?;
    Ok(tx.changes() > 0)
}

/// `(device_id, signing_secret_wrapped, dh_secret_wrapped, cert_blob)`.
struct IdentityRow {
    device_id: [u8; 16],
    signing_wrapped: Vec<u8>,
    dh_wrapped: Option<Vec<u8>>,
    cert_blob: Vec<u8>,
}

struct AccountIdentityRow {
    identity_id: [u8; 16],
    id_s_pub: [u8; 32],
    id_d_pub: [u8; 32],
    id_s_priv_wrapped: Vec<u8>,
    id_d_priv_wrapped: Vec<u8>,
}

fn load_identity_row(db: &Db) -> Result<Option<IdentityRow>, KeychainError> {
    let row = db
        .conn()
        .query_row(
            "SELECT device_id, signing_secret_wrapped, dh_secret_wrapped, cert_blob
             FROM local_identity WHERE id = 1",
            [],
            |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, Option<Vec<u8>>>(2)?,
                    r.get::<_, Vec<u8>>(3)?,
                ))
            },
        )
        .optional()?;
    Ok(row.map(|(d, s, dh, c)| IdentityRow {
        device_id: to16(&d),
        signing_wrapped: s,
        dh_wrapped: dh,
        cert_blob: c,
    }))
}

fn load_account_identity_row(db: &Db) -> Result<Option<AccountIdentityRow>, KeychainError> {
    let row = db
        .conn()
        .query_row(
            "SELECT identity_id, id_s_pub, id_d_pub, id_s_priv_wrapped, id_d_priv_wrapped
             FROM identity WHERE id = 1",
            [],
            |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                    r.get::<_, Vec<u8>>(3)?,
                    r.get::<_, Vec<u8>>(4)?,
                ))
            },
        )
        .optional()?;
    Ok(row.map(|(id, sp, dp, sw, dw)| AccountIdentityRow {
        identity_id: to16(&id),
        id_s_pub: to32(&sp),
        id_d_pub: to32(&dp),
        id_s_priv_wrapped: sw,
        id_d_priv_wrapped: dw,
    }))
}

fn to16(raw: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    let take = raw.len().min(16);
    out[..take].copy_from_slice(&raw[..take]);
    out
}

fn to32(raw: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let take = raw.len().min(32);
    out[..take].copy_from_slice(&raw[..take]);
    out
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
/// crypto crate requires.
pub(crate) struct RngAdapter<'a>(pub &'a dyn Rng);

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

    fn open(d: &mut Db, root: &VaultRootKey) -> Keychain {
        Keychain::open(d, root.clone(), &clock(), &SystemRng, None).unwrap()
    }

    #[test]
    fn first_open_creates_identity_and_rows() {
        let root = VaultRootKey::from_bytes([0xab; 32]);
        let mut d = db(&root);
        let kc = open(&mut d, &root);

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

        let n: i64 = d
            .conn()
            .query_row(
                "SELECT count(*) FROM devices WHERE device_id = ?",
                params![&kc.device_id()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);

        // The cert is signed by the ACCOUNT identity, not by the device.
        let parsed = DeviceCert::from_cbor(kc.cert_blob()).unwrap();
        parsed
            .verify_binding(&kc.identity_signing_pub(), &kc.identity_id())
            .expect("identity-signed");
        assert!(
            parsed.verify(&kc.device_signing_pub()).is_err(),
            "a self-signed cert is exactly what ADR-0024 removes"
        );
        assert_eq!(parsed.body.device_id, kc.device_id());
        assert_eq!(parsed.body.d_d_pub, kc.device_dh_pub());
    }

    #[test]
    fn second_open_loads_same_identity() {
        let root = VaultRootKey::from_bytes([0xcd; 32]);
        let mut d = db(&root);
        let first = open(&mut d, &root);
        let first_id = first.device_id();
        let first_cert = first.cert_blob().to_vec();
        let first_identity = first.identity_id();
        let first_id_d = first.identity_dh_pub();
        drop(first);

        let second = open(&mut d, &root);
        assert_eq!(second.device_id(), first_id);
        assert_eq!(second.cert_blob(), &first_cert[..]);
        assert_eq!(second.identity_id(), first_identity);
        assert_eq!(second.identity_dh_pub(), first_id_d);
    }

    #[test]
    fn wrong_vault_root_fails_cleanly() {
        let root = VaultRootKey::from_bytes([1u8; 32]);
        let mut d = db(&root);
        let _ = open(&mut d, &root);

        let bad = VaultRootKey::from_bytes([2u8; 32]);
        let err = Keychain::open(&mut d, bad, &clock(), &SystemRng, None).unwrap_err();
        assert!(matches!(err, KeychainError::VaultRootMismatch));
    }

    /// The property the derivation used to give for free, and which envelopes
    /// now have to earn: two devices sharing a vault root no longer agree on a
    /// Stream key, because the key is random.
    #[test]
    fn two_devices_on_one_root_do_not_share_keys_by_accident() {
        let root = VaultRootKey::from_bytes([9u8; 32]);
        let mut da = db(&root);
        let mut db2 = db(&root);
        let ka = open(&mut da, &root);
        let kb = open(&mut db2, &root);
        assert_ne!(ka.device_id(), kb.device_id());

        let sid = [5u8; 16];
        let a_key = da
            .with_tx(|tx| ka.mint_epoch(tx, &sid, &SystemRng, 0))
            .unwrap();
        let b_key = db2
            .with_tx(|tx| kb.mint_epoch(tx, &sid, &SystemRng, 0))
            .unwrap();
        assert_eq!(a_key.0, 1);
        assert_eq!(b_key.0, 1);
        assert_ne!(
            a_key.1, b_key.1,
            "random keys must not collide across devices"
        );
    }

    #[test]
    fn seal_open_round_trip() {
        let root = VaultRootKey::from_bytes([0x11; 32]);
        let mut d = db(&root);
        let kc = open(&mut d, &root);
        let sid = [0u8; 16];
        let (epoch, key) = d
            .with_tx(|tx| kc.mint_epoch(tx, &sid, &SystemRng, 0))
            .unwrap();
        let inner = b"inner op cbor bytes";
        let env = kc
            .seal_op_at(
                sid,
                1,
                sunrise_cbor::hlc::Hlc::at(42),
                inner,
                &SystemRng,
                epoch,
                &key,
            )
            .unwrap();
        assert_eq!(kc.open_op(&env).unwrap(), inner);
    }

    /// Two keys at one epoch: the AEAD tag picks, not a stored discriminator.
    #[test]
    fn an_op_opens_under_whichever_key_sealed_it() {
        let root = VaultRootKey::from_bytes([0x12; 32]);
        let mut d = db(&root);
        let kc = open(&mut d, &root);
        let sid = [3u8; 16];
        let (epoch, mine) = d
            .with_tx(|tx| kc.mint_epoch(tx, &sid, &SystemRng, 0))
            .unwrap();
        // A second device's concurrently minted key for the same epoch.
        let theirs = StreamKey::from_bytes([0x99; 32]);
        d.with_tx(|tx| {
            kc.absorb_stream_key(tx, &sid, epoch, &theirs, KeySource::Envelope, &SystemRng, 0)
        })
        .unwrap();
        assert_eq!(kc.stream_keys_at(&sid, epoch).len(), 2);

        for key in [&mine, &theirs] {
            let env = kc
                .seal_op_at(
                    sid,
                    1,
                    sunrise_cbor::hlc::Hlc::at(1),
                    b"payload",
                    &SystemRng,
                    epoch,
                    key,
                )
                .unwrap();
            assert_eq!(kc.open_op(&env).unwrap(), b"payload");
        }
    }

    #[test]
    fn minting_advances_the_epoch() {
        let root = VaultRootKey::from_bytes([0x13; 32]);
        let mut d = db(&root);
        let kc = open(&mut d, &root);
        let sid = [4u8; 16];
        assert_eq!(d.with_tx(|tx| kc.current_epoch_tx(tx, &sid)).unwrap(), None);
        assert_eq!(
            d.with_tx(|tx| kc.mint_epoch(tx, &sid, &SystemRng, 0))
                .unwrap()
                .0,
            1
        );
        assert_eq!(
            d.with_tx(|tx| kc.mint_epoch(tx, &sid, &SystemRng, 0))
                .unwrap()
                .0,
            2
        );
        assert_eq!(
            d.with_tx(|tx| kc.current_epoch_tx(tx, &sid)).unwrap(),
            Some(2)
        );
    }

    #[test]
    fn absorbing_the_same_key_twice_is_a_no_op() {
        let root = VaultRootKey::from_bytes([0x14; 32]);
        let mut d = db(&root);
        let kc = open(&mut d, &root);
        let sid = [6u8; 16];
        let key = StreamKey::from_bytes([0x21; 32]);
        assert!(d
            .with_tx(|tx| kc.absorb_stream_key(
                tx,
                &sid,
                1,
                &key,
                KeySource::Envelope,
                &SystemRng,
                0
            ))
            .unwrap());
        assert!(!d
            .with_tx(|tx| kc.absorb_stream_key(
                tx,
                &sid,
                1,
                &key,
                KeySource::Envelope,
                &SystemRng,
                0
            ))
            .unwrap());
        assert_eq!(kc.stream_keys_at(&sid, 1).len(), 1);
    }

    #[test]
    fn key_envelopes_round_trip_to_both_recipient_classes() {
        let root = VaultRootKey::from_bytes([0x15; 32]);
        let mut d = db(&root);
        let kc = open(&mut d, &root);
        let sid = [7u8; 16];
        let key = StreamKey::from_bytes([0x31; 32]);

        for (recipient, pubkey) in [
            (EnvelopeRecipient::Device, kc.device_dh_pub()),
            (EnvelopeRecipient::Identity, kc.identity_dh_pub()),
        ] {
            let sealed = kc
                .seal_key_envelope(&pubkey, &sid, 2, &key, &SystemRng)
                .unwrap();
            assert_eq!(
                kc.open_key_envelope(recipient, &sid, 2, &sealed).unwrap(),
                key
            );
            // Wrong epoch does not open.
            assert!(kc.open_key_envelope(recipient, &sid, 3, &sealed).is_err());
        }
    }

    /// A vault written by the pre-ADR-0024 code path opens, gains an identity,
    /// keeps its device id, and can still read the ops it already had.
    #[test]
    fn a_legacy_vault_is_adopted_once() {
        let root = VaultRootKey::from_bytes([0x77; 32]);
        let mut d = db(&root);
        let sid = [8u8; 16];

        // Build the pre-0017 state by hand: a `local_identity` row with no DH
        // secret and no `identity` row, plus an op sealed under the derived key.
        let device_signing = DeviceSigningKeyPair::from_secret_bytes(&[0x41; 32]);
        let device_id = device_id_from_pub(&device_signing.public_bytes());
        let legacy_identity = IdentitySigningKeyPair::from_secret_bytes(&[0x41; 32]);
        let legacy_cert = DeviceCert::issue(
            DeviceCertInner {
                v: 1,
                device_id,
                d_s_pub: device_signing.public_bytes(),
                d_d_pub: [0x42; 32],
                identity_id: identity_id_from_pub(&device_signing.public_bytes()),
                created_at_ms: 1,
                nickname: "old laptop".into(),
                platform: "macos".into(),
            },
            &legacy_identity,
        )
        .unwrap()
        .to_cbor()
        .unwrap();
        let mut secret = device_signing.secret_bytes();
        let wrapped = wrap_secret(&root, &secret, &device_aad(&device_id), &SystemRng);
        secret.zeroize();
        d.with_tx(|tx| {
            tx.execute(
                "INSERT INTO local_identity
                 (id, device_id, signing_secret_wrapped, cert_blob, created_at_ms)
                 VALUES (1, ?, ?, ?, 1)",
                params![&device_id[..], wrapped, legacy_cert],
            )?;
            tx.execute(
                "INSERT INTO streams
                 (stream_id, head_root, last_op_seq, created_at_ms, updated_at_ms)
                 VALUES (?, X'00', 0, 0, 0)",
                params![&sid[..]],
            )?;
            Ok(())
        })
        .unwrap();

        // An op sealed under exactly the key the old derivation produced.
        let derived = legacy_derived_stream_key(&root, &sid, LEGACY_EPOCH);
        let legacy_env = encode_envelope(
            b"a pre-0017 op",
            sid,
            device_id,
            1,
            sunrise_cbor::hlc::Hlc::at(1),
            AeadAlgId::XChaCha20Poly1305,
            LEGACY_EPOCH,
            [0x11; AEAD_NONCE_LEN],
            Some(&derived),
            &device_signing,
        )
        .unwrap();

        let kc = open(&mut d, &root);
        assert_eq!(kc.device_id(), device_id, "the device keeps its id");
        assert_eq!(
            kc.open_op(&legacy_env).unwrap(),
            b"a pre-0017 op",
            "adoption must not lose the ops the vault already had"
        );
        let parsed = DeviceCert::from_cbor(kc.cert_blob()).unwrap();
        parsed
            .verify_binding(&kc.identity_signing_pub(), &kc.identity_id())
            .expect("re-issued under the new account identity");
        assert_eq!(parsed.body.nickname, "old laptop", "the name is kept");
        assert_eq!(parsed.body.d_d_pub, kc.device_dh_pub());
        // Meta and Inbox are in the adopted set even with no rows naming them.
        assert_eq!(
            kc.stream_keys_at(&crate::engine::META_STREAM, LEGACY_EPOCH)
                .len(),
            1
        );
        assert_eq!(
            kc.stream_keys_at(&INBOX_STREAM_BYTES, LEGACY_EPOCH).len(),
            1
        );

        // Idempotent: a second open changes nothing.
        let identity_id = kc.identity_id();
        let cert = kc.cert_blob().to_vec();
        drop(kc);
        let again = open(&mut d, &root);
        assert_eq!(again.identity_id(), identity_id);
        assert_eq!(again.cert_blob(), &cert[..]);
        assert_eq!(again.open_op(&legacy_env).unwrap(), b"a pre-0017 op");
    }

    /// A device opened with a pairing payload joins the *sender's* account and
    /// inherits its keys, rather than minting an account of its own.
    #[test]
    fn a_paired_device_adopts_the_senders_identity_and_keys() {
        let root = VaultRootKey::from_bytes([0x55; 32]);
        let mut da = db(&root);
        let ka = open(&mut da, &root);
        let sid = [0x0c; 16];
        let (epoch, key) = da
            .with_tx(|tx| ka.mint_epoch(tx, &sid, &SystemRng, 0))
            .unwrap();
        let payload = ka.export_pairing_payload(&da).unwrap();

        let mut db2 = db(&root);
        let kb =
            Keychain::open(&mut db2, root.clone(), &clock(), &SystemRng, Some(&payload)).unwrap();

        assert_eq!(kb.identity_id(), ka.identity_id());
        assert_eq!(kb.identity_signing_pub(), ka.identity_signing_pub());
        assert_ne!(kb.device_id(), ka.device_id(), "a distinct device");
        assert_ne!(kb.device_dh_pub(), ka.device_dh_pub());
        assert_eq!(kb.stream_keys_at(&sid, epoch), vec![key.clone()]);

        // B's cert verifies under the shared account identity, which is what
        // makes it a member rather than a stranger.
        DeviceCert::from_cbor(kb.cert_blob())
            .unwrap()
            .verify_binding(&ka.identity_signing_pub(), &ka.identity_id())
            .expect("B's cert is issued under A's identity");

        // ...and an op A sealed is readable on B.
        let env = ka
            .seal_op_at(
                sid,
                1,
                sunrise_cbor::hlc::Hlc::at(1),
                b"from A",
                &SystemRng,
                epoch,
                &key,
            )
            .unwrap();
        let decoded = decode_envelope(&env).unwrap();
        let opened = open_envelope(
            &decoded,
            &ka.device_signing_pub(),
            Some(&kb.stream_keys_at(&sid, epoch)[0]),
        )
        .unwrap();
        assert_eq!(opened, b"from A");
    }

    #[test]
    fn a_payload_from_another_account_is_refused() {
        let root = VaultRootKey::from_bytes([0x56; 32]);
        let mut da = db(&root);
        let ka = open(&mut da, &root);
        let mut payload = ka.export_pairing_payload(&da).unwrap();
        payload.id_s_priv = [0x01; 32];

        let mut db2 = db(&root);
        let err = Keychain::open(&mut db2, root, &clock(), &SystemRng, Some(&payload)).unwrap_err();
        assert!(matches!(err, KeychainError::IdentityIdMismatch));
    }
}
