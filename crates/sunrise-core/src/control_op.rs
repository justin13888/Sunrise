//! Control-op payloads: the op families that carry key material and trust
//! rather than user data.
//!
//! Per [ADR-0024](../../../docs/11-adr/0024-key-hierarchy.md) and
//! `docs/03-crypto/data-encryption-format.md` §Control ops. Four families; the
//! first three are new at `DOC_SCHEMA_V = 5` and the fourth at `6`:
//!
//! - [`KeyEnvelopePayload`] distributes one `(stream_id, epoch)` Stream key to
//!   one recipient by HPKE. It is what makes a Stream key reachable by a device
//!   that did not mint it, and — sealed a second time to the account identity —
//!   what makes recovery restore readable content.
//! - [`DeviceRevokePayload`] records that a device is no longer a member. The
//!   op itself does not rotate anything; the emitter mints new epochs alongside
//!   it. What the op does is put the same *record* on every replica — and
//!   nothing more: no replica refuses a revoked device's ops or withholds a key
//!   from it. Membership is a fact that has to converge whether or not anything
//!   yet acts on it; acting on it is `#76` for reads and `#82`, behind `#80`,
//!   for writes.
//! - `DeviceCertPublish` carries a canonical-CBOR [`DeviceCert`] so a device
//!   admitted by pairing becomes known to every replica without a manual
//!   trust step. It replaces `Command::TrustDevice`, which accepted any
//!   self-signed cert and so could not be revoked from.
//! - [`IdentityTransitionPayload`] replaces the account identity itself with a
//!   successor, carrying in one op the successor's public halves, a re-issued
//!   cert for every surviving device, and the successor's secrets sealed to
//!   each of them. It is what finally closes the gap
//!   [`crate::keychain::Keychain::issue_cert_for`] names: every member can mint
//!   a valid cert, so a revoked device can certify itself under a fresh id, and
//!   no revocation can undo that — only a new identity can.
//!
//!
//! Two more families arrive at `DOC_SCHEMA_V = 10`, and they carry the
//! vault's feature state rather than keys (ADR-0045 §7):
//!
//! - [`VaultRequiresPayload`] names feature ids the vault's data now depends
//!   on. The vault's required set is the union of every one applied, a
//!   grow-only set, so concurrent enables converge. A build that lacks a
//!   required feature keeps syncing and refuses local writes on the feature's
//!   scope ([`crate::feature`]).
//! - [`DeviceFeaturesPayload`] is a device saying which feature ids it
//!   supports. It is read as the latest op per device, so a device that
//!   upgrades replaces what it said before.
//!
//! [`ShareGrantOpPayload`] arrives at `DOC_SCHEMA_V = 32`: the account's
//! record of one Stream shared with one Person, carrying the sealed viewer
//! grant and the recipient key a rotation re-seals the next epoch to
//! (`docs/03-crypto/sharing-with-others.md`).
//!
//! [`ShareRevokePayload`] and [`ShareDeclinePayload`] arrive at
//! `DOC_SCHEMA_V = 33`, with `ShareGrantOpPayload::reseal`: the account's
//! record that a share ended, by the owner's revocation or the recipient's
//! decline, and which grants only carry a share forward.
//! [`ShareResealRequestPayload`] arrives beside them: a device without
//! `ID_S_priv` that minted an epoch names the shares its re-seal would have
//! reached, so a holder re-seals it to those and no others.
//!
//! [`StreamWriterCertPayload`] and [`StreamShareGrantPayload`] arrive at
//! `DOC_SCHEMA_V = 34`, and are the two control families logged in a shared
//! Stream's own log rather than the vault-meta stream: a device's cert, and a
//! re-sealed grant, for a recipient that reads only that Stream (ADR-0076).
//!
//! These are **not** entities. They have no row, no LWW stamp and no
//! materialization: `inner_op`'s `OpEffect::Control` variant exists so the
//! compiler cannot route one into the entity materializer, where the `_ =>`
//! catch-all in the kind table would have filed it as a task.
//!
//! [`DeviceCert`]: sunrise_crypto::DeviceCert

use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;

// Every fixed-width id and every opaque blob below carries
// `#[serde(with = "serde_bytes")]`. Without it `[u8; N]` and `Vec<u8>` encode
// as CBOR *arrays of integers* — two bytes per byte, and a shape no other id in
// `data-encryption-format.md` has, where every id is `bstr .size 16`. These
// families are new at `DOC_SCHEMA_V = 5`, so this is the one moment the choice
// is free.

/// Who a [`KeyEnvelopePayload`] is sealed to.
///
/// The two classes are what make revocation and recovery both work, and they
/// are deliberately not collapsed into "a public key": a replica has to be able
/// to tell *whose* copy an envelope is without trial-decrypting it, and a
/// device must not try its own key against the identity's copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Recipient {
    /// Sealed to a device's `D_D_pub`, so the device learns the epochs it is
    /// entitled to. Every device is a recipient, revoked or not: the epoch is
    /// sealed to the account identity as well, and every paired device holds
    /// `ID_D_priv`, so leaving a revoked device out of this list would withhold
    /// nothing from it. See `#76`.
    Device(#[serde(with = "serde_bytes")] [u8; 16]),
    /// Sealed to the account identity's `ID_D_pub`. Opened by the recovery
    /// blob's `ID_D_priv`, so a recovery with no surviving device still reaches
    /// the content.
    Identity(#[serde(with = "serde_bytes")] [u8; 16]),
}

/// One Stream key, sealed to one recipient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEnvelopePayload {
    /// The stream whose key this is.
    #[serde(with = "serde_bytes")]
    pub stream_id: [u8; 16],
    /// The epoch this key belongs to.
    pub epoch: u32,
    /// Who can open it.
    pub recipient: Recipient,
    /// `BLAKE3.derive_key("sunrise.stream_key_id.v1", stream_key, 8)`.
    ///
    /// A disambiguator, not an authenticator. Two devices can mint the same
    /// epoch concurrently and both keys are kept; this says which row a
    /// recipient should file the opened key under without having to open it
    /// first. Nothing trusts it — the value is re-derived from the opened key.
    #[serde(with = "serde_bytes")]
    pub key_id: [u8; 8],
    /// `enc(32) || ciphertext(32) || tag(16)`, RFC 9180 Base.
    #[serde(with = "serde_bytes")]
    pub hpke_ciphertext: Vec<u8>,
}

/// Why a device was revoked.
///
/// Recorded rather than inferred. Nothing branches on it — the rotation is
/// identical in every case, and so is the revocation, which is to say inert —
/// but the reason is what a device list has to show a user months later, and it
/// is not recoverable from anything else in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RevokeReason {
    /// Misplaced; may still turn up.
    Lost,
    /// Known to be in someone else's hands.
    Stolen,
    /// Deliberately retired by its owner.
    Retired,
    /// Believed to have had its keys extracted.
    Compromised,
}

