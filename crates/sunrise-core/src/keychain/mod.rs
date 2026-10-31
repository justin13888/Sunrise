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
//! `D_S` into a real [`sunrise_crypto::OpEnvelope`].
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
//! private `legacy_derived_stream_key` in the `legacy` submodule — because a
//! vault written before
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
//! - **id_s_priv_wrapped** — the same, with AAD
//!   `"sunrise.local_identity.identity.sign.v2" || identity_id`.
//! - **id_d_priv_wrapped** — the same, with AAD
//!   `"sunrise.local_identity.identity.dh.v2" || identity_id`. A *different*
//!   domain from the line above, because they are different secrets with
//!   different capabilities; one AAD for both made the two blobs
//!   interchangeable to the AEAD.
//! - **stream key** — 32 random bytes; stored as
//!   `wrap_stream_key(vault_root, key, stream_id, epoch)`.
//!
//! ## Failures that are not absences, and the ones that still are
//!
//! A failure to open, decode or verify can be raised or it can be turned into a
//! benign-looking value, and a module that does both without saying which is
//! which has a class of defect in it rather than a set of decisions. This one
//! was audited end to end for that shape. The list below is the record; every
//! site named carries the same reasoning at the call site.
//!
//! **Now errors**, because the benign value was indistinguishable from a real
//! state and a caller acted on the wrong one:
//!
//! * [`Keychain::current_stream_key_tx`] — an unopenable *live* Stream key read
//!   as `Ok(None)`, which `Engine::ensure_stream_epoch` answers by minting. One
//!   corrupt row therefore produced a full unannounced epoch rotation,
//!   broadcast to every peer, with no revocation and no error. The audit's
//!   case.
//! * [`Keychain::current_epoch_tx`] — an epoch outside `u32` read as `1`, the
//!   genesis epoch: a valid-looking answer that fed the above and would have
//!   had `mint_epoch` write epoch 2 over a stream far past it.
//! * `rows::insert_stream_key_row` — a failed wrap returned `Ok(false)`, whose
//!   meaning is "the row already existed". `absorb_stream_key` then dropped an
//!   arriving key and skipped the deferred drain; `mint_epoch` returned a key
//!   it had not persisted.
//! * [`Keychain::genesis_pair`] — a present-but-short genesis column read as
//!   "never rotated" and answered with the identity *in force*, which is the
//!   moving value the fold anchor exists not to be. Its sibling
//!   [`Keychain::genesis_identity_id`] already raised on the same blob.
//! * [`Keychain::open_op`] — a forged signature came back as
//!   [`KeychainError::NoStreamKey`], whose handling is "park and retry". The
//!   signature is now checked before the key loop, so a forgery is a forgery.
//! * `load_cache` — an epoch outside `u32` was clamped to `0` and the key
//!   *misfiled* against a real slot. Now skipped.
//!
//! * [`Keychain::rotation_set`] — a `stream_id` column that is not 16 bytes was
//!   dropped from the **revocation** set, so the revoked device went on reading
//!   that stream while `revoke_device` returned success. It still does not
//!   raise — refusing the whole revocation over one row is worse, because the
//!   device that is gone is the whole scenario — but it no longer stays quiet:
//!   the row comes back in [`RotationSet::unrotatable`], reaches the caller on
//!   `CommandResult::unrotated_streams`, and is disclosed. Same shape as issue
//!   #160 one level up.
//!
//! **Still fail open, as decisions**: `load_cache`'s skip of an unopenable row
//! and of a malformed stream id, [`Keychain::stream_keys_at`]'s empty result,
//! `Keychain::held_epochs_tx`'s dropped rows,
//! [`Keychain::device_labels_tx`]'s placeholder labels,
//! `legacy::legacy_stream_set`, `legacy::legacy_cert_labels`, and
//! `rows`'s `minted_by_device_id` and `created_at_ms`. Each says why where it
//! is. The common shape: they sit on a path where raising loses a whole vault
//! or a whole adoption over one row, and the row they drop is one nothing could
//! have used anyway.
//!
//! ## Module map
//!
//! [`Keychain`] itself stays in this file, and so does every `impl` on it. Its
//! fields are private and nearly every method reads several of them, so moving
//! methods into siblings would mean publishing the fields of the crate's only
//! key-custody type to the rest of the crate — a worse trade than a long file.
//!
//! What moved out is what never took `&self` and never touched a field:
//!
//! - `crypto` — wrapping the device and identity secrets under the vault root,
//!   and the AAD prefixes that bind each wrapped blob to the id it belongs to.
//! - `rows` — every `INSERT` and `SELECT` the keychain issues, plus the two row
//!   structs they load into.
//! - `device` — minting this device's id, its two keypairs and its cert.
//! - `legacy` — the pre-ADR-0024 adoption path, deleted whole at 1.0.
//! - `fixture` — test-only: the committed vault this build must still open,
//!   and the `#[ignore]`d generator that writes it.
//!
//! `to16` and `to32` stay here: `to16` is the crate's blob-to-id decoder
//! and `core` and `engine` reach it as `crate::keychain::to16`.

mod crypto;
mod device;
/// One real vault, sealed by this build and committed, that the keychain must
/// open. Test-only, and a module inside the crate rather than a file under
/// `tests/` because the AAD builders it asserts against are `pub(super)`.
#[cfg(test)]
mod fixture;
mod legacy;
mod rows;
#[cfg(test)]
mod tests;

use crate::config::{Clock, Rng};
use crate::engine::hex_short;
use crate::unlock::IdentitySeed;
use parking_lot::Mutex;
use rusqlite::{params, OptionalExtension};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use sunrise_crypto::aead::AEAD_NONCE_LEN;
use sunrise_crypto::blake3_kdf::derive_key_32;
use sunrise_crypto::identity_transition::{
    IdentityTransitionBody, IdentityTransitionError, IdentityTransitionSigs,
};
use sunrise_crypto::op_envelope::open_envelope;
use sunrise_crypto::{
    decode_envelope, encode_chained_envelope, hpke_open, hpke_open_identity, hpke_seal,
    identity_carry_info, identity_id_from_pub, identity_share_info, key_envelope_info,
    sign_identity_transition, unwrap_stream_key, verify_envelope, AeadAlgId, ChainLinks,
    DeviceCert, DeviceCertInner, DeviceDhKeyPair, DeviceSigningKeyPair, HpkeError,
    IdentityDhKeyPair, IdentitySigningKeyPair, OpEnvelopeError, StreamKey, VaultRootKey,
};
use sunrise_domain::PRIVATE_DOMAIN_BYTES;
use sunrise_pairing::{PairingGrant, PairingOffer, PairingPayload, PairingRequest};
use sunrise_storage::Db;
use thiserror::Error;
use zeroize::Zeroize;

use crypto::{
    device_aad, device_dh_aad, unwrap_identity, unwrap_secret, wrap_device_secrets, wrap_identity,
};
use device::{
    adopt_granted_device_keys, device_id_from_pub, issue_cert, mint_own_device_keys, DeviceMaterial,
};
use legacy::{legacy_cert_labels, legacy_derived_stream_key, legacy_stream_set, LEGACY_EPOCH};
use rows::{
    insert_device_row, insert_identity_row, insert_local_identity_row, insert_stream_key_row,
    load_account_identity_row, load_identity_row, AccountIdentityRow, IdentityRow,
};
// Named only by the tests below and by the test-only `issue_cert_for`: the
// production Stream-key path now reaches these through `device` and `rows`.
// `DeviceCert` is no longer among them — identity rotation issues certs under
// the successor from the production path, so it is imported unconditionally
// above.
#[cfg(test)]
use crypto::wrap_secret;
#[cfg(test)]
use sunrise_crypto::stream_key_id;

/// The first epoch of any stream. Named because the vault-meta stream's
/// genesis is the one epoch in this hierarchy that is derived rather than
/// drawn — see [`Keychain::meta_genesis_key`].
const GENESIS_EPOCH: u32 = 1;

/// BLAKE3 `derive_key` context for the vault-meta stream's genesis key.
///
/// Versioned like every other context string in the tree, so a future change
/// to the derivation is a new key rather than a silently different one.
const META_GENESIS_CONTEXT: &str = "sunrise.meta_genesis_key.v1";

/// The SQL predicate, over a `stream_keys` row aliased `k`, that the row is a
/// key this device may **write** under: no device in `device_read_bounds`
/// delivered it (issue #280, ADR-0041 §Decision 4).
///
/// `device_read_bounds` is the engine's table and not this module's; the
/// keychain reads it here, in one place, because the live epoch is the
/// keychain's answer and the bound is part of the question.
const WRITABLE_KEY: &str = "NOT EXISTS (
    SELECT 1 FROM stream_key_senders s
    JOIN device_read_bounds b ON b.device_id = s.sender_device_id
    WHERE s.stream_id = k.stream_id AND s.epoch = k.epoch AND s.key_id = k.key_id)";

/// Where a Stream key came from.
///
/// The route never decides whether a key is *read*: a key opens an op or it
/// does not, whichever route delivered it. It decides one thing about whether
/// the key is *written under*. An [`Envelope`](Self::Envelope) key names the
/// device that sealed it, and a key a read-bounded device sealed is never the
/// live key (see [`Keychain::current_epoch_tx`]). Carrying the sender in the
/// variant, rather than beside it, means no caller can record an envelope key
/// without saying who sent it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// Minted on this device.
    Local,
    /// Arrived in a `key_envelope` op signed by `sender`.
    Envelope {
        /// The device that signed the `key_envelope`. Opening the envelope
        /// proves it holds the key, because the `key_id` is re-derived from
        /// the opened key before this is recorded.
        sender: [u8; 16],
    },
    /// Arrived in a pairing payload.
    Pairing,
    /// Arrived in the account's key bundle, published and signed by
    /// `publisher` (ADR-0069).
    ///
    /// Recorded as a delivery by that device, exactly as an
    /// [`Envelope`](Self::Envelope) key is by its sender: a bundle is the
    /// same claim as the identity-addressed envelopes it stands in for, and a
    /// key a read-bounded device delivered is never written under however it
    /// arrived.
    Bundle {
        /// The device whose cert and signature the bundle carried.
        publisher: [u8; 16],
    },
    /// Recomputed from the pre-ADR-0024 derivation while adopting a legacy
    /// vault.
    Legacy,
    /// Carried by a recovery blob re-issued after an identity rotation: the
    /// vault-meta genesis key, which the successor identity cannot derive
    /// (ADR-0070). No device delivered it; the code did.
    Recovery,
}

impl KeySource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Envelope { .. } => "envelope",
            Self::Pairing => "pairing",
            Self::Bundle { .. } => "bundle",
            Self::Legacy => "legacy",
            Self::Recovery => "recovery",
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
    /// Recovery-blob seal failure.
    #[error("recovery blob: {0}")]
    Recovery(#[from] sunrise_crypto::RecoveryError),
    /// HPKE seal/open failure.
    #[error("key envelope: {0}")]
    Hpke(#[from] HpkeError),
    /// An `identity_transition` could not be signed or verified.
    #[error("identity transition: {0}")]
    Transition(#[from] IdentityTransitionError),
    /// Pairing payload codec failure.
    #[error("pairing payload: {0}")]
    Pairing(#[from] sunrise_pairing::PairingPayloadError),
    /// A key bundle could not be sealed, or was refused (ADR-0069).
    #[error("key bundle: {0}")]
    KeyBundle(#[from] sunrise_crypto::KeyBundleError),
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
    /// An identity-sealed `key_envelope` was offered to a device that holds no
    /// `ID_D_priv` — which is every device admitted by pairing.
    ///
    /// Not a corruption. The identity copy exists for the recovery path, and a
    /// paired device is meant to open its own `Recipient::Device` copy instead.
    #[error(
        "this device holds no identity unwrapping key; the identity copy is the recovery path's"
    )]
    IdentitySecretAbsent,
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
    /// A stored row holds a blob of the wrong length for the field it fills.
    #[error("corrupt vault row: {0} is not the right length")]
    CorruptRow(&'static str),
    /// A stored row exists and does not decode into the value it should hold —
    /// an unopenable wrap, an out-of-range integer, a blob that is not the
    /// shape its column promises.
    ///
    /// Distinct from [`Self::CorruptRow`], which is a width; this is a row that
    /// is the right shape and the wrong content. Both name the column, because
    /// the whole point of raising them is that the failure says which row to
    /// look at rather than presenting as an absence.
    #[error("unreadable vault row: {0}")]
    UnreadableRow(&'static str),
    /// A secret could not be wrapped under the vault root.
    ///
    /// Reachable only through an AEAD size error on a fixed-width key, which is
    /// to say not reachable — but the alternative to raising it was returning a
    /// value that means "this key was already stored", and a caller acting on
    /// that discards the key silently.
    #[error("could not wrap {0} under the vault root")]
    WrapFailed(&'static str),
    /// A pre-ADR-0024 vault with more than one device row cannot be adopted:
    /// see `Keychain::adopt_legacy_vault`.
    #[error(
        "this vault predates the key hierarchy and holds {0} devices; adopting it would mint a \
         different account identity on each one and silently split the account. Re-pair the other \
         devices from this one after it upgrades, or open it on a single device only."
    )]
    LegacyMultiDevice(i64),
    /// The account's `ID_S_priv` is not on this device, and the operation
    /// asked for needs it.
    ///
    /// Every device admitted by pairing is in this state, and that is the whole
    /// of `#105`'s fix: the sponsor issues the joiner's cert, the joiner is
    /// handed the public half only, and no amount of local state lets it
    /// produce a `DeviceCert` for anything afterwards. The operations that
    /// land here are the three that speak *for* the account rather than as a
    /// member of it — issuing a cert, signing an identity transition, and
    /// sealing a recovery blob.
    ///
    /// Distinct from [`Self::IdentitySecretAbsent`], which is about `ID_D_priv`
    /// and the read path. A device can easily be missing both; they are missing
    /// for different reasons and bound different things.
    #[error(
        "this device holds no account signing key: it was admitted by pairing, and only the \
         device that created the account can issue certs, rotate the identity, or seal a \
         recovery blob"
    )]
    IdentitySigningKeyAbsent,
    /// A pairing grant's `DeviceCert` does not match the keys this device
    /// minted, or is not signed by the identity the offer named.
    #[error("pairing grant certificate: {0}")]
    BadGrantCert(&'static str),
}

/// The account's Ed25519 signing key, or as much of it as this device has.
///
/// The one structural expression of `#105`'s fix. `ID_S_priv` signs every
/// `DeviceCert`, a `DeviceCert` names only its subject, and its single signature
/// is the identity's — so a device holding the secret can mint a valid cert for
/// any device id it invents, including one the account's revocation register has
/// never heard of. Pairing used to hand that secret to every device. It does
/// not now, and this type is why the difference cannot be papered over: there is
/// no `secret_bytes()` on the public arm to reach for.
///
/// An enum rather than an `Option` beside a `[u8; 32]`, because those two can
/// disagree and this cannot: [`Self::public_bytes`] answers from whichever arm
/// is present and the secret arm derives it.
pub enum IdentitySigningKey {
    /// This device holds `ID_S_priv`.
    ///
    /// True on the device that created the account, on one restored from the
    /// recovery code, and on a device that minted a successor identity itself.
    /// Not true anywhere else, and specifically never true because of pairing.
    Held(IdentitySigningKeyPair),
    /// This device holds `ID_S_pub` and nothing more.
    ///
    /// Every device admitted by pairing. It can verify any cert the account
    /// issued — including its own — and can issue none.
    PublicOnly([u8; 32]),
}

impl IdentitySigningKey {
    /// `ID_S_pub`, whichever arm this is.
    #[must_use]
    pub fn public_bytes(&self) -> [u8; 32] {
        match self {
            Self::Held(kp) => kp.public_bytes(),
            Self::PublicOnly(pk) => *pk,
        }
    }

    /// The signing key, on a device that has it.
    ///
    /// Returns a `Result` rather than an `Option` so that every caller that
    /// needs the key states, in one `?`, what it is unable to do without it.
    ///
    /// # Errors
    /// [`KeychainError::IdentitySigningKeyAbsent`] on a device admitted by
    /// pairing.
    pub fn require(&self) -> Result<&IdentitySigningKeyPair, KeychainError> {
        match self {
            Self::Held(kp) => Ok(kp),
            Self::PublicOnly(_) => Err(KeychainError::IdentitySigningKeyAbsent),
        }
    }

    /// Whether the secret is here.
    #[must_use]
    pub const fn is_held(&self) -> bool {
        matches!(self, Self::Held(_))
    }
}

impl std::fmt::Debug for IdentitySigningKey {
    /// Says which arm and never the bytes, so a `Debug` on the identity that
    /// walks into a log cannot carry the account's signing key with it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Held(_) => "IdentitySigningKey::Held(<redacted>)",
            Self::PublicOnly(_) => "IdentitySigningKey::PublicOnly",
        })
    }
}

