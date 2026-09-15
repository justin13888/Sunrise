//! The account identity: the chain, its head, and the rotation that extends it.
//!
//! ADR-0032 concluded that revocation cannot bound cert issuance, because the
//! capability a revocation needs to take away — `ID_S_priv` — is held by every
//! paired device and named by none of them. ADR-0037 is the answer: the account
//! *becomes a different identity*, and the device being cut is simply not among
//! the recipients of the successor's share.
//!
//! Everything that decides **who the account is** lives here rather than in
//! `sync`, because the two answer different questions. `sync` decides whether
//! one op may be applied; this module decides, for the whole replica, which
//! identity a `DeviceCert` has to verify under before any of those decisions
//! mean anything. The fold is the only thing either of them agrees on, and a
//! second reader of `identity_transitions` is exactly what should not exist.

use super::ids::hex_short;
use super::{Engine, EngineError, META_STREAM};
use crate::control_op::{IdentityTransitionPayload, KeyShare, RosterEntry};
use crate::inner_op::{decode_inner_op, encode_inner_op, InnerOp};
use crate::keychain::{to16, to32, to64, Keychain, SuccessorPublics};
use rusqlite::{params, OptionalExtension, Transaction};
use std::collections::BTreeSet;
use sunrise_crypto::identity_transition::{IdentityTransitionBody, IdentityTransitionSigs};
use sunrise_crypto::{roster_digest, shares_digest, verify_identity_transition, DeviceCert};
use sunrise_storage::Db;

/// How many links [`Engine::chain_identities`] will walk before it stops.
///
/// An account rotates its identity on a revocation and on an explicit request,
/// so 64 is several lifetimes of ordinary use — the bound is not a budget, it
/// is a termination guarantee for a function that decides who the account is
/// and must not be able to run forever on a table somebody else's op wrote
/// into. `to_identity_id` is a primary key, so the only cycle the schema admits
/// is a self-loop, and the visited set catches that; this catches everything
/// the schema would admit if it ever stopped being a primary key.
///
/// A chain truncated here is not silently wrong in the dangerous direction: the
/// fold stops early, the head is a *superseded* identity, and every device
/// certified under the real head reads as not-current. That fails closed —
/// nothing is sealed to anybody — rather than admitting a device it should not.
const MAX_TRANSITION_CHAIN: usize = 64;

/// One device that survives a rotation, with everything its roster entry and
/// its share need.
///
/// A struct rather than the five-tuple this was: `clippy::type_complexity` is
/// denied, and the fields are worth names anyway. `d_s_pub` is read out of the
/// stored cert rather than from a column, on the same single-source-of-truth
/// rule `RosterEntry` follows — a column beside the cert could disagree with
/// the body the identity signed.
struct Survivor {
    device_id: [u8; 16],
    d_s_pub: [u8; 32],
    d_d_pub: [u8; 32],
    nickname: String,
    platform: String,
}

/// What one call to `Engine::rotate_identity` actually did.
///
/// `carried_recovery_code` is the field with consequences: the caller asked,
/// and the answer can be no. See `rotate_identity` for the one case that
/// refuses, and `Command::RotateIdentity` for what a UI owes the user when it
/// comes back false.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RotationOutcome {
    pub(super) to_identity_id: [u8; 16],
    pub(super) carried_recovery_code: bool,
    pub(super) devices_kept: usize,
    pub(super) op_id: [u8; 16],
    pub(super) seq: u64,
}

/// The account identity in force on a replica: the last link of its chain.
///
/// A pair rather than the id alone because every use needs both — the id to
/// compare a `devices` row against, and the key to verify a cert under — and
/// two separate reads could see them a rotation apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IdentityHead {
    /// `identity_id` of the identity in force.
    pub identity_id: [u8; 16],
    /// Its Ed25519 public key, which every current cert verifies under.
    pub id_s_pub: [u8; 32],
}

