//! Withdrawing a revocation (ADR-0056): the op's apply, the command that
//! emits it, and the one question both ask of the ledger, whether a live
//! revocation names a device.
//!
//! Two ops withdraw. A device withdraws its own ordinary revocation with a
//! `device_revoke_withdraw`; the account identity in force withdraws its own
//! authority revocation with a `device_revoke_withdraw_by_identity`
//! (ADR-0058 §6), which reaches only the sender's authority pair and counts
//! only while it verifies under the head ([`pair_heads`]).
//!
//! Split out of [`super::revocation`] because it is the one path that runs the
//! other way. Everything there makes or folds a revocation; this makes a
//! sender's pair inert, and is the only path on which a read bound is released
//! for a device with a cert here ([`Engine::refold_device_revocations`] does
//! the release, on the three conditions of ADR-0056 §4).

use super::identity::IdentityHead;
use super::revocation::{ledger_row, RevokeLedgerRow, WITHDRAW_KIND};
use super::{Engine, EngineError, META_STREAM};
use crate::bytes::{hex_bytes, hex_short, to16};
use crate::commands::{CommandResult, Withdrawal, WithdrawalOutcome};
use crate::control_op::{DeviceRevokeWithdrawByIdentityPayload, DeviceRevokeWithdrawPayload};
use crate::feature::{REVOKE_WITHDRAW_AUTHORITY_FEATURE, REVOKE_WITHDRAW_FEATURE};
use crate::inner_op::{encode_inner_op, InnerOp};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use sunrise_cbor::hlc::Hlc;
use sunrise_crypto::WithdrawalClaim;
use sunrise_id::EntityRef;
use sunrise_storage::Db;

/// What one withdrawal op claims, as [`Engine::apply_device_revoke_withdraw`]
/// stores it: the target, and for a `device_revoke_withdraw_by_identity` the
/// identity it names and that identity's signature.
pub(super) struct WithdrawClaim {
    revoked_device_id: [u8; 16],
    authority: Option<([u8; 16], [u8; 64])>,
}

/// A `device_revoke_withdraw`: a row of the sender's ordinary pair.
impl From<&DeviceRevokeWithdrawPayload> for WithdrawClaim {
    fn from(p: &DeviceRevokeWithdrawPayload) -> Self {
        Self {
            revoked_device_id: p.revoked_device_id,
            authority: None,
        }
    }
}

/// A `device_revoke_withdraw_by_identity`, whatever its signature: the fold
/// decides whether it withdraws the sender's authority pair, against the head
/// it folds (ADR-0058 §6).
impl From<&DeviceRevokeWithdrawByIdentityPayload> for WithdrawClaim {
    fn from(p: &DeviceRevokeWithdrawByIdentityPayload) -> Self {
        Self {
            revoked_device_id: p.revoked_device_id,
            authority: Some((p.identity_id, p.identity_sig)),
        }
    }
}

impl RevokeLedgerRow {
    /// The pair this row belongs to: `(sender, target, names an identity)`,
    /// the partition [`Engine::compact_device_revoke_ops`] and
    /// [`Engine::cap_device_revoke_targets`] use.
    fn pair(&self) -> (&[u8], &[u8], bool) {
        (
            &self.sender,
            &self.revoked,
            self.authority_identity_id.is_some(),
        )
    }

    /// Whether this row outranks `other` as the head of their pair: the
    /// greater stamp, then a revocation over a withdrawal (ADR-0056 §2: the
    /// tie fails closed), then the greater identity, the order
    /// [`Engine::compact_device_revoke_ops`] keeps by.
    fn outranks(&self, other: &Self) -> bool {
        (
            self.hlc_ms,
            self.hlc_logical,
            !self.withdraw,
            &self.authority_identity_id,
        ) > (
            other.hlc_ms,
            other.hlc_logical,
            !other.withdraw,
            &other.authority_identity_id,
        )
    }
}

