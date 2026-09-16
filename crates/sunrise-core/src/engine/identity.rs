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

/// How many candidate rows [`Engine::chain_identities`] verifies at one link.
///
/// A link's successors are taken greatest-first and the first that *verifies*
/// wins, so a row that does not verify costs one Ed25519 pair — about 52.5 µs
/// measured — and buys whoever wrote it one place in the ordering. Sixteen caps
/// the fold at roughly **0.84 ms per link** against about 52.5 µs for an
/// honest one, which has a single successor (two or three when devices rotate
/// concurrently without having seen each other).
///
/// This is a bound on *work*, and deliberately not on chain length. It replaced
/// `MAX_TRANSITION_CHAIN = 64`, which bounded length and could put an account
/// in a state it could never leave: any holder of `ID_S_priv` could rotate 63
/// times, and from then on the fold stopped at the 64th identity, every later
/// transition was a row the walk never reached, and no rotation could ever take
/// effect again — which means revocation could never take effect again either,
/// because a revocation *is* a rotation. Nothing recovered from that, and
/// nothing detected it: the head was a real identity that a real set of devices
/// was current under.
///
/// Length needed no bound. Every iteration of the walk either stops or inserts
/// a previously unseen `to_identity_id` into the visited set, that column is the
/// table's primary key, and the table is finite — so the walk terminates in at
/// most `count(*)` steps whatever the table contains, including a table with a
/// cycle in it. The old constant's own doc called itself "a termination
/// guarantee"; the visited set already was one.
///
/// The cap here is safe only because ingest keeps **the same sixteen rows this
/// query reads**: [`Engine::apply_control_op`]'s `IdentityTransition` arm holds
/// a predecessor to [`MAX_SIBLINGS_PER_PREDECESSOR`] rows, ordered by
/// [`SIBLING_ORDER_DESC`] — the order below — so the rows it discards are the
/// rows this `LIMIT` would never have reached anyway. Without that agreement a
/// member could occupy all sixteen places above an honest successor and
/// suppress it, which is the *other* unleavable state, and why the two
/// constants have to be read together.
///
/// It used to be an *arrival-order* cap, and that was
/// [#232](https://github.com/justin13888/Sunrise/issues/232): the first sixteen
/// rows to land held the places whatever they ranked, so sixteen forgeries of a
/// predecessor this replica had not yet established — where `prev_sig` cannot be
/// checked and deliberately is not — turned the honest successor away for good.
/// See [`MAX_SIBLINGS_PER_PREDECESSOR`] and ADR-0040.
///
/// # What this number actually enforces, and what it does not
///
/// **It can never truncate.** `apply_control_op` is the only writer of
/// `identity_transitions` — one `INSERT OR IGNORE`, behind
/// [`Engine::admit_sibling`] — and that keeps a predecessor at
/// [`MAX_SIBLINGS_PER_PREDECESSOR`] rows, so a predecessor tops out at exactly
/// that many. The `LIMIT` here is the same number, so the query is never asked
/// for a row it will not return. Raising this constant changes nothing; only
/// lowering it below the ingest cap does, and what that would do is make the
/// rows above the limit permanently unreachable — the unleavable state one
/// level down, which is the whole reason the two numbers are pinned equal.
///
/// **It is barely exercised, and that is now a statement about work rather than
/// about coverage.** The walk's `find_map` stops at the first candidate that
/// verifies, and after #232 there are two kinds of row it can step past: one
/// admitted while its predecessor was unknown and not yet swept by
/// [`Engine::purge_unverifiable_siblings`], and one from a genuinely concurrent
/// rotation that lost the ordering. Both are bounded by this number.
/// `engine::tests::forged_siblings_cannot_suppress_the_successor_of_an_unestablished_predecessor`
/// builds the first and is the suite's only exercise of the scan-past-a-bad-row
/// behaviour; before it existed, setting this constant to `1` left the entire
/// `sunrise-core` suite green — as did `2` and `3` — which was the measurement
/// rather than the claim.
///
/// The inequality is pinned by
/// `engine::tests::the_fold_looks_at_every_row_ingest_will_store`, and the
/// stronger property it stands in for — that the set ingest *keeps* is the set
/// this query *reads* — by
/// `engine::tests::what_ingest_keeps_is_what_the_fold_reads_whatever_order_it_arrives_in`.
pub(super) const MAX_SIBLING_CANDIDATES: usize = 16;

