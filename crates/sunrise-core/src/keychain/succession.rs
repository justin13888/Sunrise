//! Identity transition (ADR-0032): minting a successor, its shares, adopting it.

use crate::config::Rng;
use rusqlite::{params, OptionalExtension};
use sunrise_crypto::identity_transition::{IdentityTransitionBody, IdentityTransitionSigs};
use sunrise_crypto::{
    hpke_open, hpke_open_identity, hpke_seal, identity_carry_info, identity_id_from_pub,
    identity_share_info, sign_identity_transition, DeviceCert, DeviceCertInner, IdentityDhKeyPair,
    IdentitySigningKeyPair,
};
use zeroize::Zeroize;

use super::crypto::wrap_identity;
use super::{Identity, IdentitySigningKey, Keychain, KeychainError, RngAdapter, SuccessorPublics};

impl Keychain {
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
}