/// Each pair's head, split by kind (ADR-0056 §2), at a fold whose identity
/// in force is `head`.
///
/// The **live** heads are the revocations, in the canonical order `rows`
/// came in: the rows [`Engine::refold_device_revocations`] reads. The
/// **withdrawn** heads are the pairs a withdrawal made inert, which
/// contribute nothing to the gate, the discount or the walk, and which
/// [`release_withdrawn_read_bounds`] reads.
///
/// # An ordinary pair
///
/// Its head is its greatest row, a revocation winning a tie. A lesser row is
/// dropped here exactly as [`Engine::compact_device_revoke_ops`] deletes it,
/// so the fold is the same function of the full ledger as of the compacted
/// one.
///
/// # An authority pair (ADR-0058 §6)
///
/// The rows naming an identity are read as two: the pair's greatest
/// revocation `R`, and its greatest identity-signed withdrawal `W`, each by
/// stamp and then identity. `W` withdraws the pair when it outranks `R` (a
/// revocation still wins a tie) and either
///
/// - **(a)** `W` is an authority row at `head`: it names the identity in
///   force and verifies under that identity's `ID_S_pub`; or
/// - **(b)** `R` is **not** an authority row at `head`, and `W` names the
///   identity `R` names. Both were signed under one identity the chain has
///   since moved past (or `R`'s signature never verified), so `R` folds as
///   an ordinary claim of its sender, and `W`, carried by the same sender,
///   withdraws it under ADR-0056's own rule. This is what keeps a pair the
///   identity withdrew inert across a rotation, which does not carry it
///   forward ([`Engine::authority_revocations_at`]).
///
/// Anything else leaves `R` live: a withdrawal signed by a retired identity
/// cannot withdraw an authority row of the identity in force, and one whose
/// signature does not verify withdraws nothing an authority row holds. The
/// sender is the authenticated envelope's, and every row of the pair is that
/// sender's, so no device's signature reaches another sender's pair, and no
/// identity-signed withdrawal reaches an ordinary pair at all.
///
/// The compaction keeps exactly `R` and `W` of an authority pair, so this
/// too is the same function of the full ledger as of the compacted one.
pub(super) fn pair_heads(
    rows: Vec<RevokeLedgerRow>,
    head: &IdentityHead,
) -> (Vec<RevokeLedgerRow>, Vec<RevokeLedgerRow>) {
    // Per pair and kind, the greatest row. An ordinary pair reads only the
    // greater of its two; an authority pair reads both.
    let mut greatest: BTreeMap<(Vec<u8>, Vec<u8>, bool, bool), usize> = BTreeMap::new();
    for (i, row) in rows.iter().enumerate() {
        let (sender, revoked, named) = row.pair();
        let key = (sender.to_vec(), revoked.to_vec(), named, row.withdraw);
        match greatest.get(&key) {
            Some(&j) if !row.outranks(&rows[j]) => {}
            _ => {
                greatest.insert(key, i);
            }
        }
    }
    let mut live_at: BTreeSet<usize> = BTreeSet::new();
    let mut withdrawn_at: BTreeSet<usize> = BTreeSet::new();
    for (&(ref sender, ref revoked, named, withdraw), &i) in &greatest {
        if withdraw {
            continue;
        }
        let w = greatest
            .get(&(sender.clone(), revoked.clone(), named, true))
            .copied()
            .filter(|&w| rows[w].outranks(&rows[i]));
        let Some(w) = w else {
            live_at.insert(i);
            continue;
        };
        let (r_row, w_row) = (&rows[i], &rows[w]);
        let withdraws = !named
            || w_row.is_authority_at(head)
            || (!r_row.is_authority_at(head)
                && w_row.authority_identity_id == r_row.authority_identity_id);
        if withdraws {
            withdrawn_at.insert(w);
        } else {
            live_at.insert(i);
        }
    }
    // An ordinary pair whose only row is a withdrawal: the compaction removed
    // the revocation it withdrew, and it is still that pair's head.
    for (&(ref sender, ref revoked, named, withdraw), &w) in &greatest {
        if withdraw
            && !named
            && !greatest.contains_key(&(sender.clone(), revoked.clone(), named, false))
        {
            withdrawn_at.insert(w);
        }
    }
    let mut live = Vec::new();
    let mut withdrawn = Vec::new();
    for (i, row) in rows.into_iter().enumerate() {
        if live_at.contains(&i) {
            live.push(row);
        } else if withdrawn_at.contains(&i) {
            withdrawn.push(row);
        }
    }
    (live, withdrawn)
}