/// How many `identity_transitions` rows this replica stores for one predecessor.
///
/// An honest predecessor has exactly one successor; two or three when devices
/// rotate concurrently. Sixteen is that with room, and it is deliberately the
/// same number as [`MAX_SIBLING_CANDIDATES`] so the fold can never be asked to
/// look past what ingest admitted: a seventeenth row would be one the walk
/// could never reach, which is the shape the old chain cap had.
///
/// Costed at the cap: sixteen rows under one predecessor is 0.84 ms of fold,
/// and storing each one already cost its writer a valid `next_sig`, a roster
/// every entry of which verifies under the successor, and — when this replica
/// knows the predecessor — a valid `prev_sig`. The cap is what stops that being
/// unbounded, not what makes it expensive.
///
/// # It is a rank, not a queue
///
/// **Which** sixteen matters as much as how many, because the third of those
/// three costs is the one an adversary does not pay. A transition naming a
/// predecessor this replica has not established carries a `prev_sig` nothing
/// here can check — deliberately, so a replica may receive a chain newest-first
/// — so the cheapest possible row is one Ed25519 signature over an empty
/// roster. While this was a first-sixteen-to-arrive queue, sixteen of those
/// refused the honest successor for good
/// ([#232](https://github.com/justin13888/Sunrise/issues/232)).
///
/// [`Engine::admit_sibling`] therefore admits by [`SIBLING_ORDER_DESC`] and
/// evicts the weakest row rather than refusing the newest one. Two consequences,
/// and both are the point:
///
/// * **`meta_epoch` sorts first**, and a device cut by a rotation provably holds
///   no key above the epoch it was cut at, so its rows sort below the rotation
///   that cuts it however it dates its HLC (ADR-0037 §4). It can no longer
///   spend the budget the honest successor needs.
/// * **The retained set stops depending on arrival order.** Two replicas holding
///   the same ops keep the same rows and fold to the same head; under the queue
///   they could differ, which was a convergence defect independent of the
///   suppression.
pub(super) const MAX_SIBLINGS_PER_PREDECESSOR: i64 = 16;

/// The total order both ingest and the fold put a predecessor's successors in,
/// greatest first, as a SQL `ORDER BY` tail.
///
/// One string used by [`Engine::chain_identities`]' `LIMIT` and by
/// [`Engine::admit_sibling`]'s eviction, because the two agreeing is the whole
/// safety argument for [`MAX_SIBLING_CANDIDATES`]: a row ingest discards must be
/// one the fold would never have reached. Two copies could drift, and the
/// drift's symptom is a stored row the walk cannot see — silent, and the shape
/// of every bound in this file that has gone wrong.
///
/// `to_identity_id` is last and is not decorative. Without it the order is
/// partial — two rows from one device at one HLC tie — and SQLite would break
/// the tie by whatever the scan happened to produce, differently on two
/// replicas holding identical rows. It is the table's primary key, so with it
/// the order is total.
pub(super) const SIBLING_ORDER_DESC: &str = "meta_epoch DESC, hlc_physical_ms DESC, \
     hlc_logical DESC, emitter_device_id DESC, to_identity_id DESC";

/// How many devices one transition's roster, or its share list, may name.
///
/// Both are `Vec`s in a payload, so both are sized by whoever wrote the op, and
/// every roster entry costs a `DeviceCert` decode plus one Ed25519 verification
/// before the transition can be judged at all. 256 devices is far past any real
/// account and caps that check at about **13.4 ms** for one op
/// (256 × ~52.5 µs). Checked *before* the first verification runs, so an
/// oversized roster costs a length comparison rather than its own size — which
/// is the whole point: the work has to be bounded before it is done, not after.
pub(super) const MAX_ROSTER_ENTRIES: usize = 256;

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

