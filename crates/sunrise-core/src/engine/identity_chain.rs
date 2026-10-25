//! A recovery code across an identity rotation (ADR-0070): the rotation that
//! retires a code, the chain record a device pins beside the recovery blob,
//! and seeding a recovered vault from that record.
//!
//! `identity` owns the fold and the rotation's single op. This module owns
//! what the recovery code needs from them:
//!
//! - **A rotation that retires the code** mints the vault-meta epoch its
//!   transition is sealed under before the identity moves, sealed to the
//!   devices alone, so no holder of the outgoing `ID_D_priv` can seal a
//!   competing transition that outranks it; then rotates every Stream, so the
//!   identity copies of the epochs written from now on are the successor's.
//! - **The chain record** is the fold's links, genesis to head, as the relay
//!   pins them. A device whose code is older than the head reads it before it
//!   can verify anything else.
//! - **Seeding** stores those links in a vault restored from the code and
//!   walks the carry shares to the head.

use rusqlite::{params, OptionalExtension};
use serde_bytes::ByteBuf;
use sunrise_crypto::{ChainLink, IdentityChain, IdentityTransitionBody, IdentityTransitionSigs};
use sunrise_storage::Db;

use super::identity::{RotationOutcome, SiblingRank};
use super::ids::hex_short;
use super::oplog::EnvelopeRecipients;
use super::{Engine, EngineError, META_STREAM};
use crate::control_op::IdentityTransitionPayload;
use crate::inner_op::{decode_inner_op, encode_inner_op, InnerOp};
use crate::keychain::{to16, to32, to64};

/// What seeding a vault from the account's identity chain did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainSeeded {
    /// Links this vault did not hold before.
    pub stored: usize,
    /// Whether this device now signs under the identity the chain ends at.
    pub at_head: bool,
}

/// One `identity_transitions` row, as the chain record needs it.
type LinkRow = (
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    i64,
    i64,
    i64,
    Vec<u8>,
    Vec<u8>,
);

impl Engine {
    /// Seal this account's identity keys into a recovery blob under `seed`.
    ///
    /// A vault that has rotated holds a successor, so its blob names the
    /// genesis beside it, which is what the account record names and so what
    /// the AAD binds, and carries the vault-meta genesis keys a vault restored
    /// from it could not derive (ADR-0070 §2).
    ///
    /// # Errors
    /// [`crate::keychain::KeychainError::IdentitySecretAbsent`] on a device
    /// admitted by pairing, which holds no `ID_D_priv` to seal.
    pub(crate) fn seal_recovery_blob(
        &self,
        db: &Db,
        seed: &[u8; 32],
    ) -> Result<Vec<u8>, crate::keychain::KeychainError> {
        let keychain = &self.keychain;
        let (genesis_id, genesis_pub) = keychain.genesis_pair(db)?;
        if genesis_id == keychain.identity_id() {
            return keychain.seal_recovery_blob(seed, self.rng.as_ref());
        }
        let meta = keychain.stream_keys_at(&META_STREAM, 1);
        keychain.seal_recovery_blob_for(seed, Some((&genesis_pub, &meta)), self.rng.as_ref())
    }

