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
//! decodes as a `viewer` grant, and the grant's signature verifies either
//! under the head of the account's identity chain, or under a retired
//! identity on it from a sender whose reads are not bounded. The chain rather
//! than the head alone, so a share outlives an identity rotation; the bound on
//! the sender, so a device evicted while it still held a retired identity's
//! `ID_S_priv` cannot name a recipient for the next epoch.
//!
//! Of the grants that count, each recipient's latest by envelope stamp
//! (HLC, then device, then `seq`) is its share. A re-seal is a later grant of
//! the same recipient, so the share moves to the newest epoch with it.
//!
//! # What is not here
//!
//! No `share_revoke` or `share_decline` op exists yet, so a re-seal leaves
//! nobody out but the expired; and a rotation on a device that does not hold
//! `ID_S_priv` cannot sign one, so its epoch reaches no recipient
//! (`core.share.reseal_skipped`).

use super::merge::to_sql;
use super::stream::read_stream;
use super::{Engine, EngineError, META_STREAM};
use crate::bytes::hex_short;
use crate::control_op::ShareGrantOpPayload;
use crate::inner_op::{decode_inner_op, InnerOp};
use crate::keychain::KeychainError;
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
        self.stream_shares_in(db.conn(), stream.bytes())
    }

    /// The share fold over the op log. See the [module docs](self).
    fn stream_shares_in(
        &self,
        conn: &Connection,
        stream_id: &[u8; 16],
    ) -> Result<Vec<StreamShare>, EngineError> {
        let envelopes: Vec<Vec<u8>> = {
            let mut stmt =
                conn.prepare("SELECT envelope FROM ops WHERE stream_id = ? AND inner_kind = ?")?;
            let rows = stmt.query_map(params![&META_STREAM[..], SHARE_GRANT_KIND], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        if envelopes.is_empty() {
            return Ok(Vec::new());
        }
        let signers = self.device_signing_keys(conn)?;
        let chain = self.chain_identities(conn)?;
        let mut latest: BTreeMap<[u8; 16], (Stamp, StreamShare)> = BTreeMap::new();
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
            let Ok(InnerOp::ShareGrant(payload)) = decode_inner_op(&inner) else {
                continue;
            };
            let Ok(grant) = self.believe_share_grant(conn, &chain, &payload, &env.device_id)?
            else {
                continue;
            };
            let terms = grant.terms();
            if terms.stream_id != *stream_id {
                continue;
            }
            let stamp = (env.hlc, env.device_id, env.seq);
            if latest
                .get(&terms.recipient_identity_id)
                .is_some_and(|(held, _)| *held >= stamp)
            {
                continue;
            }
            latest.insert(
                terms.recipient_identity_id,
                (
                    stamp,
                    StreamShare {
                        person: payload.person,
                        recipient_identity_id: terms.recipient_identity_id,
                        recipient_id_d_pub: payload.recipient_id_d_pub,
                        epoch: terms.epoch,
                        expires_at_ms: terms.expires_at_ms,
                        grant: payload.grant,
                    },
                ),
            );
        }
        Ok(latest.into_values().map(|(_, share)| share).collect())
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
        // The head's word stands whoever carried it: whoever signed under it
        // holds the account's key now. A retired identity's word stands only
        // from a sender whose reads are not bounded, because its holders
        // include whoever the rotation that retired it evicted.
        if chain.last().is_some_and(|(_, pk)| grant.verify(pk).is_ok()) {
            return Ok(Ok(grant));
        }
        if !chain.iter().any(|(_, pk)| grant.verify(pk).is_ok()) {
            return Ok(Err(Refusal::Signature));
        }
        if self.is_read_bounded(conn, sender)? {
            return Ok(Err(Refusal::BoundedSender));
        }
        Ok(Ok(grant))
    }

    /// Apply a `ShareGrant` op: nothing is stored beside it, and the share
    /// fold decides what it counts for. One the fold would not believe today
    /// is logged, because a share that silently fails to reach its recipient
    /// is the failure nobody else would see.
    pub(super) fn apply_share_grant(
        &self,
        tx: &Transaction<'_>,
        p: &ShareGrantOpPayload,
        sender: &[u8; 16],
    ) -> rusqlite::Result<()> {
        let chain = self.chain_identities(tx)?;
        if let Err(why) = self.believe_share_grant(tx, &chain, p, sender)? {
            tracing::warn!(
                ev = "core.share.grant_refused",
                reason = why.as_str(),
                sender_h = hex_short(sender),
                "a share grant op is not believed; it is kept and counts for no share"
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
    /// device reading.
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
        let shares = self.stream_shares_in(tx, stream_id).map_err(to_sql)?;
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
                unknown: sunrise_domain::Unknowns::new(),
            });
            self.emit_control_op(tx, &op, now_ms, seal_under)?;
        }
        Ok(())
    }
}