impl RevokeReason {
    /// The stored form, matching the CBOR encoding.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lost => "Lost",
            Self::Stolen => "Stolen",
            Self::Retired => "Retired",
            Self::Compromised => "Compromised",
        }
    }

    /// Parse the stored form.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Lost" => Some(Self::Lost),
            "Stolen" => Some(Self::Stolen),
            "Retired" => Some(Self::Retired),
            "Compromised" => Some(Self::Compromised),
            _ => None,
        }
    }
}

/// A device is no longer a member of this account.
///
/// # There is no `effective_at` field, and that is the design
///
/// The cut is the **HLC of the op that declares it**, read off the envelope by
/// every replica rather than chosen by the emitter. Nothing compares anything
/// against it today — the register is inert, see [`Recipient::Device`] — but it
/// is recorded as a *time* so that whatever eventually enforces it can let the
/// device's earlier work stand. A revocation that retroactively erased that
/// history would be worse than none: a laptop being retired did not un-write
/// the six months of work it did.
///
/// An `effective_at_ms` field was tried and removed. It was a plain field of
/// the signed envelope, so it was the whole of a revocation's meaning and
/// entirely emitter-controlled, and no bound on it survived contact:
///
/// * bounding it *ahead* of the op's HLC left a window in which a cut could be
///   parked far enough forward to refuse nothing;
/// * bounding it *behind* had nothing to anchor to. `Hlc::observe` refuses
///   readings from the future only; the past is unbounded by design, and
///   nothing re-bounds a device's own HLC when its wall clock moves. A device
///   more than a few minutes slow emits, through the ordinary command with no
///   crafted input at all, a cut far enough in the past to refuse its target's
///   entire history.
///
/// The op's own HLC has neither problem: it is monotone, it is merged from
/// every peer this device has heard from rather than read off the wall clock,
/// and the clock gate already refuses one too far ahead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRevokePayload {
    /// The device being revoked.
    #[serde(with = "serde_bytes")]
    pub revoked_device_id: [u8; 16],
    /// Why.
    pub reason_code: RevokeReason,
}

/// A device is no longer a member, **on the account identity's word**
/// (ADR-0058 §2, `DOC_SCHEMA_V = 23`).
///
/// The same claim as [`DeviceRevokePayload`], signed by the `ID_S_priv` of
/// the identity it names. The revocation fold reads it as an *authority* row
/// when `identity_id` is the head of the identity chain at that fold and
/// `identity_sig` verifies under the head's `ID_S_pub`; otherwise it folds as
/// an ordinary revocation from its sender. The signature covers
/// [`sunrise_crypto::RevocationClaim`], which takes the sender and the HLC
/// from the authenticated envelope rather than from this payload, so it
/// cannot be lifted into another device's op or re-dated.
///
/// A new op kind rather than a field on `DeviceRevoke`: an older build
/// ignores an unknown field and would fold the op as ordinary, which gates it
/// differently, and the two builds would converge on different registers
/// (ADR-0056 §Alternatives (d)). It is emitted only into a vault that
/// requires `core.revoke_authority` ([`crate::feature`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRevokeByIdentityPayload {
    /// The device being revoked.
    #[serde(with = "serde_bytes")]
    pub revoked_device_id: [u8; 16],
    /// Why.
    pub reason_code: RevokeReason,
    /// The identity that signs, `identity_id_from_pub(ID_S_pub)`.
    #[serde(with = "serde_bytes")]
    pub identity_id: [u8; 16],
    /// `Ed25519(ID_S_priv, "sunrise.device_revoke.identity.v1" ||
    /// BLAKE3(canonical_cbor(claim)))`.
    #[serde(with = "serde_bytes")]
    pub identity_sig: [u8; 64],
}

/// The sender **withdraws its own revocation** of a device (ADR-0056 §2,
/// `DOC_SCHEMA_V = 28`).
///
/// It names only the target: the sender is the authenticated envelope's, so
/// the op can withdraw nothing but its author's own claim, and that is the
/// whole of its authority (ADR-0056 §3). The ledger stores it as a row of the
/// same `(sender, revoked_device_id)` pair as the sender's ordinary
/// revocations, and the fold reads each pair's greatest-stamped row: a pair
/// whose head is a withdrawal is inert, and a later revocation makes it live
/// again. On a tie the revocation wins.
///
/// A new op kind rather than a reason or a field on `DeviceRevoke`, because an
/// older build would read either as a revocation and fold the opposite of what
/// its author meant (ADR-0056 §Alternatives (d)). It is emitted only into a
/// vault that requires `core.revoke_withdraw` ([`crate::feature`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRevokeWithdrawPayload {
    /// The device whose revocation by the sender is withdrawn.
    #[serde(with = "serde_bytes")]
    pub revoked_device_id: [u8; 16],
}

/// The account identity **withdraws its own revocation** of a device
/// (ADR-0058 §6, `DOC_SCHEMA_V = 30`).
///
/// [`DeviceRevokeWithdrawPayload`] signed the way
/// [`DeviceRevokeByIdentityPayload`] is: by the `ID_S_priv` of the identity it
/// names, over [`sunrise_crypto::WithdrawalClaim`], which takes the sender and
/// the HLC from the authenticated envelope. The ledger stores it as a row of
/// the sender's **authority** pair, the pair its identity-signed revocations
/// sit in, and the fold lets it withdraw that pair only while it verifies
/// under the head of the identity chain. It never reaches a sender's ordinary
/// pair, so the identity cannot withdraw a revocation a device made
/// (ADR-0056 §Alternatives (b)).
///
/// A new op kind rather than an optional signature on
/// `DeviceRevokeWithdraw`: an older build would ignore the field and fold the
/// op as a withdrawal of the sender's ordinary pair, leaving the authority
/// pair live while every newer build withdrew it (ADR-0056 §Alternatives
/// (d)). It is emitted only into a vault that requires
/// `core.revoke_withdraw_authority` ([`crate::feature`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRevokeWithdrawByIdentityPayload {
    /// The device whose authority revocation by the sender is withdrawn.
    #[serde(with = "serde_bytes")]
    pub revoked_device_id: [u8; 16],
    /// The identity that signs, `identity_id_from_pub(ID_S_pub)`.
    #[serde(with = "serde_bytes")]
    pub identity_id: [u8; 16],
    /// `Ed25519(ID_S_priv, "sunrise.device_revoke_withdraw.identity.v1" ||
    /// BLAKE3(canonical_cbor(claim)))`.
    #[serde(with = "serde_bytes")]
    pub identity_sig: [u8; 64],
}

/// One surviving device's re-issued [`DeviceCert`], signed by the **new**
/// `ID_S_priv`.
///
/// There is deliberately **no `device_id` field**. The id is inside the cert,
/// in a body the identity signed; a copy beside it would be a second source of
/// truth that nothing keeps in step, and every reader that trusted the outer
/// copy would be trusting an unsigned field. `roster_digest` reads the id back
/// out of the cert for exactly this reason.
///
/// A one-field struct rather than a bare `Vec<u8>` because the roster is the
/// place a later revision most obviously grows a field (a per-device status,
/// say), and an array of bare byte strings could not take one without
/// re-shaping the op.
///
/// [`DeviceCert`]: sunrise_crypto::DeviceCert
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterEntry {
    /// Canonical-CBOR [`DeviceCert`], as [`sunrise_crypto::DeviceCert::to_cbor`]
    /// writes it.
    ///
    /// [`DeviceCert`]: sunrise_crypto::DeviceCert
    #[serde(with = "serde_bytes")]
    pub cert: Vec<u8>,
}

