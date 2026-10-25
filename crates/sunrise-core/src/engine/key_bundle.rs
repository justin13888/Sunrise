//! The account's key bundle on this replica: what it would publish, what it
//! makes of the one the relay pins, and absorbing one (ADR-0069).
//!
//! The format and its checks are `sunrise_crypto::key_bundle`'s. This module
//! decides the three things that need the vault:
//!
//! - **Whether this device publishes at all**, and what. A device publishes
//!   only while it signs under the identity in force and this replica has not
//!   bounded it, and only the keys it may write under.
//! - **Whether a pinned bundle is one to keep.** It must verify under the
//!   identity in force, and its publisher must not be a device this replica
//!   has bounded: a revoked device that pinned a bundle of the epochs it was
//!   cut at is replaced by the next honest publisher.
//! - **Absorbing one**, which is how a device restored from the recovery code
//!   gets its keys once the relay has trimmed the envelopes. Each key is
//!   recorded as delivered by the publisher, so the read bound applies to it
//!   exactly as to an envelope from that device.

use sunrise_crypto::{decode_key_bundle, BundledKey, KeyBundleError, ManifestEntry};
use sunrise_storage::Db;

use super::ids::hex_short;
use super::{Engine, EngineError};
use crate::events::DomainEvent;
use crate::keychain::{KeySource, KeychainError};

/// The highest epoch a bundle may name. Rotation adds one per revocation, so
/// no account comes near it; it stops a hostile bundle from absorbing an epoch
/// at the top of `u32`, from which `mint_epoch` could never move on.
const MAX_BUNDLE_EPOCH: u32 = 1 << 24;

/// What this replica would publish as the account's key bundle.
#[derive(Debug, Clone)]
pub(crate) struct KeyBundlePlan {
    /// Every key it may write under.
    pub(crate) keys: Vec<BundledKey>,
    /// Their manifest.
    pub(crate) manifest: Vec<ManifestEntry>,
    /// The manifest's digest, which the relay stores beside the pin.
    pub(crate) digest: [u8; 32],
}

/// What this replica makes of the bundle the relay pins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PinnedKeyBundle {
    /// A bundle of this account by a device in good standing here, with its
    /// manifest.
    Valid(Vec<ManifestEntry>),
    /// Not one to keep: malformed, of another identity, forged, or published
    /// by a device this replica has bounded. Named for the log.
    Refused(&'static str),
}

/// What absorbing a key bundle did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyBundleApplied {
    /// Keys this replica did not hold before.
    pub absorbed: usize,
    /// Keys the bundle carried in all.
    pub carried: usize,
}

fn refusal(e: &KeyBundleError) -> &'static str {
    match e {
        KeyBundleError::Malformed(_) => "malformed",
        KeyBundleError::OtherIdentity => "other_identity",
        KeyBundleError::ForeignSigner => "foreign_signer",
        KeyBundleError::BadSignature => "bad_signature",
        KeyBundleError::Open => "does_not_open",
    }
}

