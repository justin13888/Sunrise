//! Withdrawing a revocation (ADR-0056): the op's apply, the command that
//! emits it, and the one question both ask of the ledger, whether a live
//! revocation names a device.
//!
//! Split out of [`super::revocation`] because it is the one path that runs the
//! other way. Everything there makes or folds a revocation; this makes a
//! sender's pair inert, and is the only path on which a read bound is released
//! for a device with a cert here ([`Engine::refold_device_revocations`] does
//! the release, on the three conditions of ADR-0056 §4).

use super::revocation::{RevokeLedgerRow, WITHDRAW_KIND};
use super::{Engine, EngineError, META_STREAM};
use crate::bytes::{hex_bytes, hex_short, to16};
use crate::commands::{CommandResult, Withdrawal, WithdrawalOutcome};
use crate::control_op::DeviceRevokeWithdrawPayload;
use crate::feature::REVOKE_WITHDRAW_FEATURE;
use crate::inner_op::{encode_inner_op, InnerOp};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use sunrise_cbor::hlc::Hlc;
use sunrise_id::EntityRef;
use sunrise_storage::Db;

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

/// Each pair's head, split by kind (ADR-0056 §2).
///
/// The **live** heads are the revocations, in the canonical order `rows`
/// came in: the rows [`Engine::refold_device_revocations`] reads. The
/// **withdrawn** heads are the pairs whose head is a withdrawal, which
/// contribute nothing to the gate, the discount or the walk, and which
/// [`release_withdrawn_read_bounds`] reads.
///
/// A lesser row of a pair is dropped here exactly as
/// [`Engine::compact_device_revoke_ops`] would delete it, so the fold is the
/// same function of the full ledger as of the compacted one.
pub(super) fn pair_heads(
    rows: Vec<RevokeLedgerRow>,
) -> (Vec<RevokeLedgerRow>, Vec<RevokeLedgerRow>) {
    let mut head: BTreeMap<(Vec<u8>, Vec<u8>, bool), usize> = BTreeMap::new();
    for (i, row) in rows.iter().enumerate() {
        let (sender, revoked, named) = row.pair();
        let key = (sender.to_vec(), revoked.to_vec(), named);
        match head.get(&key) {
            Some(&j) if !row.outranks(&rows[j]) => {}
            _ => {
                head.insert(key, i);
            }
        }
    }
    let heads: BTreeSet<usize> = head.into_values().collect();
    let mut live = Vec::new();
    let mut withdrawn = Vec::new();
    for (i, row) in rows.into_iter().enumerate() {
        if !heads.contains(&i) {
            continue;
        }
        if row.withdraw {
            withdrawn.push(row);
        } else {
            live.push(row);
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

/// Whether a live revocation names `target`: a pair whose head is a
/// revocation, from `sender` when one is given and from anyone otherwise.
///
/// The pair and the head are the fold's (ADR-0056 §2): `(sender, target,
/// names an identity)`, the greatest stamp, a revocation winning a tie. Gated
/// or not, because the question is whether a claim stands in the ledger, which
/// is what keeps a read bound (§4 (b)) and what a device may withdraw (§1).
pub(crate) fn live_revocation_names(
    conn: &Connection,
    target: &[u8],
    sender: Option<&[u8]>,
) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM (
                 SELECT sender, kind, ROW_NUMBER() OVER (
                     PARTITION BY sender, authority_identity_id IS NULL
                     ORDER BY op_hlc_ms DESC, op_hlc_logical DESC, kind = 'revoke' DESC
                 ) AS pos
                 FROM device_revoke_ops
                 WHERE revoked_device_id = ?1
             )
             WHERE pos = 1 AND kind = 'revoke' AND (?2 IS NULL OR sender = ?2)
         )",
        params![target, sender],
        |r| r.get(0),
    )
}

/// Whether this device's own **ordinary** revocation of `target` is live: the
/// one claim [`Engine::withdraw_revocation`] can withdraw.
///
/// An authority revocation (ADR-0058) is not one: the identity is its author,
/// and the identity-signed withdrawal ADR-0058 §6 decides is not built.
fn own_ordinary_revocation_live(
    conn: &Connection,
    me: &[u8; 16],
    target: &[u8; 16],
) -> rusqlite::Result<bool> {
    let head: Option<String> = conn
        .query_row(
            "SELECT kind FROM device_revoke_ops
             WHERE sender = ?1 AND revoked_device_id = ?2
               AND authority_identity_id IS NULL
             ORDER BY op_hlc_ms DESC, op_hlc_logical DESC, kind = 'revoke' DESC
             LIMIT 1",
            params![&me[..], &target[..]],
            |r| r.get(0),
        )
        .optional()?;
    Ok(head.as_deref() == Some("revoke"))
}

