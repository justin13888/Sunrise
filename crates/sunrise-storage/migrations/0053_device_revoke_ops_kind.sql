-- 0053: a ledger row is a revocation or a withdrawal (ADR-0056 §2, issue
-- #383).
--
-- Why
-- ---
--
-- A device may withdraw a revocation it made itself. The withdrawal is a
-- `device.revoke_withdraw` op, and the ledger keeps it as a row of the same
-- `(sender, revoked_device_id)` pair as the sender's revocations, so the fold
-- can read each pair's head: the greatest-stamped row, a revocation winning a
-- tie with a withdrawal. A pair whose head is a withdrawal is inert.
--
-- `kind` joins the row's identity. Without it a revocation and a withdrawal
-- from one sender to one target at one stamp would collapse onto one key, and
-- `INSERT OR IGNORE` would keep whichever arrived first, which is an order the
-- fold must not depend on. Migration 0027 makes the same argument for
-- `revoked_device_id`.
--
-- How
-- ---
--
-- `kind` is added with the default `revoke`, which every existing row is: no
-- build before this one wrote a withdrawal. A withdrawal carries no reason, so
-- its `reason` is the empty string, which `RevokeReason::parse` refuses and the
-- fold never reads, because a withdrawal never enters the register. The unique
-- index is rebuilt with the column; the table itself is not, because adding a
-- column with a constant default needs no copy.

ALTER TABLE device_revoke_ops ADD COLUMN kind TEXT NOT NULL DEFAULT 'revoke'
    CHECK (kind IN ('revoke', 'withdraw'));

DROP INDEX device_revoke_ops_identity;

CREATE UNIQUE INDEX device_revoke_ops_identity ON device_revoke_ops (
    op_hlc_ms, op_hlc_logical, sender, revoked_device_id,
    ifnull(authority_identity_id, x''), kind
);
