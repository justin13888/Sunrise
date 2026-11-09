-- 0048: a revocation can carry the account identity's signature (ADR-0058 §3,
-- issue #454).
--
-- Why
-- ---
--
-- An authority revocation is a `device.revoke_by_identity` op: a revocation
-- signed by the `ID_S_priv` of the identity in force. The fold verifies the
-- signature, not the apply, because a replica can hold an authority revocation
-- signed under an identity whose transition has not reached it yet. So the
-- ledger keeps what the fold needs to verify it later: the identity the op
-- names and its signature. Both are NULL on an ordinary row.
--
-- `authority_identity_id` joins the row's identity. Without it the pair
-- compaction (#250) would keep only the greater-stamped of an ordinary row and
-- an authority row from one sender to one target, and could delete the
-- authority row; and `INSERT OR IGNORE` would keep whichever of two such rows
-- at one stamp arrived first, which is an order the fold must not depend on.
--
-- `device_revocations.by_authority` is the register saying an authority row
-- names the device at this fold, so the device list can tell a revocation no
-- later op can unwind from one that can. 0 for every existing row: no build
-- before this one wrote an authority row.
--
-- How
-- ---
--
-- SQLite cannot change a primary key in place, so `device_revoke_ops` is
-- rebuilt and every row copied with both new columns NULL.
--
-- The identity is a UNIQUE index rather than a PRIMARY KEY, because a NULL in
-- a rowid table's composite PRIMARY KEY is allowed and never equal to another
-- NULL, so two copies of one ordinary op would both be kept. The index reads
-- the column through `ifnull(…, x'')`, which every ordinary row shares and no
-- identity id (16 bytes) is.

CREATE TABLE device_revoke_ops_0048 (
    op_hlc_ms              INTEGER NOT NULL,
    op_hlc_logical         INTEGER NOT NULL,
    sender                 BLOB NOT NULL,
    revoked_device_id      BLOB NOT NULL,
    reason                 TEXT NOT NULL,
    recorded_at_ms         INTEGER NOT NULL,
    authority_identity_id  BLOB,
    authority_sig          BLOB
);

INSERT INTO device_revoke_ops_0048
    (op_hlc_ms, op_hlc_logical, sender, revoked_device_id, reason, recorded_at_ms)
SELECT op_hlc_ms, op_hlc_logical, sender, revoked_device_id, reason, recorded_at_ms
FROM device_revoke_ops;

DROP TABLE device_revoke_ops;
ALTER TABLE device_revoke_ops_0048 RENAME TO device_revoke_ops;

CREATE UNIQUE INDEX device_revoke_ops_identity ON device_revoke_ops (
    op_hlc_ms, op_hlc_logical, sender, revoked_device_id,
    ifnull(authority_identity_id, x'')
);

ALTER TABLE device_revocations ADD COLUMN by_authority INTEGER NOT NULL DEFAULT 0;
