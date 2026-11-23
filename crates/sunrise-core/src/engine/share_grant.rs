//! Share grants as ops: one Stream shared with one Person
//! (`docs/03-crypto/sharing-with-others.md`, #1318).
//!
//! `sunrise_crypto::share_grant` is the cryptography: the sealed grant, its
//! signature, its expiry, and the re-seal a rotation emits. This module is
//! what the account does with it:
//!
//! - [`Engine::share_stream`] seals the Stream's live epoch to a Person's
//!   identity, signed by the account identity in force, and logs it as an
//!   `InnerOp::ShareGrant` on the vault-meta stream.
//! - Applying one stores nothing beside the op. The share fold reads the
//!   grants back out of the op log each time it is asked, so every replica
//!   holding the same ops reaches the same shares, and nothing a sibling
//!   emitted is believed on arrival rather than at the fold.
//! - A Stream-key rotation re-seals the new epoch to every share still in
//!   force ([`Engine::reseal_shares`]), from `emit_key_envelopes`, the one
//!   place every rotation distributes its epoch through.
//!
//! # What the fold believes
//!
//! A `ShareGrant` op counts only where its `person` is a Person ref, its grant
//! decodes as a `viewer` grant, the grant's signature verifies under an
//! identity on the account's chain for the op's `recipient_id_d_pub`, and its
//! sender's reads are not bounded. The chain rather than the head alone, so a
//! share outlives an identity rotation.
//!
//! The signature covers the `ID_D_pub` the grant is sealed to, which the op
//! carries beside it: a device that re-files a grant the account signed,
//! naming its own key, files one that verifies under nobody, so no re-seal,
//! the account's own included, ever copies that key forward.
//!
//! The bound on the sender holds whoever signed the grant: an evicted device
//! may hold a retired identity's `ID_S_priv`, and can replay an older grant of
//! the head's under a later stamp. A share whose latest grant a now-bounded
//! device filed therefore falls back to an earlier grant of the same
//! recipient, or lapses until the owner shares it again.
//!
//! A `ShareRevoke` or `ShareDecline` op ends a recipient's share
//! ([`Engine::revoke_share`], [`Engine::record_share_decline`]). It counts
//! where its sender's reads are not bounded; it carries no identity
//! signature, because only the account's own devices write the vault-meta
//! stream, and ending a share hands nobody a key.
//!
//! Each recipient's events are ordered by envelope stamp (HLC, then device,
//! then `seq`). The **cut** is the latest fresh grant or end. Where the cut
//! is an end, the share has ended, and no re-seal stamped after it revives
//! it: a sibling that rotated before it saw the revocation re-sealed to a
//! share it did not know had ended, and the next rotation must still leave
//! the recipient out. Otherwise the share is the latest grant stamped at or
//! after the cut, fresh or re-sealed, so it moves to the newest epoch with
//! each re-seal. Where a recipient has no believed fresh grant and no end,
//! its latest re-seal is its share.
//!
//! # Re-sealing
//!
//! Every rotation re-seals its epoch to the shares in force, leaving out the
//! ended and the expired. A device without `ID_S_priv` cannot sign that
//! re-seal (`core.share.reseal_skipped`), so a holder fills the gap
//! ([`Engine::backfill_share_grants`]): whenever a device that holds
//! `ID_S_priv` learns a Stream key, or applies a share grant, it re-seals
//! every held epoch above a share's own, up to the live one, to that share.

use super::merge::to_sql;
use super::stream::read_stream;
use super::{Engine, EngineError, META_STREAM};
use crate::bytes::hex_short;
use crate::control_op::{ShareDeclinePayload, ShareGrantOpPayload, ShareRevokePayload};
use crate::inner_op::{decode_inner_op, InnerOp};
use crate::keychain::{Keychain, KeychainError};
use rusqlite::{params, Connection, Transaction};
use std::collections::BTreeMap;
use sunrise_cbor::hlc::Hlc;
use sunrise_crypto::op_envelope::open_envelope;
use sunrise_crypto::{
    decode_envelope, decode_share_grant, ShareGrant, ShareGrantTerms, ShareRecipient, ShareRole,
    SharedEpoch, StreamKey, StreamMetadata,
};
use sunrise_domain::{Person, PRIVATE_DOMAIN_BYTES};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::Db;