/// Release the read bound of each device a withdrawal has reinstated on this
/// replica (ADR-0056 §4). The last step of
/// [`Engine::refold_device_revocations`], over what it computed.
///
/// The bound section of that fold says why the bound is a ratchet an unwind
/// never releases. A withdrawal is the one thing that does, and only when all
/// three of these hold:
///
/// - **(a)** the register the fold just computed does not name the device;
/// - **(b)** no live pair names it anywhere in the ledger, gated or not, so
///   an unwound device, which still has its expelled revoker's live claim,
///   stays bounded (ADR-0041 item 5);
/// - **(c)** at least one withdrawal naming it comes from a sender the walk
///   does not gate: the sender has no revoker, after the discount, other than
///   the device the row names. So a revoked author cannot release an
///   accomplice it revoked while it was honest.
///
/// Per replica, like the bound itself: a replica that learned the author was
/// revoked before it saw the withdrawal fails (c) and keeps the bound, and one
/// that saw the withdrawal first has already released it. That is the
/// non-convergence [#1167](https://github.com/justin13888/Sunrise/issues/1167)
/// owns. Keys a released device is then sent are not taken back by a later
/// revocation of the withdrawal's author; revoke the device again.
///
/// A row adopted from a sponsor (`from_sponsor = 1`) is released too. The
/// mark exists so that [`Engine::release_orphan_read_bounds`] does not release
/// a bound before the ledger rows behind it arrive, and (c) is itself such a
/// row: the withdrawal is the head of a pair whose revocation this replica
/// holds or has compacted away.
pub(super) fn release_withdrawn_read_bounds(
    tx: &Transaction<'_>,
    withdrawn: &[RevokeLedgerRow],
    register: &BTreeMap<Vec<u8>, RevokeLedgerRow>,
    live_named: &BTreeSet<Vec<u8>>,
    revokers: &BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>,
) -> rusqlite::Result<()> {
    let released: BTreeSet<&Vec<u8>> = withdrawn
        .iter()
        .filter(|w| {
            !register.contains_key(&w.revoked)
                && !live_named.contains(&w.revoked)
                && !revokers
                    .get(&w.sender)
                    .is_some_and(|who| who.iter().any(|r| *r != w.revoked))
        })
        .map(|w| &w.revoked)
        .collect();
    for device_id in released {
        let n = tx.execute(
            "DELETE FROM device_read_bounds WHERE device_id = ?",
            params![device_id],
        )?;
        if n > 0 {
            tracing::info!(
                ev = "core.device.read_bound_released",
                subject_h = hex_bytes(&device_id[..device_id.len().min(4)]),
                "a withdrawal reinstated a device, so this replica gives it keys again"
            );
        }
    }
    Ok(())
}

/// The live pairs `(sender, target, names an identity)` of the ledger rows
/// `filter` selects, read at their heads as the fold reads them at `head`
/// ([`pair_heads`]). `filter` is a `WHERE` clause over `device_revoke_ops`
/// with `?1` bound to `arg`.
fn live_pairs(
    conn: &Connection,
    head: &IdentityHead,
    filter: &str,
    arg: &[u8],
) -> rusqlite::Result<Vec<RevokeLedgerRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT op_hlc_ms, op_hlc_logical, sender, revoked_device_id, reason,
                recorded_at_ms, authority_identity_id, authority_sig, kind
         FROM device_revoke_ops
         WHERE {filter}
         ORDER BY op_hlc_ms ASC, op_hlc_logical ASC, sender ASC,
                  revoked_device_id ASC, authority_identity_id ASC, kind ASC"
    ))?;
    let rows = stmt
        .query_map(params![arg], ledger_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(pair_heads(rows, head).0)
}