/// The successor identity's secrets, sealed to one surviving device.
///
/// Sealed under [`sunrise_crypto::identity_share_info`], which binds the blob
/// to the successor identity *and* to this device — HPKE Base authenticates no
/// sender, so without the device in the `info` a blob could be moved between
/// entries and the roster would name one device beside another's share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyShare {
    /// The device this share is for.
    #[serde(with = "serde_bytes")]
    pub device_id: [u8; 16],
    /// `enc(32) || ct(32) || tag(16)` — 80 bytes, RFC 9180 Base.
    #[serde(with = "serde_bytes")]
    pub hpke_ciphertext: Vec<u8>,
}

/// The account identity is replaced by a successor.
///
/// ADR-0032 and `docs/03-crypto/key-rotation.md` §Identity rotation. One op
/// carries the whole hand-over, because the parts are not independently
/// applicable: a replica that learned the new `ID_S_pub` without the re-issued
/// certs would reject every device in the account, and one that learned the
/// certs without the transition would have no key to verify them under.
///
/// `identity_id` is `identity_id_from_pub(ID_S_pub)`, a derivation rather than
/// a field, so a new `ID_S` *is* a new id: hence `from_identity_id` and
/// `to_identity_id` rather than the single unchanged id
/// `docs/03-crypto/key-rotation.md` draws. Neither is believed — both are
/// recomputed by [`sunrise_crypto::verify_identity_transition`] from the key
/// they name.
///
/// The devices' own `D_S`/`D_D` are untouched. Those wrap under an AAD binding
/// to `device_id`, not to the identity, and nothing about rotating the account
/// key invalidates a device key; only the *cert* — the identity's statement
/// about the device — has to be re-issued.
///
/// # There is no `effective_at` field, for [`DeviceRevokePayload`]'s reasons
///
/// The same argument applies, and applies harder. An emitter-chosen cut on a
/// revocation decided which of one device's ops to refuse; an emitter-chosen
/// cut on a transition decides which *identity* every op in the account
/// verifies under. Bounded ahead of the op's HLC it parks the hand-over far
/// enough forward to take effect nowhere; bounded behind it has nothing to
/// anchor to, and a device a few minutes slow would retroactively re-attribute
/// the account's history to a key that did not sign it. The op's own HLC is
/// monotone, merged from every peer, and already gated for drift.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityTransitionPayload {
    /// The identity being retired.
    #[serde(with = "serde_bytes")]
    pub from_identity_id: [u8; 16],
    /// The successor identity.
    #[serde(with = "serde_bytes")]
    pub to_identity_id: [u8; 16],
    /// The successor's `ID_S_pub`.
    #[serde(with = "serde_bytes")]
    pub to_id_s_pub: [u8; 32],
    /// The successor's `ID_D_pub`.
    #[serde(with = "serde_bytes")]
    pub to_id_d_pub: [u8; 32],
    /// One re-issued cert per surviving device.
    pub roster: Vec<RosterEntry>,
    /// The successor's secrets, sealed to each surviving device.
    pub device_shares: Vec<KeyShare>,
    /// The successor's `ID_S_priv || ID_D_priv` (64 B) sealed to the
    /// **outgoing** `ID_D_pub` — 112 B — under
    /// [`sunrise_crypto::identity_carry_info`].
    ///
    /// Optional because the outgoing `ID_D_priv` is held by at most one device
    /// and by the recovery blob, and a rotation triggered by *suspected key
    /// extraction* is precisely the case where carrying the successor forward
    /// under the compromised key would hand the attacker the new identity too.
    /// Present, a holder of the old recovery code follows the account forward
    /// without re-enrolling; absent, they do not, and that is the point.
    ///
    /// A [`ByteBuf`] rather than `Option<Vec<u8>>` with a `serde_bytes`
    /// attribute: the attribute form is easy to drop in a later edit and the
    /// field would silently start encoding as an array of integers, which is
    /// the failure the note at the top of this module exists to prevent. The
    /// type carries the rule instead.
    ///
    /// [`ByteBuf`]: serde_bytes::ByteBuf
    pub identity_share: Option<ByteBuf>,
    /// Ed25519 by the **outgoing** `ID_S_priv` over
    /// `"sunrise.identity_transition.v1" || body_hash`.
    #[serde(with = "serde_bytes")]
    pub prev_sig: [u8; 64],
    /// Ed25519 by the **successor** `ID_S_priv` over
    /// `"sunrise.identity_transition.succ.v1" || body_hash || prev_sig`.
    #[serde(with = "serde_bytes")]
    pub next_sig: [u8; 64],
}

/// The vault's data now depends on these features (ADR-0045 §7).
///
/// Sealed on the vault-meta stream like every control op. Applying one adds
/// each well-formed id to the vault's required set, which only grows: no op
/// removes a feature, so two devices that enable different features at once
/// both end up requiring both. An id that is not well formed
/// ([`crate::feature::is_feature_id`]) is skipped rather than failing the op,
/// so one bad id cannot hide the good ones beside it.
///
/// It is emitted **before** the first op that uses the feature. Cross-stream
/// delivery order is not guaranteed, so this is a signal and not a lock: a
/// build that meets an op it cannot read parks it whether or not the signal
/// has arrived.
///
/// Fields a later build adds are kept in `unknown` and written back
/// unchanged, as every record on the wire does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VaultRequiresPayload {
    /// The feature ids, `[a-z][a-z0-9_]*(\.[a-z0-9_]+)+`.
    pub features: Vec<String>,
    /// Fields this build does not know, kept byte for byte.
    #[serde(flatten)]
    pub unknown: sunrise_domain::Unknowns,
}

/// The features the sending device supports (ADR-0045 §7).
///
/// There is no `device_id` field, for [`RosterEntry`]'s reason: the op is
/// about the device that signed it, and the envelope already names that
/// device under a signature. A copy beside it would be an unsigned second
/// source of truth.
///
/// Read as the **latest op per device**, by HLC. A device emits one whenever
/// what it supports changes, which in practice is the first open after an
/// upgrade. A device that has never emitted one supports nothing, which is
/// exactly what every build older than this family looks like, so a feature
/// is never enabled over such a device without the user being told.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceFeaturesPayload {
    /// The feature ids this device's build supports.
    pub features: Vec<String>,
    /// Fields this build does not know, kept byte for byte.
    #[serde(flatten)]
    pub unknown: sunrise_domain::Unknowns,
}