/// The op-log `inner_kind` of a share grant.
const SHARE_GRANT_KIND: &str = "share.grant";
/// The op-log `inner_kind` of a share revocation.
const SHARE_REVOKE_KIND: &str = "share.revoke";
/// The op-log `inner_kind` of a share decline.
const SHARE_DECLINE_KIND: &str = "share.decline";

/// An op's envelope stamp, `(hlc, device, seq)`: the order a recipient's
/// grants are told apart in.
type Stamp = (Hlc, [u8; 16], u64);

/// One Stream shared with one Person, as the share fold reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamShare {
    /// The Person the Stream is shared with.
    pub person: EntityRef,
    /// The recipient identity the grant is sealed to.
    pub recipient_identity_id: [u8; 16],
    /// The recipient's `ID_D_pub`, which the next re-seal is sealed to.
    pub recipient_id_d_pub: [u8; 32],
    /// The epoch of the newest grant.
    pub epoch: u32,
    /// When the share ends, on the recipient's clock; `None` never.
    pub expires_at_ms: Option<u64>,
    /// The newest grant, in its canonical encoding: what reaches the
    /// recipient beside the relay grant's claim token.
    pub grant: Vec<u8>,
}

/// How a share ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareEnd {
    /// The owner revoked it (`ShareRevoke`).
    Revoked,
    /// The recipient declined it (`ShareDecline`).
    Declined,
}

/// A share of a Stream that has ended, as the share fold reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndedShare {
    /// The recipient identity whose share ended.
    pub recipient_identity_id: [u8; 16],
    /// Whether the owner revoked it or the recipient declined it.
    pub end: ShareEnd,
    /// The op's own time: `effective_at_ms` or `declined_at_ms`.
    pub at_ms: u64,
}

/// One Stream's shares: those in force and those that ended.
#[derive(Debug, Default)]
struct ShareFold {
    shares: Vec<StreamShare>,
    ended: Vec<EndedShare>,
}

impl ShareFold {
    fn ended_ids(&self) -> Vec<[u8; 16]> {
        self.ended.iter().map(|e| e.recipient_identity_id).collect()
    }
}

/// One recipient's believed events, as the fold collects them.
#[derive(Default)]
struct RecipientEvents {
    /// Every grant: its stamp, whether it is a re-seal, and the share.
    grants: Vec<(Stamp, bool, StreamShare)>,
    /// Every end, with its stamp.
    ends: Vec<(Stamp, EndedShare)>,
}

impl RecipientEvents {
    /// The share in force, or how it ended. See the [module docs](self).
    fn resolve(self) -> Option<Result<StreamShare, EndedShare>> {
        let fresh = self
            .grants
            .iter()
            .filter(|(_, r, _)| !r)
            .map(|(s, _, _)| *s);
        let last_fresh = fresh.max();
        let last_end = self.ends.into_iter().max_by_key(|(s, _)| *s);
        let cut = match (last_fresh, last_end) {
            (_, Some((end_at, end))) if last_fresh.is_none_or(|f| f < end_at) => {
                return Some(Err(end));
            }
            (cut, _) => cut,
        };
        self.grants
            .into_iter()
            .filter(|(s, _, _)| cut.is_none_or(|c| *s >= c))
            .max_by_key(|(s, _, _)| *s)
            .map(|(_, _, share)| Ok(share))
    }
}

/// Why the fold does not believe a `ShareGrant` op, as its log line spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// `person` is not a `per_` ref.
    NotAPerson,
    /// The grant does not decode, or confers a role this build does not grant.
    Undecodable,
    /// The grant verifies under no identity on the account's chain.
    Signature,
    /// The sender's reads are bounded.
    BoundedSender,
}

impl Refusal {
    const fn as_str(self) -> &'static str {
        match self {
            Self::NotAPerson => "not_a_person",
            Self::Undecodable => "undecodable",
            Self::Signature => "signature",
            Self::BoundedSender => "bounded_sender",
        }
    }
}