    /// Rotate the identity and **retire the recovery code** (ADR-0070 §2).
    ///
    /// Three transactions, in an order that is the security argument:
    ///
    /// 1. A fresh vault-meta epoch, sealed to every current device and not to
    ///    the identity. Its identity copy, sealed to the outgoing `ID_D_pub`,
    ///    would reach exactly the holders of the old code this rotation is
    ///    for.
    /// 2. The rotation, which [`Self::rotate_identity`] seals under the live
    ///    vault-meta epoch, now the one just minted. A holder of the outgoing
    ///    `ID_S_priv` can sign a competing transition from the same
    ///    predecessor, but not seal it at an epoch it holds no key for, so its
    ///    row sorts below this one however it dates its HLC (ADR-0037 §4).
    /// 3. With this device now under the successor: the identity copy of the
    ///    new vault-meta epoch, sealed to the successor, and a new epoch for
    ///    every other Stream. The epochs the old code's holder could open stop
    ///    being the ones the account writes under.
    ///
    /// # Errors
    /// As [`Self::rotate_identity`], checked before anything is minted, and
    /// [`EngineError::Invalid`] on a device this replica holds as revoked.
    pub(super) fn rotate_identity_retiring_code(
        &self,
        db: &mut Db,
    ) -> Result<RotationOutcome, EngineError> {
        if !self.keychain.can_rotate_identity() {
            // The same refusal, by the same function, before anything is
            // minted.
            return self.rotate_identity(db, None, false);
        }
        let now_ms = self.clock.now_ms();
        let me = self.keychain.device_id();
        let minted = db.with_tx(|tx| {
            let head = self.current_identity(tx)?;
            if head.identity_id != self.keychain.identity_id() || self.is_revoked(tx, &me)? {
                return Ok(None);
            }
            let seal_under = self.ensure_stream_epoch(tx, &META_STREAM, now_ms)?;
            let (epoch, key) =
                self.keychain
                    .mint_epoch(tx, &META_STREAM, self.rng.as_ref(), now_ms)?;
            self.emit_key_envelopes_to(
                tx,
                &META_STREAM,
                epoch,
                &key,
                now_ms,
                Some(&seal_under),
                EnvelopeRecipients::DevicesOnly,
            )?;
            Ok(Some((seal_under, (epoch, key))))
        })?;
        let Some((before, meta)) = minted else {
            return Err(EngineError::Invalid(
                "this device does not hold the account's current identity and cannot rotate it"
                    .into(),
            ));
        };
        let outcome = self.rotate_identity(db, None, false)?;
        db.with_tx(|tx| {
            if self.keychain.identity_id() != outcome.to_identity_id {
                // The transition lost the fold to a sibling. Sealing to an
                // identity this device does not hold would be sealing to the
                // winner's holder, which this device has no reason to trust.
                return Ok(());
            }
            self.emit_key_envelopes_to(
                tx,
                &META_STREAM,
                meta.0,
                &meta.1,
                now_ms,
                Some(&before),
                EnvelopeRecipients::IdentityOnly,
            )?;
            let set = self.keychain.rotation_set(tx)?;
            for stream_id in set.streams.iter().filter(|s| **s != META_STREAM) {
                let (epoch, key) =
                    self.keychain
                        .mint_epoch(tx, stream_id, self.rng.as_ref(), now_ms)?;
                self.emit_key_envelopes(tx, stream_id, epoch, &key, now_ms, Some(&meta))?;
            }
            Ok(())
        })?;
        Ok(outcome)
    }

