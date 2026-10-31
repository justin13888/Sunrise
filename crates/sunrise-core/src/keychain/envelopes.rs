//! Sealing and opening: key envelopes, the key bundle, and op envelopes.

use crate::config::Rng;
use sunrise_crypto::aead::AEAD_NONCE_LEN;
use sunrise_crypto::op_envelope::open_envelope;
use sunrise_crypto::{
    decode_envelope, encode_chained_envelope, hpke_open, hpke_open_identity, hpke_seal,
    key_envelope_info, verify_envelope, AeadAlgId, ChainLinks, OpEnvelopeError, StreamKey,
};

use super::{to16, EnvelopeRecipient, Keychain, KeychainError, RngAdapter, WRITABLE_KEY};

impl Keychain {
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
}