/// A stream no share can name: the vault-meta stream and the private key
/// domain hold the account's own state, never a Stream a Person reads.
fn unshareable(stream_id: &[u8; 16]) -> bool {
    *stream_id == META_STREAM || *stream_id == PRIVATE_DOMAIN_BYTES
}

impl Engine {
    /// Share `stream` with `recipient` as a viewer, until `expires_at_ms` on
    /// the recipient's clock, or for good when it is `None`.
    ///
    /// Seals the Stream's live epoch, with its name and color, to
    /// `recipient_id_d_pub` under the Person's identity, signs it as the
    /// account identity in force, and logs the grant as a `ShareGrant`
    /// control op on the vault-meta stream. Returns the grant's canonical
    /// encoding, which is what the recipient opens.
    ///
    /// # Errors
    /// [`EngineError::Invalid`] for a ref that is not a Stream, the vault-meta
    /// stream or the private domain, a deleted Person or one with no identity,
    /// the account's own identity, an expiry already past, a device the
    /// register calls revoked, a device that does not hold `ID_S_priv`, or a
    /// grant that does not seal; [`EngineError::NotFound`] for a Stream this
    /// replica does not hold or holds deleted. Nothing is written on any of
    /// them. Storage failures otherwise.
    pub fn share_stream(
        &self,
        db: &mut Db,
        stream: EntityRef,
        recipient: &Person,
        recipient_id_d_pub: [u8; 32],
        expires_at_ms: Option<u64>,
    ) -> Result<Vec<u8>, EngineError> {
        let now_ms = self.clock.now_ms();
        let stream_id = *stream.bytes();
        if stream.kind() != EntityKind::Stream || unshareable(&stream_id) {
            return Err(EngineError::Invalid(format!(
                "{stream} is not a Stream that can be shared"
            )));
        }
        if recipient.id.kind() != EntityKind::Person || recipient.deleted {
            return Err(EngineError::Invalid(
                "a share is to a Person who is not deleted".into(),
            ));
        }
        let Some(identity) = recipient
            .identity_id
            .filter(|r| r.kind() == EntityKind::Identity)
        else {
            return Err(EngineError::Invalid(
                "the Person has no identity to seal a share to".into(),
            ));
        };
        let recipient_identity_id = *identity.bytes();
        if recipient_identity_id == self.keychain.identity_id() {
            return Err(EngineError::Invalid(
                "a Stream is not shared with the account's own identity".into(),
            ));
        }
        let terms_at = |epoch| ShareGrantTerms {
            stream_id,
            epoch,
            recipient_identity_id,
            role: ShareRole::Viewer,
            expires_at_ms,
        };
        if terms_at(0).is_expired_at(now_ms) {
            return Err(EngineError::Invalid(
                "the share would have expired before it was sent".into(),
            ));
        }
        let me = self.keychain.device_id();
        let mut refused: Option<EngineError> = None;
        // A refusal inside the transaction returns this, so the transaction
        // rolls back whatever it wrote first, a first epoch included.
        let abort = || rusqlite::Error::InvalidQuery;
        let written = db.with_tx(|tx| -> rusqlite::Result<Vec<u8>> {
            // A revoked device holds no pen, for `rotate_stream_key`'s reason:
            // a grant it signed would hand its recipient a key the account no
            // longer writes under, and bind nothing it could not read anyway.
            if self.is_revoked(tx, &me)? {
                refused = Some(EngineError::Invalid(
                    "this device is revoked and cannot share a Stream".into(),
                ));
                return Err(abort());
            }
            let row = read_stream(tx, &stream_id).map_err(to_sql)?;
            let Some(row) = row.filter(|r| !r.deleted) else {
                refused = Some(EngineError::NotFound(format!("stream {stream}")));
                return Err(abort());
            };
            let metadata = StreamMetadata {
                name: row.name.clone(),
                color: row.color.as_str().to_owned(),
            };
            let (epoch, key) = self.ensure_stream_epoch(tx, &stream_id, now_ms)?;
            let grant = match self.keychain.seal_share_grant(
                terms_at(epoch),
                &recipient_id_d_pub,
                &key,
                &metadata,
                self.rng.as_ref(),
            ) {
                Ok(grant) => grant,
                Err(e) => {
                    refused = Some(EngineError::Invalid(format!("the share did not seal: {e}")));
                    return Err(abort());
                }
            };
            let Ok(bytes) = grant.encode() else {
                refused = Some(EngineError::Invalid("the share did not encode".into()));
                return Err(abort());
            };
            let op = InnerOp::ShareGrant(ShareGrantOpPayload {
                person: recipient.id,
                recipient_id_d_pub,
                grant: bytes.clone(),
                reseal: false,
                unknown: sunrise_domain::Unknowns::new(),
            });
            self.emit_control_op(tx, &op, now_ms, None)?;
            Ok(bytes)
        });
        match (written, refused) {
            (_, Some(e)) => Err(e),
            (written, None) => Ok(written?),
        }
    }