/// Carry a [`KeychainError`] out through a `rusqlite::Result` boundary.
///
/// The engine's transaction closures are all `rusqlite::Result`, and
/// `rusqlite::Error` has no variant for "the caller's own invariant broke".
/// The house workaround has been `rusqlite::Error::ExecuteReturnedResults` as a
/// bare sentinel, which loses the message — and the whole point of the failures
/// routed through here is that they *name the column*, because each of them
/// used to present as an absence instead.
///
/// `FromSqlConversionFailure` is the variant that fits without inventing one:
/// it is literally "these bytes would not convert into the value that was
/// asked for", it carries a boxed source error, and it is not feature-gated.
pub(crate) fn sqlite_from(err: KeychainError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Blob, Box::new(err))
}

/// The fold anchor a vault being created records, and the vault-meta genesis
/// keys it is handed rather than derives.
///
/// A device being paired takes the sponsor's anchor, which is the account's; a
/// founding, adopting or recovering vault *is* the genesis and says so.
/// Inferring it from `identity` would be wrong on exactly one path and
/// silently — a device paired into an account that has already rotated would
/// record the identity it joined at and fold from a point no peer shares.
///
/// A blob re-issued after a rotation carries a successor and names the genesis
/// beside it (ADR-0070). The genesis is what every other replica folds from,
/// so it is what this one folds from too; taking the successor as the anchor
/// would leave every cert issued before the rotation verifying under nothing
/// on this chain. Such a blob also carries the vault-meta genesis keys,
/// because the successor's `ID_D_priv` derives a different one, and a vault
/// that minted its own on its first write would seal that write under a key
/// no peer holds.
fn create_anchor(
    seed: &IdentitySeed,
    identity: &Identity,
) -> (([u8; 16], [u8; 32]), Vec<StreamKey>) {
    match seed {
        IdentitySeed::Paired(p) => ((p.genesis_identity_id, p.genesis_id_s_pub), Vec::new()),
        IdentitySeed::Recovered(r) => {
            let anchor = r.account_id_s_pub.map_or(
                (identity.identity_id, identity.signing.public_bytes()),
                |pk| (identity_id_from_pub(&pk), pk),
            );
            let meta = r
                .meta_genesis_keys
                .iter()
                .map(|k| StreamKey::from_bytes(*k))
                .collect();
            (anchor, meta)
        }
        IdentitySeed::Own => (
            (identity.identity_id, identity.signing.public_bytes()),
            Vec::new(),
        ),
    }
}

/// [`sqlite_from`] for the common case: a row that exists and does not decode.
pub(crate) fn keychain_row_error(column: &'static str) -> rusqlite::Error {
    sqlite_from(KeychainError::UnreadableRow(column))
}

/// What [`Keychain::rotation_set`] found: the streams a revocation can rotate,
/// and the rows it cannot.
///
/// A pair rather than a bare `Vec` because the second list is the point. See
/// [`Keychain::rotation_set`] for why an unrotatable row is reported rather
/// than dropped or raised.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RotationSet {
    /// Every stream with a live epoch, ascending. The vault-meta stream and
    /// the private domain are always present.
    pub streams: Vec<[u8; 16]>,
    /// The distinct `stream_id` column values that are not 16 bytes, ascending.
    ///
    /// Raw bytes rather than an id type, because these are precisely the values
    /// that are *not* ids — there is no `[u8; 16]` to hand back, and padding
    /// one out is the bug `to16` exists to prevent. A caller that wants to
    /// show them renders hex.
    pub unrotatable: Vec<Vec<u8>>,
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
    /// `ID_S`, or its public half alone on a device admitted by pairing.
    ///
    /// The asymmetry this encodes is `#105`'s fix: see [`IdentitySigningKey`].
    signing: IdentitySigningKey,
    /// `ID_D_pub`. Every device holds this: sealing a `key_envelope` to the
    /// identity needs only the public half.
    dh_pub: [u8; 32],
    /// `ID_D_priv`, and only where it belongs.
    ///
    /// `Some` on the device that created the account, and on one restored from
    /// the recovery code. `None` on every device admitted by pairing, because
    /// `PairingPayload` stopped carrying it — which is what lets a revocation
    /// bound a device's reads at all (`#76`). A device with `None` here can
    /// seal to the identity and cannot open what was sealed to it, which is
    /// the asymmetry the whole mechanism rests on.
    dh_secret: Option<IdentityDhKeyPair>,
    /// When the account identity was minted, ms since the epoch.
    ///
    /// The `identity` row has carried this since migration 0017 and nothing
    /// read it back. It is field 6 of the recovery blob's plaintext
    /// (`docs/03-crypto/recovery.md` §Recovery blob construction), which calls
    /// it `created_at` and means the *account's* creation rather than the
    /// blob's — so it is loaded from the row rather than stamped at seal time.
    created_at_ms: u64,
}

impl Identity {
    /// The `identity_id` — `identity_id_from_pub(ID_S_pub)`.
    #[must_use]
    pub const fn identity_id(&self) -> [u8; 16] {
        self.identity_id
    }

    /// `ID_S_pub`.
    #[must_use]
    pub fn id_s_pub(&self) -> [u8; 32] {
        self.signing.public_bytes()
    }

    /// `ID_D_pub`.
    #[must_use]
    pub const fn id_d_pub(&self) -> [u8; 32] {
        self.dh_pub
    }

    /// Whether this device holds the account's `ID_S_priv`.
    ///
    /// False on every device admitted by pairing, which is what stops it
    /// issuing a `DeviceCert` for anything — see [`IdentitySigningKey`].
    #[must_use]
    pub const fn holds_signing_key(&self) -> bool {
        self.signing.is_held()
    }
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("identity_id", &hex_short(&self.identity_id))
            .finish_non_exhaustive()
    }
}

/// Every Stream key one device holds, indexed by `(stream_id, epoch)`.
///
/// A `Vec` per slot rather than one key: two devices can mint the same epoch
/// concurrently and both keys are kept, with the AEAD tag deciding which one
/// opens a given op.
type StreamKeyCache = Mutex<HashMap<([u8; 16], u32), Vec<StreamKey>>>;

/// The successor's public halves, as the signed body of an
/// `identity_transition` states them.
///
/// A type rather than three loose parameters because the three are only
/// meaningful together: `identity_id` is the derivation of `id_s_pub`, and
/// passing a mismatched pair is the one mistake the share openers exist to
/// catch. `Self::into_identity` is the single place that check lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SuccessorPublics {
    /// The successor `identity_id`, and the value both HPKE `info` strings
    /// bind to.
    pub identity_id: [u8; 16],
    /// The successor `ID_S_pub`.
    pub id_s_pub: [u8; 32],
    /// The successor `ID_D_pub`.
    pub id_d_pub: [u8; 32],
}

impl SuccessorPublics {
    /// Rebuild the successor [`Identity`] from an opened share.
    ///
    /// Every claim is checked against a key rather than believed: the opened
    /// signing secret must produce `id_s_pub`, `identity_id` must be its
    /// derivation, and an opened `ID_D_priv` must produce `id_d_pub`. A share
    /// is sealed under HPKE Base, which authenticates no sender, so these are
    /// the only things standing between "the signed body said so" and "the
    /// blob decrypted to something".
    fn into_identity(
        self,
        signing: IdentitySigningKey,
        dh_secret: Option<IdentityDhKeyPair>,
        created_at_ms: u64,
    ) -> Result<Identity, KeychainError> {
        if signing.public_bytes() != self.id_s_pub
            || identity_id_from_pub(&signing.public_bytes()) != self.identity_id
        {
            return Err(KeychainError::IdentityIdMismatch);
        }
        if let Some(dh) = dh_secret.as_ref() {
            if dh.public_bytes() != self.id_d_pub {
                return Err(KeychainError::IdentityIdMismatch);
            }
        }
        Ok(Identity {
            identity_id: self.identity_id,
            signing,
            dh_pub: self.id_d_pub,
            dh_secret,
            created_at_ms,
        })
    }
}

/// The account identity in force, and this device's cert under it.
///
/// One lock over both because they are one fact. A cert is the identity's
/// statement about this device, so a `cert_blob` issued by the predecessor
/// beside an [`Identity`] that is already the successor is a pair no verifier
/// accepts — and two locks would make exactly that pair reachable, for the
/// width of two acquisitions, on every replica that rotates.
struct IdentityState {
    identity: Identity,
    cert_blob: Vec<u8>,
}

/// Persistent per-device keychain.
pub struct Keychain {
    device_id: [u8; 16],
    signing: DeviceSigningKeyPair,
    device_dh: DeviceDhKeyPair,
    /// The account identity and this device's cert under it, behind a lock.
    ///
    /// Interior mutability rather than `&mut self`, and for one reason: an
    /// `identity_transition` lands while the session that absorbed it is
    /// running, and every `&self` method that signs — `issue_cert_for`, the
    /// envelope path, the recovery blob — must pick up the successor at the
    /// next call rather than keep signing under an identity the account has
    /// retired. A `&mut self` rotation would need every holder of a `&Keychain`
    /// to be gone first, which on a live session is never.
    ///
    /// `parking_lot::Mutex` for the same reason [`Self::cache`] uses one: it is
    /// the house style here and it does not poison, so a panic inside one
    /// short critical section does not make the whole keychain unusable.
    identity: Mutex<IdentityState>,
    /// The account identity this keychain was **constructed** at, which
    /// adoption deliberately never touches.
    ///
    /// The durable copy is `identity.genesis_identity_id` /
    /// `genesis_id_s_pub`, and that is what a real vault's fold reads. This is
    /// the in-memory answer for the one shape that has no such row: a keychain
    /// built by `for_test*`, which every engine unit test uses. Without it the
    /// fallback would read `self.identity`, which adoption moves — so a test
    /// vault would forget its own genesis the instant it rotated, and a cert
    /// issued under the retired identity would stop verifying. That is exactly
    /// the regression the chain exists to prevent, so the fallback must not be
    /// the thing that causes it.
    genesis: ([u8; 16], [u8; 32]),
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
    /// [`Self::for_test_random_keys`] opts an engine test back into random
    /// keys wherever it needs a key this device genuinely does not hold, so
    /// the deferral path has coverage here too.
    #[cfg(test)]
    test_derived_keys: bool,
}

impl std::fmt::Debug for Keychain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keychain")
            .field("device_id", &hex_short(&self.device_id))
            .field(
                "identity_id",
                &hex_short(&self.identity.lock().identity.identity_id),
            )
            .finish_non_exhaustive()
    }
}

impl Keychain {
    /// Load the device identity, or create it on first open.
    ///
    /// Five cases, and the fourth is the one that carries the migration:
    ///
    /// 1. **Fresh vault** — mint `ID_S`/`ID_D`, mint `D_S`/`D_D`, issue a cert
    ///    signed by `ID_S_priv`, persist both rows.
    /// 2. **Fresh vault + [`IdentitySeed::Paired`]** — adopt the identity's
    ///    public halves out of the pairing payload (`ID_S` is held as
    ///    `PublicOnly`, so this device can certify nothing, itself included:
    ///    `#105`), adopt the device keys and the cert its sponsor granted
    ///    them (`adopt_granted_device_keys`), and import every Stream key the
    ///    payload carries.
    /// 3. **Fresh vault + [`IdentitySeed::Recovered`]** — adopt the identity
    ///    out of an opened recovery blob instead, which restores `ID_S_priv`,
    ///    so this device mints its own keys and signs its own cert as case 1
    ///    does. One more difference is the whole of
    ///    `docs/03-crypto/recovery.md` §Recovery flow step 8: the
    ///    blob carries `ID_D_priv`, so this vault **keeps** it and can
    ///    therefore open the identity-addressed copy of every `key_envelope`
    ///    in the op log. It imports no Stream keys, because there is no
    ///    sibling to have sent any — it reads them out of the log instead.
    /// 4. **Pre-ADR-0024 vault** — a `local_identity` row with no `identity`
    ///    row. `Self::adopt_legacy_vault` runs, once and idempotently.
    /// 5. **Known vault** — load and unwrap, failing cleanly on a wrong root.
    ///
    /// # Errors
    /// Storage/crypto failures, or a wrong vault root / corrupt identity row.
    pub fn open(
        db: &mut Db,
        vault_root: VaultRootKey,
        clock: &dyn Clock,
        rng: &dyn Rng,
        seed: &IdentitySeed,
    ) -> Result<Self, KeychainError> {
        let local = load_identity_row(db)?;
        let identity_row = load_account_identity_row(db)?;

        let kc = match (local, identity_row) {
            (Some(local), Some(row)) => Self::load(db, vault_root, &local, &row)?,
            (Some(local), None) => Self::adopt_legacy_vault(db, vault_root, clock, rng, &local)?,
            (None, _) => Self::create(db, vault_root, clock, rng, seed)?,
        };
        kc.load_cache(db)?;
        Ok(kc)
    }

