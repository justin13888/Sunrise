//! The account identity's authority over membership (ADR-0058): which ledger
//! row is an authority row, whether `Command::RevokeDevice` signs, and the
//! carry-forward across a rotation.
//!
//! Split out of [`super::revocation`] because these share one input the rest
//! of that module does not read: the identity at the head of the chain and
//! its `ID_S_pub`. The fold there asks [`RevokeLedgerRow::is_authority_at`]
//! once per row and seats [`IDENTITY`] for every row that answers yes; the
//! rest of what it does is ADR-0041's.

use super::identity::IdentityHead;
use super::revocation::{ledger_row, RevokeLedgerRow};
use super::{Engine, META_STREAM};
use crate::control_op::{DeviceRevokeByIdentityPayload, RevokeReason};
use crate::feature::REVOKE_AUTHORITY_FEATURE;
use crate::inner_op::{encode_inner_op, InnerOp};
use crate::keychain::{Identity, Keychain};
use rusqlite::{params, Transaction};
use std::collections::BTreeMap;
use sunrise_crypto::{RevocationClaim, StreamKey};

/// The revoker an authority row seats in its target's set (ADR-0058 §4).
///
/// No device's id: every device id is 16 bytes and this is not, so no row
/// revokes it and the discount, which removes a revoker only when a row
/// revokes that revoker, never removes it. And since it is never the device a
/// row names, a device with an authority revocation always has a revoker other
/// than the target of its own row, so the mutual exception never reaches it.
pub(super) const IDENTITY: &[u8] = b"identity";

impl RevokeLedgerRow {
    /// Whether this row is an **authority row at a fold whose head is
    /// `head`** (ADR-0058 §3): it names the head, and its signature verifies
    /// under the head's `ID_S_pub` over the claim rebuilt from the row.
    ///
    /// Every other row is an ordinary row from its sender, including one
    /// signed under an identity the chain has moved past, one whose
    /// signature does not verify, and one whose columns are the wrong width
    /// for any claim to have been signed over.
    pub(super) fn is_authority_at(&self, head: &IdentityHead) -> bool {
        let (Some(id), Some(sig)) = (&self.authority_identity_id, &self.authority_sig) else {
            return false;
        };
        if id.as_slice() != head.identity_id {
            return false;
        }
        let (Ok(sig), Ok(sender), Ok(revoked), Ok(ms), Ok(logical)) = (
            <[u8; 64]>::try_from(sig.as_slice()),
            <[u8; 16]>::try_from(self.sender.as_slice()),
            <[u8; 16]>::try_from(self.revoked.as_slice()),
            u64::try_from(self.hlc_ms),
            u32::try_from(self.hlc_logical),
        ) else {
            return false;
        };
        RevocationClaim {
            identity_id: head.identity_id,
            sender_device_id: sender,
            revoked_device_id: revoked,
            hlc_physical_ms: ms,
            hlc_logical: logical,
            reason_code: self.reason.clone(),
        }
        .verify(&head.id_s_pub, &sig)
        .is_ok()
    }
}

/// Whether [`Engine::revoke_device`] signs, decided inside its transaction.
pub(super) enum RevokeAuthority {
    /// Sign. `require_first` when the vault does not require
    /// `core.revoke_authority` yet, so the command emits `VaultRequires`
    /// before the op.
    Sign { require_first: bool },
    /// Emit an ordinary revocation. `awaiting` names the devices whose build
    /// does not advertise the feature, when they are all that stands between
    /// this device and signing; empty when this device cannot sign at all.
    Withheld { awaiting: Vec<[u8; 16]> },
}

impl Engine {
    /// Whether [`Self::revoke_device`] signs its revocation of `target` as
    /// the account identity (ADR-0058 §7).
    ///
    /// It signs when this device holds the `ID_S_priv` of the identity at the
    /// head of the chain, this build has `core.revoke_authority`, and the
    /// vault requires it or every device that is not revoked advertises it.
    /// The device being revoked is left out of that count: it is the one
    /// device the op makes revoked, and its standing is what the op is about.
    /// An attacker device that never advertises the feature would otherwise
    /// keep the account from ever revoking it by authority.
    pub(super) fn revocation_authority(
        &self,
        tx: &Transaction<'_>,
        target: &[u8; 16],
    ) -> rusqlite::Result<RevokeAuthority> {
        let holds_head = self.keychain.can_rotate_identity()
            && self.current_identity(tx)?.identity_id == self.keychain.identity_id();
        if !holds_head
            || !self
                .features
                .iter()
                .any(|f| f.id == REVOKE_AUTHORITY_FEATURE)
        {
            return Ok(RevokeAuthority::Withheld {
                awaiting: Vec::new(),
            });
        }
        if self.vault_requires(tx, REVOKE_AUTHORITY_FEATURE)? {
            return Ok(RevokeAuthority::Sign {
                require_first: false,
            });
        }
        let awaiting: Vec<[u8; 16]> = self
            .devices_lacking(tx, REVOKE_AUTHORITY_FEATURE)?
            .into_iter()
            .filter(|d| d != target)
            .collect();
        Ok(if awaiting.is_empty() {
            RevokeAuthority::Sign {
                require_first: true,
            }
        } else {
            RevokeAuthority::Withheld { awaiting }
        })
    }