    /// Revoke `stream`'s share with `recipient_identity_id`, and rotate the
    /// Stream key so the recipient reads nothing written from now on
    /// (`docs/03-crypto/sharing-with-others.md` §Revoking access).
    ///
    /// Logs a `ShareRevoke` op, then mints the Stream's next epoch and
    /// distributes it in the same transaction. The rotation's re-seal reads
    /// the revocation, so it leaves the recipient out, and so does every
    /// later rotation on every replica that holds the op. A device without
    /// `ID_S_priv` can revoke: its rotation re-seals to nobody, and a device
    /// that holds `ID_S_priv` re-seals the epoch to the shares still in force
    /// once it learns it ([`Self::backfill_share_grants`]). Returns the
    /// epoch the rotation minted.
    ///
    /// Withdrawing the relay viewer grant (step 3) is the caller's.
    ///
    /// # Errors
    /// [`EngineError::Invalid`] for a ref that is not a shareable Stream, a
    /// device the register calls revoked, or a recipient with no share of
    /// the Stream in force on this replica. Nothing is written on any of
    /// them. Storage failures otherwise.
    pub fn revoke_share(
        &self,
        db: &mut Db,
        stream: EntityRef,
        recipient_identity_id: [u8; 16],
    ) -> Result<u32, EngineError> {
        let now_ms = self.clock.now_ms();
        let stream_id = *stream.bytes();
        let op = InnerOp::ShareRevoke(ShareRevokePayload {
            stream_id,
            recipient_identity_id,
            effective_at_ms: now_ms,
            unknown: sunrise_domain::Unknowns::new(),
        });
        self.end_share(db, stream, recipient_identity_id, &op, true)
    }

    /// Record that `recipient_identity_id` declined `stream`'s share
    /// (`docs/03-crypto/sharing-with-others.md` §Granting access step 7).
    ///
    /// The recipient's decline reaches the owner out of band, as the grant
    /// reached it. This logs the account's record of it as a `ShareDecline`
    /// op, which ends the share as a revocation does: no later re-seal
    /// reaches the recipient. It rotates nothing, because the recipient chose
    /// not to read; the next rotation leaves it out.
    ///
    /// # Errors
    /// As [`Self::revoke_share`].
    pub fn record_share_decline(
        &self,
        db: &mut Db,
        stream: EntityRef,
        recipient_identity_id: [u8; 16],
    ) -> Result<(), EngineError> {
        let now_ms = self.clock.now_ms();
        let op = InnerOp::ShareDecline(ShareDeclinePayload {
            stream_id: *stream.bytes(),
            recipient_identity_id,
            declined_at_ms: now_ms,
            unknown: sunrise_domain::Unknowns::new(),
        });
        self.end_share(db, stream, recipient_identity_id, &op, false)
            .map(|_| ())
    }