impl Engine {
    /// Withdraw this device's own revocation of `device_id` (ADR-0056).
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
    /// 1. `VaultRequires` naming `core.revoke_withdraw` is emitted first when
    ///    the vault does not require it yet (ADR-0045 §7's emission order).
    /// 2. The `device_revoke_withdraw` op is applied through the same
    ///    [`Self::apply_control_op`] a peer's takes, and logged. The fold
    ///    releases the device's read bound here when ADR-0056 §4's three
    ///    conditions hold.
    /// 3. This replica's unsent `relay_revocation_intents` row for the device
    ///    is deleted (§5). One already sent cannot be taken back: the relay
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
        if !own_ordinary_revocation_live(db.conn(), &me, &target)? {
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
        let mut lacking = self.devices_lacking(db.conn(), REVOKE_WITHDRAW_FEATURE)?;
        if !advertises(db.conn(), &target, REVOKE_WITHDRAW_FEATURE)? {
            lacking.push(target);
            lacking.sort_unstable();
            lacking.dedup();
        }
        if !lacking.is_empty() {
            return Err(EngineError::FeatureUnsupportedByDevices {
                feature: REVOKE_WITHDRAW_FEATURE.to_owned(),
                devices: lacking,
            });
        }

        let op_id = self.fresh_op_id(now_ms);
        let report = db.with_tx(|tx| -> rusqlite::Result<(u64, Withdrawal)> {
            if !self.vault_requires(tx, REVOKE_WITHDRAW_FEATURE)? {
                self.require_in_tx(tx, REVOKE_WITHDRAW_FEATURE, now_ms)?;
            }
            let bounded_before = self.is_read_bounded(tx, &target)?;
            let sent_before = envelopes_sent_to(tx, &target)?;

            let op = InnerOp::DeviceRevokeWithdraw(DeviceRevokeWithdrawPayload {
                revoked_device_id: target,
            });
            let inner =
                encode_inner_op(&op).map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
            let hlc = self.hlc.send();
            let at_epoch = self
                .keychain
                .current_epoch_tx(tx, &META_STREAM)?
                .unwrap_or(0);
            self.apply_control_op(tx, &op, &me, hlc, now_ms, at_epoch)?;
            let seal_under = self.ensure_stream_epoch(tx, &META_STREAM, now_ms)?;
            let seq = self.next_seq_tx(tx, &META_STREAM)?;
            self.ops_insert_at(
                tx,
                &op_id,
                &META_STREAM,
                seq,
                hlc,
                &inner,
                op.inner_kind(),
                op.target_kind(),
                Some(&target),
                Some(now_ms),
                None,
                now_ms,
                &[],
                seal_under.0,
                &seal_under.1,
            )?;

            let relay_intent_cancelled = tx.execute(
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

            let head = self.current_identity(tx)?;
            let outcome =
                if self.is_revoked(tx, &target)? || live_revocation_names(tx, &target, None)? {
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

    /// Apply one `device_revoke_withdraw`, from a peer or from this device's
    /// own [`Self::withdraw_revocation`].
    ///
    /// Stored like a revocation and folded with them: a row of the sender's
    /// ordinary pair, kind `withdraw`, with no reason and no identity
    /// (migration 0053). The sender is the authenticated envelope's, so the
    /// row can make inert nothing but its author's own claim, and the
    /// register needs no check of the author's standing (ADR-0056 §3): a
    /// withdrawn pair reads as a pair its sender never wrote. The read bound
    /// does need one, and the fold applies it (§4 (c)).
    ///
    /// A withdrawal naming its own sender is refused, as a revocation naming
    /// its own sender is: no such pair can exist for it to withdraw.
    pub(super) fn apply_device_revoke_withdraw(
        &self,
        tx: &Transaction<'_>,
        p: &DeviceRevokeWithdrawPayload,
        sender: &[u8; 16],
        hlc: Hlc,
        now_ms: u64,
    ) -> rusqlite::Result<Vec<([u8; 16], u32)>> {
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
        tx.execute(
            "INSERT OR IGNORE INTO device_revoke_ops
                 (op_hlc_ms, op_hlc_logical, sender, revoked_device_id, reason,
                  recorded_at_ms, authority_identity_id, authority_sig, kind)
             VALUES (?1, ?2, ?3, ?4, '', ?5, NULL, NULL, ?6)",
            params![
                i64::try_from(hlc.physical_ms).unwrap_or(i64::MAX),
                i64::from(hlc.logical),
                &sender[..],
                &p.revoked_device_id[..],
                i64::try_from(now_ms).unwrap_or(i64::MAX),
                WITHDRAW_KIND,
            ],
        )?;
        if Self::cap_device_revoke_targets(tx, sender, &p.revoked_device_id, None)? {
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