    /// The account's identity chain as this replica folds it, encoded for the
    /// relay; `None` when there is nothing to publish.
    ///
    /// Nothing, unless this device signs under the identity in force and this
    /// replica holds it as neither revoked nor read-bounded: the same rule a
    /// key bundle's publisher meets (ADR-0069 §3). An account that never
    /// rotated has no links, and nothing to publish either.
    ///
    /// # Errors
    /// Storage failures, and [`EngineError::Invalid`] for a chain that will
    /// not encode.
    pub(crate) fn identity_chain_plan(&self, db: &Db) -> Result<Option<Vec<u8>>, EngineError> {
        let conn = db.conn();
        let me = self.keychain.device_id();
        let chain = self.chain_identities(conn)?;
        let head = chain.last().map(|(id, _)| *id);
        if chain.len() < 2
            || head != Some(self.keychain.identity_id())
            || self.is_revoked(conn, &me)?
            || self.is_read_bounded(conn, &me)?
        {
            return Ok(None);
        }
        let mut stmt = conn.prepare(
            "SELECT to_id_d_pub, roster_digest, shares_digest, prev_sig, next_sig,
                    meta_epoch, hlc_physical_ms, hlc_logical, emitter_device_id, payload
             FROM identity_transitions WHERE to_identity_id = ?1",
        )?;
        let mut links = Vec::with_capacity(chain.len() - 1);
        for pair in chain.windows(2) {
            let ((from, _), (to, to_s_pub)) = (pair[0], pair[1]);
            let row: Option<LinkRow> = stmt
                .query_row(params![&to[..]], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                    ))
                })
                .optional()?;
            let link = row.and_then(
                |(d, roster, shares, prev, next, epoch, ms, logical, emitter, payload)| {
                    let carry_share = match decode_inner_op(&payload) {
                        Ok(InnerOp::IdentityTransition(p)) => {
                            p.identity_share.map(serde_bytes::ByteBuf::into_vec)
                        }
                        _ => None,
                    };
                    Some(ChainLink {
                        body: IdentityTransitionBody {
                            from_identity_id: from,
                            to_identity_id: to,
                            to_id_s_pub: to_s_pub,
                            to_id_d_pub: to32(&d)?,
                            roster_digest: to32(&roster)?,
                            shares_digest: to32(&shares)?,
                        },
                        sigs: IdentityTransitionSigs {
                            prev_sig: to64(&prev)?,
                            next_sig: to64(&next)?,
                        },
                        meta_epoch: u32::try_from(epoch).ok()?,
                        hlc_physical_ms: u64::try_from(ms).ok()?,
                        hlc_logical: u32::try_from(logical).ok()?,
                        emitter_device_id: to16(&emitter)?,
                        carry_share,
                    })
                },
            );
            // The fold verified this link from these very columns, so a row
            // that does not convert is not one it could have walked.
            let Some(link) = link else {
                return Ok(None);
            };
            links.push(link);
        }
        let (_, genesis_id_s_pub) = chain[0];
        let record = IdentityChain {
            genesis_id_s_pub,
            links,
        };
        record
            .encode()
            .map(Some)
            .map_err(|e| EngineError::Invalid(e.to_string()))
    }

    /// Store the links of the account's identity chain in this vault and walk
    /// this device to the identity it ends at.
    ///
    /// What a vault restored from the recovery code does before it reads
    /// anything: every cert, snapshot record and key bundle it will meet
    /// verifies only under an identity it can reach, and the ops that carry
    /// the transitions may be gone. The record must start at this vault's
    /// genesis and every link must verify; each is stored as a transition row
    /// with no roster, which the transition's own op fills in when it arrives
    /// (`apply_control_op`). Then [`Self::recompute_identity_head`] opens the
    /// carry share of each link in turn.
    ///
    /// # Errors
    /// [`EngineError::Invalid`] for a record that is malformed, of another
    /// account, or holds a link that does not verify; storage failures.
    pub(crate) fn seed_identity_chain(
        &self,
        db: &mut Db,
        bytes: &[u8],
    ) -> Result<ChainSeeded, EngineError> {
        let invalid = |e: sunrise_crypto::IdentityChainError| {
            EngineError::Invalid(format!("identity chain: {e}"))
        };
        let record = IdentityChain::decode(bytes).map_err(invalid)?;
        let (genesis, _) = self
            .keychain
            .genesis_pair(db)
            .map_err(|e| EngineError::Keychain(e.to_string()))?;
        record.verify(&genesis).map_err(invalid)?;
        let (head, _) = record.head();
        let stored = db.with_tx(|tx| {
            let mut stored = 0usize;
            for link in &record.links {
                let rank = SiblingRank {
                    meta_epoch: i64::from(link.meta_epoch),
                    hlc_physical_ms: i64::try_from(link.hlc_physical_ms).unwrap_or(i64::MAX),
                    hlc_logical: i64::from(link.hlc_logical),
                    emitter_device_id: link.emitter_device_id.to_vec(),
                    to_identity_id: link.body.to_identity_id.to_vec(),
                };
                if !self.admit_sibling(tx, &link.body.from_identity_id, &rank)? {
                    continue;
                }
                let stub = InnerOp::IdentityTransition(Box::new(IdentityTransitionPayload {
                    from_identity_id: link.body.from_identity_id,
                    to_identity_id: link.body.to_identity_id,
                    to_id_s_pub: link.body.to_id_s_pub,
                    to_id_d_pub: link.body.to_id_d_pub,
                    roster: Vec::new(),
                    device_shares: Vec::new(),
                    identity_share: link.carry_share.clone().map(ByteBuf::from),
                    prev_sig: link.sigs.prev_sig,
                    next_sig: link.sigs.next_sig,
                }));
                let payload =
                    encode_inner_op(&stub).map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
                stored += tx.execute(
                    "INSERT OR IGNORE INTO identity_transitions
                     (to_identity_id, from_identity_id, to_id_s_pub, to_id_d_pub,
                      meta_epoch, hlc_physical_ms, hlc_logical, emitter_device_id,
                      payload, prev_sig, next_sig, roster_digest, shares_digest)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    params![
                        &link.body.to_identity_id[..],
                        &link.body.from_identity_id[..],
                        &link.body.to_id_s_pub[..],
                        &link.body.to_id_d_pub[..],
                        rank.meta_epoch,
                        rank.hlc_physical_ms,
                        rank.hlc_logical,
                        &link.emitter_device_id[..],
                        payload,
                        &link.sigs.prev_sig[..],
                        &link.sigs.next_sig[..],
                        &link.body.roster_digest[..],
                        &link.body.shares_digest[..],
                    ],
                )?;
            }
            self.recompute_identity_head(tx)?;
            Ok(stored)
        })?;
        let at_head = self.keychain.identity_id() == head;
        tracing::info!(
            ev = "core.identity.chain_seeded",
            head_h = hex_short(&head),
            n_total = record.links.len() as u64,
            n_imported = stored as u64,
            result = if at_head { "ok" } else { "behind" },
            "the account's identity chain was stored and this device walked its carry shares"
        );
        Ok(ChainSeeded { stored, at_head })
    }
}

/// Whether a stored transition payload is a seeded one: a row
/// [`Engine::seed_identity_chain`] wrote from the chain record, which carries
/// no roster. An honest transition's roster always names its emitter, so an
/// empty one is never an op's.
pub(super) fn is_seeded_payload(payload: &[u8]) -> bool {
    matches!(
        decode_inner_op(payload),
        Ok(InnerOp::IdentityTransition(p)) if p.roster.is_empty()
    )
}