/// One Stream shared with one Person, as the owner's account records it
/// (`docs/03-crypto/sharing-with-others.md`, `DOC_SCHEMA_V = 32`).
///
/// `grant` is a [`sunrise_crypto::ShareGrant`] in its own canonical encoding:
/// the Stream key of one epoch, HPKE-sealed to the recipient identity's
/// `ID_D_pub` and signed by the granting account identity's `ID_S_priv`. This
/// op is the account's record of it, sealed on the vault-meta stream like
/// every control op, so the recipient never reads it here; the grant reaches
/// the recipient beside the relay viewer grant's claim token (ADR-0074).
///
/// The op carries what the grant does not and a later rotation needs:
/// `recipient_id_d_pub`, the key a re-seal of the next epoch is sealed to, and
/// `person`, the Person the owner shared with. The grant names the recipient
/// only by identity id, which a re-seal cannot seal to.
///
/// The receiver stores it whatever it says; the share fold
/// (`engine::share_grant`) reads the grant and believes it only where it
/// decodes, names a Person, and verifies under an identity on the account's
/// chain, from a sender whose reads are not bounded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShareGrantOpPayload {
    /// The Person the Stream is shared with: a `per_` ref whose
    /// `identity_id` is the grant's recipient.
    pub person: sunrise_id::EntityRef,
    /// The recipient identity's `ID_D_pub`, which every re-seal is sealed to.
    #[serde(with = "serde_bytes")]
    pub recipient_id_d_pub: [u8; 32],
    /// The canonical [`sunrise_crypto::ShareGrant`], at most
    /// [`sunrise_crypto::MAX_SHARE_GRANT_BYTES`].
    #[serde(with = "serde_bytes")]
    pub grant: Vec<u8>,
    /// This grant carries a share forward to a later epoch, as a rotation or
    /// a backfill re-seals it, rather than sharing the Stream anew
    /// (`DOC_SCHEMA_V = 33`). Absent on the wire when false, so a fresh
    /// grant encodes as it did at 32. A re-seal never revives a share a
    /// `ShareRevoke` or `ShareDecline` ended; only a fresh grant does.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub reseal: bool,
    /// Fields this build does not know, kept byte for byte.
    #[serde(flatten)]
    pub unknown: sunrise_domain::Unknowns,
}

/// The owner's account ends one share: `stream_id` is no longer shared with
/// `recipient_identity_id` (`docs/03-crypto/sharing-with-others.md`
/// §Revoking access, `DOC_SCHEMA_V = 33`).
///
/// The op ends every grant of that recipient the sender's replica had logged
/// before it. The emitter rotates the Stream key beside it, and that
/// rotation's re-seal, and every later one, leaves the recipient out. Only a
/// fresh `ShareGrant` logged after it shares the Stream with them again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShareRevokePayload {
    /// The Stream no longer shared.
    #[serde(with = "serde_bytes")]
    pub stream_id: [u8; 16],
    /// The recipient identity the share was sealed to.
    #[serde(with = "serde_bytes")]
    pub recipient_identity_id: [u8; 16],
    /// When the owner revoked, on the emitting device's clock: the cutoff a
    /// recipient discards later ops at.
    pub effective_at_ms: u64,
    /// Fields this build does not know, kept byte for byte.
    #[serde(flatten)]
    pub unknown: sunrise_domain::Unknowns,
}

/// The owner's account records that a recipient declined one share
/// (`docs/03-crypto/sharing-with-others.md` §Granting access step 7,
/// `DOC_SCHEMA_V = 33`).
///
/// It ends the share as a [`ShareRevokePayload`] does, with no rotation of
/// its own: the recipient chose not to read. The recipient's decline reaches
/// the owner out of band, as the grant reached the recipient; this op is the
/// owner account's record of it, so every owner device stops re-sealing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShareDeclinePayload {
    /// The Stream the recipient declined.
    #[serde(with = "serde_bytes")]
    pub stream_id: [u8; 16],
    /// The recipient identity that declined.
    #[serde(with = "serde_bytes")]
    pub recipient_identity_id: [u8; 16],
    /// When the decline was recorded, on the recording device's clock.
    pub declined_at_ms: u64,
    /// Fields this build does not know, kept byte for byte.
    #[serde(flatten)]
    pub unknown: sunrise_domain::Unknowns,
}

/// A device without `ID_S_priv` minted `(stream_id, epoch)` and could not
/// re-seal it, and asks a device that holds `ID_S_priv` to re-seal it to
/// `recipients` (`docs/03-crypto/sharing-with-others.md` §Revoking access,
/// `DOC_SCHEMA_V = 33`).
///
/// `recipients` is the share fold on the minting device when it minted: the
/// shares its own re-seal would have reached. A holder re-seals the epoch to
/// no one else, so a share the minter knew had ended is never re-sealed by a
/// holder that has not yet applied the end.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShareResealRequestPayload {
    /// The Stream whose epoch was minted.
    #[serde(with = "serde_bytes")]
    pub stream_id: [u8; 16],
    /// The epoch minted.
    pub epoch: u32,
    /// `BLAKE3.derive_key("sunrise.stream_key_id.v1", stream_key, 8)` of the
    /// key minted, as a `KeyEnvelope` carries it: which of two keys minted
    /// at the same epoch is meant.
    #[serde(with = "serde_bytes")]
    pub key_id: [u8; 8],
    /// The recipients the minter's re-seal would have reached.
    pub recipients: Vec<ResealRecipient>,
    /// Fields this build does not know, kept byte for byte.
    #[serde(flatten)]
    pub unknown: sunrise_domain::Unknowns,
}

/// One recipient a [`ShareResealRequestPayload`] names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResealRecipient {
    /// The recipient identity whose share was in force.
    #[serde(with = "serde_bytes")]
    pub identity_id: [u8; 16],
}

/// One device's identity-signed cert, logged in a shared Stream's own log so
/// a recipient that reads none of the account's vault-meta log can verify
/// the device's ops there (ADR-0076 §1, `DOC_SCHEMA_V = 34`).
///
/// Logged per `(Stream, epoch, device)`: a recipient holds only the epochs it
/// was sealed. Any device of the account may log any device's cert, because
/// the identity's signature over it, not the envelope's, is what a recipient
/// checks. The account's own replicas store nothing from it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamWriterCertPayload {
    /// The canonical-CBOR `DeviceCert`.
    #[serde(with = "serde_bytes")]
    pub cert: Vec<u8>,
    /// The account's identity chain record, genesis first
    /// (`sunrise_crypto::IdentityChain`), as the emitter folded it.
    #[serde(with = "serde_bytes")]
    pub chain: Vec<u8>,
    /// Fields this build does not know, kept byte for byte.
    #[serde(flatten)]
    pub unknown: sunrise_domain::Unknowns,
}

