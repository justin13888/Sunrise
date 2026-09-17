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
-- `(op_hlc_ms, op_hlc_logical, sender, revoked_device_id)`, the register's own
-- LWW comparator extended by the one column that makes it total. The order
-- decides the register; what decides the *gate* is a set built from the whole
-- table, because the sort key is the sender's own to choose. A row whose
-- sender this table revokes anywhere is stored and skipped; everything else
-- lands. The result is a
-- pure function of the op set, so two replicas holding the same ops hold the
-- same register whatever order the ops arrived in, and the fold is recomputed
-- from here each time one lands.
--
-- Keeping the op is also what bounds the skip's cost, though it is worth
-- being exact about what it does and does not buy. A skipped row is never
-- discarded, so there is nothing to re-request and no dependence on relay
-- retention — but nor is it un-skipped by correcting a cut. **The gate reads
-- no cut**: it is a set question about who has revoked whom, and the HLC
-- decides only which row wins the register. A correction appends another
-- revocation of the same sender by the same party, so the gate answers the
-- same way and the skipped row stays skipped
-- (`a_cut_correction_does_not_re_fold_a_skipped_revocation`). What un-skips it
-- is somebody revoking that sender's revoker, which its sender cannot author.
-- So of the two options
-- [#82](https://github.com/justin13888/Sunrise/issues/82) defers between —
-- re-request the op, or accept the loss and say so — this table takes the
-- second: the op is kept, the *effect* is lost, and the remedy is to revoke
-- again from a device the account still trusts. ADR-0041 §"What a user sees"
-- item 3 records it.
--
-- The order does not need the op id, and the primary key says why: these four
-- columns are the op's whole identity **for this fold**. The fold reads the
-- stamp, the sender and the target and nothing else, so two ops agreeing on
-- all four fold identically whatever op ids carried them — an op id in the key
-- would separate rows the fold cannot tell apart, and add an ordering column
-- that never decides anything.
--
-- `revoked_device_id` is in the key because the *sender* chooses its own HLC.
-- It is tempting to argue that an HLC is monotonic per device and so one
-- sender cannot stamp two ops with one reading, but that is a property of this
-- vault's own `MonotonicHlc` and not of a peer: nothing in the envelope ties a
-- remote op's stamp to that sender's `seq`, to its meta epoch, or to any
-- earlier stamp it sent. Two `device_revoke` ops from one member at one HLC
-- naming two different targets are two ops, they both reach
-- `apply_device_revoke`, and they must be two rows. Collapsed onto one key the
-- live path's `INSERT OR IGNORE` would keep whichever arrived first, so two
-- replicas that received them the other way round would fold different
-- registers and never reconcile — the divergence ADR-0034 corollary 3 forbids
-- and this table exists to avoid.
--
-- `sender` breaks the tie between two devices at the same instant, exactly as
-- `revoked_by` does in `device_revocations`; `revoked_device_id` breaks the
-- last one, between two targets from one sender at one instant.
CREATE TABLE device_revoke_ops (
    op_hlc_ms          INTEGER NOT NULL,
    op_hlc_logical     INTEGER NOT NULL,
    sender             BLOB NOT NULL,
    revoked_device_id  BLOB NOT NULL,
    reason             TEXT NOT NULL,
    recorded_at_ms     INTEGER NOT NULL,
    PRIMARY KEY (op_hlc_ms, op_hlc_logical, sender, revoked_device_id)
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
--
-- A bare `INSERT` and not `INSERT OR IGNORE`, which the live path uses for a
-- reason that does not apply here. `device_revocations` is keyed on
-- `device_id`, so every row it holds carries a distinct `revoked_device_id`
-- and the key above makes the seeded rows distinct by construction — two of
-- them sharing `(cut_ms, cut_logical, revoked_by)` is ordinary and no longer a
-- collision. 0017's own backfill produces exactly that shape, giving several
-- devices `revoked_at_ms` at logical 0 under an empty `revoked_by`. There is
-- nothing left for a no-op to swallow, and `docs/04-storage/migrations.md`
-- §"Who provides the no-op" is why one is not written anyway: a migration that
-- runs once states what it means and fails loudly if the vault disagrees.
INSERT INTO device_revoke_ops
    (op_hlc_ms, op_hlc_logical, sender, revoked_device_id, reason, recorded_at_ms)
SELECT cut_ms, cut_logical, revoked_by, device_id, reason, recorded_at_ms
FROM device_revocations;