    /// Log `end`, which ends `stream`'s share with `recipient_identity_id`,
    /// and rotate the Stream key beside it where `rotate` says so. Returns
    /// the minted epoch, or the live one where nothing rotated.
    fn end_share(
        &self,
        db: &mut Db,
        stream: EntityRef,
        recipient_identity_id: [u8; 16],
        end: &InnerOp,
        rotate: bool,
    ) -> Result<u32, EngineError> {
        let now_ms = self.clock.now_ms();
        let stream_id = *stream.bytes();
        if stream.kind() != EntityKind::Stream || unshareable(&stream_id) {
            return Err(EngineError::Invalid(format!(
                "{stream} is not a Stream that can be shared"
            )));
        }
        let mut refused: Option<EngineError> = None;
        let abort = || rusqlite::Error::InvalidQuery;
        let written = db.with_tx(|tx| -> rusqlite::Result<u32> {
            // A revoked device holds no pen, for `rotate_stream_key`'s reason.
            if self.is_revoked(tx, &self.keychain.device_id())? {
                refused = Some(EngineError::Invalid(
                    "this device is revoked and cannot end a share".into(),
                ));
                return Err(abort());
            }
            let fold = self.share_fold(tx, &stream_id).map_err(to_sql)?;
            if !fold
                .shares
                .iter()
                .any(|s| s.recipient_identity_id == recipient_identity_id)
            {
                refused = Some(EngineError::Invalid(format!(
                    "{stream} has no share in force with that identity"
                )));
                return Err(abort());
            }
            let seal_under = self.ensure_stream_epoch(tx, &META_STREAM, now_ms)?;
            self.emit_control_op(tx, end, now_ms, Some(&seal_under))?;
            if !rotate {
                return Ok(self.keychain.current_epoch_tx(tx, &stream_id)?.unwrap_or(0));
            }
            let (epoch, key) =
                self.keychain
                    .mint_epoch(tx, &stream_id, self.rng.as_ref(), now_ms)?;
            self.emit_key_envelopes(tx, &stream_id, epoch, &key, now_ms, Some(&seal_under))?;
            Ok(epoch)
        });
        match (written, refused) {
            (_, Some(e)) => Err(e),
            (written, None) => Ok(written?),
        }
    }

    /// Every share of `stream` in force on this replica, one per recipient,
    /// ordered by recipient identity. An expired share is listed: expiry is
    /// the recipient's check, and the next rotation is what leaves it out.
    ///
    /// # Errors
    /// Storage failures.
    pub fn stream_shares(
        &self,
        db: &Db,
        stream: EntityRef,
    ) -> Result<Vec<StreamShare>, EngineError> {
        Ok(self.share_fold(db.conn(), stream.bytes())?.shares)
    }

    /// Every share of `stream` that a revocation or a decline ended on this
    /// replica, and was not shared again since, ordered by recipient
    /// identity: what the owner's UI shows as revoked or declined.
    ///
    /// # Errors
    /// Storage failures.
    pub fn ended_stream_shares(
        &self,
        db: &Db,
        stream: EntityRef,
    ) -> Result<Vec<EndedShare>, EngineError> {
        Ok(self.share_fold(db.conn(), stream.bytes())?.ended)
    }