    /// Every device an authority row names at a fold whose head is `head`,
    /// with the reason of the greatest such row in the fold's order.
    ///
    /// What [`Self::rotate_identity`] carries forward (ADR-0058 §5): read
    /// before the rotation moves the head, because from the first fold that
    /// sees the successor these rows are ordinary.
    pub(super) fn authority_revocations_at(
        &self,
        conn: &rusqlite::Connection,
        head: &IdentityHead,
    ) -> rusqlite::Result<Vec<([u8; 16], RevokeReason)>> {
        let mut stmt = conn.prepare(
            "SELECT op_hlc_ms, op_hlc_logical, sender, revoked_device_id, reason,
                    recorded_at_ms, authority_identity_id, authority_sig
             FROM device_revoke_ops
             WHERE authority_identity_id = ?1
             ORDER BY op_hlc_ms ASC, op_hlc_logical ASC, sender ASC,
                      revoked_device_id ASC",
        )?;
        let rows = stmt
            .query_map(params![&head.identity_id[..]], ledger_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut named: BTreeMap<[u8; 16], RevokeReason> = BTreeMap::new();
        for row in rows.iter().filter(|r| r.is_authority_at(head)) {
            let (Ok(target), Some(reason)) = (
                <[u8; 16]>::try_from(row.revoked.as_slice()),
                RevokeReason::parse(&row.reason),
            ) else {
                continue;
            };
            named.insert(target, reason);
        }
        Ok(named.into_iter().collect())
    }

    /// Emit, under the successor a rotation is minting, an authority
    /// revocation of each device in `carry` (ADR-0058 §5).
    ///
    /// A row signed under a retired identity is ordinary from the first fold
    /// that sees the new head, so without this every rotation would undo
    /// every authority revocation and the shape of #394 would reopen. Run by
    /// [`Self::rotate_identity`] in the transaction that emits the
    /// transition, which it has already applied, so each op lands as an
    /// authority row at once. The rotating device holds both keys, which is
    /// what lets it sign these.
    ///
    /// The carry-forward does not judge the rows it carries: a vault restored
    /// after a theft carries what the thief signed too, and those devices are
    /// left out of the roster either way, because the register revokes them.
    pub(super) fn carry_authority_forward(
        &self,
        tx: &Transaction<'_>,
        successor: &Identity,
        carry: &[([u8; 16], RevokeReason)],
        seal_under: &(u32, StreamKey),
        now_ms: u64,
    ) -> rusqlite::Result<()> {
        if carry.is_empty() {
            return Ok(());
        }
        // The vault holds the op kind already, so it requires the feature on
        // every replica that has applied the `VaultRequires` the first
        // authority revocation followed. This replica may hold the row
        // without it; the seal guard refuses the op until it has it.
        if !self.vault_requires(tx, REVOKE_AUTHORITY_FEATURE)? {
            self.require_in_tx(tx, REVOKE_AUTHORITY_FEATURE, now_ms)?;
        }
        let me = self.keychain.device_id();
        for (target, reason) in carry {
            let hlc = self.hlc.send();
            let claim = RevocationClaim {
                identity_id: successor.identity_id(),
                sender_device_id: me,
                revoked_device_id: *target,
                hlc_physical_ms: hlc.physical_ms,
                hlc_logical: hlc.logical,
                reason_code: reason.as_str().to_owned(),
            };
            let identity_sig = Keychain::sign_revocation_claim_as(successor, &claim)
                .map_err(crate::keychain::sqlite_from)?;
            let op = InnerOp::DeviceRevokeByIdentity(DeviceRevokeByIdentityPayload {
                revoked_device_id: *target,
                reason_code: *reason,
                identity_id: claim.identity_id,
                identity_sig,
            });
            let encoded =
                encode_inner_op(&op).map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
            self.apply_control_op(tx, &op, &me, hlc, now_ms, seal_under.0)?;
            let op_id = self.fresh_op_id(now_ms);
            let seq = self.next_seq_tx(tx, &META_STREAM)?;
            self.ops_insert_at(
                tx,
                &op_id,
                &META_STREAM,
                seq,
                hlc,
                &encoded,
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
        }
        Ok(())
    }
}