/// Where one `identity_transitions` row sits in [`SIBLING_ORDER_DESC`].
///
/// The field order **is** the comparison — `derive(Ord)` is lexicographic over
/// declaration order — so this type and that string say the same thing twice,
/// in the only two languages the decision has to be made in.
/// `engine::tests::the_rank_type_and_the_sql_order_agree` is what stops them
/// disagreeing.
///
/// The two id fields are `Vec<u8>` rather than `[u8; 16]` because one side of
/// every comparison comes out of SQLite as a blob of whatever width the row
/// holds. Rust compares byte slices the way SQLite compares blobs — `memcmp`,
/// shorter-is-less on a prefix — so a malformed row sorts consistently in both
/// rather than panicking on a width conversion.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct SiblingRank {
    /// The vault-meta epoch the op was sealed under. First, because it is the
    /// only component an excluded device cannot raise — see
    /// [`MAX_SIBLINGS_PER_PREDECESSOR`] and ADR-0037 §4.
    pub meta_epoch: i64,
    /// The emitter's HLC physical half, as stored.
    pub hlc_physical_ms: i64,
    /// The emitter's HLC logical half: two ops in one millisecond still order.
    pub hlc_logical: i64,
    /// The device that signed the envelope.
    pub emitter_device_id: Vec<u8>,
    /// The successor named, which is the table's primary key and makes the
    /// order total.
    pub to_identity_id: Vec<u8>,
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
    /// speak for the account cannot hand it on — or holds no `ID_S_priv` at
    /// all, which since #105 is every device admitted by pairing. The second is
    /// checked first and by name, because it is a permanent property of the
    /// device rather than a state it might be in.
    pub(super) fn rotate_identity(
        &self,
        db: &mut Db,
        exclude: Option<[u8; 16]>,
        keep_recovery_code: bool,
    ) -> Result<RotationOutcome, EngineError> {
        if !self.keychain.can_rotate_identity() {
            return Err(EngineError::Invalid(
                "this device was admitted by pairing and holds no account signing key, so it \
                 cannot rotate the account identity; run this from the device that created the \
                 account"
                    .into(),
            ));
        }
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
    /// # What bounds it
    ///
    /// **Termination** is the visited set and nothing else. Every iteration
    /// either breaks or inserts a `to_identity_id` the walk has not seen;
    /// candidates already in the set are skipped; that column is the table's
    /// primary key. So the walk takes at most `count(identity_transitions)`
    /// steps on any table at all, including one with a cycle in it. There is no
    /// chain-length cap, and there was one — see [`MAX_SIBLING_CANDIDATES`] for
    /// the account-freezing state it could reach and why length was never the
    /// thing that needed bounding.
    ///
    /// **Work** is [`MAX_SIBLING_CANDIDATES`] per link, applied as a `LIMIT` so
    /// the rows beyond it are never read rather than read and discarded.
    ///
    /// Neither bound changes *which* link wins, only how much the walk costs.
    /// Standing stays a pure function of the op set (ADR-0037 §2): nothing here
    /// is stored, memoized or carried between calls, so two replicas holding
    /// the same rows still agree whatever order those rows arrived in.
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
        let mut stmt = conn.prepare(&format!(
            "SELECT to_identity_id, to_id_s_pub, to_id_d_pub, roster_digest,
                    shares_digest, prev_sig, next_sig
             FROM identity_transitions
             WHERE from_identity_id = ?1
             ORDER BY {SIBLING_ORDER_DESC}
             LIMIT ?2"
        ))?;
        let limit = i64::try_from(MAX_SIBLING_CANDIDATES).unwrap_or(i64::MAX);
        // Terminates on the visited set: see this function's doc. Each pass
        // either breaks or adds an id no pass has added before, and
        // `to_identity_id` is the table's primary key.
        loop {
            let (from_id, from_pub) = *chain.last().expect("the chain starts non-empty");
            let candidates = stmt
                .query_map(params![&from_id[..], limit], |r| {
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

    /// Whether `rank` may take one of `from_identity_id`'s
    /// [`MAX_SIBLINGS_PER_PREDECESSOR`] places, **evicting the weakest row to
    /// make one**.
    ///
    /// The register is full at the cap and stays full; what changes is which
    /// rows are in it. An arriving row that outranks the weakest displaces it,
    /// and one that does not is refused — so the sixteen rows a predecessor
    /// ends up with are the sixteen greatest of everything ever offered, which
    /// is a function of the op set and not of the order it arrived in. See
    /// [`MAX_SIBLINGS_PER_PREDECESSOR`] for why that is the security property
    /// and not merely a tidier one.
    ///
    /// The count excludes `rank.to_identity_id` so a **re-delivery is never
    /// refused by a full register**: the insert behind this is `OR IGNORE`, and
    /// a row already present occupies no fresh place. Excluding it also keeps a
    /// re-delivery from evicting anything, since a row cannot outrank itself.
    ///
    /// Eviction is a `DELETE`, and a deleted row is not lost standing. The
    /// evicted row is by construction one [`Self::chain_identities`] would
    /// never have read — same order, same count — so no fold on any replica
    /// holding these rows could have reached it.
    pub(super) fn admit_sibling(
        &self,
        tx: &Transaction<'_>,
        from_identity_id: &[u8; 16],
        rank: &SiblingRank,
    ) -> rusqlite::Result<bool> {
        let held: i64 = tx.query_row(
            "SELECT count(*) FROM identity_transitions
             WHERE from_identity_id = ?1 AND to_identity_id != ?2",
            params![&from_identity_id[..], &rank.to_identity_id],
            |r| r.get(0),
        )?;
        if held < MAX_SIBLINGS_PER_PREDECESSOR {
            return Ok(true);
        }
        // The weakest row held, read in the same order the fold reads and this
        // one ascending, so "the weakest" and "the one the fold reaches last"
        // are the same row by construction.
        let weakest: Option<SiblingRank> = tx
            .query_row(
                &format!(
                    "SELECT meta_epoch, hlc_physical_ms, hlc_logical, emitter_device_id,
                            to_identity_id
                     FROM identity_transitions
                     WHERE from_identity_id = ?1 AND to_identity_id != ?2
                     ORDER BY {SIBLING_ORDER_DESC}
                     LIMIT 1 OFFSET ?3"
                ),
                params![
                    &from_identity_id[..],
                    &rank.to_identity_id,
                    MAX_SIBLINGS_PER_PREDECESSOR - 1
                ],
                |r| {
                    Ok(SiblingRank {
                        meta_epoch: r.get(0)?,
                        hlc_physical_ms: r.get(1)?,
                        hlc_logical: r.get(2)?,
                        emitter_device_id: r.get(3)?,
                        to_identity_id: r.get(4)?,
                    })
                },
            )
            .optional()?;
        let Some(weakest) = weakest else {
            // `held >= cap` and yet the cap-th row does not exist. Unreachable
            // against a consistent table; refusing is the safe half, because
            // admitting here would leave the register over its bound.
            return Ok(false);
        };
        if *rank <= weakest {
            return Ok(false);
        }
        tx.execute(
            "DELETE FROM identity_transitions WHERE to_identity_id = ?",
            params![&weakest.to_identity_id],
        )?;
        tracing::warn!(
            ev = "core.identity.sibling_evicted",
            issuer_h = hex_short(from_identity_id),
            "an identity's successors are at the cap; the lowest-ordered row was \
             dropped for a higher-ordered one"
        );
        Ok(true)
    }

    /// Delete every row stored under `from_identity_id` that its **now known**
    /// key does not sign, and return how many went.
    ///
    /// The converging half of [#232](https://github.com/justin13888/Sunrise/issues/232).
    /// A transition naming a predecessor this replica has not established is
    /// stored with `prev_sig` unchecked — it must be, or a replica could not
    /// receive a chain newest-first — so a predecessor can accumulate rows
    /// nobody has verified. The moment the predecessor lands on the chain those
    /// rows become decidable, and the ones that fail are deleted rather than
    /// left holding places.
    ///
    /// **Deleting is permanent and that is sound.** `from_identity_id` is
    /// `identity_id_from_pub` of the key a row has to verify under, so the id
    /// determines the key: a row that fails here fails under every chain state
    /// this replica or any other could ever reach. It is not a link now and
    /// never will be, and no fold could have used it.
    ///
    /// Costs at most [`MAX_SIBLINGS_PER_PREDECESSOR`] Ed25519 pairs, once per
    /// identity per establishment — the same bound the fold already pays at one
    /// link, and paid on the path that removes the reason to pay it again.
    pub(super) fn purge_unverifiable_siblings(
        &self,
        tx: &Transaction<'_>,
        from_identity_id: &[u8; 16],
        from_pub: &[u8; 32],
    ) -> rusqlite::Result<usize> {
        let rows: Vec<TransitionRow> = {
            let mut stmt = tx.prepare(
                "SELECT to_identity_id, to_id_s_pub, to_id_d_pub, roster_digest,
                        shares_digest, prev_sig, next_sig
                 FROM identity_transitions
                 WHERE from_identity_id = ?1",
            )?;
            let rows = stmt
                .query_map(params![&from_identity_id[..]], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let mut dropped = 0usize;
        for row in &rows {
            // A row whose columns are the wrong width is one no signature could
            // have been taken over, which the fold reads as "not a link" too.
            let verified = TransitionLink::from_row(*from_identity_id, row).is_some_and(|link| {
                verify_identity_transition(&link.body, from_pub, &link.sigs).is_ok()
            });
            if verified {
                continue;
            }
            dropped += tx.execute(
                "DELETE FROM identity_transitions WHERE to_identity_id = ?",
                params![&row.0],
            )?;
        }
        if dropped > 0 {
            tracing::warn!(
                ev = "core.identity.siblings_purged",
                issuer_h = hex_short(from_identity_id),
                n_dropped = dropped,
                "an identity this replica has now established had successors stored \
                 under it that it never signed; they were dropped"
            );
        }
        Ok(dropped)
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
    /// Takes no clock reading: adoption no longer stamps anything. The cert
    /// this device ends up holding is the roster's, whose `created_at_ms` is
    /// the rotation's, so a second reading here could only disagree with it.
    pub(crate) fn recompute_identity_head(&self, tx: &Transaction<'_>) -> rusqlite::Result<()> {
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
        // This device's cert under the successor, taken from the roster rather
        // than re-issued locally. Since `#105` most devices hold no `ID_S_priv`
        // and could not re-issue it; they do not need to, because the roster is
        // the successor's own signed statement about who survives and every
        // entry in it has already been verified against `to_id_s_pub`.
        //
        // A device with a share but no roster entry is a malformed transition:
        // `rotate_identity` builds both lists from the same survivor set. It
        // lands in the same non-adoption as having no share at all.
        let mine = p.roster.iter().find_map(|e| {
            DeviceCert::from_cbor(&e.cert)
                .ok()
                .filter(|c| c.body.device_id == me)
                .map(|_| e.cert.clone())
        });
        let Some(roster_cert) = mine else {
            tracing::warn!(
                ev = "core.identity.not_in_roster",
                head_h = hex_short(&head.identity_id),
                "this device holds a share of the account's successor identity but no cert \
                 in its roster, so there is nothing to adopt under"
            );
            return Ok(());
        };
        match self
            .keychain
            .adopt_successor_identity(tx, successor, roster_cert, self.rng.as_ref())
        {
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