/// Whether a live revocation names `target`: a pair whose head is a
/// revocation, from `sender` when one is given and from anyone otherwise.
///
/// The pair and the head are the fold's (ADR-0056 §2, ADR-0058 §6), read at
/// `head`. Gated or not, because the question is whether a claim stands in the
/// ledger, which is what keeps a read bound (§4 (b)) and what a device may
/// withdraw (§1).
pub(crate) fn live_revocation_names(
    conn: &Connection,
    head: &IdentityHead,
    target: &[u8],
    sender: Option<&[u8]>,
) -> rusqlite::Result<bool> {
    Ok(live_pairs(conn, head, "revoked_device_id = ?1", target)?
        .iter()
        .any(|r| sender.is_none_or(|s| r.sender == s)))
}

/// Which of `me`'s own pairs are live, per device they name, at `head`: the
/// claims [`Engine::withdraw_revocation`] can withdraw, and what
/// `DeviceRow::withdrawable` reads.
pub(crate) struct OwnClaims {
    /// Devices `me`'s ordinary pair revokes.
    pub(crate) ordinary: BTreeSet<Vec<u8>>,
    /// Devices `me`'s authority pair revokes, whether or not the row is an
    /// authority row at `head`: one signed under a retired identity is still
    /// `me`'s own claim in that pair, and only an identity-signed withdrawal
    /// reaches it.
    pub(crate) authority: BTreeSet<Vec<u8>>,
}

impl OwnClaims {
    /// Read `me`'s live pairs at `head`.
    pub(crate) fn read(
        conn: &Connection,
        head: &IdentityHead,
        me: &[u8; 16],
    ) -> rusqlite::Result<Self> {
        let mut out = Self {
            ordinary: BTreeSet::new(),
            authority: BTreeSet::new(),
        };
        for row in live_pairs(conn, head, "sender = ?1", &me[..])? {
            if row.authority_identity_id.is_some() {
                out.authority.insert(row.revoked);
            } else {
                out.ordinary.insert(row.revoked);
            }
        }
        Ok(out)
    }
}

