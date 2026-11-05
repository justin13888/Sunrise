//! The revocation ledger's bounds: the pair compaction, the per-sender cap,
//! and the release of a read bound only the cap left behind.
//!
//! Split out of [`super::revocation`] because the three share one table,
//! `device_revoke_ops`, and one reason to change: how much of the ledger a
//! replica keeps. None of them decides what the register says; the fold in
//! [`Engine::refold_device_revocations`] does, and each function here states
//! why it leaves that fold unchanged or, for the cap, how it changes it.

use super::Engine;
use rusqlite::{params, OptionalExtension, Transaction};

impl Engine {
    /// Delete every ledger row the fold can never read the value of: all but
    /// the greatest-stamped row of each `(sender, revoked_device_id)` pair.
    ///
    /// # Why this changes no fold
    ///
    /// [`Self::refold_device_revocations`] reads a row in two places, and
    /// neither can tell a pair's lesser rows from its greatest:
    ///
    /// - **The gate** is built from `(sender, revoked)` pairs and nothing else
    ///   — `revokers_all`, the discount, and the walk's condition. Deleting a
    ///   row whose pair survives in another row leaves every one of them
    ///   unchanged, and so leaves every row gated or ungated as it was.
    /// - **The register** takes, per revoked device, the greatest ungated row in
    ///   the canonical order. Whether a row is gated depends only on its pair,
    ///   so a pair's rows are gated or ungated together; and within one pair
    ///   `sender` and `revoked_device_id` are equal, so the canonical order is
    ///   the HLC alone. If any row of a pair could win, the pair's greatest row
    ///   sorts after it and wins instead. A lesser row never wins.
    ///
    /// So the fold is the same function of the compacted ledger as of the full
    /// one, every register it produces is the same register, and
    /// `device_read_bounds` — which ratchets over those registers — takes the
    /// same rows it would have. That is the property ADR-0034 corollary 3 and
    /// ADR-0041 need from the ledger, and it is why this can delete from a
    /// table whose whole design is keeping ops: it deletes only rows the fold
    /// provably ignores.
    ///
    /// The compacted ledger is also itself a function of the op set rather
    /// than of arrival order: whichever order the ops came in, what remains is
    /// each pair's greatest stamp. A lesser op that arrives after its pair's
    /// greatest is stored, folded, reported on if gated, and removed here in
    /// the same transaction.
    ///
    /// # What it bounds, and what it does not
    ///
    /// The ledger holds at most one row per distinct `(sender, target)` pair.
    /// Repeating a revocation — a cut correction, a re-sent op with a new
    /// stamp, or a revoked device naming the same target over and over — no
    /// longer grows it. What it does not bound is the number of distinct
    /// targets an authenticated sender names: `apply_device_revoke` stores a
    /// row for an id no device on the account has, deliberately, because a
    /// cert can arrive after the revocation of its device. That is
    /// [`Self::cap_device_revoke_targets`]' bound, and unlike this one it
    /// does change a fold, which is why it runs before the fold and not here.
    ///
    /// Global rather than scoped to the pair the caller just wrote, so a vault
    /// that accumulated lesser rows before this existed is compacted by its
    /// next `device_revoke`, and one pass costs the same order as the fold's
    /// own sorted read.
    ///
    /// # The pair includes the identity an authority row names
    ///
    /// `(sender, revoked_device_id, authority_identity_id)`, since migration
    /// 0048. An ordinary row and an authority row from one sender to one
    /// target are folded differently (ADR-0058 §4), so neither can stand in
    /// for the other, and without the third column this would keep only the
    /// greater-stamped and could delete the authority row. Rows naming two
    /// identities differ the same way: at most one of them names the head.
    /// Within the widened pair everything above holds, with one exception
    /// the sender alone can reach. Its rows name one identity, so a moved head
    /// demotes them together; but each carries its own signature, and a pair
    /// whose greatest row does not verify would lose a lesser row that does.
    /// Only the pair's own sender can write either row, because the envelope
    /// authenticates it, so this costs a sender nothing but its own claims,
    /// which [`Self::cap_device_revoke_targets`] already lets it drop.
    /// `NULL`s fall in one partition, as SQL window partitions group them.
    pub(super) fn compact_device_revoke_ops(tx: &Transaction<'_>) -> rusqlite::Result<usize> {
        tx.execute(
            "DELETE FROM device_revoke_ops WHERE rowid IN (
                 SELECT rowid FROM (
                     SELECT rowid, ROW_NUMBER() OVER (
                         PARTITION BY sender, revoked_device_id, authority_identity_id
                         ORDER BY op_hlc_ms DESC, op_hlc_logical DESC
                     ) AS pos
                     FROM device_revoke_ops
                 ) WHERE pos > 1
             )",
            [],
        )
    }

    /// Keep at most [`REVOKE_TARGETS_PER_SENDER`] distinct targets per sender,
    /// and say whether the pair `(sender, target)` just written was dropped.
    ///
    /// # Why the ledger needs a cap at all
    ///
    /// [`Self::compact_device_revoke_ops`] bounds the rows per pair, not the
    /// pairs. `apply_device_revoke` has to store a row for an id no device on
    /// this account has, because a cert can arrive after the revocation of its
    /// device, so an authenticated sender naming ids it made up grew the
    /// ledger, the register, and `device_read_bounds` by one row per id, and
    /// every later fold by the same. A revoked device could do it too: its ops
    /// are stored whatever its standing. The relay cannot bound it: envelopes
    /// are opaque to it, and with `require_device_sig = false` it does not
    /// know the sender device either
    /// ([#315](https://github.com/justin13888/Sunrise/issues/315)).
    ///
    /// # What it keeps, and why that is still a function of the op set
    ///
    /// Per sender, the pairs whose greatest row sorts highest in the fold's
    /// canonical order, `(op_hlc_ms, op_hlc_logical, revoked_device_id)`
    /// descending, which is total within one sender because a pair is one
    /// sender and one target. Whole pairs are kept or dropped, so this commutes
    /// with the pair compaction. And it merges: a pair dropped here ranked
    /// below the sender's K-th pair, that K-th key only rises as rows arrive,
    /// and a later row of the dropped pair is ranked on its own stamp, which
    /// is either its pair's true greatest or lower than the stamp that already
    /// lost. So whatever order the ops arrive in, the ledger ends as the same
    /// rows, and the fold over it, which runs after this, is the same fold.
    ///
    /// # Whom it costs: only the sender over the cap
    ///
    /// It drops a sender's **own** claims and nobody else's. None of those
    /// claims can ungate that sender, because the gate on a sender is built
    /// from rows revoking it, and the discount of one of its revokers needs a
    /// row whose sender is not that sender; so a device cannot use the cap to
    /// escape [`Self::refold_device_revocations`]' gate. What it can do is
    /// withdraw its own earlier revocations by naming
    /// [`REVOKE_TARGETS_PER_SENDER`] new ids. Those revocations are already
    /// hostage to that device's standing, since the moment it is revoked they
    /// unwind anyway, and a device the account still trusts holds keys to
    /// everything. The remedy is this family's usual one: revoke again from a
    /// device the account still trusts. [`Self::revoke_device`] refuses a
    /// target with no row in `devices`, so an honest device reaches the cap
    /// only by revoking that many devices that really paired.
    ///
    /// # A pair includes the identity an authority row names
    ///
    /// Since migration 0048 the pair is `(sender, target, identity)`, as in
    /// [`Self::compact_device_revoke_ops`], and an authority row counts toward
    /// its sender's cap like any row (ADR-0058 §3). The rank breaks a tie on
    /// the identity last, so it stays total within one sender.
    pub(super) fn cap_device_revoke_targets(
        tx: &Transaction<'_>,
        sender: &[u8; 16],
        target: &[u8; 16],
        authority_identity_id: Option<&[u8]>,
    ) -> rusqlite::Result<bool> {
        tx.execute(
            "WITH heads AS (
                 SELECT sender, revoked_device_id, authority_identity_id,
                        op_hlc_ms, op_hlc_logical
                 FROM (
                     SELECT sender, revoked_device_id, authority_identity_id,
                            op_hlc_ms, op_hlc_logical,
                            ROW_NUMBER() OVER (
                                PARTITION BY sender, revoked_device_id,
                                             authority_identity_id
                                ORDER BY op_hlc_ms DESC, op_hlc_logical DESC
                            ) AS pos
                     FROM device_revoke_ops
                 ) WHERE pos = 1
             ),
             ranked AS (
                 SELECT sender, revoked_device_id, authority_identity_id,
                        ROW_NUMBER() OVER (
                            PARTITION BY sender
                            ORDER BY op_hlc_ms DESC, op_hlc_logical DESC,
                                     revoked_device_id DESC,
                                     authority_identity_id DESC
                        ) AS pos
                 FROM heads
             )
             DELETE FROM device_revoke_ops
             WHERE EXISTS (
                 SELECT 1 FROM ranked r
                 WHERE r.pos > ?1
                   AND r.sender = device_revoke_ops.sender
                   AND r.revoked_device_id = device_revoke_ops.revoked_device_id
                   AND r.authority_identity_id IS device_revoke_ops.authority_identity_id
             )",
            params![REVOKE_TARGETS_PER_SENDER],
        )?;
        let kept: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM device_revoke_ops
                 WHERE sender = ?1 AND revoked_device_id = ?2
                   AND authority_identity_id IS ?3
                 LIMIT 1",
                params![&sender[..], &target[..], authority_identity_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(kept.is_none())
    }

    /// Delete each `device_read_bounds` row whose device this replica holds no
    /// cert for and no ledger row names.
    ///
    /// The fold bounds every device its register holds, so without this the
    /// cap in [`Self::cap_device_revoke_targets`] bounded the ledger and the
    /// register and left the read bound growing by one row per made-up id: a
    /// sender over the cap names a new one, the fold lands it, and the next
    /// op evicts it from the ledger while its bound stays.
    ///
    /// **Why this does not release a bound that bounds anything.** The four
    /// key-distribution sites seal only to devices in `devices`, and nothing
    /// deletes from `devices`, so a device this replica holds a cert for is
    /// never touched here: an unwind still cannot readmit it. A row goes only
    /// when its id is certless *and* named by no ledger row, and since the
    /// pair compaction keeps every pair, the only thing that takes the last row
    /// naming an id is the per-sender cap. So what this releases is a bound on
    /// an id that only a sender over the cap ever revoked, and that no device
    /// here has. If its cert arrives later, the id is current, which is what
    /// the capped ledger says of it, and a replica that held the cert first
    /// keeps its bound. That is the non-convergence the bound already has
    /// ([#411](https://github.com/justin13888/Sunrise/issues/411)), reached
    /// here only through a sender that named more ids than the cap.
    ///
    /// **A bound adopted from a sponsor is not released here while it stays
    /// marked** (`from_sponsor = 1`, migration 0030). The fold clears the mark
    /// when it bounds the same device itself, and from then on the row is the
    /// fold's own and meets this function's premise. That premise is that
    /// the fold wrote the bound from a ledger row, so a certless id no row
    /// names is one only the cap could have emptied. An adopted bound had no
    /// ledger row here to begin with: the joiner writes it before it holds any
    /// cert but its own, and releasing it at the joiner's first
    /// `device_revoke` would readmit the device the moment its cert arrived.
    /// See [`super::revocation::adopt_sponsor_read_bounds`].
    pub(super) fn release_orphan_read_bounds(tx: &Transaction<'_>) -> rusqlite::Result<usize> {
        tx.execute(
            "DELETE FROM device_read_bounds
             WHERE from_sponsor = 0
               AND NOT EXISTS (
                       SELECT 1 FROM devices d
                       WHERE d.device_id = device_read_bounds.device_id
                   )
               AND NOT EXISTS (
                       SELECT 1 FROM device_revoke_ops o
                       WHERE o.revoked_device_id = device_read_bounds.device_id
                   )",
            [],
        )
    }
}

/// The most distinct targets one sender's `device_revoke` ops keep in the
/// ledger. See [`Engine::cap_device_revoke_targets`].
///
/// Far above what an honest device reaches: [`Engine::revoke_device`] refuses
/// a target this replica holds no cert for, so an honest device gets here only
/// by revoking this many devices that really paired with the account. The
/// ledger is then at most this times the number of senders, and so is each
/// fold's work.
pub(super) const REVOKE_TARGETS_PER_SENDER: i64 = 256;