impl Engine {
    /// The key bundle this replica would publish, or `None` when it must not
    /// publish one: it no longer signs under the identity in force, or this
    /// replica holds it as revoked or read-bounded.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn key_bundle_plan(
        &self,
        db: &mut Db,
    ) -> Result<Option<KeyBundlePlan>, EngineError> {
        let me = self.keychain.device_id();
        let keys = db.with_tx(|tx| {
            let head = self.current_identity(tx)?;
            if head.identity_id != self.keychain.identity_id()
                || self.is_revoked(tx, &me)?
                || self.is_read_bounded(tx, &me)?
            {
                return Ok(None);
            }
            self.keychain
                .writable_stream_keys_tx(tx)
                .map(Some)
                .map_err(crate::keychain::sqlite_from)
        })?;
        Ok(keys.filter(|k| !k.is_empty()).map(|keys| {
            let manifest = sunrise_crypto::manifest_of(&keys);
            let digest = sunrise_crypto::manifest_digest(&manifest);
            KeyBundlePlan {
                keys,
                manifest,
                digest,
            }
        }))
    }

    /// Seal `plan` into a key bundle signed by this device.
    ///
    /// # Errors
    /// [`EngineError::Keychain`].
    pub(crate) fn seal_key_bundle(&self, plan: &KeyBundlePlan) -> Result<Vec<u8>, EngineError> {
        self.keychain
            .seal_key_bundle(&plan.keys, self.clock.now_ms(), self.rng.as_ref())
            .map_err(|e| EngineError::Keychain(e.to_string()))
    }

    /// Judge the bundle the relay pins, without opening it: any device can,
    /// since the manifest and the signature are cleartext.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn judge_pinned_key_bundle(
        &self,
        db: &Db,
        bytes: &[u8],
    ) -> Result<PinnedKeyBundle, EngineError> {
        let bundle = match decode_key_bundle(bytes) {
            Ok(b) => b,
            Err(e) => return Ok(PinnedKeyBundle::Refused(refusal(&e))),
        };
        let verified = match self.keychain.verify_key_bundle(&bundle) {
            Ok(v) => v,
            Err(KeychainError::KeyBundle(e)) => return Ok(PinnedKeyBundle::Refused(refusal(&e))),
            Err(e) => return Err(EngineError::Keychain(e.to_string())),
        };
        let publisher = verified.signer().body.device_id;
        if self.is_revoked(db.conn(), &publisher)? || self.is_read_bounded(db.conn(), &publisher)? {
            return Ok(PinnedKeyBundle::Refused("publisher_bounded"));
        }
        Ok(PinnedKeyBundle::Valid(verified.header().manifest.clone()))
    }

    /// Absorb a key bundle: verify it under the identity in force, open it
    /// with `ID_D_priv`, and store every key, crediting each to the
    /// publisher. Ops parked for a key that arrived are drained.
    ///
    /// # Errors
    /// [`EngineError::Invalid`] for a bundle that is malformed, of another
    /// identity, forged, or does not open; [`EngineError::Keychain`] on a
    /// device that holds no `ID_D_priv`; storage failures.
    pub(crate) fn apply_key_bundle(
        &self,
        db: &mut Db,
        bytes: &[u8],
    ) -> Result<(KeyBundleApplied, Vec<DomainEvent>), EngineError> {
        let invalid = |e: KeyBundleError| EngineError::Invalid(format!("key bundle: {e}"));
        let bundle = decode_key_bundle(bytes).map_err(invalid)?;
        let verified = match self.keychain.verify_key_bundle(&bundle) {
            Ok(v) => v,
            Err(KeychainError::KeyBundle(e)) => return Err(invalid(e)),
            Err(e) => return Err(EngineError::Keychain(e.to_string())),
        };
        let keys = match self.keychain.open_key_bundle(&verified) {
            Ok(k) => k,
            Err(KeychainError::KeyBundle(e)) => return Err(invalid(e)),
            Err(e) => return Err(EngineError::Keychain(e.to_string())),
        };
        if keys.iter().any(|k| k.epoch > MAX_BUNDLE_EPOCH) {
            return Err(EngineError::Invalid(
                "key bundle: an epoch is out of range".to_owned(),
            ));
        }
        let publisher = verified.signer().body.device_id;
        let now_ms = self.clock.now_ms();
        let learned: Vec<([u8; 16], u32)> = db.with_tx(|tx| {
            let mut learned = Vec::new();
            for k in &keys {
                if self.keychain.absorb_stream_key(
                    tx,
                    &k.stream_id,
                    k.epoch,
                    &k.key,
                    KeySource::Bundle { publisher },
                    self.rng.as_ref(),
                    now_ms,
                )? {
                    learned.push((k.stream_id, k.epoch));
                }
            }
            Ok(learned)
        })?;
        let mut events = Vec::new();
        for (stream, epoch) in &learned {
            events.extend(self.drain_deferred(db, stream, *epoch)?);
        }
        tracing::info!(
            ev = "core.key_bundle.applied",
            sender_h = hex_short(&publisher),
            n_total = keys.len() as u64,
            n_imported = learned.len() as u64,
            "the account's key bundle was opened under the identity key and its keys stored"
        );
        Ok((
            KeyBundleApplied {
                absorbed: learned.len(),
                carried: keys.len(),
            },
            events,
        ))
    }
}