    /// Case 5: both rows present.
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

        let identity = unwrap_identity(&vault_root, row, Some(&local.device_id))?;
        Ok(Self::assemble(
            local.device_id,
            signing,
            device_dh,
            identity,
            local.cert_blob.clone(),
            vault_root,
        ))
    }

    /// Which account identity a vault being created belongs to.
    ///
    /// Split out of [`Self::create`] because it is a decision with three
    /// distinct answers and two consequences — whether this device ends up
    /// holding `ID_D_priv`, and whether it ends up holding `ID_S_priv` — and
    /// those consequences are readable in one place here rather than spread
    /// through the rest of the open path.
    fn identity_for_create(
        vault_root: &VaultRootKey,
        rng: &dyn Rng,
        seed: &IdentitySeed,
        existing: Option<&AccountIdentityRow>,
        now_ms: u64,
    ) -> Result<Identity, KeychainError> {
        match (seed, existing) {
            // A device being paired adopts the account's identity — both public
            // halves, and neither private one.
            (IdentitySeed::Paired(p), existing) => {
                let identity = Identity {
                    identity_id: p.identity_id,
                    // `#105`. A paired device gets `ID_S_pub` and cannot issue
                    // a `DeviceCert` for anything, itself included: the one it
                    // holds was issued by its sponsor, which is the only device
                    // in the account that can sign one. Before this, every
                    // paired device held the secret and a revoked one simply
                    // minted a fresh device id and certified itself back in.
                    signing: IdentitySigningKey::PublicOnly(p.id_s_pub),
                    dh_pub: p.id_d_pub,
                    // `#76`. A paired device adopts the identity's public half
                    // and never its unwrapping key.
                    dh_secret: None,
                    // A paired device cannot seal a recovery blob at all, so
                    // this stamp is never the one a blob reports.
                    created_at_ms: now_ms,
                };
                if identity_id_from_pub(&identity.signing.public_bytes()) != identity.identity_id {
                    return Err(KeychainError::IdentityIdMismatch);
                }
                if let Some(row) = existing {
                    if row.identity_id != identity.identity_id {
                        return Err(KeychainError::IdentityConflict);
                    }
                }
                Ok(identity)
            }
            // A device restored from the recovery code. Everything the blob
            // carried is checked against itself before any of it is trusted:
            // the two public halves have to be the ones the two secrets
            // produce, and the identity id has to be the one `ID_S_pub`
            // derives. A blob that passes `unseal_recovery_blob` is already
            // AEAD-bound to `identity_id`, so this is not the security
            // boundary — it is the one that stops a *malformed* blob becoming
            // a vault that quietly reads nothing.
            (IdentitySeed::Recovered(r), existing) => {
                let signing = IdentitySigningKeyPair::from_secret_bytes(&r.id_s_priv);
                let dh = IdentityDhKeyPair::from_secret_bytes(r.id_d_priv);
                if signing.public_bytes() != r.id_s_pub
                    || dh.public_bytes() != r.id_d_pub
                    || identity_id_from_pub(&signing.public_bytes()) != r.identity_id
                {
                    return Err(KeychainError::IdentityIdMismatch);
                }
                if let Some(row) = existing {
                    if row.identity_id != r.identity_id {
                        return Err(KeychainError::IdentityConflict);
                    }
                }
                Ok(Identity {
                    identity_id: r.identity_id,
                    dh_pub: r.id_d_pub,
                    // Held, not `PublicOnly`: the blob is the *only* carrier
                    // that still hands over `ID_S_priv` since #105, and it has
                    // to — a recovering account with no surviving device would
                    // otherwise come up unable to certify the very device
                    // doing the recovering, and unable ever to sponsor another.
                    signing: IdentitySigningKey::Held(signing),
                    // The point of the whole mode, and the one thing a
                    // `PairingPayload` could never carry. Without it this
                    // vault opens, syncs, and reads nothing: every
                    // `key_envelope` addressed to the identity stays shut and
                    // every op sealed under those epochs parks in
                    // `deferred_ops` until the TTL drops it — a recovery that
                    // looks exactly like success. See
                    // `docs/03-crypto/recovery.md` §Recovery flow step 8.
                    dh_secret: Some(dh),
                    // The account's creation time, off the blob. Not this
                    // moment, which is when the *recovery* happened.
                    created_at_ms: r.created_at_ms,
                })
            }
            // No `local_identity` row, so this vault is about to mint a
            // device id it does not have yet, which is by construction not the
            // one that minted an identity already sitting on disk. `None`
            // therefore withholds `ID_D_priv`, and correctly.
            (IdentitySeed::Own, Some(row)) => unwrap_identity(vault_root, row, None),
            (IdentitySeed::Own, None) => {
                let mut seed = [0u8; 32];
                rng.fill_bytes(&mut seed);
                let signing = IdentitySigningKeyPair::from_secret_bytes(&seed);
                seed.zeroize();
                let mut dh_seed = [0u8; 32];
                rng.fill_bytes(&mut dh_seed);
                let dh = IdentityDhKeyPair::from_secret_bytes(dh_seed);
                dh_seed.zeroize();
                let identity_id = identity_id_from_pub(&signing.public_bytes());
                Ok(Identity {
                    identity_id,
                    dh_pub: dh.public_bytes(),
                    // The account's creator is the one device that holds
                    // `ID_S_priv`, and so the only one that can ever issue a
                    // `DeviceCert` — for itself at the bottom of this function,
                    // and for every device it later sponsors.
                    signing: IdentitySigningKey::Held(signing),
                    // The account's creator keeps it, because until the
                    // recovery blob exists this is the only copy and dropping
                    // it would make the account unrecoverable. That is the one
                    // device a revocation cannot bound the reads of; see
                    // `docs/03-crypto/key-rotation.md` §Revocation.
                    dh_secret: Some(dh),
                    created_at_ms: now_ms,
                })
            }
        }
    }

    /// Cases 1, 2 and 3: no `local_identity` row at all.
    fn create(
        db: &mut Db,
        vault_root: VaultRootKey,
        clock: &dyn Clock,
        rng: &dyn Rng,
        seed: &IdentitySeed,
    ) -> Result<Self, KeychainError> {
        let now_ms = clock.now_ms();
        let existing = load_account_identity_row(db)?;

        let identity =
            Self::identity_for_create(&vault_root, rng, seed, existing.as_ref(), now_ms)?;

        // Where this device's keys and cert come from, and it is the whole of
        // what the three-message pairing protocol changed here.
        //
        // A founding vault mints both and signs its own cert, because it holds
        // `ID_S_priv` and is the account. A **paired** vault cannot: it holds no
        // signing key, so the cert has to have been issued for it by its
        // sponsor — which means the keys that cert names had to exist before
        // the pairing finished. The joiner minted them for `PairingRequest`,
        // the sponsor wrote them into a `DeviceCert`, and what arrives here is
        // that pair plus that cert. Minting fresh keys at this point would
        // produce a device whose only cert names a `D_S_pub` it does not hold.
        let DeviceMaterial {
            signing,
            device_dh,
            device_id,
            cert_blob,
            nickname,
            platform,
        } = match seed {
            IdentitySeed::Paired(p) => adopt_granted_device_keys(&identity, p)?,
            // A recovering vault mints its own, exactly as a founding one does:
            // the blob restored `ID_S_priv`, so it can sign its own cert, and
            // the device it is replacing is gone rather than superseded. See
            // `docs/03-crypto/key-rotation.md` §Device key rotation.
            IdentitySeed::Own | IdentitySeed::Recovered(_) => {
                mint_own_device_keys(&identity, rng, now_ms)?
            }
        };

        let wrapped_identity = wrap_identity(&vault_root, &identity, rng);
        let (wrapped_signing, wrapped_dh) =
            wrap_device_secrets(&vault_root, &signing, &device_dh, &device_id, rng);
        let imported = match seed {
            // Only pairing hands Stream keys over. A recovering device is
            // handed none and must not be: it reads them out of the op log,
            // and that is the difference between a recovery that restores
            // history and one that restores an empty vault.
            IdentitySeed::Paired(p) => p.stream_keys.clone(),
            IdentitySeed::Own | IdentitySeed::Recovered(_) => BTreeMap::new(),
        };
        let write_identity = existing.is_none();
        // Which arms of the match above ended up holding `ID_D_priv`: the
        // founder, which generated it, and the recovered vault, which was
        // handed it by the blob. A paired device holds only `ID_D_pub` and
        // must not be recorded, or `unwrap_identity` would hand it a key it
        // was deliberately not given. See [`insert_identity_row`].
        let holds_identity_secret = !matches!(seed, IdentitySeed::Paired(_));
        let minted_by = (holds_identity_secret && write_identity).then_some(&device_id);
        // The anchor, and what a re-issued blob hands over: see `create_anchor`.
        let (genesis, recovered_meta) = create_anchor(seed, &identity);

        db.with_tx(|tx| {
            if write_identity {
                insert_identity_row(tx, &identity, &wrapped_identity, minted_by, genesis)?;
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
            for key in &recovered_meta {
                insert_stream_key_row(
                    tx,
                    &vault_root,
                    &crate::engine::META_STREAM,
                    GENESIS_EPOCH,
                    key,
                    KeySource::Recovery,
                    rng,
                    now_ms,
                )?;
            }
            // The sponsor's read bound, in the transaction that creates the
            // vault, so no paired vault is ever open without it. Without it a
            // joiner learned every revocation from the relay after the fact,
            // met the ones whose revoker had since been revoked already gated,
            // and sealed keys to devices its sponsor had cut off (#282).
            if let IdentitySeed::Paired(p) = seed {
                crate::engine::adopt_sponsor_read_bounds(tx, &p.read_bounds, now_ms)?;
            }
            Ok(())
        })?;

        Ok(Self::assemble(
            device_id, signing, device_dh, identity, cert_blob, vault_root,
        ))
    }

    /// Case 4: a vault written before ADR-0024.
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
    ///
    /// # The one vault this refuses
    ///
    /// A pre-0017 account with **more than one device** cannot be adopted, and
    /// this returns [`KeychainError::LegacyMultiDevice`] rather than try.
    ///
    /// The identity is minted here, from this device's RNG. Two devices sharing
    /// a vault root that both upgrade therefore mint two *different* accounts,
    /// and nothing afterwards notices: each writes `d_d_pub` only for its own
    /// `devices` row, so the peer's stays NULL from the migration and
    /// `emit_key_envelopes` — which selects `d_d_pub IS NOT NULL` — never makes
    /// it a recipient of any new epoch. Each device's `DeviceCertPublish` then
    /// fails the other's `verify_binding`, so the NULL is never repaired. The
    /// two go on syncing the streams they already share on the legacy epoch-1
    /// keys while every stream created after the upgrade parks in the peer's
    /// `deferred_ops` forever, with nothing on either screen saying so.
    ///
    /// A silent fork of one account into two is the worst outcome available
    /// here, and it is worse than an error the user can act on. The way
    /// forward is the ordinary one: upgrade on a single device, then re-pair
    /// the others from it, which is what hands them the *same* identity.
    fn adopt_legacy_vault(
        db: &mut Db,
        vault_root: VaultRootKey,
        clock: &dyn Clock,
        rng: &dyn Rng,
        local: &IdentityRow,
    ) -> Result<Self, KeychainError> {
        let devices: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM devices", [], |r| r.get(0))?;
        if devices > 1 {
            return Err(KeychainError::LegacyMultiDevice(devices));
        }
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
            dh_pub: id_dh.public_bytes(),
            // Minted here, so this device is the account's creator and holds
            // the signing key — the same arm the founding path takes.
            signing: IdentitySigningKey::Held(id_signing),
            // A vault upgrading from pre-0017 is minting the account identity
            // here, which makes this device its creator; it keeps the secret
            // for the same reason the founder branch does.
            dh_secret: Some(id_dh),
            created_at_ms: now_ms,
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
            // A pre-0017 vault mints the account identity right here, which
            // makes this device its creator on exactly the terms the founder
            // branch of `create` is.
            // A legacy vault being adopted mints the identity here, so it is
            // its own genesis by construction.
            let genesis = (identity.identity_id, identity.signing.public_bytes());
            insert_identity_row(tx, &identity, &wrapped_identity, Some(&device_id), genesis)?;
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

        Ok(Self::assemble(
            device_id, signing, device_dh, identity, cert_blob, vault_root,
        ))
    }

    /// Put one together from parts every open path has already computed.
    ///
    /// Three paths — load, create, adopt — reached the same struct literal, and
    /// each one of them grew three lines when the identity moved behind a lock.
    /// One constructor means a fourth field added to [`IdentityState`] is one
    /// edit rather than three that can disagree.
    fn assemble(
        device_id: [u8; 16],
        signing: DeviceSigningKeyPair,
        device_dh: DeviceDhKeyPair,
        identity: Identity,
        cert_blob: Vec<u8>,
        vault_root: VaultRootKey,
    ) -> Self {
        Self {
            device_id,
            signing,
            genesis: (identity.identity_id, identity.signing.public_bytes()),
            device_dh,
            identity: Mutex::new(IdentityState {
                identity,
                cert_blob,
            }),
            vault_root,
            cache: Mutex::new(HashMap::new()),
            #[cfg(test)]
            test_derived_keys: false,
        }
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
            // A row whose id is not 16 bytes is skipped for the same reason an
            // unopenable wrap is: it is one corrupt row, not a lost vault. It
            // is *not* zero-padded into `[0u8; 16]`, which is the vault-meta
            // stream and would file a stranger's key against it.
            let Some(stream_id) = to16(&stream_id) else {
                continue;
            };
            // A negative or out-of-`u32` epoch is skipped rather than clamped.
            // `unwrap_or(0)` filed the key at epoch 0 — a real slot on a real
            // stream — which is worse than dropping it: the row was not
            // skipped, it was *misfiled*, and a key filed at the wrong epoch
            // opens nothing while looking like a key that does.
            let Ok(epoch) = u32::try_from(epoch) else {
                continue;
            };
            // **A documented fail-open.** A row whose wrap does not open under
            // this root is skipped, not raised: the rest of the vault still
            // works, and refusing to open the whole keychain over one
            // unreadable row would turn a corrupt byte into a lost vault. The
            // cost is that this and "this device never received that key" are
            // the same outcome *in the cache* — which is why
            // `current_stream_key_tx` now tells them apart at the one point of
            // use where the difference decides something.
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

    /// Sign `message` with `D_S_priv`; 64 raw Ed25519 bytes.
    ///
    /// The counterpart accessor — one that handed `D_S_priv` out — is
    /// deliberately absent. This module's whole premise is that the device
    /// secrets live wrapped under the vault root and are unwrapped only here;
    /// the one method that exports key material is called
    /// `export_vault_root_for_pairing`, at that length, so that adding a second
    /// is a decision somebody has to defend rather than a convenience.
    ///
    /// A caller that needs a request signature does not need the key, so it
    /// gets the signature. `sunrise_http_sig::sign_with` is written to take
    /// exactly this.
    #[must_use]
    pub fn sign_device(&self, message: &[u8]) -> [u8; 64] {
        self.signing.sign(message)
    }

    /// The device DH public key (`D_D_pub`) — what a `key_envelope` seals to.
    #[must_use]
    pub fn device_dh_pub(&self) -> [u8; 32] {
        self.device_dh.public_bytes()
    }

    /// The account identity id **in force**.
    ///
    /// No longer `const`, and no longer a field read: an
    /// `identity_transition` replaces it under the lock while a session is
    /// running, so a caller that cached this value across a rotation would be
    /// naming an identity the account has retired.
    #[must_use]
    pub fn identity_id(&self) -> [u8; 16] {
        self.identity.lock().identity.identity_id
    }

    /// The identity this keychain was constructed at, for a vault that has no
    /// `identity` row to read one from.
    ///
    /// See the field: this is the unit-test fallback and nothing else. A real
    /// vault answers from [`Self::genesis_identity_id`] and the row beside it.
    #[must_use]
    pub const fn constructed_at_identity(&self) -> ([u8; 16], [u8; 32]) {
        self.genesis
    }

    /// The account's fold anchor: `(genesis_identity_id, genesis_id_s_pub)`.
    ///
    /// Falls back to the identity in force when the row is absent or its
    /// columns are NULL. That is exact on a vault that has never rotated —
    /// which is every vault whose row predates migrations 0022/0023 and every
    /// keychain the unit tests build — and it is the only case the fallback can
    /// be reached in, because `insert_identity_row` has written both columns
    /// since 0023.
    ///
    /// A column that is **present and the wrong width** is not that case and no
    /// longer rides along with it. It used to: the `?`-on-`Option` chain folded
    /// `to16`/`to32` failing in with the NULLs, so a corrupted anchor byte on a
    /// rotated vault silently answered with the identity in force — which is
    /// the moving value the anchor exists not to be, and which
    /// `export_pairing_payload` then ships to every newly paired device.
    /// [`Self::genesis_identity_id`] already raised `CorruptRow` for the same
    /// blob; the two now agree.
    ///
    /// # Errors
    /// SQLite failure, or [`KeychainError::CorruptRow`] for a present column of
    /// the wrong width.
    pub fn genesis_pair(&self, db: &Db) -> Result<([u8; 16], [u8; 32]), KeychainError> {
        /// The two nullable columns, as SQLite hands them back. A `type` alias
        /// before the first statement, because `clippy::type_complexity` is
        /// denied and `clippy::items_after_statements` is too.
        type GenesisColumns = Option<(Option<Vec<u8>>, Option<Vec<u8>>)>;
        let row: GenesisColumns = db
            .conn()
            .query_row(
                "SELECT genesis_identity_id, genesis_id_s_pub FROM identity WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let found = match row {
            Some((Some(id), Some(pk))) => Some((
                to16(&id).ok_or(KeychainError::CorruptRow("identity.genesis_identity_id"))?,
                to32(&pk).ok_or(KeychainError::CorruptRow("identity.genesis_id_s_pub"))?,
            )),
            // No row, or either column NULL: the never-rotated case above.
            _ => None,
        };
        Ok(found.unwrap_or(self.genesis))
    }

    /// The identity the account **started** as, which no rotation changes.
    ///
    /// Read from the `identity` row rather than held in memory: it is the fold
    /// anchor (migration 0022), it is written once, and a copy in this struct
    /// would be a second place a rotation could forget to leave alone.
    ///
    /// # Errors
    /// SQLite failure, or a corrupt `identity` row.
    pub fn genesis_identity_id(&self, db: &Db) -> Result<Option<[u8; 16]>, KeychainError> {
        let raw: Option<Option<Vec<u8>>> = db
            .conn()
            .query_row(
                "SELECT genesis_identity_id FROM identity WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        raw.flatten()
            .map(|b| to16(&b).ok_or(KeychainError::CorruptRow("identity.genesis_identity_id")))
            .transpose()
    }

    /// The account identity signing public key (`ID_S_pub`) — every cert in
    /// this vault must verify under it.
    #[must_use]
    pub fn identity_signing_pub(&self) -> [u8; 32] {
        self.identity.lock().identity.signing.public_bytes()
    }

    /// The account identity DH public key (`ID_D_pub`) — the recovery
    /// recipient of every `key_envelope`.
    #[must_use]
    pub fn identity_dh_pub(&self) -> [u8; 32] {
        self.identity.lock().identity.dh_pub
    }

    /// Whether this vault holds `ID_D_priv`, the account identity's X25519
    /// secret — and therefore whether it can seal a recovery blob.
    ///
    /// True on exactly one device per account: the one that created it.
    /// `Keychain::create` mints the identity there and keeps `dh_secret`;
    /// every device admitted by pairing gets `dh_secret: None`, because
    /// `PairingPayload` stopped carrying the key (that is the whole of the #76
    /// read bound — see [`sunrise_pairing::payload`]).
    ///
    /// **Whether it is the *only* copy depends on whether a blob was sealed.**
    /// The second copy is the recovery blob, and [`Self::seal_recovery_blob`]
    /// has production callers on both clients: `sunrise bootstrap` seals one at
    /// account creation, and since #181 so does `RecoveryCodeModel`, the moment
    /// an Apple vault is created. Where the ceremony did not complete — an
    /// upload that failed, a vault with no relay — this device is still the
    /// only place `ID_D_priv` exists, and if it is lost the key is gone
    /// permanently: every `Recipient::Identity` copy in the op log becomes
    /// unopenable forever, and no recovery feature shipped afterwards can
    /// retrieve it, because sealing a blob needs the key it would carry.
    ///
    /// Callers should surface this, not act on it. It answers "does this
    /// device hold the key", and with it the consequence of losing this vault.
    /// Both clients now do: `sunrise devices` prints it under the listing and
    /// the Apple device list states it on this device's row (#144).
    ///
    /// See `docs/03-crypto/recovery.md` §Implementation status.
    #[must_use]
    pub fn holds_only_copy_of_identity_key(&self) -> bool {
        self.identity.lock().identity.dh_secret.is_some()
    }

    /// Whether this device can speak *for* the account rather than as a member
    /// of it: issue a `DeviceCert`, sponsor a pairing, rotate the identity.
    ///
    /// True on the device that created the account and on one restored from the
    /// recovery code. False on every device admitted by pairing, which is
    /// `#105`'s fix and not a gap — see [`IdentitySigningKey`].
    ///
    /// The sibling of [`Self::holds_only_copy_of_identity_key`], and
    /// deliberately a second question rather than the same one: that answers
    /// for `ID_D_priv` and bounds what this device can *read*, this answers for
    /// `ID_S_priv` and bounds what it can *say*. They happen to agree on every
    /// vault this build produces, and the reason they are two methods is that nothing
    /// enforces the agreement.
    ///
    /// Callers should branch on this rather than provoke the error: a "pair
    /// another device" button on a vault that cannot sponsor is a button that
    /// only ever fails, and `Engine::revoke_device` reads it to decide whether
    /// a revocation can rotate the identity as well as the Stream keys.
    #[must_use]
    pub fn can_rotate_identity(&self) -> bool {
        self.identity.lock().identity.holds_signing_key()
    }

    /// Seal this account's identity keys into a recovery blob under `seed`.
    ///
    /// `seed` is the 32 bytes behind the user's BIP-39 recovery code
    /// (`sunrise_crypto::bip39`). The result is the opaque ciphertext
    /// `POST /api/v1/accounts` stores and
    /// `GET /api/v1/accounts/me/recovery_blob` serves back;
    /// `docs/03-crypto/recovery.md` §Recovery blob construction is its format.
    ///
    /// This is the **second copy** of `ID_D_priv` that
    /// [`Self::holds_only_copy_of_identity_key`] says does not exist until it
    /// is made. Sealing one is therefore the one operation that turns that
    /// method's answer from "the only copy is here" into "this device holds
    /// the key", and the reason it is a method on the keychain rather than an
    /// exported secret: the counterpart accessor that handed `ID_S_priv` and
    /// `ID_D_priv` out would be a second `export_vault_root_for_pairing`, and
    /// this module's premise is that there is exactly one of those.
    ///
    /// # Errors
    /// [`KeychainError::IdentitySecretAbsent`] on a device admitted by
    /// pairing. That device holds `ID_D_pub` and not `ID_D_priv`, so a blob it
    /// sealed would restore an identity that cannot open a single
    /// identity-addressed `key_envelope` — a recovery code that decrypts to a
    /// useless key is worse than no recovery code, because the user believes
    /// they have one. The founding device is the only one that can produce
    /// this, which is exactly the asymmetry `#76` introduced.
    ///
    /// Also Argon2id parameter or CBOR failures, which are structural.
    pub fn seal_recovery_blob(
        &self,
        seed: &[u8; 32],
        rng: &dyn Rng,
    ) -> Result<Vec<u8>, KeychainError> {
        self.seal_recovery_blob_for(seed, None, rng)
    }

    /// [`Self::seal_recovery_blob`], for an identity that may not be the
    /// account's genesis (ADR-0070).
    ///
    /// `account` is `Some` when this keychain holds a **successor**: the
    /// genesis `ID_S_pub`, which the blob's AAD then binds because it is what
    /// the account record names, and the vault-meta genesis keys, which a
    /// vault restored from this blob cannot derive from the successor's
    /// `ID_D_priv`. Without them it would mint a vault-meta genesis of its own
    /// on its first write, and every peer would park that write for a key
    /// nobody else holds.
    ///
    /// # Errors
    /// As [`Self::seal_recovery_blob`].
    pub fn seal_recovery_blob_for(
        &self,
        seed: &[u8; 32],
        account: Option<(&[u8; 32], &[StreamKey])>,
        rng: &dyn Rng,
    ) -> Result<Vec<u8>, KeychainError> {
        let state = self.identity.lock();
        let identity = &state.identity;
        let dh = identity
            .dh_secret
            .as_ref()
            .ok_or(KeychainError::IdentitySecretAbsent)?;
        // Both halves or nothing. A blob carrying `ID_D_priv` and not
        // `ID_S_priv` would restore an identity that can read the account's
        // history and issue no cert for the device doing the restoring, which
        // is a recovery that cannot complete. In practice the two are held by
        // the same device — the creator — so this second `?` fires only on a
        // vault whose rows disagree.
        let signing = identity.signing.require()?;
        let mut payload = sunrise_crypto::recovery::RecoveryPayload {
            id_s_priv: signing.secret_bytes(),
            id_d_priv: dh.secret_bytes(),
            id_s_pub: identity.signing.public_bytes(),
            id_d_pub: identity.dh_pub,
            identity_id: identity.identity_id,
            created_at_ms: identity.created_at_ms,
            account_id_s_pub: account.map(|(pk, _)| *pk),
            meta_genesis_keys: account
                .map(|(_, keys)| keys.iter().map(|k| *k.as_bytes()).collect())
                .unwrap_or_default(),
        };
        // The salt and the nonce come from the core's injected `Rng`, through
        // the same adapter every other seal in this module uses, so the whole
        // vault draws from one source of randomness and a seeded test is
        // reproducible end to end.
        let mut adapter = RngAdapter(rng);
        let out = sunrise_crypto::seal_recovery_blob(seed, &payload, &mut adapter);
        payload.zeroize();
        Ok(out?)
    }

    /// The identity-signed device cert bytes (canonical CBOR).
    ///
    /// Owned rather than borrowed since the cert moved behind the rotation
    /// lock: a `&[u8]` would have to outlive the guard, and every caller
    /// already copied it.
    #[must_use]
    pub fn cert_blob(&self) -> Vec<u8> {
        self.identity.lock().cert_blob.clone()
    }

    /// Copy the vault root out, for handing to a newly paired device.
    ///
    /// This is the one operation that deliberately breaks the rule the rest of
    /// this module exists to enforce — that the vault root never leaves the
    /// keychain. It is no longer sufficient on its own: since ADR-0024 a device
    /// also needs the identity and the Stream keys, which is what
    /// [`Self::issue_pairing_grant`] hands over. The root is still what keys
    /// the local database, so it still has to travel.
    ///
    /// The name is long and unpleasant on purpose. Callers must send the result
    /// only through an authenticated encrypted channel
    /// (`sunrise_pairing::PairedChannel`) and drop it immediately after.
    #[must_use]
    pub fn export_vault_root_for_pairing(&self) -> VaultRootKey {
        VaultRootKey::from_bytes(*self.vault_root.as_bytes())
    }

    /// Message 1: what this device offers a joiner, before it has been asked
    /// for anything.
    ///
    /// Public material only. **This is where the export used to hand over
    /// `ID_S_priv`, the vault root and every Stream key in one shot**, and the
    /// split is `#105`: a joiner that received the signing key could mint a
    /// `DeviceCert` for any device id it invented, so a revoked one certified
    /// itself back in. Nothing the sponsor can be made to emit before it has
    /// seen a request is worth stealing now; [`Self::issue_pairing_grant`] is
    /// where the account actually changes hands, and it only runs on a request
    /// this device accepted.
    ///
    /// # Errors
    /// SQLite failures reading the device labels or the fold anchor.
    pub fn export_pairing_offer(&self, db: &Db) -> Result<PairingOffer, KeychainError> {
        let (nickname, platform) = self.local_labels(db)?;
        // The fold anchor, read from the row rather than assumed equal to the
        // identity in force. They are equal only until the account rotates
        // once, and a device paired after that would otherwise anchor its fold
        // at the identity it happened to join at — leaving two replicas of one
        // account folding from different starts and disagreeing about who the
        // account is, silently, because each stays internally consistent.
        let genesis = self.genesis_pair(db)?;
        let state = self.identity.lock();
        Ok(PairingOffer {
            id_s_pub: state.identity.signing.public_bytes(),
            id_d_pub: state.identity.dh_pub,
            identity_id: state.identity.identity_id,
            genesis_identity_id: genesis.0,
            genesis_id_s_pub: genesis.1,
            nickname,
            platform,
        })
    }

    /// Message 3: issue the joiner's `DeviceCert` and hand over the account.
    ///
    /// The half of pairing that needs `ID_S_priv`, and the reason a device
    /// admitted by pairing cannot sponsor another one. The cert is signed here,
    /// on a device that already holds the key, over the keys the joiner minted
    /// and published in its request — which is why pairing needs a round trip
    /// at all: this signature cannot exist before those keys do.
    ///
    /// `request.identity_id` is checked against this account's. A request is
    /// public material and carries no secret, so a request captured from one
    /// pairing can be handed to any sponsor; the check stops *this* device
    /// spending a grant on one. The joiner makes the matching check in
    /// [`sunrise_pairing::PairingGrant::accept`], which is the one that binds.
    ///
    /// # Errors
    /// [`KeychainError::IdentitySigningKeyAbsent`] on a device admitted by
    /// pairing, [`KeychainError::IdentityConflict`] when the request answers a
    /// different account's offer, and [`KeychainError::Cert`] for a nickname
    /// outside `1..=`[`sunrise_crypto::MAX_NICKNAME_BYTES`] bytes.
    ///
    /// `read_bounds` is this vault's `device_read_bounds`, read by the caller
    /// with `crate::engine::read_bounds_for_pairing` because the table is the
    /// engine's; the grant carries it so the joiner starts with this device's
    /// bound rather than none (issue #282).
    pub fn issue_pairing_grant(
        &self,
        request: &PairingRequest,
        read_bounds: BTreeSet<[u8; 16]>,
        now_ms: u64,
    ) -> Result<PairingGrant, KeychainError> {
        if request.identity_id != self.identity_id() {
            return Err(KeychainError::IdentityConflict);
        }
        let device_cert = self.issue_cert_for(
            request.device_id,
            request.d_s_pub,
            request.d_d_pub,
            &request.nickname,
            &request.platform,
            now_ms,
        )?;
        Ok(PairingGrant {
            device_cert,
            vault_root: *self.vault_root.as_bytes(),
            stream_keys: self.held_stream_keys(),
            read_bounds,
        })
    }

    /// Every Stream key this device holds: `stream_id -> epoch -> key`.
    ///
    /// What a [`PairingGrant`] carries, and separately readable because it is
    /// also the honest answer to "did a revocation actually bound this device's
    /// reads" — a question that has to be answerable on the **revoked** device,
    /// which since `#105` cannot sponsor a pairing and so cannot be asked
    /// through [`Self::issue_pairing_grant`]. Asking what a device holds beats
    /// asking whether it has had time to receive something.
    ///
    /// One key per `(stream, epoch)`. Where two devices minted the same epoch
    /// concurrently the other still reaches a joiner through the `key_envelope`
    /// op that distributed it; carrying both would double the message already
    /// most likely to hit the transport limit.
    #[must_use]
    pub fn held_stream_keys(&self) -> BTreeMap<[u8; 16], BTreeMap<u32, [u8; 32]>> {
        let mut stream_keys: BTreeMap<[u8; 16], BTreeMap<u32, [u8; 32]>> = BTreeMap::new();
        for ((stream_id, epoch), keys) in self.cache.lock().iter() {
            if let Some(first) = keys.first() {
                stream_keys
                    .entry(*stream_id)
                    .or_default()
                    .insert(*epoch, *first.as_bytes());
            }
        }
        stream_keys
    }

    /// The keys a share of `streams` carries: every epoch this device holds
    /// of each named Stream, and of nothing else (ADR-0046 §2).
    ///
    /// It takes [`StreamId`](sunrise_domain::StreamId)s, which cannot name the
    /// vault-meta domain or the private domain, so a stream-scoped grant can
    /// never carry either: the private domain holds the account's stream-less
    /// tasks and blocks, and is shared only by moving them into a Stream. The
    /// sharing flow (`docs/03-crypto/sharing-with-others.md`) is not built;
    /// this is the selection it makes.
    #[must_use]
    pub fn stream_grant_keys(
        &self,
        streams: &[sunrise_domain::StreamId],
    ) -> BTreeMap<[u8; 16], BTreeMap<u32, [u8; 32]>> {
        let named: std::collections::BTreeSet<[u8; 16]> = streams
            .iter()
            .map(|s| sunrise_domain::KeyDomain::Stream(*s).bytes())
            .collect();
        let mut held = self.held_stream_keys();
        held.retain(|id, _| named.contains(id));
        held
    }

    /// Run a whole pairing against this sponsor, in one process, and return
    /// what the joiner would hand `Keychain::create`.
    ///
    /// Offer, request, grant, accept — the same four calls and the same checks a
    /// real driver makes, with the messages handed straight across instead of
    /// through a Noise channel or a pair of files. There is no transport here at
    /// all, which is the point: the protocol is transport-agnostic and this is
    /// what that claim looks like when it is exercised.
    ///
    /// Exported rather than `#[cfg(test)]` because three crates' tests need it —
    /// `sunrise-core`'s own, `sunrise-e2e`'s two-replica fixtures, and
    /// `sunrise-cli`'s — and a helper copied three times is three helpers that
    /// drift. It is not a shortcut around the protocol and cannot be made into
    /// one: it goes through [`Self::issue_pairing_grant`], so a sponsor that
    /// holds no `ID_S_priv` fails here exactly as it would on a real device.
    ///
    /// `seed_s` and `seed_d` are the joiner's device keys, taken explicitly so a
    /// seeded test is reproducible.
    ///
    /// # Errors
    /// Whatever [`Self::issue_pairing_grant`] refuses, plus
    /// [`KeychainError::Pairing`] if the joiner rejects the cert it is issued.
    pub fn pair_device_in_process(
        &self,
        db: &Db,
        nickname: String,
        platform: String,
        seed_s: [u8; 32],
        seed_d: [u8; 32],
        now_ms: u64,
    ) -> Result<PairingPayload, KeychainError> {
        let offer = self.export_pairing_offer(db)?;
        let joiner = sunrise_pairing::PairingJoiner::new(offer, nickname, platform, seed_s, seed_d);
        let read_bounds = crate::engine::read_bounds_for_pairing(db.conn())?;
        let grant = self.issue_pairing_grant(joiner.request(), read_bounds, now_ms)?;
        Ok(joiner.accept(grant)?)
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

    /// **Every** `(stream_id, epoch)` this device holds a key for.
    ///
    /// It used to be `MAX(epoch)` per stream, and the reasoning recorded
    /// alongside it was that older epochs had already travelled in the new
    /// device's `PairingPayload`. That holds only when a device pairs and is
    /// certified in one uninterrupted step. A rotation between the two — and
    /// one is minted whenever a Stream is created, which is an ordinary event
    /// rather than a rare one — left the device with the payload's epochs and
    /// the current one, and a permanent hole where the epochs in between should
    /// have been. Ops sealed under those parked in `deferred_ops` until
    /// [`DEFERRED_TTL_MS`](crate::engine) dropped them, and nothing surfaced
    /// it: it presents as "some items from around when I set up this device
    /// never arrived". That was
    /// [#107](https://github.com/justin13888/Sunrise/issues/107).
    ///
    /// The cost of the wider set is real and is bounded twice over.
    /// `key_envelope_recipients` (migration 0018) records who has been sent
    /// which `(stream, epoch)`, so each pair is sealed to a given device **at
    /// most once per replica** however many certs it republishes — the `S x E x
    /// D` figure is the worst case of a first backfill, not a recurring one.
    /// And the epochs a replica holds are the epochs that stream has ever had
    /// on it, which grows only by rotation.
    ///
    /// A tempting third bound was rejected: deriving the set from the epochs
    /// still referenced by parked `deferred_ops`. Those rows live on the
    /// replica running the backfill and describe what *it* could not open — a
    /// replica that has been online throughout has none, and the device that
    /// actually needs the keys is the one whose state nobody here can see. It
    /// bounds the wrong replica's ignorance.
    ///
    /// Read from `stream_keys` rather than the in-memory cache so that a key
    /// absorbed inside the caller's own open transaction is included: the cache
    /// is loaded at open and refreshed on absorption, and a backfill running in
    /// the same transaction as the absorption that provoked it would otherwise
    /// miss exactly the epoch it was called about.
    ///
    /// Sorted so that two devices running the same backfill emit the same
    /// envelopes in the same order, which keeps a diff of two vaults' op logs
    /// readable.
    pub(crate) fn held_epochs_tx(
        tx: &rusqlite::Transaction<'_>,
    ) -> rusqlite::Result<Vec<([u8; 16], u32)>> {
        let mut stmt = tx.prepare(
            "SELECT DISTINCT stream_id, epoch FROM stream_keys ORDER BY stream_id, epoch",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // **A documented fail-open.** A `stream_keys` row whose id is not 16
        // bytes, or whose epoch is outside `u32`, is dropped from the set this
        // returns — which is the set `backfill_key_envelopes` seals to a newly
        // certified device, so the device is never sent that `(stream, epoch)`
        // and its ops park until the deferred TTL drops them. That is #107's
        // failure mode arriving from a different direction.
        //
        // Left open, because the alternative is worse in the direction that
        // matters here. This runs on the *sender's* side of an admission, and a
        // hard error would refuse to backfill any of a newly paired device's
        // keys because one unrelated row in the table is malformed — turning a
        // corrupt byte on one stream into a device that can read nothing. The
        // rows that are skipped are rows this device cannot use either:
        // `current_epoch_tx` now refuses the same epoch and
        // `current_stream_key_tx` refuses the same wrap, so the stream is
        // already loudly broken locally and the peer's silence about it is not
        // what a reader will notice first.
        Ok(rows
            .into_iter()
            .filter_map(|(sid, epoch)| Some((to16(&sid)?, u32::try_from(epoch).ok()?)))
            .collect())
    }

    /// Every key this device holds at `(stream_id, epoch)`.
    ///
    /// Plural because two devices can mint the same epoch concurrently and both
    /// keys are retained; the AEAD tag decides which one actually opens an op.
    /// Losing one to a last-writer-wins on the minting op would lose every op
    /// sealed under it.
    ///
    /// # This is infallible on purpose, and what that costs
    ///
    /// An empty result is "no key here", and it does not distinguish "this
    /// device never received one" from "`load_cache` had a row and could not
    /// open it". That is a **documented fail-open**: the caller is the deferral
    /// path, which parks an op it cannot open and retries it after every
    /// absorbed key, and parking is the right answer for the common case by a
    /// very large margin — a `key_envelope` that has not arrived yet is
    /// ordinary and self-resolving.
    ///
    /// The case where the difference decides something is the *live* key at the
    /// *current* epoch, because there the answer decides whether to mint. That
    /// is [`Self::current_stream_key_tx`], which reads the row rather than this
    /// cache and now raises rather than reporting an absence.
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

    /// The live epoch for `stream_id`: the one this device seals its own writes
    /// under, read from the table inside `tx`.
    ///
    /// On an ordinary stream it is the highest epoch held, **provided that
    /// epoch holds a key no read-bounded device delivered** (issue #280,
    /// ADR-0041 §Decision 4). A key a read-bounded device sealed to this one
    /// is still stored, still in the cache, and still opens every op sealed
    /// under it; it is only never chosen to write under. Without that, a
    /// revoked device running a modified client could mint an epoch above the
    /// live one, seal it to every honest device's public `D_D`, and read what
    /// they wrote next, because this was `MAX(epoch)`.
    ///
    /// When the highest epoch is not writable the answer is `None`, never an
    /// older writable epoch below it. The epoch above exists because a
    /// rotation superseded the ones below, and a device an earlier revocation
    /// cut held every epoch sealed before its cut. Falling back would write
    /// under one of them: with Q revoked and R the sole sender of the
    /// rotation that cut Q, bounding R later must not send this device back
    /// to an epoch Q holds.
    ///
    /// What this cannot see is who holds a key minted here or delivered by a
    /// current device. An epoch live before a revocation stays live after it,
    /// on a replica where no bounded device delivered it, and the revoked
    /// device holds it until the rotation's envelope arrives and moves the
    /// highest epoch past it.
    ///
    /// The bound is read here, at the moment of choosing, and not when the
    /// key arrived, so a revocation applied after the envelope still takes
    /// effect. It is `device_read_bounds`, the engine's per-replica ratchet,
    /// for the reason every key-distribution site reads it: the class to keep
    /// out is the devices whose reads are bounded, and an unwound device is
    /// in it while the register has stopped naming it.
    ///
    /// # When no key is writable
    ///
    /// That happens on a stream whose highest epoch the revoked device
    /// delivered here, by its own rotation or by `backfill_key_envelopes` when
    /// this device's cert arrived. An epoch minted here, or delivered only by
    /// current devices, stays writable. What happens next depends on the
    /// stream.
    ///
    /// * **An ordinary stream reads as having no live key**, so
    ///   `Engine::ensure_stream_epoch` mints one above every held key and
    ///   seals it to every unbounded device. On the revoking device the
    ///   rotation has already done that; elsewhere it is a mint the rotation's
    ///   own envelope would otherwise have supplied a moment later.
    /// * **The vault-meta stream never mints here.** It reads as its highest
    ///   writable epoch and, failing one, as the highest epoch held whoever
    ///   delivered it. It carries the revocation itself, and the revocation
    ///   is sealed under the epoch the cut device still holds so that the cut
    ///   device, and every peer that has not yet received the rotation, can
    ///   read it. A meta mint here would also seal its own `key_envelope`s
    ///   under the epoch being minted, which no other device can open. The
    ///   residual is a window: on a replica that has bounded a device and not
    ///   yet received the rotation's meta key, meta ops are written under an
    ///   epoch a revoked device holds, either one the bounded device planted
    ///   or an older one an earlier-revoked device was sealed before its cut.
    ///   The rotation's envelopes follow the revocation in the same stream,
    ///   so the window closes as the rotation is applied.
    ///
    /// [`Self::max_epoch_tx`] is the other question, "what is the highest
    /// epoch this device holds at all", and it is what minting counts from.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn current_epoch_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        stream_id: &[u8; 16],
    ) -> rusqlite::Result<Option<u32>> {
        if *stream_id == crate::engine::META_STREAM {
            let sql = format!(
                "SELECT MAX(k.epoch) FROM stream_keys k WHERE k.stream_id = ? AND {WRITABLE_KEY}"
            );
            return match Self::epoch_query(tx, &sql, stream_id)? {
                None => self.max_epoch_tx(tx, stream_id),
                found => Ok(found),
            };
        }
        // The highest held epoch, and only if it is writable. Never an older
        // one below it: the epoch above exists because someone rotated, and
        // the epochs it superseded are held by whoever that rotation cut.
        let Some(max) = self.max_epoch_tx(tx, stream_id)? else {
            return Ok(None);
        };
        let writable: bool = tx.query_row(
            &format!(
                "SELECT EXISTS (SELECT 1 FROM stream_keys k
                 WHERE k.stream_id = ? AND k.epoch = ? AND {WRITABLE_KEY})"
            ),
            params![&stream_id[..], max],
            |r| r.get(0),
        )?;
        Ok(writable.then_some(max))
    }

    /// The highest epoch this device holds a key for on `stream_id`, whoever
    /// delivered it.
    ///
    /// Minting counts from this and not from [`Self::current_epoch_tx`], so a
    /// fresh epoch lands above every key a read-bounded device delivered
    /// rather than beside one of them. The `MAX_EPOCH_LEAP` bound on an
    /// absorbed epoch reads it too: it is a bound on how far this device's
    /// epochs can be dragged, which is a question about every key it holds.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn max_epoch_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        stream_id: &[u8; 16],
    ) -> rusqlite::Result<Option<u32>> {
        Self::epoch_query(
            tx,
            "SELECT MAX(epoch) FROM stream_keys WHERE stream_id = ?",
            stream_id,
        )
    }

    fn epoch_query(
        tx: &rusqlite::Transaction<'_>,
        sql: &str,
        stream_id: &[u8; 16],
    ) -> rusqlite::Result<Option<u32>> {
        let epoch: Option<i64> = tx
            .query_row(sql, params![&stream_id[..]], |r| r.get(0))
            .optional()?
            .flatten();
        // A negative or out-of-`u32` epoch used to read as `1`, the genesis
        // epoch — a valid-looking answer that sends the key lookup to a row
        // that is not there and `mint_epoch` to overwrite a stream that is far
        // past it. There is no safe substitute for an epoch, so there is no
        // substitute.
        epoch
            .map(|e| u32::try_from(e).map_err(|_| keychain_row_error("stream_keys.epoch")))
            .transpose()
    }

    /// The live `(epoch, key)` for `stream_id`, or `None` if this device holds
    /// no key for it at all.
    ///
    /// When two devices minted the same epoch this returns the lowest `key_id`
    /// among the keys [`Self::current_epoch_tx`] may choose, so a device's
    /// choice of which to seal under is deterministic rather than dependent on
    /// row order, and a key a read-bounded device delivered at the live epoch
    /// is passed over there too.
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
        // A writable key first, then the lowest `key_id`. At an epoch
        // `current_epoch_tx` chose for holding a writable key, that is the
        // lowest writable one. At the vault-meta fallback epoch none is
        // writable, and it is the plain lowest.
        let wrapped: Option<Vec<u8>> = tx
            .query_row(
                &format!(
                    "SELECT k.wrapped FROM stream_keys k
                     WHERE k.stream_id = ? AND k.epoch = ?
                     ORDER BY CASE WHEN {WRITABLE_KEY} THEN 0 ELSE 1 END, k.key_id
                     LIMIT 1"
                ),
                params![&stream_id[..], epoch],
                |r| r.get(0),
            )
            .optional()?;
        let Some(wrapped) = wrapped else {
            return Ok(None);
        };
        // An unopenable *live* key is an error, not an absence.
        //
        // `Ok(None)` here meant "this stream has never had a key", which is
        // what line 1 of this function returns when the epoch row is genuinely
        // missing — and the sole caller, `Engine::ensure_stream_epoch`, answers
        // it by minting. `mint_epoch` reads `MAX(epoch)` and bumps it, so the
        // mint is not a repair: it is epoch N+1, broadcast to every peer as
        // `key_envelope` ops, with every subsequent op sealed under it. One
        // corrupt or wrong-root row therefore produced a full, silent,
        // unannounced rotation — no revocation, no operator action, no error —
        // and parked every peer's ops at epoch N until the deferred TTL dropped
        // them. The corrupt row survived and never re-triggered, because
        // `MAX(epoch)` now pointed past it.
        //
        // `load_cache` still skips such a row at open (one unreadable row is
        // not a lost vault, and that is the right call there, because the rest
        // of the vault works). This is the point of use, where the answer
        // decides whether to mint, and here the two cases have to be told
        // apart.
        match unwrap_stream_key(&self.vault_root, &wrapped, stream_id, epoch) {
            Ok(key) => Ok(Some((epoch, key))),
            Err(_) => Err(keychain_row_error("stream_keys.wrapped")),
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
        // Above every key held, not only above the live one. Minting beside a
        // key a read-bounded device delivered would share its epoch number,
        // and on a replica that has not bounded that device the two would tie
        // on epoch and be split by `key_id`, a coin the minter does not hold.
        // It also keeps the one derived key, the vault-meta genesis, from ever
        // being minted over a row that already holds it: epoch 1 is chosen
        // only when this stream holds no key at all.
        let epoch = self
            .max_epoch_tx(tx, stream_id)?
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
        if let Some(key) = self.meta_genesis_key(stream_id, epoch) {
            return key;
        }
        let mut bytes = [0u8; 32];
        rng.fill_bytes(&mut bytes);
        let key = StreamKey::from_bytes(bytes);
        bytes.zeroize();
        key
    }

    /// The vault-meta stream's **genesis** key, derived from the account
    /// identity instead of drawn at random. `None` for every other
    /// `(stream, epoch)` and on any device that does not hold `ID_D_priv`.
    ///
    /// # Why one key in this hierarchy is derived
    ///
    /// ADR-0024 decision 4 seals every `(stream_id, epoch)` key to the
    /// identity's `ID_D_pub` as well as to each device, "so the recovery path
    /// can reach them". Every one of those `key_envelope` ops lives in the
    /// **vault-meta stream**, and an op in a stream is sealed under that
    /// stream's key — including, at epoch 1, the ops that carry that very key.
    /// [`crate::engine::Engine::emit_key_envelopes`] passes `seal_under: None`
    /// at a genesis mint, and `emit_control_op` resolves that to the
    /// vault-meta epoch it is in the middle of minting.
    ///
    /// So the identity copy of the vault-meta genesis key is sealed under the
    /// vault-meta genesis key. A recovering device holds `ID_D_priv` and can
    /// open nothing with it: the outer AEAD never yields the inner HPKE
    /// ciphertext, the op parks in `deferred_ops`, and every later epoch —
    /// which is sealed under its predecessor, correctly — is stranded behind
    /// it. The vault syncs, applies nothing, and presents as an empty account,
    /// which is what `docs/03-crypto/recovery.md` §Recovery flow step 8 warns
    /// looks exactly like success.
    ///
    /// Deriving this one key from `ID_D_priv` breaks the cycle and nothing
    /// else: the recovering vault recomputes the founder's vault-meta genesis
    /// key, opens the log, and takes every other Stream key out of it through
    /// the identity envelopes that were already being emitted.
    ///
    /// # What it costs
    ///
    /// Nothing that `ID_D_priv` did not already cost. `recovery.md` §The cost
    /// of that, stated plainly already records that `ID_D_priv` is a long-lived
    /// unwrapping key whose compromise reaches every epoch ever sealed to it;
    /// this makes the genesis epoch reachable by the same key rather than by a
    /// second one. It is gated by the same 256-bit recovery code through the
    /// same Argon2id, it adds no primitive outside ADR-0004's frozen suite
    /// (BLAKE3 `derive_key`, which already produces `device_id` and
    /// `stream_key_id`), and it changes no stored or wire format: the result is
    /// an ordinary `stream_keys` row.
    ///
    /// A device admitted by **pairing** holds no `ID_D_priv` and returns `None`
    /// here, which is correct and costs it nothing: its payload carried the
    /// account's epochs, so it never mints a vault-meta genesis at all.
    fn meta_genesis_key(&self, stream_id: &[u8; 16], epoch: u32) -> Option<StreamKey> {
        if *stream_id != crate::engine::META_STREAM || epoch != GENESIS_EPOCH {
            return None;
        }
        // Under the lock for the same reason every other reader is: identity
        // rotation swaps this in place, and a genesis key derived from half of
        // one identity and half of another would open nothing.
        let state = self.identity.lock();
        let dh = state.identity.dh_secret.as_ref()?;
        let mut ikm = [0u8; 48];
        let mut secret = dh.secret_bytes();
        ikm[..32].copy_from_slice(&secret);
        ikm[32..].copy_from_slice(&state.identity.identity_id);
        secret.zeroize();
        let mut bytes = derive_key_32(META_GENESIS_CONTEXT, &ikm);
        ikm.zeroize();
        let key = StreamKey::from_bytes(bytes);
        bytes.zeroize();
        Some(key)
    }

    fn cache_insert(&self, stream_id: &[u8; 16], epoch: u32, key: &StreamKey) {
        let mut cache = self.cache.lock();
        let slot = cache.entry((*stream_id, epoch)).or_default();
        if !slot.iter().any(|k| k == key) {
            slot.push(key.clone());
        }
    }

    /// Every `(stream_id, live epoch)` this device holds a key for, **and the
    /// rows it could not turn into one**.
    ///
    /// This is the rotation set: revoking a device mints a new epoch for every
    /// stream in it, the vault-meta stream and the private domain included
    /// (ADR-0046 §2: it rotates with every Stream, without exception).
    ///
    /// # Why this returns two lists and not one
    ///
    /// A `stream_keys`, `streams` or `ops` row whose `stream_id` is not 16
    /// bytes cannot be rotated — the id names nothing, and there is no stream
    /// to mint an epoch for. It is *not* zero-padded into `[0u8; 16]`, which is
    /// the vault-meta stream and would file a stranger's key against it.
    ///
    /// Dropping it silently is the part that was wrong. This is the revocation
    /// set, so a row dropped here is a stream the revoked device goes on
    /// reading — and `revoke_device` used to return success having not rotated
    /// it. A security operation that reports success without having done the
    /// thing is the failure this module spent the rest of its audit removing.
    ///
    /// Raising instead would be worse, and that is not the alternative. The
    /// user's answer to "my laptop was stolen" must not be an error that leaves
    /// *every* stream unrotated, because the device that is gone is the whole
    /// scenario. So the revocation succeeds partially and says so: the caller
    /// rotates [`RotationSet::streams`] and reports
    /// [`RotationSet::unrotatable`], and a user learns their revocation is
    /// incomplete and which rows it could not reach.
    ///
    /// The same distinction one level up is issue #160 and
    /// [`Core::relay_revocation_pending`](crate::Core::relay_revocation_pending):
    /// a revocation has halves with different guarantees and a user is entitled
    /// to know which they have.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn rotation_set(&self, tx: &rusqlite::Transaction<'_>) -> rusqlite::Result<RotationSet> {
        let mut streams: std::collections::BTreeSet<[u8; 16]> = std::collections::BTreeSet::new();
        let mut unrotatable: std::collections::BTreeSet<Vec<u8>> =
            std::collections::BTreeSet::new();
        streams.insert(crate::engine::META_STREAM);
        streams.insert(PRIVATE_DOMAIN_BYTES);
        for sql in [
            "SELECT DISTINCT stream_id FROM stream_keys",
            "SELECT stream_id FROM streams",
            "SELECT DISTINCT stream_id FROM ops",
        ] {
            let mut stmt = tx.prepare(sql)?;
            let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
            for row in rows {
                let raw = row?;
                // A `BTreeSet`, because the same malformed blob can appear in
                // all three queries and a user counting "streams I could not
                // rotate" must not be told three.
                match to16(&raw) {
                    Some(id) => {
                        streams.insert(id);
                    }
                    None => {
                        unrotatable.insert(raw);
                    }
                }
            }
        }
        Ok(RotationSet {
            streams: streams.into_iter().collect(),
            unrotatable: unrotatable.into_iter().collect(),
        })
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
                // Absent on every device pairing admitted. Reaching here is
                // not a corrupt vault: it means this device was offered an
                // envelope addressed to the recovery path and has no business
                // opening it. Its own `Recipient::Device` copy is what it is
                // supposed to use, and `backfill_key_envelopes` is what mints
                // that copy when an epoch predates the device.
                let state = self.identity.lock();
                let dh = state
                    .identity
                    .dh_secret
                    .as_ref()
                    .ok_or(KeychainError::IdentitySecretAbsent)?;
                hpke_open_identity(dh, &info, sealed, b"")?
            }
        };
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| KeychainError::WrappedLen)?;
        Ok(StreamKey::from_bytes(arr))
    }

    // ---- the key bundle (ADR-0069) ----

    /// Every Stream key this device may **write** under, with its epoch: what
    /// the account's key bundle carries.
    ///
    /// A key a read-bounded device delivered is left out (the `WRITABLE_KEY` predicate).
    /// The bundle is read by a device that knows nothing else about the
    /// account yet, and it credits every key to this device; carrying such a
    /// key would launder it into one the reader writes under, which is the
    /// hole issue #280 closed for envelopes.
    ///
    /// # Errors
    /// SQLite failure, or [`KeychainError::UnreadableRow`] for an epoch that
    /// does not fit.
    pub fn writable_stream_keys_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
    ) -> Result<Vec<sunrise_crypto::BundledKey>, KeychainError> {
        let rows: Vec<(Vec<u8>, i64, Vec<u8>)> = {
            let sql = format!(
                "SELECT k.stream_id, k.epoch, k.key_id FROM stream_keys k WHERE {WRITABLE_KEY}"
            );
            let mut stmt = tx.prepare(&sql)?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let cache = self.cache.lock();
        let mut out = Vec::with_capacity(rows.len());
        for (stream, epoch, key_id) in rows {
            // A malformed stream id is skipped as `load_cache` skips it: it
            // names no stream, so no reader could use the key.
            let Some(stream_id) = to16(&stream) else {
                continue;
            };
            let epoch = u32::try_from(epoch).map_err(|_| KeychainError::UnreadableRow("epoch"))?;
            let held = cache.get(&(stream_id, epoch)).and_then(|keys| {
                keys.iter()
                    .find(|k| sunrise_crypto::stream_key_id(k).as_slice() == key_id.as_slice())
            });
            if let Some(key) = held {
                out.push(sunrise_crypto::BundledKey {
                    stream_id,
                    epoch,
                    key: key.clone(),
                });
            }
        }
        Ok(out)
    }

    /// Seal `keys` into a key bundle for the identity in force, signed by this
    /// device.
    ///
    /// Any device can: sealing needs `ID_D_pub` only, which every device holds.
    ///
    /// # Errors
    /// [`KeychainError::KeyBundle`].
    pub fn seal_key_bundle(
        &self,
        keys: &[sunrise_crypto::BundledKey],
        now_ms: u64,
        rng: &dyn Rng,
    ) -> Result<Vec<u8>, KeychainError> {
        let (identity_id, dh_pub, cert) = {
            let state = self.identity.lock();
            (
                state.identity.identity_id,
                state.identity.dh_pub,
                state.cert_blob.clone(),
            )
        };
        let mut adapter = RngAdapter(rng);
        Ok(sunrise_crypto::seal_key_bundle(
            &identity_id,
            &dh_pub,
            &cert,
            now_ms,
            keys,
            &|m| self.signing.sign(m),
            &mut adapter,
        )?)
    }

    /// Decode and verify a key bundle against the identity in force: sealed to
    /// it, published by a device it certified, signed by that device.
    ///
    /// # Errors
    /// [`KeychainError::KeyBundle`] naming which check failed.
    pub fn verify_key_bundle<'a>(
        &self,
        bundle: &'a sunrise_crypto::KeyBundle,
    ) -> Result<sunrise_crypto::VerifiedKeyBundle<'a>, KeychainError> {
        let (identity_id, id_s_pub) = {
            let state = self.identity.lock();
            (
                state.identity.identity_id,
                state.identity.signing.public_bytes(),
            )
        };
        Ok(bundle.verify(&identity_id, &id_s_pub)?)
    }

    /// Open a verified key bundle with `ID_D_priv`.
    ///
    /// # Errors
    /// [`KeychainError::IdentitySecretAbsent`] on a device that holds no
    /// `ID_D_priv` (every paired one), [`KeychainError::KeyBundle`] for keys
    /// that do not open.
    pub fn open_key_bundle(
        &self,
        verified: &sunrise_crypto::VerifiedKeyBundle<'_>,
    ) -> Result<Vec<sunrise_crypto::BundledKey>, KeychainError> {
        let state = self.identity.lock();
        let dh = state
            .identity
            .dh_secret
            .as_ref()
            .ok_or(KeychainError::IdentitySecretAbsent)?;
        Ok(verified.open(dh)?)
    }

    // ---- op envelopes ----

    /// Seal an inner-op CBOR blob into a real [`sunrise_crypto::OpEnvelope`] under an explicit
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
        self.seal_chained_op_at(
            stream_id,
            seq,
            hlc,
            inner,
            rng,
            epoch,
            stream_key,
            ChainLinks::default(),
        )
    }

    /// [`Self::seal_op_at`], stamping the chain fields `chain` carries
    /// (envelope fields 14 and 15, ADR-0043). The engine's op-log writer is
    /// the caller that fills them; an empty `chain` is a legacy link.
    ///
    /// # Errors
    /// AEAD / CBOR failures from the envelope codec, and a field 15 out of
    /// shape.
    #[allow(clippy::too_many_arguments)]
    pub fn seal_chained_op_at(
        &self,
        stream_id: [u8; 16],
        seq: u64,
        hlc: sunrise_cbor::hlc::Hlc,
        inner: &[u8],
        rng: &dyn Rng,
        epoch: u32,
        stream_key: &StreamKey,
        chain: ChainLinks,
    ) -> Result<Vec<u8>, OpEnvelopeError> {
        let mut nonce = [0u8; AEAD_NONCE_LEN];
        rng.fill_bytes(&mut nonce);
        encode_chained_envelope(
            inner,
            stream_id,
            self.device_id,
            seq,
            hlc,
            AeadAlgId::XChaCha20Poly1305,
            epoch,
            nonce,
            chain,
            Some(stream_key),
            &self.signing,
        )
    }

    /// Decode, verify, and decrypt a stored envelope back to inner-op CBOR.
    ///
    /// Every key at the envelope's `(stream_id, epoch)` is tried, because two
    /// devices can have minted that epoch concurrently.
    ///
    /// # Why the signature is checked before the key loop
    ///
    /// `open_envelope` does two things — it verifies the Ed25519 signature
    /// against `D_S_pub` and it opens the AEAD — and the loop discarded which
    /// of them failed. A **forged signature** on an op attributed to this
    /// device therefore came back as [`KeychainError::NoStreamKey`], whose
    /// documented handling is "park it in `deferred_ops` and wait for a
    /// `key_envelope`". A forgery was retried indefinitely as a missing key.
    ///
    /// Checking the signature once, up front, separates them: a bad signature
    /// is [`KeychainError::Envelope`] and a genuinely absent key is
    /// [`KeychainError::NoStreamKey`]. The loop below then narrows to the AEAD,
    /// which is the one place trying every key at the epoch is the right
    /// answer — two devices can mint an epoch concurrently and the tag is what
    /// picks.
    ///
    /// # Errors
    /// Bad magic, a signature this device did not make
    /// ([`KeychainError::Envelope`]), an unknown signing device, or
    /// [`KeychainError::NoStreamKey`] when this device holds no key that opens
    /// it.
    pub fn open_op(&self, envelope_bytes: &[u8]) -> Result<Vec<u8>, KeychainError> {
        let env = decode_envelope(envelope_bytes)?;
        if env.device_id != self.device_id {
            return Err(KeychainError::UnknownDevice);
        }
        verify_envelope(&env, &self.device_signing_pub())?;
        for key in self.stream_keys_at(&env.stream_id, env.epoch) {
            if let Ok(inner) = open_envelope(&env, &self.device_signing_pub(), Some(&key)) {
                return Ok(inner);
            }
        }
        Err(KeychainError::NoStreamKey)
    }

    /// Issue a cert naming **another** device, under this keychain's account
    /// identity.
    ///
    /// Not a hypothetical: pairing hands every device `ID_S_priv`
    /// (`docs/03-crypto/pairing-and-onboarding.md` §102-103), so any member can
    /// build one of these for any sibling and it verifies. That is what makes
    /// "the sender must be the device the cert names" a real check rather than
    /// a formality, and this is how the engine test builds the op that check
    /// has to refuse. The underlying gap -- every member can mint a valid cert,
    /// so a revoked device can certify itself under a fresh id -- is not closed
    /// by revocation and needs identity rotation. There is no narrower check:
    /// both signatures on a self-issued cert belong to the party producing it,
    /// so nothing about it attributes it to the device that signed. See
    /// ADR-0032 and `docs/03-crypto/key-rotation.md` §Identity rotation.
    ///
    /// It has a production caller now, which is the rotation itself: step 3 of
    /// §Identity rotation re-issues a cert for *every surviving device* under
    /// the new `ID_S_priv`, and those certs are the roster of an
    /// `identity_transition`. The method was `#[cfg(test)]` only because until
    /// there was an identity to rotate to, "certify a sibling" was a capability
    /// with no legitimate use — which is exactly what the gap above says.
    ///
    /// `nickname` and `platform` are parameters rather than the placeholders
    /// they were when only a test called this. A re-issued cert must carry the
    /// labels the device already has: a roster that renamed every device to
    /// "impostor" would be a valid transition that renamed the user's whole
    /// device list.
    ///
    /// # Errors
    /// [`KeychainError::IdentitySigningKeyAbsent`] on a device admitted by
    /// pairing — which is the whole of `#105`'s fix, and the assertion
    /// `a_paired_device_cannot_issue_a_cert_for_a_fresh_device_id` makes — and
    /// [`KeychainError::Cert`] for a nickname outside
    /// `1..=`[`sunrise_crypto::MAX_NICKNAME_BYTES`] bytes, or a CBOR failure.
    pub fn issue_cert_for(
        &self,
        device_id: [u8; 16],
        d_s_pub: [u8; 32],
        d_d_pub: [u8; 32],
        nickname: &str,
        platform: &str,
        now_ms: u64,
    ) -> Result<Vec<u8>, KeychainError> {
        let state = self.identity.lock();
        let signing = state.identity.signing.require()?;
        let body = DeviceCertInner {
            v: 1,
            device_id,
            d_s_pub,
            d_d_pub,
            identity_id: state.identity.identity_id,
            created_at_ms: now_ms,
            nickname: nickname.to_string(),
            platform: platform.to_string(),
        };
        Ok(DeviceCert::issue(body, signing)?.to_cbor()?)
    }

    // ---- identity transition (ADR-0032) ----

    /// Issue a cert naming `device_id` under a **successor** identity that is
    /// not yet in force.
    ///
    /// The roster of an `identity_transition` is exactly this, once per
    /// surviving device: [`Self::issue_cert_for`] signs under the identity the
    /// keychain currently holds, and a roster signed by the predecessor would
    /// be a roster no verifier accepts — `apply_control_op` checks every entry
    /// against `to_id_s_pub`, because a roster is the *successor's* statement
    /// about who survives rather than a list the emitter wrote.
    ///
    /// # Errors
    /// [`KeychainError::IdentitySigningKeyAbsent`] when `successor` is a
    /// successor this device adopted rather than minted, so it holds only the
    /// public half; [`KeychainError::Cert`] for an invalid nickname length or a
    /// CBOR failure.
    pub fn issue_roster_cert(
        successor: &Identity,
        device_id: [u8; 16],
        d_s_pub: [u8; 32],
        d_d_pub: [u8; 32],
        nickname: &str,
        platform: &str,
        now_ms: u64,
    ) -> Result<Vec<u8>, KeychainError> {
        let body = DeviceCertInner {
            v: 1,
            device_id,
            d_s_pub,
            d_d_pub,
            identity_id: successor.identity_id,
            created_at_ms: now_ms,
            nickname: nickname.to_string(),
            platform: platform.to_string(),
        };
        Ok(DeviceCert::issue(body, successor.signing.require()?)?.to_cbor()?)
    }

    /// Sign a transition body with the **outgoing** identity this keychain
    /// holds and the successor's own key.
    ///
    /// The one place a transition can be signed. `ID_S_priv` never leaves this
    /// module — the sole exported secret is `export_vault_root_for_pairing`,
    /// at that length and for that reason — so an emitter hands the body in and
    /// gets the pair back rather than borrowing the key.
    ///
    /// # Errors
    /// [`KeychainError::IdentitySigningKeyAbsent`] on a device admitted by
    /// pairing. A transition's `prev_sig` is the *outgoing* identity's
    /// statement that it hands over to this successor, and a device that never
    /// held the outgoing key cannot make it — so since `#105` only the account's
    /// creator can rotate the identity. `Engine::revoke_device` still cuts a
    /// revoked device off from every future epoch without one; see
    /// `docs/03-crypto/key-rotation.md` §Revocation for what that does and does
    /// not bound.
    ///
    /// [`KeychainError::Transition`] when the body does not name this
    /// keychain's identity as its predecessor or the successor's key as its
    /// successor. Both are checked before either signature is produced.
    pub fn sign_transition(
        &self,
        body: &IdentityTransitionBody,
        successor: &Identity,
    ) -> Result<IdentityTransitionSigs, KeychainError> {
        let state = self.identity.lock();
        Ok(sign_identity_transition(
            body,
            state.identity.signing.require()?,
            successor.signing.require()?,
        )?)
    }

    /// Mint a successor account identity: a fresh `ID_S`/`ID_D` pair.
    ///
    /// `created_at_ms` is the **account's** creation, carried forward from the
    /// identity being retired rather than stamped now. It is field 6 of the
    /// recovery blob and it means "when this account began"; a rotation does
    /// not begin a new account, and re-stamping it would make every rotation
    /// look like one to anything reading the blob.
    ///
    /// Both halves are kept here, so the minting device can seal the shares.
    /// Whether it *stores* `ID_D_priv` is decided later, by
    /// [`Self::adopt_successor_identity`], on the same rule
    /// `unwrap_identity` enforces at rest.
    pub fn mint_successor_identity(&self, rng: &dyn Rng) -> Identity {
        let created_at_ms = self.identity.lock().identity.created_at_ms;
        let mut seed = [0u8; 32];
        rng.fill_bytes(&mut seed);
        let signing = IdentitySigningKeyPair::from_secret_bytes(&seed);
        seed.zeroize();
        let mut dh_seed = [0u8; 32];
        rng.fill_bytes(&mut dh_seed);
        let dh = IdentityDhKeyPair::from_secret_bytes(dh_seed);
        dh_seed.zeroize();
        Identity {
            identity_id: identity_id_from_pub(&signing.public_bytes()),
            signing: IdentitySigningKey::Held(signing),
            dh_pub: dh.public_bytes(),
            dh_secret: Some(dh),
            created_at_ms,
        }
    }

    /// Seal one surviving device's share of the successor to its `D_D_pub`.
    ///
    /// 32 bytes of plaintext, so 80 on the wire, and **which** 32 bytes is the
    /// decision `#105` turns on.
    ///
    /// This used to be the successor's `ID_S_priv`, for every survivor. That
    /// undid at the first rotation exactly what withholding the key from
    /// `PairingPayload` achieves: a device admitted without a signing key was
    /// handed one the moment anybody revoked anything, and could mint a cert
    /// for a fresh device id from then on. **A rotation must move `ID_S_priv`
    /// to nobody.** So the share carries the successor's `ID_S_pub` instead,
    /// except for the one addressed to the emitting device — which minted the
    /// successor, already holds the outgoing secret, and would otherwise lose
    /// the ability to rotate again on the rotation it just performed.
    ///
    /// The share still does the work it always did. Its *presence* is the
    /// roster signal — a device with no share adopts nothing and logs
    /// `not_in_roster` — and `shares_digest` binds the set into the signed
    /// body, so a relay cannot add or drop one. What it no longer does is
    /// distribute a capability.
    ///
    /// `ID_D_priv` was never in here and still is not: ADR-0024's asymmetry is
    /// that a paired device can seal to the identity and cannot open what was
    /// sealed to it, and a rotation that handed every device the new unwrapping
    /// key would undo #76 in one op.
    ///
    /// # Errors
    /// [`KeychainError::Hpke`] for a malformed recipient key, and
    /// [`KeychainError::IdentitySigningKeyAbsent`] when this device is sealing
    /// its own share for a successor it does not hold the key to — which cannot
    /// happen, because minting the successor is what produced it.
    pub fn seal_successor_device_share(
        &self,
        successor: &Identity,
        recipient_d_d_pub: &[u8; 32],
        device_id: &[u8; 16],
        rng: &dyn Rng,
    ) -> Result<Vec<u8>, KeychainError> {
        let info = identity_share_info(&successor.identity_id, device_id);
        let mut plain = if *device_id == self.device_id {
            successor.signing.require()?.secret_bytes()
        } else {
            successor.signing.public_bytes()
        };
        let mut adapter = RngAdapter(rng);
        let out = hpke_seal(recipient_d_d_pub, &info, &plain, b"", &mut adapter);
        plain.zeroize();
        Ok(out?)
    }

    /// Open the share addressed to **this** device and rebuild the successor.
    ///
    /// The public halves come from the transition's signed body and the share
    /// from the sealed blob, and the two are checked against each other before
    /// anything is returned. Nothing else could check them: HPKE Base
    /// authenticates no sender, so a share is only as trustworthy as the
    /// signature over the body that names the key it should open to.
    ///
    /// The 32 bytes are the successor's `ID_S_pub` on every device but the one
    /// that emitted the rotation, which gets `ID_S_priv` — see
    /// [`Self::seal_successor_device_share`] for why. The two are told apart by
    /// what they *derive*, not by a flag: bytes that produce `body.id_s_pub`
    /// when read as a seed are the secret, bytes that **are** `body.id_s_pub`
    /// are the public half, and nothing else is either. Finding a seed whose
    /// public key equals a given 32 bytes that are themselves that public key
    /// is not a thing an attacker gets to arrange.
    ///
    /// # Errors
    /// [`KeychainError::Hpke`] when the blob is not for this device or was
    /// sealed under another transition, [`KeychainError::WrappedLen`] for a
    /// wrong-sized plaintext, [`KeychainError::IdentityIdMismatch`] when the
    /// opened share is neither the key the body names nor its public half.
    pub fn open_successor_device_share(
        &self,
        body: &SuccessorPublics,
        sealed: &[u8],
    ) -> Result<Identity, KeychainError> {
        let info = identity_share_info(&body.identity_id, &self.device_id);
        let opened = hpke_open(&self.device_dh, &info, sealed, b"")?;
        let mut bytes: [u8; 32] = opened
            .as_slice()
            .try_into()
            .map_err(|_| KeychainError::WrappedLen)?;
        let as_secret = IdentitySigningKeyPair::from_secret_bytes(&bytes);
        let signing = if as_secret.public_bytes() == body.id_s_pub {
            IdentitySigningKey::Held(as_secret)
        } else {
            // Not the secret. `into_identity` refuses it unless it is the public
            // half the signed body names, so a share carrying anything else
            // fails there rather than installing a key nothing can verify.
            IdentitySigningKey::PublicOnly(bytes)
        };
        bytes.zeroize();
        body.into_identity(signing, None, self.identity.lock().identity.created_at_ms)
    }

    /// Seal the successor's `ID_S_priv || ID_D_priv` to the **outgoing**
    /// `ID_D_pub` — the carry-forward share.
    ///
    /// 64 bytes of plaintext, so 112 on the wire. Sealed to the account key
    /// rather than to any device, because its recipient is whoever holds the
    /// *old* recovery blob: without it that person's code opens an identity the
    /// account has retired, and they would have to re-enrol from a device.
    ///
    /// Producing one is always possible for the minting device; whether to
    /// carry it is the caller's decision, and a rotation provoked by suspected
    /// identity-key extraction must not — it would hand the attacker the
    /// successor under the very key that is suspected.
    ///
    /// # Errors
    /// [`KeychainError::Hpke`] if the outgoing `ID_D_pub` is malformed, or
    /// [`KeychainError::IdentitySecretAbsent`] if the successor was handed in
    /// without its `ID_D_priv` — which is the state of a device that received
    /// the transition rather than minting it, and which therefore cannot
    /// produce this share.
    pub fn seal_successor_carry_share(
        &self,
        successor: &Identity,
        rng: &dyn Rng,
    ) -> Result<Vec<u8>, KeychainError> {
        let dh = successor
            .dh_secret
            .as_ref()
            .ok_or(KeychainError::IdentitySecretAbsent)?;
        let info = identity_carry_info(&successor.identity_id);
        let mut plain = [0u8; 64];
        // The one place a rotation still moves `ID_S_priv`, and it moves it to
        // a *key*, not to a device: sealed to the outgoing `ID_D_pub`, whose
        // private half lives in the recovery blob and on the account's creator.
        // A recovery has to be able to issue certs or it cannot re-admit the
        // device doing the recovering.
        let mut s = successor.signing.require()?.secret_bytes();
        let mut d = dh.secret_bytes();
        plain[..32].copy_from_slice(&s);
        plain[32..].copy_from_slice(&d);
        s.zeroize();
        d.zeroize();
        let outgoing_dh_pub = self.identity.lock().identity.dh_pub;
        let mut adapter = RngAdapter(rng);
        let out = hpke_seal(&outgoing_dh_pub, &info, &plain, b"", &mut adapter);
        plain.zeroize();
        Ok(out?)
    }

    /// Open the carry-forward share with the **outgoing** `ID_D_priv`.
    ///
    /// The result carries `ID_D_priv`, so this is the one path by which the
    /// account's unwrapping key survives a rotation at all.
    ///
    /// # Errors
    /// [`KeychainError::IdentitySecretAbsent`] on a device that holds no
    /// outgoing `ID_D_priv` — every device admitted by pairing — plus the
    /// errors [`Self::open_successor_device_share`] documents.
    pub fn open_successor_carry_share(
        &self,
        body: &SuccessorPublics,
        sealed: &[u8],
    ) -> Result<Identity, KeychainError> {
        let info = identity_carry_info(&body.identity_id);
        let state = self.identity.lock();
        let outgoing = state
            .identity
            .dh_secret
            .as_ref()
            .ok_or(KeychainError::IdentitySecretAbsent)?;
        let mut opened = hpke_open_identity(outgoing, &info, sealed, b"")?;
        if opened.len() != 64 {
            opened.zeroize();
            return Err(KeychainError::WrappedLen);
        }
        let mut s = [0u8; 32];
        let mut d = [0u8; 32];
        s.copy_from_slice(&opened[..32]);
        d.copy_from_slice(&opened[32..]);
        opened.zeroize();
        let signing = IdentitySigningKey::Held(IdentitySigningKeyPair::from_secret_bytes(&s));
        s.zeroize();
        let dh = IdentityDhKeyPair::from_secret_bytes(d);
        d.zeroize();
        let created_at_ms = state.identity.created_at_ms;
        drop(state);
        body.into_identity(signing, Some(dh), created_at_ms)
    }

    /// Adopt `successor` as the identity in force.
    ///
    /// Three writes and one swap, all inside the caller's transaction:
    ///
    /// 1. the `identity` row is re-wrapped under the new `identity_id` — the
    ///    at-rest AAD is `identity_aad(identity_id)`, so the old ciphertext
    ///    does not open under the new id and re-wrapping is not optional;
    /// 2. this device's cert is re-issued under the new `ID_S_priv` and
    ///    replaces the one on `local_identity` and on its `devices` row;
    /// 3. the in-memory `IdentityState` is swapped, so the next `&self` call
    ///    signs under the successor.
    ///
    /// `genesis_identity_id` and `created_at_ms` are deliberately **not**
    /// touched: the first is the fold anchor migration 0022 exists for, and the
    /// second is when the account began rather than when this key was minted.
    ///
    /// `local_identity.signing_secret_wrapped` and `dh_secret_wrapped` are
    /// deliberately **not** touched either. `D_S` and `D_D` are unchanged by a
    /// rotation, and their AAD binds to `device_id`, not to `identity_id`, so
    /// re-wrapping them would be a write that could only go wrong.
    ///
    /// `minted_by_device_id` follows the successor's `ID_D_priv` and nothing
    /// else, which keeps `unwrap_identity`'s guard true across the rotation:
    /// a device that adopts a successor it did not mint stores an empty
    /// `id_d_priv_wrapped` and a NULL minter, exactly as a paired device does.
    ///
    /// `roster_cert` is this device's entry from the transition's roster, and
    /// is **taken rather than re-issued**. It used to be signed here, under
    /// `successor.signing` — which since `#105` most devices do not hold. They
    /// do not need to: the roster already carries a cert for every survivor,
    /// signed by the successor and bound into the signed body by
    /// `roster_digest`, and `apply_control_op` has verified every entry in it
    /// against `to_id_s_pub` before this runs. Re-issuing was producing a second
    /// cert for the same device under the same identity, which is at best
    /// redundant and at worst a body that disagrees with the one every peer
    /// applied.
    ///
    /// Returns the cert blob, which the caller publishes.
    ///
    /// # Errors
    /// SQLite failures, or [`KeychainError::BadGrantCert`] if the roster entry
    /// does not name this device under the successor — which
    /// `apply_control_op` has already excluded, and which is checked again here
    /// because this is the function that writes it to three rows.
    pub fn adopt_successor_identity(
        &self,
        tx: &rusqlite::Transaction<'_>,
        successor: Identity,
        roster_cert: Vec<u8>,
        rng: &dyn Rng,
    ) -> Result<Vec<u8>, KeychainError> {
        let cert_blob = roster_cert;
        {
            let cert = DeviceCert::from_cbor(&cert_blob)
                .map_err(|_| KeychainError::BadGrantCert("the roster cert does not decode"))?;
            cert.verify_binding(&successor.signing.public_bytes(), &successor.identity_id)
                .map_err(|_| {
                    KeychainError::BadGrantCert("the roster cert is not signed by the successor")
                })?;
            if cert.body.device_id != self.device_id
                || cert.body.d_s_pub != self.signing.public_bytes()
                || cert.body.d_d_pub != self.device_dh.public_bytes()
            {
                return Err(KeychainError::BadGrantCert(
                    "the roster cert names another device",
                ));
            }
        }

        let wrapped = wrap_identity(&self.vault_root, &successor, rng);
        let minted_here = successor.dh_secret.is_some();
        tx.execute(
            "UPDATE identity
             SET identity_id = ?, id_s_pub = ?, id_d_pub = ?,
                 id_s_priv_wrapped = ?, id_d_priv_wrapped = ?,
                 minted_by_device_id = ?
             WHERE id = 1",
            params![
                &successor.identity_id[..],
                &successor.signing.public_bytes()[..],
                &successor.dh_pub[..],
                wrapped.0,
                wrapped.1,
                minted_here.then(|| self.device_id.to_vec()),
            ],
        )?;
        tx.execute(
            "UPDATE local_identity SET cert_blob = ? WHERE id = 1",
            params![cert_blob],
        )?;
        tx.execute(
            "UPDATE devices SET cert_blob = ?, identity_id = ? WHERE device_id = ?",
            params![cert_blob, &successor.identity_id[..], &self.device_id[..]],
        )?;

        let mut state = self.identity.lock();
        state.identity = successor;
        state.cert_blob.clone_from(&cert_blob);
        Ok(cert_blob)
    }

    /// The device labels a re-issued cert must carry, read inside a
    /// transaction.
    ///
    /// Separate from `Self::local_labels` only because that one takes a
    /// `&Db`: a rotation re-issues certs in the same transaction that writes
    /// them, and reading through a second connection could see a device list
    /// from before the rotation started.
    ///
    /// # Errors
    /// SQLite failure.
    /// **A documented fail-open.** A device with no `devices` row is labelled
    /// `sunrise-device` / this host's OS rather than raising.
    ///
    /// The consequential caller is `issue_roster_cert`, so a rotation whose
    /// emitter has no row for itself re-issues its own cert under a placeholder
    /// name — a device renamed in the roster, which is the outcome this
    /// function's neighbours warn about.
    ///
    /// Left open because a nickname is presentation and a rotation is not, and
    /// the trade is one-sided: refusing here fails the *revocation* that drove
    /// the rotation, over a label. The row's absence is also not a corruption —
    /// it is the ordinary state of a device between its cert publish and its
    /// `devices` row, which is exactly when a concurrent rotation can run.
    pub fn device_labels_tx(
        tx: &rusqlite::Transaction<'_>,
        device_id: &[u8; 16],
    ) -> rusqlite::Result<(String, String)> {
        let row: Option<(String, String)> = tx
            .query_row(
                "SELECT nickname, platform FROM devices WHERE device_id = ?",
                params![&device_id[..]],
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

    /// A test keychain that mints **real random** keys, so a Stream key this
    /// device was never handed is one it genuinely does not hold.
    ///
    /// A keychain from [`Self::for_test_seeded`] derives every key from the
    /// shared vault root, so it can always open an op and never observe one
    /// arriving before the key that opens it. A test about that case needs
    /// this constructor instead.
    #[cfg(test)]
    pub(crate) fn for_test_random_keys(vault_root: VaultRootKey, signing_seed: [u8; 32]) -> Self {
        Self::for_test_inner(vault_root, signing_seed, false)
    }

    /// A test keychain shaped like a device **admitted by pairing**: the same
    /// account, the same device keys, and `ID_S_pub` instead of `ID_S`.
    ///
    /// Every other test keychain here is a founder, which is fine for the
    /// engine's ordinary paths and wrong for the two that turn on who holds the
    /// account's signing key — `revoke_device`'s rotation step and
    /// `rotate_identity` itself. Without this they would only ever be exercised
    /// on the one device in an account that can perform them.
    ///
    /// Its `cert_blob` is issued here under the real `ID_S_priv`, exactly as a
    /// sponsor would have issued it, and then the key is dropped rather than
    /// stored — which is the whole of what pairing does.
    #[cfg(test)]
    pub(crate) fn for_test_paired(vault_root: VaultRootKey, signing_seed: [u8; 32]) -> Self {
        let kc = Self::for_test_inner(vault_root, signing_seed, false);
        {
            let mut state = kc.identity.lock();
            let public = state.identity.signing.public_bytes();
            state.identity.signing = IdentitySigningKey::PublicOnly(public);
            // `#76`'s half of the same asymmetry, so the fixture is a device a
            // pairing could actually have produced rather than a half of one.
            state.identity.dh_secret = None;
        }
        kc
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
        let id_dh = IdentityDhKeyPair::from_secret_bytes([0x5b; 32]);
        let identity = Identity {
            identity_id: identity_id_from_pub(&id_signing.public_bytes()),
            signing: IdentitySigningKey::Held(id_signing),
            dh_pub: id_dh.public_bytes(),
            dh_secret: Some(id_dh),
            created_at_ms: 0,
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
            genesis: (identity.identity_id, identity.signing.public_bytes()),
            device_dh,
            identity: Mutex::new(IdentityState {
                identity,
                cert_blob,
            }),
            vault_root,
            cache: Mutex::new(HashMap::new()),
            test_derived_keys,
        }
    }
}

/// A 16-byte id read out of a DB blob, or `None` if the blob is not 16 bytes.
///
/// The crate's one blob-to-id decoder, and it lives here because this is where
/// its reason does — next to [`to32`], whose doc points at it.
///
/// It used to zero-pad, and that was a silent way of manufacturing a *valid*
/// value from a corrupt one: a truncated `identity_id` padded with zeros still
/// compares equal to itself, so the vault opened and every cert bound to an
/// identity that never existed. A short blob is a corrupt row, and the caller
/// is in a position to say so — which is what every caller does: the
/// Subscribe-frame builders in `core` skip a row that cannot name a stream or
/// a device, and [`crate::Engine`]'s `emit_key_envelopes` skips a `devices`
/// row rather than sealing a Stream key to a padded id.
pub(crate) fn to16(raw: &[u8]) -> Option<[u8; 16]> {
    raw.try_into().ok()
}

/// A 32-byte key read out of a DB blob, or `None` if the blob is not 32 bytes.
/// See [`to16`] for why this is not a pad.
pub(crate) fn to32(raw: &[u8]) -> Option<[u8; 32]> {
    raw.try_into().ok()
}

/// A 64-byte signature read out of a DB blob, or `None` if the blob is not 64
/// bytes. See [`to16`] for why this is not a pad.
///
/// Every caller is in the identity chain, where a signature that is the wrong
/// width is a row the fold must refuse rather than one it can pad into
/// verifying against nothing.
pub(crate) fn to64(raw: &[u8]) -> Option<[u8; 64]> {
    raw.try_into().ok()
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