    /// The share fold over the op log. See the [module docs](self).
    fn share_fold(
        &self,
        conn: &Connection,
        stream_id: &[u8; 16],
    ) -> Result<ShareFold, EngineError> {
        let envelopes: Vec<Vec<u8>> = {
            let mut stmt = conn.prepare(
                "SELECT envelope FROM ops WHERE stream_id = ? AND inner_kind IN (?, ?, ?)",
            )?;
            let rows = stmt.query_map(
                params![
                    &META_STREAM[..],
                    SHARE_GRANT_KIND,
                    SHARE_REVOKE_KIND,
                    SHARE_DECLINE_KIND
                ],
                |r| r.get(0),
            )?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        if envelopes.is_empty() {
            return Ok(ShareFold::default());
        }
        let signers = self.device_signing_keys(conn)?;
        let chain = self.chain_identities(conn)?;
        let mut events: BTreeMap<[u8; 16], RecipientEvents> = BTreeMap::new();
        for bytes in envelopes {
            let Ok(env) = decode_envelope(&bytes) else {
                continue;
            };
            let Some(signer) = signers.get(&env.device_id) else {
                continue;
            };
            let Some(inner) = self
                .keychain
                .stream_keys_at(&env.stream_id, env.epoch)
                .iter()
                .find_map(|k| open_envelope(&env, signer, Some(k)).ok())
            else {
                continue;
            };
            let stamp = (env.hlc, env.device_id, env.seq);
            let (recipient, end) = match decode_inner_op(&inner) {
                Ok(InnerOp::ShareGrant(payload)) => {
                    let Ok(grant) =
                        self.believe_share_grant(conn, &chain, &payload, &env.device_id)?
                    else {
                        continue;
                    };
                    let terms = grant.terms();
                    if terms.stream_id != *stream_id {
                        continue;
                    }
                    events
                        .entry(terms.recipient_identity_id)
                        .or_default()
                        .grants
                        .push((
                            stamp,
                            payload.reseal,
                            StreamShare {
                                person: payload.person,
                                recipient_identity_id: terms.recipient_identity_id,
                                recipient_id_d_pub: payload.recipient_id_d_pub,
                                epoch: terms.epoch,
                                expires_at_ms: terms.expires_at_ms,
                                grant: payload.grant,
                            },
                        ));
                    continue;
                }
                Ok(InnerOp::ShareRevoke(p)) if p.stream_id == *stream_id => (
                    p.recipient_identity_id,
                    (ShareEnd::Revoked, p.effective_at_ms),
                ),
                Ok(InnerOp::ShareDecline(p)) if p.stream_id == *stream_id => (
                    p.recipient_identity_id,
                    (ShareEnd::Declined, p.declined_at_ms),
                ),
                _ => continue,
            };
            if self.is_read_bounded(conn, &env.device_id)? {
                continue;
            }
            events.entry(recipient).or_default().ends.push((
                stamp,
                EndedShare {
                    recipient_identity_id: recipient,
                    end: end.0,
                    at_ms: end.1,
                },
            ));
        }
        let mut fold = ShareFold::default();
        for recipient in events.into_values() {
            match recipient.resolve() {
                Some(Ok(share)) => fold.shares.push(share),
                Some(Err(ended)) => fold.ended.push(ended),
                None => {}
            }
        }
        Ok(fold)
    }

    /// Whether the fold believes `p` from `sender`, and the grant it carries
    /// where it does.
    fn believe_share_grant(
        &self,
        conn: &Connection,
        chain: &[([u8; 16], [u8; 32])],
        p: &ShareGrantOpPayload,
        sender: &[u8; 16],
    ) -> rusqlite::Result<Result<ShareGrant, Refusal>> {
        if p.person.kind() != EntityKind::Person {
            return Ok(Err(Refusal::NotAPerson));
        }
        let Ok(grant) = decode_share_grant(&p.grant) else {
            return Ok(Err(Refusal::Undecodable));
        };
        // The signature covers the op's `recipient_id_d_pub`, so a grant
        // beside a key its signer did not seal it to verifies under nobody:
        // no device, evicted or not, can name its own key beside a grant the
        // account signed and have a re-seal carry it forward.
        if !chain
            .iter()
            .any(|(_, pk)| grant.verify(pk, &p.recipient_id_d_pub).is_ok())
        {
            return Ok(Err(Refusal::Signature));
        }
        // A sender whose reads are bounded is refused whoever signed the
        // grant, the head included: it may still hold a retired identity's
        // `ID_S_priv` and sign grants under it, and it could replay an older
        // grant of the head's with a later stamp to restore an expiry the
        // owner has since changed.
        if self.is_read_bounded(conn, sender)? {
            return Ok(Err(Refusal::BoundedSender));
        }
        Ok(Ok(grant))
    }

    /// Apply a `ShareGrant` op: nothing is stored beside it, and the share
    /// fold decides what it counts for. One the fold would not believe today
    /// is logged, because a share that silently fails to reach its recipient
    /// is the failure nobody else would see.
    ///
    /// One it believes may name an epoch below one this replica holds, as a
    /// share made while a sibling without `ID_S_priv` rotated does; the
    /// backfill carries it to the live epoch ([`Self::backfill_share_grants`]).
    pub(super) fn apply_share_grant(
        &self,
        tx: &Transaction<'_>,
        p: &ShareGrantOpPayload,
        sender: &[u8; 16],
        now_ms: u64,
    ) -> rusqlite::Result<()> {
        let chain = self.chain_identities(tx)?;
        match self.believe_share_grant(tx, &chain, p, sender)? {
            Ok(grant) => self.backfill_share_grants(tx, &grant.terms().stream_id, now_ms),
            Err(why) => {
                tracing::warn!(
                    ev = "core.share.grant_refused",
                    reason = why.as_str(),
                    sender_h = hex_short(sender),
                    "a share grant op is not believed; it is kept and counts for no share"
                );
                Ok(())
            }
        }
    }

    /// Apply a `ShareRevoke` or `ShareDecline` op: as for a grant, nothing is
    /// stored beside it, and one from a sender whose reads are bounded is
    /// logged, because the fold will not count it.
    pub(super) fn apply_share_end(
        &self,
        tx: &Transaction<'_>,
        sender: &[u8; 16],
    ) -> rusqlite::Result<()> {
        if self.is_read_bounded(tx, sender)? {
            tracing::warn!(
                ev = "core.share.end_refused",
                reason = Refusal::BoundedSender.as_str(),
                sender_h = hex_short(sender),
                "a share revocation or decline is not believed; it is kept and ends no share"
            );
        }
        Ok(())
    }

    /// Re-seal `(stream_id, epoch)`, which a rotation just minted, to every
    /// share of the Stream still in force, as `ShareGrant` ops sealed under
    /// `seal_under` beside the rotation's key envelopes.
    ///
    /// A re-seal never fails its rotation. A device that does not hold
    /// `ID_S_priv`, a Stream whose row has not arrived, and a re-seal that
    /// does not seal each leave the epoch unsealed to the recipients and say
    /// so (`core.share.reseal_skipped`): the rotation's purpose is the
    /// account's own keys, and refusing it over a share would keep a revoked
    /// device reading. A holder fills the gap when it learns the epoch
    /// ([`Self::backfill_share_grants`]).
    ///
    /// A share a revocation or a decline ended is left out, in the fold and
    /// again in the revoked list the re-seal is handed.
    pub(super) fn reseal_shares(
        &self,
        tx: &Transaction<'_>,
        stream_id: &[u8; 16],
        epoch: u32,
        key: &StreamKey,
        now_ms: u64,
        seal_under: Option<&(u32, StreamKey)>,
    ) -> rusqlite::Result<()> {
        if unshareable(stream_id) {
            return Ok(());
        }
        let fold = self.share_fold(tx, stream_id).map_err(to_sql)?;
        self.reseal_to(
            tx,
            (stream_id, epoch, key),
            &fold.shares,
            &fold.ended_ids(),
            now_ms,
            seal_under,
        )
    }

    /// Carry every share of `stream_id` whose epoch is below the live one to
    /// each held epoch above its own, up to the live one, in ascending order,
    /// so the last grant of each is at the live epoch.
    ///
    /// This is how an epoch a device without `ID_S_priv` minted reaches the
    /// Stream's share recipients: its rotation cannot sign a re-seal, and the
    /// first device holding `ID_S_priv` to learn the epoch, or to apply a
    /// share below it, re-seals it here. It runs on such a device only, and
    /// not on one the register calls revoked or whose reads are bounded. Each
    /// holder that learns the epoch re-seals it, so a recipient may be sent
    /// the same epoch twice; the fold keeps the later grant.
    pub(super) fn backfill_share_grants(
        &self,
        tx: &Transaction<'_>,
        stream_id: &[u8; 16],
        now_ms: u64,
    ) -> rusqlite::Result<()> {
        if unshareable(stream_id) || !self.keychain.can_rotate_identity() {
            return Ok(());
        }
        let me = self.keychain.device_id();
        if self.is_revoked(tx, &me)? || self.is_read_bounded(tx, &me)? {
            return Ok(());
        }
        let Some((live, live_key)) = self.keychain.current_stream_key_tx(tx, stream_id)? else {
            return Ok(());
        };
        let fold = self.share_fold(tx, stream_id).map_err(to_sql)?;
        let Some(lowest) = fold.shares.iter().map(|s| s.epoch).min() else {
            return Ok(());
        };
        if lowest >= live {
            return Ok(());
        }
        let ended = fold.ended_ids();
        let epochs: Vec<u32> = Keychain::held_epochs_tx(tx)?
            .into_iter()
            .filter(|(s, e)| s == stream_id && *e > lowest && *e < live)
            .map(|(_, e)| e)
            .chain(std::iter::once(live))
            .collect();
        for epoch in epochs {
            let key = if epoch == live {
                live_key.clone()
            } else {
                let Some(key) = self
                    .keychain
                    .stream_keys_at(stream_id, epoch)
                    .into_iter()
                    .next()
                else {
                    continue;
                };
                key
            };
            let behind: Vec<StreamShare> = fold
                .shares
                .iter()
                .filter(|s| s.epoch < epoch)
                .cloned()
                .collect();
            self.reseal_to(tx, (stream_id, epoch, &key), &behind, &ended, now_ms, None)?;
        }
        Ok(())
    }

    /// Re-seal `(stream_id, epoch, key)` to `shares`, leaving out `ended`
    /// and the expired, as re-seal `ShareGrant` ops sealed under
    /// `seal_under`. Never fails over a share; see [`Self::reseal_shares`].
    fn reseal_to(
        &self,
        tx: &Transaction<'_>,
        (stream_id, epoch, key): (&[u8; 16], u32, &StreamKey),
        shares: &[StreamShare],
        ended: &[[u8; 16]],
        now_ms: u64,
        seal_under: Option<&(u32, StreamKey)>,
    ) -> rusqlite::Result<()> {
        if shares.is_empty() {
            return Ok(());
        }
        let skipped = |reason: &str| {
            tracing::warn!(
                ev = "core.share.reseal_skipped",
                reason,
                stream_h = hex_short(stream_id),
                epoch,
                n_total = shares.len(),
                "a rotated epoch was not re-sealed to the Stream's share recipients"
            );
        };
        let Some(row) = read_stream(tx, stream_id).map_err(to_sql)? else {
            skipped("no_stream_row");
            return Ok(());
        };
        let metadata = StreamMetadata {
            name: row.name.clone(),
            color: row.color.as_str().to_owned(),
        };
        let recipients: Vec<ShareRecipient> = shares
            .iter()
            .map(|s| ShareRecipient {
                identity_id: s.recipient_identity_id,
                id_d_pub: s.recipient_id_d_pub,
                expires_at_ms: s.expires_at_ms,
            })
            .collect();
        let grants = match self.keychain.reseal_share_grants(
            SharedEpoch {
                stream_id: *stream_id,
                epoch,
                key,
                metadata: &metadata,
            },
            &recipients,
            ended,
            now_ms,
            self.rng.as_ref(),
        ) {
            Ok(grants) => grants,
            Err(KeychainError::IdentitySigningKeyAbsent) => {
                skipped("no_identity_signing_key");
                return Ok(());
            }
            Err(_) => {
                skipped("did_not_seal");
                return Ok(());
            }
        };
        for grant in grants {
            let Some(share) = shares
                .iter()
                .find(|s| s.recipient_identity_id == grant.terms().recipient_identity_id)
            else {
                continue;
            };
            let Ok(bytes) = grant.encode() else {
                skipped("did_not_encode");
                continue;
            };
            let op = InnerOp::ShareGrant(ShareGrantOpPayload {
                person: share.person,
                recipient_id_d_pub: share.recipient_id_d_pub,
                grant: bytes,
                reseal: true,
                unknown: sunrise_domain::Unknowns::new(),
            });
            self.emit_control_op(tx, &op, now_ms, seal_under)?;
        }
        Ok(())
    }
}
