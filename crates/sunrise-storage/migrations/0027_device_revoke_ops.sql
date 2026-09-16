-- 0027: keep every `device_revoke` op, so the register can be a fold rather
-- than a running upsert.
--
-- Why the register alone is not enough
-- ------------------------------------
--
-- `device_revocations` (0017) is an LWW register keyed on the revoked id, and
-- an upsert keeps only the winner. That is sufficient while every member's
-- revocations count equally, which is what the engine assumed: the arm applying
-- a `device_revoke` refused exactly one thing, an op naming its own sender.
--
-- So a device the account had already revoked could still revoke every *other*
-- device in it. Its cert is still on the chain, it still holds the vault-meta
-- key for the epoch it was cut at, and peers keep old epoch keys — so the op
-- verifies, decrypts and applies on every replica. And revocation has no
-- inverse: nothing in the tree deletes a `device_revocations` row, and
-- `Engine::is_revoked` is presence and nothing else, so the result is permanent
-- on every replica. An expelled laptop could expel the account
-- (issue #82).
--
-- Refusing such an op where it arrives does not work, because it makes the
-- answer depend on delivery order: a replica that applied the op before it
-- learned its sender was revoked would keep the row, one that met them the
-- other way round would not, and the two would disagree forever. That is the
-- divergence ADR-0034 exists to keep out, and its corollary 3 says peer-side
-- enforcement must not reintroduce it.
--
-- What this table makes possible
-- ------------------------------
--
-- With every `device_revoke` op kept, the register stops being a running
-- upsert and becomes a **fold** over this table in one canonical order —
-- `(op_hlc_ms, op_hlc_logical, sender)`, the same total order the register's
-- own LWW comparator uses. A row whose sender is already revoked by the prefix
-- of that order is stored and skipped; everything else lands. The result is a
-- pure function of the op set, so two replicas holding the same ops hold the
-- same register whatever order the ops arrived in, and the fold is recomputed
-- from here each time one lands.
--
-- It is also what makes the skip recoverable. A cut is an LWW register that
-- moves in both directions on purpose, and when it moves the fold runs again
-- over rows that were never discarded — so a revocation skipped under one cut
-- is folded under a corrected one, with nothing to re-request and no
-- dependence on relay retention. That is the question
-- [#82](https://github.com/justin13888/Sunrise/issues/82) defers, answered by
-- keeping the op rather than by deciding what to do once it is gone.
--
-- The order does not need the op id, and the primary key says why: an HLC is
-- monotonic per device, so one sender cannot stamp two ops with one reading.
-- `sender` breaks the remaining tie between two devices at the same instant,
-- exactly as `revoked_by` does in `device_revocations`.
CREATE TABLE device_revoke_ops (
    op_hlc_ms          INTEGER NOT NULL,
    op_hlc_logical     INTEGER NOT NULL,
    sender             BLOB NOT NULL,
    revoked_device_id  BLOB NOT NULL,
    reason             TEXT NOT NULL,
    recorded_at_ms     INTEGER NOT NULL,
    PRIMARY KEY (op_hlc_ms, op_hlc_logical, sender)
);

-- Seed from the register, so an upgraded vault folds to what it already holds.
--
-- The ops that lost an LWW contest before this migration are not recoverable —
-- they were never written down, which is the defect — so the ledger starts as
-- the surviving register and grows from there. That is the conservative
-- direction: the fold's input is a subset of the true op set, so it can only
-- fail to skip a revocation it has no record of, never invent one.
--
-- `revoked_by` is `X''` for a row carried over from pre-0017
-- `devices.revoked_at_ms`, which names no sender. It is copied as-is: it
-- matches no device id, so it is never gated, and inventing one would be
-- claiming a fact this vault does not have.
INSERT INTO device_revoke_ops
    (op_hlc_ms, op_hlc_logical, sender, revoked_device_id, reason, recorded_at_ms)
SELECT cut_ms, cut_logical, revoked_by, device_id, reason, recorded_at_ms
FROM device_revocations;