/// A re-sealed share grant, logged in the shared Stream's own log under the
/// epoch the share's previous grant sealed, so its recipient reads the next
/// epoch from the channel it already reads (ADR-0076 §2,
/// `DOC_SCHEMA_V = 34`).
///
/// A copy of a re-seal `ShareGrant` op's `grant`, logged beside it. The
/// recipient checks it as it checks the grant its link carried, against the
/// identity that signed it, which `chain` must hold beside the granter's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamShareGrantPayload {
    /// The canonical [`sunrise_crypto::ShareGrant`], at most
    /// [`sunrise_crypto::MAX_SHARE_GRANT_BYTES`].
    #[serde(with = "serde_bytes")]
    pub grant: Vec<u8>,
    /// The account's identity chain record, genesis first.
    #[serde(with = "serde_bytes")]
    pub chain: Vec<u8>,
    /// Fields this build does not know, kept byte for byte.
    #[serde(flatten)]
    pub unknown: sunrise_domain::Unknowns,
}

// The payloads above, and the two `inner_op` keeps beside the op family it
// defines, described field by field for the canonical document schema
// (`crate::doc_schema`). A field or variant added to one of these types and
// not described here fails this crate's build.
#[allow(unused_imports)]
use crate::inner_op::{
    FrontierWire, GroupWire, InnerOp, OpRefWire, PatchPayload, StreamDigestPayload,
};
#[allow(unused_imports)]
use sunrise_domain::Unknowns;
#[allow(unused_imports)]
use sunrise_id::EntityRef;

sunrise_id::describe_value_types! {
    /// Every value type an `InnerOp` payload the entity registry does not
    /// declare carries, field by field. Read only by the test-only
    /// `crate::doc_schema`; the checks against each type run in every build.
    #[cfg(test)]
    pub(crate) static PAYLOAD_VALUE_TYPES;
    records: [
        KeyEnvelopePayload {
            stream_id: [u8; 16];
            epoch: u32;
            recipient: Recipient;
            key_id: [u8; 8];
            hpke_ciphertext: Vec<u8>;
        }
        DeviceRevokePayload {
            revoked_device_id: [u8; 16];
            reason_code: RevokeReason;
        }
        DeviceRevokeByIdentityPayload {
            revoked_device_id: [u8; 16];
            reason_code: RevokeReason;
            identity_id: [u8; 16];
            identity_sig: [u8; 64];
        }
        DeviceRevokeWithdrawPayload {
            revoked_device_id: [u8; 16];
        }
        DeviceRevokeWithdrawByIdentityPayload {
            revoked_device_id: [u8; 16];
            identity_id: [u8; 16];
            identity_sig: [u8; 64];
        }
        RosterEntry {
            cert: Vec<u8>;
        }
        KeyShare {
            device_id: [u8; 16];
            hpke_ciphertext: Vec<u8>;
        }
        IdentityTransitionPayload {
            from_identity_id: [u8; 16];
            to_identity_id: [u8; 16];
            to_id_s_pub: [u8; 32];
            to_id_d_pub: [u8; 32];
            roster: Vec<RosterEntry>;
            device_shares: Vec<KeyShare>;
            identity_share: Option<ByteBuf>;
            prev_sig: [u8; 64];
            next_sig: [u8; 64];
        }
        VaultRequiresPayload {
            features: Vec<String>;
            ..unknown
        }
        DeviceFeaturesPayload {
            features: Vec<String>;
            ..unknown
        }
        ShareGrantOpPayload {
            person: EntityRef;
            recipient_id_d_pub: [u8; 32];
            grant: Vec<u8>;
            reseal: bool;
            ..unknown
        }
        ShareRevokePayload {
            stream_id: [u8; 16];
            recipient_identity_id: [u8; 16];
            effective_at_ms: u64;
            ..unknown
        }
        ShareDeclinePayload {
            stream_id: [u8; 16];
            recipient_identity_id: [u8; 16];
            declined_at_ms: u64;
            ..unknown
        }
        ShareResealRequestPayload {
            stream_id: [u8; 16];
            epoch: u32;
            key_id: [u8; 8];
            recipients: Vec<ResealRecipient>;
            ..unknown
        }
        ResealRecipient {
            identity_id: [u8; 16];
        }
        StreamWriterCertPayload {
            cert: Vec<u8>;
            chain: Vec<u8>;
            ..unknown
        }
        StreamShareGrantPayload {
            grant: Vec<u8>;
            chain: Vec<u8>;
            ..unknown
        }
        PatchPayload {
            target as "ref": EntityRef;
            create: bool;
            origin: Option<String>;
            parents: Option<Vec<OpRefWire>>;
            group: Option<GroupWire>;
            fields: Unknowns;
            ..unknown
        }
        GroupWire {
            id: [u8; 16];
            size: u64;
            ..unknown
        }
        StreamDigestPayload {
            frontier: Vec<FrontierWire>;
            digest: [u8; 32];
            ..unknown
        }
    ],
    newtypes: [],
    aliases: [],
    tuples: [
        FrontierWire([u8; 16], u64, [u8; 32]);
        OpRefWire([u8; 16], [u8; 16], u64);
    ],
    variants: [
        Recipient external {
            Device = ([u8; 16]);
            Identity = ([u8; 16]);
        }
        RevokeReason external {
            Lost = unit;
            Stolen = unit;
            Retired = unit;
            Compromised = unit;
        }
    ],
}

/// Declares `OP_PAYLOADS` and checks each payload type against the
/// variant: the constructor must coerce to `fn(Payload) -> InnerOp`.
macro_rules! op_payloads {
    ($( $variant:ident($payload:ty) ),* $(,)?) => {
        /// Every `InnerOp` variant the entity registry does not declare, with
        /// its payload type as written. A test in `crate::doc_schema` holds
        /// this list to the decoder's own variant list, so it is test-only,
        /// as that module is; the check below runs in every build.
        #[cfg(test)]
        pub(crate) const OP_PAYLOADS: &[(&str, &str)] = &[
            $( (::core::stringify!($variant), ::core::stringify!($payload)), )*
        ];

        const _: () = {
            #[allow(dead_code)]
            fn described_payloads_are_the_variant_payloads() {
                $( let _: fn($payload) -> InnerOp = InnerOp::$variant; )*
            }
        };
    };
}