/// One row of `identity_transitions`, as the seven columns the fold reads.
///
/// A type alias declared before the first statement of anything that uses it,
/// because `clippy::type_complexity` is denied and this shape is genuinely a
/// row rather than a value worth a struct of its own.
type TransitionRow = (
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
);

/// A candidate next link, reassembled from its row into the shape
/// [`sunrise_crypto::verify_identity_transition`] takes.
struct TransitionLink {
    body: IdentityTransitionBody,
    sigs: IdentityTransitionSigs,
}

impl TransitionLink {
    /// `None` for any column of the wrong width, which is a row no signature
    /// could have been taken over and therefore not a link.
    fn from_row(from_identity_id: [u8; 16], row: &TransitionRow) -> Option<Self> {
        let (to_id, to_s, to_d, roster, shares, prev, next) = row;
        Some(Self {
            body: IdentityTransitionBody {
                from_identity_id,
                to_identity_id: to16(to_id)?,
                to_id_s_pub: to32(to_s)?,
                to_id_d_pub: to32(to_d)?,
                roster_digest: to32(roster)?,
                shares_digest: to32(shares)?,
            },
            sigs: IdentityTransitionSigs {
                prev_sig: to64(prev)?,
                next_sig: to64(next)?,
            },
        })
    }
}

impl Engine {
    /// Replace the account identity, and with it the membership.
    ///
    /// The emit half of ADR-0032. `exclude` is the device this rotation is
    /// leaving behind — `Some` when a revocation drives it, `None` for a
    /// user-requested rotation that keeps everybody.
    ///
    /// What it emits is one op carrying the whole hand-over, because the parts
    /// are not independently applicable: a replica that learned the new
    /// `ID_S_pub` without the re-issued certs would reject every device in the
    /// account, and one that learned the certs without the transition would
    /// have no key to verify them under.
    ///
    /// # Why the roster is built from the recipient rule and not from `devices`
    ///
    /// The surviving set is exactly "current, unrevoked, and not the device
    /// being excluded" — the same three facts `emit_key_envelopes` joins on.
    /// Reading `devices` unfiltered would put a device that is already
    /// non-current back into the roster and re-admit it in the act of
    /// rotating, which is the failure this whole mechanism exists to prevent.
    ///
    /// # The carry share, and the one case that must refuse it
    ///
    /// By default the successor is sealed to the **outgoing** `ID_D_pub` as
    /// well, so the user's existing recovery code keeps working: without it
    /// that code opens an identity the account has retired and the user must
    /// re-enrol from a device they may not have.
    ///
    /// The exception is not optional. The outgoing `ID_D_priv` is held by the
    /// account's creator, so when the device being revoked *is* the creator,
    /// carrying the successor forward under that key would hand the successor
    /// to the device the rotation exists to exclude — the rotation would
    /// complete, every check would pass, and the excluded device would hold the
    /// new identity. `keep_recovery_code` is therefore a request rather than an
    /// instruction, and the returned [`RotationOutcome`] says which way it went
    /// so a caller can tell the user their recovery code needs replacing.
    ///
    /// # Errors
    /// Storage failures, or [`EngineError::Invalid`] if this device holds no
    /// share of the identity it is trying to rotate — a device that cannot
    /// speak for the account cannot hand it on.
    pub(super) fn rotate_identity(
        &self,
        db: &mut Db,
        exclude: Option<[u8; 16]>,
        keep_recovery_code: bool,
    ) -> Result<RotationOutcome, EngineError> {
        let now_ms = self.clock.now_ms();
        let op_id = self.fresh_op_id(now_ms);
        let mut outcome = RotationOutcome {
            to_identity_id: [0u8; 16],
            carried_recovery_code: false,
            devices_kept: 0,
            op_id,
            seq: 0,
        };
        db.with_tx(|tx| -> rusqlite::Result<()> {
            let head = self.current_identity(tx)?;
            if head.identity_id != self.keychain.identity_id() {
                // This device is already not the account. Rotating from here
                // would fork the chain off a link nobody follows.
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            let successor = self.keychain.mint_successor_identity(self.rng.as_ref());

            // The surviving set, on the recipient rule. This device is always
            // in it — it is emitting — and is not in the `devices` query
            // because that one deliberately excludes self.
            let mut survivors: Vec<Survivor> = Vec::new();
            {
                let mut stmt = tx.prepare(
                    "SELECT d.device_id, d.d_d_pub, d.cert_blob, d.nickname, d.platform
                     FROM devices d
                     WHERE d.d_d_pub IS NOT NULL
                       AND d.identity_id = ?1
                       AND NOT EXISTS (
                           SELECT 1 FROM device_revocations r
                           WHERE r.device_id = d.device_id
                       )
                     ORDER BY d.device_id",
                )?;
                let rows = stmt
                    .query_map(params![&head.identity_id[..]], |r| {
                        Ok((
                            r.get::<_, Vec<u8>>(0)?,
                            r.get::<_, Vec<u8>>(1)?,
                            r.get::<_, Vec<u8>>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, String>(4)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for (id, dd, cert, nickname, platform) in rows {
                    let (Some(id), Some(dd)) = (to16(&id), to32(&dd)) else {
                        continue;
                    };
                    if Some(id) == exclude || id == self.keychain.device_id() {
                        continue;
                    }
                    // `d_s_pub` is inside the cert and nowhere else, which is
                    // the same single-source-of-truth rule `RosterEntry`
                    // follows: a column beside it could disagree with the body
                    // the identity signed.
                    let Ok(parsed) = DeviceCert::from_cbor(&cert) else {
                        continue;
                    };
                    survivors.push(Survivor {
                        device_id: id,
                        d_s_pub: parsed.body.d_s_pub,
                        d_d_pub: dd,
                        nickname,
                        platform,
                    });
                }
            }
            let (my_nick, my_platform) =
                Keychain::device_labels_tx(tx, &self.keychain.device_id())?;
            survivors.push(Survivor {
                device_id: self.keychain.device_id(),
                d_s_pub: self.keychain.device_signing_pub(),
                d_d_pub: self.keychain.device_dh_pub(),
                nickname: my_nick,
                platform: my_platform,
            });
            survivors.sort_unstable_by_key(|s| s.device_id);
            outcome.devices_kept = survivors.len();

            let mut roster: Vec<RosterEntry> = Vec::with_capacity(survivors.len());
            let mut device_shares: Vec<KeyShare> = Vec::with_capacity(survivors.len());
            for sv in &survivors {
                let cert = Keychain::issue_roster_cert(
                    &successor,
                    sv.device_id,
                    sv.d_s_pub,
                    sv.d_d_pub,
                    &sv.nickname,
                    &sv.platform,
                    now_ms,
                )
                .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
                roster.push(RosterEntry { cert });
                device_shares.push(KeyShare {
                    device_id: sv.device_id,
                    hpke_ciphertext: self
                        .keychain
                        .seal_successor_device_share(
                            &successor,
                            &sv.d_d_pub,
                            &sv.device_id,
                            self.rng.as_ref(),
                        )
                        .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?,
                });
            }

            // The creator holds the outgoing `ID_D_priv`, so a carry share
            // sealed to it reaches the creator and nobody else. Excluding the
            // creator therefore forbids the carry outright.
            let excluding_key_holder = exclude.is_some_and(|d| {
                tx.query_row(
                    "SELECT 1 FROM identity WHERE id = 1 AND minted_by_device_id = ?",
                    params![&d[..]],
                    |_| Ok(()),
                )
                .optional()
                .unwrap_or(None)
                .is_some()
            });
            let identity_share = (keep_recovery_code && !excluding_key_holder)
                .then(|| {
                    self.keychain
                        .seal_successor_carry_share(&successor, self.rng.as_ref())
                        .ok()
                        .map(serde_bytes::ByteBuf::from)
                })
                .flatten();
            outcome.carried_recovery_code = identity_share.is_some();

            let certs: Vec<&[u8]> = roster.iter().map(|e| e.cert.as_slice()).collect();
            let share_refs: Vec<([u8; 16], &[u8])> = device_shares
                .iter()
                .map(|s| (s.device_id, s.hpke_ciphertext.as_slice()))
                .collect();
            let body = sunrise_crypto::IdentityTransitionBody {
                from_identity_id: head.identity_id,
                to_identity_id: successor.identity_id(),
                to_id_s_pub: successor.id_s_pub(),
                to_id_d_pub: successor.id_d_pub(),
                roster_digest: roster_digest(&certs)
                    .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?,
                shares_digest: shares_digest(
                    &share_refs,
                    identity_share.as_ref().map(|b| b.as_slice()),
                )
                .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?,
            };
            let sigs = self
                .keychain
                .sign_transition(&body, &successor)
                .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
            outcome.to_identity_id = body.to_identity_id;

            let inner = InnerOp::IdentityTransition(Box::new(IdentityTransitionPayload {
                from_identity_id: body.from_identity_id,
                to_identity_id: body.to_identity_id,
                to_id_s_pub: body.to_id_s_pub,
                to_id_d_pub: body.to_id_d_pub,
                roster,
                device_shares,
                identity_share,
                prev_sig: sigs.prev_sig,
                next_sig: sigs.next_sig,
            }));
            let encoded =
                encode_inner_op(&inner).map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;

            // Sealed under whatever meta epoch is live **now** — and on the
            // revocation path that is the epoch the revocation has already
            // rotated *to*, not the one the departing device still holds.
            //
            // This is load-bearing and it is not visible from inside this
            // function, because it is a property of two transactions rather
            // than of one. `revoke_device` writes the revocation register,
            // mints a fresh epoch for every stream in `rotation_set` — the
            // vault-meta stream among them — seals each to the unrevoked
            // devices only, and commits. It calls this function afterwards, in
            // a second transaction, so `ensure_stream_epoch` below finds the
            // meta stream already at E+1 while the excluded device holds
            // nothing above E.
            //
            // That separation is the whole of why `meta_epoch` is the security
            // component of the fold's ordering (ADR-0037 §4). The excluded
            // device still holds `ID_S_priv` and can sign both halves of a
            // competing transition from the same predecessor; what it cannot do
            // is seal one at an epoch it has no key for, so its row sorts below
            // this one however far ahead it dates its HLC. An HLC is a claim;
            // an epoch is a key you either hold or do not.
            //
            // An earlier version of this comment said the op was sealed "under
            // the epoch the departing device still shares", which describes the
            // design in which that argument does not hold — the two rows would
            // tie on `meta_epoch` and an attacker-chosen `hlc_physical_ms`
            // would decide. `a_revocations_transition_is_sealed_above_the_epoch\
            // _the_cut_device_holds` pins the real behaviour, so the comment
            // cannot drift back to describing the broken one.
            let seal_under = self.ensure_stream_epoch(tx, &META_STREAM, now_ms)?;
            let hlc = self.hlc.send();
            outcome.seq = self.next_seq_tx(tx, &META_STREAM)?;
            self.ops_insert_at(
                tx,
                &op_id,
                &META_STREAM,
                outcome.seq,
                hlc,
                &encoded,
                "identity.transition",
                "identity",
                Some(&body.to_identity_id),
                Some(now_ms),
                None,
                now_ms,
                &[],
                seal_under.0,
                &seal_under.1,
            )?;
            // Applied locally through the same arm every peer will use, at the
            // same `meta_epoch` a peer will read off the envelope — so the
            // local fold and every remote one order this transition
            // identically.
            self.apply_control_op(
                tx,
                &inner,
                &self.keychain.device_id(),
                hlc,
                now_ms,
                seal_under.0,
            )?;
            Ok(())
        })
        .map_err(|e| match e {
            sunrise_storage::DbError::Sqlite(rusqlite::Error::QueryReturnedNoRows) => {
                EngineError::Invalid(
                    "this device does not hold the account's current identity and cannot \
                     rotate it"
                        .into(),
                )
            }
            other => EngineError::Storage(other),
        })?;
        Ok(outcome)
    }
    /// The account identity **in force** on this replica: the last link of
    /// [`Self::chain_identities`].
    ///
    /// Derived from the chain at every call and never read off the `identity`
    /// row, and the difference is load-bearing. That row holds the identity
    /// **this device signs with**, which is a different fact: a device left out
    /// of a rotation's roster never adopts the successor and keeps its own row
    /// unchanged forever. Reading the head from there would let exactly that
    /// device believe it was still the account — and it is precisely the device
    /// that must not.
    ///
    /// Same discipline as [`Self::is_revoked`]: table reads and nothing else.
    /// No clock — the two paragraphs there about what a wall clock does to a
    /// decision like this apply here word for word.
    pub(crate) fn current_identity(
        &self,
        conn: &rusqlite::Connection,
    ) -> rusqlite::Result<IdentityHead> {
        let chain = self.chain_identities(conn)?;
        let (identity_id, id_s_pub) = *chain
            .last()
            .expect("chain_identities always returns at least the genesis link");
        Ok(IdentityHead {
            identity_id,
            id_s_pub,
        })
    }

    /// Every account identity on this replica's transition chain, genesis
    /// first, head last.
    ///
    /// This is the fold, and it is the whole of what makes
    /// [#105](https://github.com/justin13888/Sunrise/issues/105) closable. A
    /// `DeviceCert` names no issuer, so "was this cert issued by this account"
    /// has only ever had one answer available — does it verify under the
    /// identity — and while there was exactly one identity, forever, that
    /// answer could not distinguish a member from a device that left and kept
    /// `ID_S_priv`. With a chain it still cannot, and no longer needs to: the
    /// cert verifies under *some* link, which is a fact, and membership is
    /// whether that link is the last one, which is a decision taken fresh at
    /// every point of use.
    ///
    /// # What the walk checks, and why here rather than at apply
    ///
    /// Each step takes the successors of the current link, picks the greatest
    /// by `(meta_epoch, hlc_physical_ms, hlc_logical, emitter_device_id)`, and
    /// **verifies both signatures** before extending: `prev_sig` under the
    /// current link's `ID_S_pub` (the outgoing identity authorizing the
    /// hand-over) and `next_sig` under the candidate's own (the successor
    /// accepting it). A link that fails either is not a link.
    ///
    /// The signatures are checked here and not in `apply_control_op` because
    /// `prev_sig` can only be checked against a predecessor the verifier has
    /// already established, and at apply time it has not: an out-of-order
    /// transition naming an identity two links ahead is a legitimate op that
    /// this replica cannot yet verify and must still store. Applying stays
    /// unconditional (ADR-0034); standing is computed from the op set, so two
    /// replicas holding the same ops agree whatever order they arrived in.
    ///
    /// Bounded at [`MAX_TRANSITION_CHAIN`] links, with the visited set as the
    /// second bound: `to_identity_id` is a primary key so a self-loop is the
    /// only cycle the schema admits, but a fold that decides who the account is
    /// must terminate on a malformed table rather than on an argument about
    /// one.
    pub(crate) fn chain_identities(
        &self,
        conn: &rusqlite::Connection,
    ) -> rusqlite::Result<Vec<([u8; 16], [u8; 32])>> {
        let genesis: Option<(Vec<u8>, Option<Vec<u8>>)> = conn
            .query_row(
                "SELECT genesis_identity_id, genesis_id_s_pub FROM identity WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let start = genesis
            .and_then(|(id, pk)| Some((to16(&id)?, to32(&pk?)?)))
            // The test-shaped vault has no `identity` row at all. The
            // keychain's *constructed-at* identity is the right answer there
            // and `identity_id()` is not: adoption moves the latter, so a test
            // vault that rotated would forget its own genesis and stop
            // verifying every cert issued before the rotation — the exact
            // regression this chain exists to prevent.
            .unwrap_or_else(|| self.keychain.constructed_at_identity());

        let mut chain = vec![start];
        let mut seen: BTreeSet<[u8; 16]> = BTreeSet::new();
        seen.insert(start.0);
        let mut stmt = conn.prepare(
            "SELECT to_identity_id, to_id_s_pub, to_id_d_pub, roster_digest,
                    shares_digest, prev_sig, next_sig
             FROM identity_transitions
             WHERE from_identity_id = ?
             ORDER BY meta_epoch DESC, hlc_physical_ms DESC, hlc_logical DESC,
                      emitter_device_id DESC",
        )?;
        while chain.len() < MAX_TRANSITION_CHAIN {
            let (from_id, from_pub) = *chain.last().expect("the chain starts non-empty");
            let candidates = stmt
                .query_map(params![&from_id[..]], |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                        r.get::<_, Vec<u8>>(3)?,
                        r.get::<_, Vec<u8>>(4)?,
                        r.get::<_, Vec<u8>>(5)?,
                        r.get::<_, Vec<u8>>(6)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            // The winner is the greatest by the ordering key, but a row that
            // does not verify is not a candidate at all — so this takes the
            // first that *verifies* in descending order rather than verifying
            // only the greatest. A forged row cannot suppress an honest one by
            // sorting above it.
            let next = candidates.into_iter().find_map(|row| {
                let link = TransitionLink::from_row(from_id, &row)?;
                if seen.contains(&link.body.to_identity_id) {
                    return None;
                }
                verify_identity_transition(&link.body, &from_pub, &link.sigs).ok()?;
                Some((link.body.to_identity_id, link.body.to_id_s_pub))
            });
            let Some(next) = next else { break };
            seen.insert(next.0);
            chain.push(next);
        }
        Ok(chain)
    }

    /// Move every device the roster names onto the successor identity.
    ///
    /// **This is what a roster is for**, and without it the whole mechanism
    /// fails closed on the wrong people: `devices.identity_id` records which
    /// chain identity certified a device, every membership test compares it to
    /// the head, and a rotation moves the head. If nothing rewrote those rows,
    /// the first rotation would make *every* device non-current — including the
    /// ones the rotation was meant to keep — and the account would stop sealing
    /// keys to anybody. That is exactly what happened, and it is the e2e
    /// revocation test that said so.
    ///
    /// So the roster is not merely evidence that the successor blessed these
    /// devices; it is the instruction to record that it did. Each entry
    /// replaces the device's `cert_blob`, `d_d_pub` and labels as well as its
    /// `identity_id`, because the re-issued cert is now the one a peer will
    /// verify that device's envelopes against.
    ///
    /// A device **not** in the roster is untouched, keeps pointing at the
    /// identity that certified it, and is current no longer. No row is deleted
    /// and no revocation is written: it is not an accusation, it is an
    /// omission, and the device can be re-admitted by an ordinary cert publish
    /// under the new identity if it turns out to be honest.
    ///
    /// Every entry has already been verified against `to_id_s_pub` by the
    /// caller, so this does not re-check — but it does re-derive `device_id`
    /// from the cert rather than trusting any outer copy, because there is no
    /// outer copy and that is deliberate.
    pub(super) fn apply_roster(
        &self,
        tx: &Transaction<'_>,
        roster: &[RosterEntry],
        to_identity_id: &[u8; 16],
    ) -> rusqlite::Result<()> {
        for entry in roster {
            let Ok(cert) = DeviceCert::from_cbor(&entry.cert) else {
                continue;
            };
            tx.execute(
                "INSERT INTO devices
                 (device_id, cert_blob, nickname, platform, created_at_ms,
                  identity_id, d_d_pub)
                 VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(device_id) DO UPDATE SET
                    cert_blob = excluded.cert_blob,
                    nickname = excluded.nickname,
                    platform = excluded.platform,
                    identity_id = excluded.identity_id,
                    d_d_pub = excluded.d_d_pub",
                params![
                    &cert.body.device_id[..],
                    entry.cert,
                    cert.body.nickname,
                    cert.body.platform,
                    i64::try_from(cert.body.created_at_ms).unwrap_or(i64::MAX),
                    &to_identity_id[..],
                    &cert.body.d_d_pub[..],
                ],
            )?;
        }
        Ok(())
    }

    /// Bring this device's own identity into step with the chain head.
    ///
    /// [`Self::chain_identities`] decides who the account *is*; this decides
    /// whether this device is still it. The two are separate on purpose, and a
    /// device that cannot follow the chain is not an error — it is the
    /// mechanism. A rotation's roster names the devices that survive it, and
    /// only those devices get a share of the successor's `ID_S_priv`, so a
    /// device left out finds no share here, adopts nothing, and goes on signing
    /// under an identity that is no longer the head. Every membership test in
    /// this file then reads it as not-current. **That is the fix for #105
    /// arriving, not failing.**
    ///
    /// Two shares are tried, carry first. The carry copy is sealed to the
    /// outgoing `ID_D_pub` and carries `ID_D_priv` as well, so a device that
    /// can open it keeps the account's unwrapping key across the rotation;
    /// preferring it is what stops the creator silently demoting itself to a
    /// device that can seal to the identity and not open what it sealed.
    ///
    /// Idempotent and safe to call on every open: it returns immediately when
    /// this device already signs under the head, which is the ordinary case.
    ///
    /// # Errors
    /// SQLite failures only. A share that will not open, a payload that will
    /// not decode and a successor this device is not in the roster of are all
    /// the same non-error outcome — no adoption — and are logged rather than
    /// returned, for the reason `apply_control_op`'s other arms give.
    pub(crate) fn recompute_identity_head(
        &self,
        tx: &Transaction<'_>,
        now_ms: u64,
    ) -> rusqlite::Result<()> {
        let head = self.current_identity(tx)?;
        if head.identity_id == self.keychain.identity_id() {
            return Ok(());
        }
        let row: Option<Vec<u8>> = tx
            .query_row(
                "SELECT payload FROM identity_transitions WHERE to_identity_id = ?",
                params![&head.identity_id[..]],
                |r| r.get(0),
            )
            .optional()?;
        let Some(payload) = row else {
            return Ok(());
        };
        let Ok(InnerOp::IdentityTransition(p)) = decode_inner_op(&payload) else {
            return Ok(());
        };
        let publics = SuccessorPublics {
            identity_id: p.to_identity_id,
            id_s_pub: p.to_id_s_pub,
            id_d_pub: p.to_id_d_pub,
        };
        let me = self.keychain.device_id();
        let successor = p
            .identity_share
            .as_ref()
            .and_then(|b| {
                self.keychain
                    .open_successor_carry_share(&publics, b.as_slice())
                    .ok()
            })
            .or_else(|| {
                p.device_shares
                    .iter()
                    .find(|s| s.device_id == me)
                    .and_then(|s| {
                        self.keychain
                            .open_successor_device_share(&publics, &s.hpke_ciphertext)
                            .ok()
                    })
            });
        let Some(successor) = successor else {
            tracing::warn!(
                ev = "core.identity.not_in_roster",
                head_h = hex_short(&head.identity_id),
                "this device holds no share of the account's current identity and no \
                 longer speaks for it"
            );
            return Ok(());
        };
        let (nickname, platform) = Keychain::device_labels_tx(tx, &me)?;
        match self.keychain.adopt_successor_identity(
            tx,
            successor,
            &nickname,
            &platform,
            now_ms,
            self.rng.as_ref(),
        ) {
            Ok(_) => {
                tracing::info!(
                    ev = "core.identity.adopted",
                    head_h = hex_short(&head.identity_id),
                    "this device adopted the account's successor identity"
                );
            }
            Err(e) => {
                tracing::warn!(
                    ev = "core.identity.adopt_failed",
                    head_h = hex_short(&head.identity_id),
                    cause = %e,
                    "could not adopt the account's successor identity"
                );
            }
        }
        Ok(())
    }
}