impl Engine {
    /// Withdraw this device's own revocation of `device_id` (ADR-0056).
    ///
    /// # Which claims it withdraws
    ///
    /// Every live claim of this device's own on the device, as the fold reads
    /// them at the head: its ordinary pair with a `device_revoke_withdraw`,
    /// and its authority pair with a `device_revoke_withdraw_by_identity`
    /// signed as the identity in force (ADR-0058 §6). It signs only where it
    /// holds the head's `ID_S_priv`; a device that does not cannot withdraw an
    /// authority claim, and when that is the only claim it holds the result is
    /// [`WithdrawalOutcome::NotAuthor`]. Each op needs its own feature,
    /// `core.revoke_withdraw` and `core.revoke_withdraw_authority`. While some
    /// device could drop the identity-signed op, a device that also holds an
    /// ordinary claim withdraws that one alone and reports
    /// [`WithdrawalOutcome::StillClaimed`]; one whose only claim is the
    /// authority one is refused as below.
    ///
    /// # What it refuses, and what it reports instead
    ///
    /// - **Itself**, as [`EngineError::Invalid`]: a device has no revocation
    ///   of itself to withdraw.
    /// - **A vault some device could drop the op in**, as
    ///   [`EngineError::FeatureUnsupportedByDevices`] naming them: every
    ///   device the register does not call revoked, and the device being
    ///   reinstated, which must fold its own reinstatement (§7). There is no
    ///   override: a device that dropped the op would fold a different
    ///   register for good. A device the user cannot update is revoked first.
    /// - **Nothing of its own to withdraw** is not an error. The result says
    ///   [`WithdrawalOutcome::NotAuthor`] and nothing is emitted: the device
    ///   list's revoked rows include other devices' revocations, and the
    ///   answer to "withdraw that" is who can.
    ///
    /// # What happens, in one transaction
    ///
    /// 1. `VaultRequires` naming each feature the ops use is emitted first
    ///    when the vault does not require it yet (ADR-0045 §7's emission
    ///    order).
    /// 2. Each withdrawal op is applied through the same
    ///    [`Self::apply_control_op`] a peer's takes, and logged. The fold
    ///    releases the device's read bound here when ADR-0056 §4's three
    ///    conditions hold.
    /// 3. This replica's unsent `relay_revocation_intents` row for the device
    ///    is deleted (§5), unless the relay is still owed it: the register
    ///    still names the device, or a live claim of this device's own does
    ///    (its identity-signed revocation, which shares the row). Then the row
    ///    stays and `relay_intent_cancelled` is `false`. One already sent
    ///    cannot be taken back: the relay
    ///    has no reinstatement yet, and [`Withdrawal::relay_intent_cancelled`]
    ///    is how a caller says so.
    /// 4. When the bound is released, every epoch this device holds is sealed
    ///    to the device through [`Self::backfill_key_envelopes`] (§4). The
    ///    epochs minted while it was revoked stay minted; the device reads them
    ///    because it was sent them now.
    #[allow(
        clippy::too_many_lines,
        reason = "its steps run in order over shared locals inside one transaction"
    )]
    pub(super) fn withdraw_revocation(
        &self,
        db: &mut Db,
        device_id: EntityRef,
    ) -> Result<CommandResult, EngineError> {
        let now_ms = self.clock.now_ms();
        let target = *device_id.bytes();
        let me = self.keychain.device_id();
        if target == me {
            return Err(EngineError::Invalid(
                "a device cannot withdraw a revocation of itself: it never made one".into(),
            ));
        }
        if !self
            .features
            .iter()
            .any(|f| f.id == REVOKE_WITHDRAW_FEATURE)
        {
            return Err(EngineError::Invalid(format!(
                "this build does not support `{REVOKE_WITHDRAW_FEATURE}`"
            )));
        }
        // Which of this device's own claims on the target stand, read at the
        // head as the fold reads them. The ordinary one it withdraws with a
        // `device_revoke_withdraw`; the authority one only as the identity in
        // force, so only where this device holds the head's `ID_S_priv`
        // (ADR-0058 §6).
        let head = self.current_identity(db.conn())?;
        let own = OwnClaims::read(db.conn(), &head, &me)?;
        let ordinary = own.ordinary.contains(&target[..]);
        let mut by_identity = own.authority.contains(&target[..])
            && self.keychain.can_rotate_identity()
            && head.identity_id == self.keychain.identity_id();
        if !ordinary && !by_identity {
            return Ok(
                CommandResult::new(device_id, None, [0u8; 16], 0).with_withdrawal(Withdrawal {
                    outcome: WithdrawalOutcome::NotAuthor,
                    read_bound_released: false,
                    keys_resent: 0,
                    relay_intent_cancelled: false,
                }),
            );
        }
        // Read outside the transaction, so the refusal is a typed error; the
        // `&mut Db` keeps anything else from writing in between.
        //
        // The authority claim is withdrawn only once every device that could
        // drop the op advertises its feature. Until then a device that also
        // holds an ordinary claim withdraws that one alone and the result says
        // the device is still claimed, as ADR-0058 §7 has the holder revoke
        // ordinarily rather than refuse; one whose only claim is the authority
        // one is refused, naming the devices to update, as ADR-0056 §7 refuses
        // the ordinary withdrawal.
        if by_identity {
            let lacking =
                self.lacking_for_withdrawal(db.conn(), &target, REVOKE_WITHDRAW_AUTHORITY_FEATURE)?;
            if !self
                .features
                .iter()
                .any(|f| f.id == REVOKE_WITHDRAW_AUTHORITY_FEATURE)
            {
                if !ordinary {
                    return Err(EngineError::Invalid(format!(
                        "this build does not support `{REVOKE_WITHDRAW_AUTHORITY_FEATURE}`"
                    )));
                }
                by_identity = false;
            } else if !lacking.is_empty() {
                if !ordinary {
                    return Err(EngineError::FeatureUnsupportedByDevices {
                        feature: REVOKE_WITHDRAW_AUTHORITY_FEATURE.to_owned(),
                        devices: lacking,
                    });
                }
                by_identity = false;
            }
        }
        if ordinary {
            let lacking =
                self.lacking_for_withdrawal(db.conn(), &target, REVOKE_WITHDRAW_FEATURE)?;
            if !lacking.is_empty() {
                return Err(EngineError::FeatureUnsupportedByDevices {
                    feature: REVOKE_WITHDRAW_FEATURE.to_owned(),
                    devices: lacking,
                });
            }
        }
        let mut needed: Vec<&str> = Vec::new();
        if ordinary {
            needed.push(REVOKE_WITHDRAW_FEATURE);
        }
        if by_identity {
            needed.push(REVOKE_WITHDRAW_AUTHORITY_FEATURE);
        }

        let mut op_id = [0u8; 16];
        let report = db.with_tx(|tx| -> rusqlite::Result<(u64, Withdrawal)> {
            for feature in &needed {
                if !self.vault_requires(tx, feature)? {
                    self.require_in_tx(tx, feature, now_ms)?;
                }
            }
            let bounded_before = self.is_read_bounded(tx, &target)?;
            let sent_before = envelopes_sent_to(tx, &target)?;

            let mut seq = 0;
            if ordinary {
                let hlc = self.hlc.send();
                let op = InnerOp::DeviceRevokeWithdraw(DeviceRevokeWithdrawPayload {
                    revoked_device_id: target,
                });
                (op_id, seq) = self.emit_withdrawal(tx, &op, &target, hlc, now_ms)?;
            }
            if by_identity {
                let hlc = self.hlc.send();
                let claim = WithdrawalClaim {
                    identity_id: head.identity_id,
                    sender_device_id: me,
                    revoked_device_id: target,
                    hlc_physical_ms: hlc.physical_ms,
                    hlc_logical: hlc.logical,
                };
                let identity_sig = self
                    .keychain
                    .sign_withdrawal_claim(&claim)
                    .map_err(crate::keychain::sqlite_from)?;
                let op = InnerOp::DeviceRevokeWithdrawByIdentity(
                    DeviceRevokeWithdrawByIdentityPayload {
                        revoked_device_id: target,
                        identity_id: head.identity_id,
                        identity_sig,
                    },
                );
                (op_id, seq) = self.emit_withdrawal(tx, &op, &target, hlc, now_ms)?;
            }

            // The intents table holds one row per device, and every claim this
            // device makes of the target shares it, the identity-signed one
            // (ADR-0058) included. So the row is cancelled only once nothing
            // still owes the relay its telling: the register no longer names
            // the device, and no live claim of this device's own does either.
            // A claim of this device's that the walk gates keeps the row too,
            // so the relay is told should the gate lift; the drain sends only
            // what the register names, so a kept row is never sent early.
            let still_owed = self.is_revoked(tx, &target)?
                || live_revocation_names(tx, &head, &target, Some(&me[..]))?;
            let relay_intent_cancelled = !still_owed
                && tx.execute(
                    "DELETE FROM relay_revocation_intents WHERE device_id = ?",
                    params![&target[..]],
                )? > 0;

            let bounded_after = self.is_read_bounded(tx, &target)?;
            let member: Option<CertColumns> = tx
                .query_row(
                    "SELECT identity_id, d_d_pub FROM devices WHERE device_id = ?",
                    params![&target[..]],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if !bounded_after {
                if let Some(d_d_pub) = member
                    .as_ref()
                    .and_then(|(_, p)| p.as_deref())
                    .and_then(|p| <[u8; 32]>::try_from(p).ok())
                {
                    self.backfill_key_envelopes(tx, &target, &d_d_pub, now_ms)?;
                }
            }
            let keys_resent = envelopes_sent_to(tx, &target)?.saturating_sub(sent_before);

            let outcome = if self.is_revoked(tx, &target)?
                || live_revocation_names(tx, &head, &target, None)?
            {
                WithdrawalOutcome::StillClaimed
            } else if member
                .as_ref()
                .and_then(|(id, _)| id.as_deref())
                .and_then(to16)
                != Some(head.identity_id)
            {
                WithdrawalOutcome::RepairRequired
            } else {
                WithdrawalOutcome::Reinstated
            };
            Ok((
                seq,
                Withdrawal {
                    outcome,
                    read_bound_released: bounded_before && !bounded_after,
                    keys_resent,
                    relay_intent_cancelled,
                },
            ))
        })?;
        let (seq, withdrawal) = report;
        tracing::info!(
            ev = "core.device.revocation_withdrawn",
            subject_h = hex_short(&target),
            reason = withdrawal.outcome.as_str(),
            "this device withdrew its own revocation of a device"
        );
        Ok(CommandResult::new(device_id, None, op_id, seq).with_withdrawal(withdrawal))
    }

    /// The devices that could drop a withdrawal op of `feature` naming
    /// `target`: every device the register does not call revoked whose build
    /// does not advertise it, and `target` itself when it does not, because it
    /// must fold its own reinstatement (ADR-0056 §7).
    fn lacking_for_withdrawal(
        &self,
        conn: &Connection,
        target: &[u8; 16],
        feature: &str,
    ) -> rusqlite::Result<Vec<[u8; 16]>> {
        let mut lacking = self.devices_lacking(conn, feature)?;
        if !advertises(conn, target, feature)? {
            lacking.push(*target);
            lacking.sort_unstable();
            lacking.dedup();
        }
        Ok(lacking)
    }

    /// Apply `op`, one withdrawal this device emits, and log it on the
    /// vault-meta stream, through the same [`Self::apply_control_op`] a
    /// peer's takes. Returns its op id and sequence number.
    fn emit_withdrawal(
        &self,
        tx: &Transaction<'_>,
        op: &InnerOp,
        target: &[u8; 16],
        hlc: Hlc,
        now_ms: u64,
    ) -> rusqlite::Result<([u8; 16], u64)> {
        let me = self.keychain.device_id();
        let inner = encode_inner_op(op).map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
        let at_epoch = self
            .keychain
            .current_epoch_tx(tx, &META_STREAM)?
            .unwrap_or(0);
        self.apply_control_op(tx, op, &me, hlc, now_ms, at_epoch)?;
        let seal_under = self.ensure_stream_epoch(tx, &META_STREAM, now_ms)?;
        let seq = self.next_seq_tx(tx, &META_STREAM)?;
        let op_id = self.fresh_op_id(now_ms);
        self.ops_insert_at(
            tx,
            &op_id,
            &META_STREAM,
            seq,
            hlc,
            &inner,
            op.inner_kind(),
            op.target_kind(),
            Some(target),
            Some(now_ms),
            None,
            now_ms,
            &[],
            seal_under.0,
            &seal_under.1,
        )?;
        Ok((op_id, seq))
    }

    /// Apply one withdrawal, from a peer or from this device's own
    /// [`Self::withdraw_revocation`]: a `device_revoke_withdraw` or a
    /// `device_revoke_withdraw_by_identity`.
    ///
    /// Stored like a revocation and folded with them, as a row of kind
    /// `withdraw` with no reason (migration 0053). A `device_revoke_withdraw`
    /// names no identity, so it is a row of the sender's **ordinary** pair. A
    /// `device_revoke_withdraw_by_identity` keeps its identity and signature,
    /// unchecked as an authority revocation's are, so it is a row of the
    /// sender's **authority** pair, and the fold decides against the head it
    /// folds whether it withdraws that pair ([`pair_heads`], ADR-0058 §6).
    ///
    /// The sender is the authenticated envelope's, so the row can make inert
    /// nothing but its author's own claim, and the register needs no check of
    /// the author's standing (ADR-0056 §3): a withdrawn pair reads as a pair
    /// its sender never wrote. The read bound does need one, and the fold
    /// applies it (§4 (c)).
    ///
    /// A withdrawal naming its own sender is refused, as a revocation naming
    /// its own sender is: no such pair can exist for it to withdraw.
    pub(super) fn apply_device_revoke_withdraw(
        &self,
        tx: &Transaction<'_>,
        p: impl Into<WithdrawClaim>,
        sender: &[u8; 16],
        hlc: Hlc,
        now_ms: u64,
    ) -> rusqlite::Result<Vec<([u8; 16], u32)>> {
        let p: WithdrawClaim = p.into();
        if p.revoked_device_id == *sender {
            tracing::warn!(
                ev = "core.device.revoke_refused",
                reason = "self",
                sender_h = hex_short(sender),
                "a device tried to withdraw a revocation of itself"
            );
            return Ok(Vec::new());
        }
        // `OR IGNORE` for the reason `apply_device_revoke` gives: a
        // re-delivered op must not move `recorded_at_ms`.
        let authority_id = p.authority.as_ref().map(|(id, _)| id.to_vec());
        let authority_sig = p.authority.as_ref().map(|(_, sig)| sig.to_vec());
        tx.execute(
            "INSERT OR IGNORE INTO device_revoke_ops
                 (op_hlc_ms, op_hlc_logical, sender, revoked_device_id, reason,
                  recorded_at_ms, authority_identity_id, authority_sig, kind)
             VALUES (?1, ?2, ?3, ?4, '', ?5, ?6, ?7, ?8)",
            params![
                i64::try_from(hlc.physical_ms).unwrap_or(i64::MAX),
                i64::from(hlc.logical),
                &sender[..],
                &p.revoked_device_id[..],
                i64::try_from(now_ms).unwrap_or(i64::MAX),
                authority_id,
                authority_sig,
                WITHDRAW_KIND,
            ],
        )?;
        if Self::cap_device_revoke_targets(
            tx,
            sender,
            &p.revoked_device_id,
            authority_id.as_deref(),
        )? {
            tracing::warn!(
                ev = "core.device.revoke_refused",
                reason = "sender_over_cap",
                sender_h = hex_short(sender),
                subject_h = hex_short(&p.revoked_device_id),
                "a device has named more distinct targets than one sender may keep"
            );
        }
        self.refold_device_revocations(tx)?;
        Self::compact_device_revoke_ops(tx)?;
        Self::release_orphan_read_bounds(tx)?;
        Ok(Vec::new())
    }
}

/// Whether `device`'s latest `DeviceFeatures` names feature `id`. A device
/// that has never emitted one advertises nothing.
fn advertises(conn: &Connection, device: &[u8; 16], id: &str) -> rusqlite::Result<bool> {
    let list: Option<String> = conn
        .query_row(
            "SELECT features FROM device_features WHERE device_id = ?",
            params![&device[..]],
            |r| r.get(0),
        )
        .optional()?;
    Ok(list.is_some_and(|l| l.split('\n').any(|f| f == id)))
}

/// A `devices` row's `identity_id` and `d_d_pub`, either of which an old row
/// may lack.
type CertColumns = (Option<Vec<u8>>, Option<Vec<u8>>);

/// How many `(stream, epoch)` keys this replica has recorded sending `device`.
fn envelopes_sent_to(conn: &Connection, device: &[u8; 16]) -> rusqlite::Result<u32> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM key_envelope_recipients WHERE recipient = ?",
        params![&device[..]],
        |r| r.get(0),
    )?;
    Ok(u32::try_from(n).unwrap_or(u32::MAX))
}
