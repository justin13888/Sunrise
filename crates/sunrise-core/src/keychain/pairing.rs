//! Admitting another device: the offer, the grant, the payload and its cert.

use rusqlite::{params, OptionalExtension};
use std::collections::{BTreeMap, BTreeSet};
use sunrise_crypto::{DeviceCert, DeviceCertInner, VaultRootKey};
use sunrise_pairing::{PairingGrant, PairingOffer, PairingPayload, PairingRequest};
use sunrise_storage::Db;

use super::{Keychain, KeychainError};

impl Keychain {
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
}