op_payloads! {
    KeyEnvelope(KeyEnvelopePayload),
    DeviceRevoke(DeviceRevokePayload),
    DeviceCertPublish(Vec<u8>),
    IdentityTransition(Box<IdentityTransitionPayload>),
    Patch(Box<PatchPayload>),
    StreamDigest(StreamDigestPayload),
    VaultRequires(VaultRequiresPayload),
    DeviceFeatures(DeviceFeaturesPayload),
    DeviceRevokeByIdentity(DeviceRevokeByIdentityPayload),
    DeviceRevokeWithdraw(DeviceRevokeWithdrawPayload),
    DeviceRevokeWithdrawByIdentity(DeviceRevokeWithdrawByIdentityPayload),
    ShareGrant(ShareGrantOpPayload),
    ShareRevoke(ShareRevokePayload),
    ShareDecline(ShareDeclinePayload),
    ShareResealRequest(ShareResealRequestPayload),
    StreamWriterCert(StreamWriterCertPayload),
    StreamShareGrant(StreamShareGrantPayload),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use sunrise_id::registry::{Tagging, ValueShape, VariantShape};

    fn encode<T: Serialize>(value: &T) -> ciborium::value::Value {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(value, &mut buf).unwrap();
        ciborium::de::from_reader(buf.as_slice()).unwrap()
    }

    fn described(name: &str) -> ValueShape {
        PAYLOAD_VALUE_TYPES
            .iter()
            .find(|v| v.name == name)
            .unwrap_or_else(|| panic!("{name} is not described"))
            .shape
    }

    /// The map keys serde writes for `value` are the fields described for
    /// `name`, less the ones `skipped` names.
    fn assert_wire_names<T: Serialize>(name: &str, value: &T, skipped: &[&str]) {
        let ValueShape::Record { fields, .. } = described(name) else {
            panic!("{name} is described as a record");
        };
        let ciborium::value::Value::Map(entries) = encode(value) else {
            panic!("{name} must encode as a map");
        };
        let written: BTreeSet<String> = entries
            .into_iter()
            .map(|(k, _)| k.into_text().expect("text key"))
            .collect();
        let names: BTreeSet<String> = fields.iter().map(|f| f.name.to_owned()).collect();
        let expected: BTreeSet<String> = names
            .iter()
            .filter(|n| !skipped.contains(&n.as_str()))
            .cloned()
            .collect();
        assert_eq!(written, expected, "{name}'s wire names drifted");
        for s in skipped {
            assert!(names.contains(*s), "{name}: `{s}` is not described");
        }
    }

    #[test]
    fn payload_records_write_their_described_names() {
        use crate::inner_op::PatchPayload;
        assert_wire_names(
            "KeyEnvelopePayload",
            &KeyEnvelopePayload {
                stream_id: [2u8; 16],
                epoch: 7,
                recipient: Recipient::Identity([3u8; 16]),
                key_id: [4u8; 8],
                hpke_ciphertext: vec![5u8; 80],
            },
            &[],
        );
        assert_wire_names(
            "DeviceRevokePayload",
            &DeviceRevokePayload {
                revoked_device_id: [6u8; 16],
                reason_code: RevokeReason::Lost,
            },
            &[],
        );
        assert_wire_names(
            "DeviceRevokeByIdentityPayload",
            &DeviceRevokeByIdentityPayload {
                revoked_device_id: [6u8; 16],
                reason_code: RevokeReason::Stolen,
                identity_id: [7u8; 16],
                identity_sig: [8u8; 64],
            },
            &[],
        );
        assert_wire_names(
            "DeviceRevokeWithdrawPayload",
            &DeviceRevokeWithdrawPayload {
                revoked_device_id: [9u8; 16],
            },
            &[],
        );
        assert_wire_names(
            "DeviceRevokeWithdrawByIdentityPayload",
            &DeviceRevokeWithdrawByIdentityPayload {
                revoked_device_id: [9u8; 16],
                identity_id: [7u8; 16],
                identity_sig: [8u8; 64],
            },
            &[],
        );
        let t = a_transition();
        assert_wire_names("IdentityTransitionPayload", &t, &[]);
        assert_wire_names("RosterEntry", &t.roster[0], &[]);
        assert_wire_names("KeyShare", &t.device_shares[0], &[]);
        assert_wire_names(
            "VaultRequiresPayload",
            &VaultRequiresPayload {
                features: Vec::new(),
                unknown: Unknowns::new(),
            },
            &[],
        );
        assert_wire_names(
            "DeviceFeaturesPayload",
            &DeviceFeaturesPayload {
                features: Vec::new(),
                unknown: Unknowns::new(),
            },
            &[],
        );
        assert_wire_names(
            "ShareGrantOpPayload",
            &ShareGrantOpPayload {
                person: EntityRef::new(sunrise_id::EntityKind::Person, [1u8; 16]),
                recipient_id_d_pub: [2u8; 32],
                grant: vec![3u8; 4],
                reseal: true,
                unknown: Unknowns::new(),
            },
            &[],
        );
        assert_wire_names(
            "PatchPayload",
            &PatchPayload {
                target: EntityRef::new(sunrise_id::EntityKind::Task, [1u8; 16]),
                create: true,
                origin: Some(PatchPayload::GENERATED.into()),
                parents: Some(vec![OpRefWire([1u8; 16], [2u8; 16], 3)]),
                group: Some(a_group()),
                fields: Unknowns::new(),
                unknown: Unknowns::new(),
            },
            &[],
        );
        assert_wire_names("GroupWire", &a_group(), &[]);
        assert_wire_names(
            "StreamDigestPayload",
            &StreamDigestPayload {
                frontier: vec![FrontierWire([1u8; 16], 2, [3u8; 32])],
                digest: [4u8; 32],
                unknown: Unknowns::new(),
            },
            &[],
        );
    }

    /// The share families' records, beside the test above.
    #[test]
    fn share_payload_records_write_their_described_names() {
        assert_wire_names(
            "ShareGrantOpPayload",
            &ShareGrantOpPayload {
                person: EntityRef::new(sunrise_id::EntityKind::Person, [1u8; 16]),
                recipient_id_d_pub: [2u8; 32],
                grant: vec![3u8; 4],
                reseal: false,
                unknown: Unknowns::new(),
            },
            &["reseal"],
        );
        assert_wire_names(
            "ShareRevokePayload",
            &ShareRevokePayload {
                stream_id: [1u8; 16],
                recipient_identity_id: [2u8; 16],
                effective_at_ms: 3,
                unknown: Unknowns::new(),
            },
            &[],
        );
        assert_wire_names(
            "ShareDeclinePayload",
            &ShareDeclinePayload {
                stream_id: [1u8; 16],
                recipient_identity_id: [2u8; 16],
                declined_at_ms: 3,
                unknown: Unknowns::new(),
            },
            &[],
        );
        let request = ShareResealRequestPayload {
            stream_id: [1u8; 16],
            epoch: 2,
            key_id: [3u8; 8],
            recipients: vec![ResealRecipient {
                identity_id: [4u8; 16],
            }],
            unknown: Unknowns::new(),
        };
        assert_wire_names("ShareResealRequestPayload", &request, &[]);
        assert_wire_names("ResealRecipient", &request.recipients[0], &[]);
        assert_wire_names(
            "StreamWriterCertPayload",
            &StreamWriterCertPayload {
                cert: vec![1u8; 4],
                chain: vec![2u8; 4],
                unknown: Unknowns::new(),
            },
            &[],
        );
        assert_wire_names(
            "StreamShareGrantPayload",
            &StreamShareGrantPayload {
                grant: vec![1u8; 4],
                chain: vec![2u8; 4],
                unknown: Unknowns::new(),
            },
            &[],
        );
    }

    /// Every described record is reached by the wire tests above.
    #[test]
    fn every_described_payload_record_is_checked_on_the_wire() {
        let checked = [
            "KeyEnvelopePayload",
            "DeviceRevokePayload",
            "DeviceRevokeByIdentityPayload",
            "DeviceRevokeWithdrawPayload",
            "DeviceRevokeWithdrawByIdentityPayload",
            "IdentityTransitionPayload",
            "RosterEntry",
            "KeyShare",
            "VaultRequiresPayload",
            "DeviceFeaturesPayload",
            "ShareGrantOpPayload",
            "ShareRevokePayload",
            "ShareDeclinePayload",
            "ShareResealRequestPayload",
            "ResealRecipient",
            "StreamWriterCertPayload",
            "StreamShareGrantPayload",
            "PatchPayload",
            "GroupWire",
            "StreamDigestPayload",
        ];
        for v in PAYLOAD_VALUE_TYPES {
            if matches!(v.shape, ValueShape::Record { .. }) {
                assert!(
                    checked.contains(&v.name),
                    "{} has no wire-name check",
                    v.name
                );
            }
        }
    }

    /// The externally tagged enums write their described variant names: a
    /// unit variant as its name, a newtype variant as a one-entry map.
    #[test]
    fn payload_enums_write_their_described_variants() {
        use ciborium::value::Value;
        let variants = |name: &str| {
            let ValueShape::Variants {
                tagging: Tagging::External,
                variants,
                keeps_unknowns: false,
            } = described(name)
            else {
                panic!("{name} is externally tagged and refuses an unknown variant");
            };
            variants
        };
        let reasons = [
            RevokeReason::Lost,
            RevokeReason::Stolen,
            RevokeReason::Retired,
            RevokeReason::Compromised,
        ];
        let described_reasons = variants("RevokeReason");
        assert_eq!(described_reasons.len(), reasons.len());
        for (reason, spec) in reasons.iter().zip(described_reasons) {
            assert!(matches!(spec.shape, VariantShape::Unit));
            assert_eq!(encode(reason), Value::Text(spec.name.into()));
            assert_eq!(reason.as_str(), spec.name);
        }
        let recipients = [Recipient::Device([1u8; 16]), Recipient::Identity([1u8; 16])];
        let described_recipients = variants("Recipient");
        assert_eq!(described_recipients.len(), recipients.len());
        for (recipient, spec) in recipients.iter().zip(described_recipients) {
            assert!(matches!(spec.shape, VariantShape::Newtype(_)));
            assert_eq!(
                encode(recipient),
                Value::Map(vec![(
                    Value::Text(spec.name.into()),
                    Value::Bytes(vec![1u8; 16])
                )])
            );
        }
    }

    /// A frontier entry is a three-item array, in its described order.
    #[test]
    fn frontier_wire_is_a_described_tuple() {
        use ciborium::value::Value;
        let ValueShape::Tuple(items) = described("FrontierWire") else {
            panic!("FrontierWire is described as a tuple");
        };
        let Value::Array(written) = encode(&FrontierWire([1u8; 16], 2, [3u8; 32])) else {
            panic!("FrontierWire must encode as an array");
        };
        assert_eq!(written.len(), items.len());
    }

    /// An op-ref is `[stream_id, device_id, seq]`: two byte strings and an
    /// integer, in its described order (ADR-0065 §1).
    #[test]
    fn op_ref_wire_is_a_described_tuple() {
        use ciborium::value::Value;
        let ValueShape::Tuple(items) = described("OpRefWire") else {
            panic!("OpRefWire is described as a tuple");
        };
        assert_eq!(
            encode(&OpRefWire([1u8; 16], [2u8; 16], 3)),
            Value::Array(vec![
                Value::Bytes(vec![1u8; 16]),
                Value::Bytes(vec![2u8; 16]),
                Value::Integer(3.into()),
            ])
        );
        assert_eq!(items.len(), 3);
    }

    /// `{ "features": [...], unknown-fields }`, as ADR-0045 §7's CDDL says,
    /// and a field a later build adds survives the round trip.
    #[test]
    fn vault_requires_payload_keeps_a_field_it_does_not_know() {
        use ciborium::value::Value;
        let wire = Value::Map(vec![
            (
                Value::Text("features".into()),
                Value::Array(vec![Value::Text("task.deadlines_v2".into())]),
            ),
            (Value::Text("later".into()), Value::Integer(9.into())),
        ]);
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&wire, &mut buf).unwrap();
        let p: VaultRequiresPayload = ciborium::de::from_reader(buf.as_slice()).unwrap();
        assert_eq!(p.features, vec!["task.deadlines_v2".to_owned()]);
        assert_eq!(p.unknown.len(), 1);
        let mut out = Vec::new();
        ciborium::ser::into_writer(&p, &mut out).unwrap();
        let back: Value = ciborium::de::from_reader(out.as_slice()).unwrap();
        assert_eq!(back, wire);
    }

    #[test]
    fn device_features_payload_round_trips() {
        let p = DeviceFeaturesPayload {
            features: vec!["core.field_ops".into(), "task.optional_stream".into()],
            unknown: sunrise_domain::Unknowns::new(),
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&p, &mut buf).unwrap();
        let back: DeviceFeaturesPayload = ciborium::de::from_reader(buf.as_slice()).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn revoke_reason_round_trips_through_its_stored_form() {
        for r in [
            RevokeReason::Lost,
            RevokeReason::Stolen,
            RevokeReason::Retired,
            RevokeReason::Compromised,
        ] {
            assert_eq!(RevokeReason::parse(r.as_str()), Some(r));
        }
        assert_eq!(RevokeReason::parse("Borrowed"), None);
    }

    /// The wire encoding is externally tagged, exactly as the CDDL in
    /// `data-encryption-format.md` says. Pinning it here means a serde
    /// attribute added upstream cannot silently re-shape a signed op.
    #[test]
    fn recipient_encodes_as_a_single_entry_map() {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&Recipient::Device([1u8; 16]), &mut buf).unwrap();
        let value: ciborium::value::Value = ciborium::de::from_reader(buf.as_slice()).unwrap();
        let ciborium::value::Value::Map(map) = value else {
            panic!("Recipient must encode as a map");
        };
        assert_eq!(map.len(), 1);
        assert_eq!(map[0].0, ciborium::value::Value::Text("Device".into()));
    }

    #[test]
    fn key_envelope_payload_round_trips() {
        let p = KeyEnvelopePayload {
            stream_id: [2u8; 16],
            epoch: 7,
            recipient: Recipient::Identity([3u8; 16]),
            key_id: [4u8; 8],
            hpke_ciphertext: vec![5u8; 80],
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&p, &mut buf).unwrap();
        let back: KeyEnvelopePayload = ciborium::de::from_reader(buf.as_slice()).unwrap();
        assert_eq!(back, p);
    }

    fn a_group() -> GroupWire {
        GroupWire {
            id: [5u8; 16],
            size: 2,
            unknown: Unknowns::new(),
        }
    }

    fn a_transition() -> IdentityTransitionPayload {
        IdentityTransitionPayload {
            from_identity_id: [7u8; 16],
            to_identity_id: [8u8; 16],
            to_id_s_pub: [9u8; 32],
            to_id_d_pub: [10u8; 32],
            roster: vec![RosterEntry {
                cert: vec![11u8; 200],
            }],
            device_shares: vec![KeyShare {
                device_id: [12u8; 16],
                hpke_ciphertext: vec![13u8; 80],
            }],
            identity_share: Some(ByteBuf::from(vec![14u8; 112])),
            prev_sig: [15u8; 64],
            next_sig: [16u8; 64],
        }
    }

    #[test]
    fn identity_transition_payload_round_trips() {
        let p = a_transition();
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&p, &mut buf).unwrap();
        let back: IdentityTransitionPayload = ciborium::de::from_reader(buf.as_slice()).unwrap();
        assert_eq!(back, p);

        // The carry-forward share is genuinely optional, and `None` must not
        // decode back as an empty blob: absent and empty mean different things
        // to `shares_digest`'s present flag.
        let mut without = a_transition();
        without.identity_share = None;
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&without, &mut buf).unwrap();
        let back: IdentityTransitionPayload = ciborium::de::from_reader(buf.as_slice()).unwrap();
        assert_eq!(back.identity_share, None);
    }

    /// Every fixed-width field is a `bstr`, never an array of integers. The
    /// note at the top of this module is the rule; this is the assertion, and
    /// it reaches the nested `KeyShare` and `RosterEntry` too, where a dropped
    /// `serde_bytes` attribute would be easiest to miss.
    #[test]
    fn identity_transition_byte_fields_encode_as_byte_strings() {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&a_transition(), &mut buf).unwrap();
        let value: ciborium::value::Value = ciborium::de::from_reader(buf.as_slice()).unwrap();
        let ciborium::value::Value::Map(map) = value else {
            panic!("the payload must encode as a map");
        };
        for (key, entry) in &map {
            let ciborium::value::Value::Text(name) = key else {
                panic!("field names are text keys");
            };
            match name.as_str() {
                "roster" | "device_shares" => {
                    let ciborium::value::Value::Array(items) = entry else {
                        panic!("{name} must be an array");
                    };
                    let ciborium::value::Value::Map(inner) = &items[0] else {
                        panic!("{name} entries must be maps");
                    };
                    for (_, v) in inner {
                        assert!(
                            matches!(v, ciborium::value::Value::Bytes(_)),
                            "{name} carries only byte strings"
                        );
                    }
                }
                _ => assert!(
                    matches!(entry, ciborium::value::Value::Bytes(_)),
                    "`{name}` must be a bstr, not an array of integers"
                ),
            }
        }
        assert_eq!(map.len(), 9);
    }

    #[test]
    fn device_revoke_payload_round_trips() {
        let p = DeviceRevokePayload {
            revoked_device_id: [6u8; 16],
            reason_code: RevokeReason::Stolen,
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&p, &mut buf).unwrap();
        let back: DeviceRevokePayload = ciborium::de::from_reader(buf.as_slice()).unwrap();
        assert_eq!(back, p);
    }

    /// Encode `p` and decode it back, as the op codec does.
    fn round_trip<T: Serialize + serde::de::DeserializeOwned>(p: &T) -> T {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(p, &mut buf).unwrap();
        ciborium::de::from_reader(buf.as_slice()).unwrap()
    }

    mod generated {
        use super::*;
        use proptest::prelude::*;

        fn blob() -> impl Strategy<Value = Vec<u8>> {
            proptest::collection::vec(any::<u8>(), 0..96)
        }

        fn recipient() -> impl Strategy<Value = Recipient> {
            prop_oneof![
                any::<[u8; 16]>().prop_map(Recipient::Device),
                any::<[u8; 16]>().prop_map(Recipient::Identity),
            ]
        }

        fn reason() -> impl Strategy<Value = RevokeReason> {
            prop_oneof![
                Just(RevokeReason::Lost),
                Just(RevokeReason::Stolen),
                Just(RevokeReason::Retired),
                Just(RevokeReason::Compromised),
            ]
        }

        fn features() -> impl Strategy<Value = Vec<String>> {
            proptest::collection::vec("[a-z][a-z0-9_]{0,8}(\\.[a-z0-9_]{1,8}){1,2}", 0..4)
        }

        proptest! {
            #[test]
            fn every_key_envelope_payload_round_trips(
                stream_id in any::<[u8; 16]>(),
                epoch in any::<u32>(),
                recipient in recipient(),
                key_id in any::<[u8; 8]>(),
                hpke_ciphertext in blob(),
            ) {
                let p = KeyEnvelopePayload { stream_id, epoch, recipient, key_id, hpke_ciphertext };
                prop_assert_eq!(round_trip(&p), p);
            }

            #[test]
            fn every_device_revoke_payload_round_trips(
                revoked_device_id in any::<[u8; 16]>(),
                reason_code in reason(),
            ) {
                let p = DeviceRevokePayload { revoked_device_id, reason_code };
                prop_assert_eq!(round_trip(&p), p);
                prop_assert_eq!(RevokeReason::parse(reason_code.as_str()), Some(reason_code));
            }

            /// Including an absent carry share against an empty one, which
            /// `shares_digest` tells apart.
            #[test]
            fn every_identity_transition_payload_round_trips(
                ids in any::<([u8; 16], [u8; 16])>(),
                pubs in any::<([u8; 32], [u8; 32])>(),
                sigs in any::<([u8; 32], [u8; 32], [u8; 32], [u8; 32])>(),
                roster in proptest::collection::vec(blob(), 0..3),
                shares in proptest::collection::vec((any::<[u8; 16]>(), blob()), 0..3),
                identity_share in proptest::option::of(blob()),
            ) {
                let sig = |a: [u8; 32], b: [u8; 32]| {
                    let mut s = [0u8; 64];
                    s[..32].copy_from_slice(&a);
                    s[32..].copy_from_slice(&b);
                    s
                };
                let p = IdentityTransitionPayload {
                    from_identity_id: ids.0,
                    to_identity_id: ids.1,
                    to_id_s_pub: pubs.0,
                    to_id_d_pub: pubs.1,
                    roster: roster.into_iter().map(|cert| RosterEntry { cert }).collect(),
                    device_shares: shares
                        .into_iter()
                        .map(|(device_id, hpke_ciphertext)| KeyShare { device_id, hpke_ciphertext })
                        .collect(),
                    identity_share: identity_share.map(ByteBuf::from),
                    prev_sig: sig(sigs.0, sigs.1),
                    next_sig: sig(sigs.2, sigs.3),
                };
                prop_assert_eq!(round_trip(&p), p);
            }

            #[test]
            fn every_feature_payload_round_trips(features in features()) {
                let d = DeviceFeaturesPayload { features: features.clone(), unknown: Unknowns::new() };
                prop_assert_eq!(round_trip(&d), d);
                let v = VaultRequiresPayload { features, unknown: Unknowns::new() };
                prop_assert_eq!(round_trip(&v), v);
            }
        }
    }
}
