-- 0022: the identity chain -- every account identity this vault has had, and
-- the one it started as.
--
-- ADR-0032 and `docs/03-crypto/key-rotation.md` §Identity rotation. Until now
-- the account identity was a single row that nothing could replace: `identity`
-- holds exactly one `identity_id` and every device cert, every `key_envelope`
-- recipient check and every op signature verifies against it. That is the
-- unclosed half of issue #105 -- a revoked device still holds `ID_S_priv`, so
-- it can mint a fresh device id and certify itself back in, and no revocation
-- reaches that. Only a new identity does.
--
-- Rotating it makes the account's trust root a *sequence* rather than a value,
-- and a sequence needs two things this schema had neither of: somewhere to put
-- the transitions, and a fixed point to read them from.

-- --- the transitions, append-only ---
-- One row per `identity_transition` op absorbed, and never an update: a
-- transition is a fact about a moment, and rewriting one would re-attribute
-- ops already signed under the identity it names.
--
-- Keyed on `to_identity_id` rather than on an op id, which makes re-delivery
-- idempotent for free: the successor id is `identity_id_from_pub(ID_S_pub)` of
-- an independently random key, so two rows can carry the same one only by
-- being the same transition. An op id would have keyed the same fact twice
-- under two names and left the fold to notice.
--
-- `from_identity_id` is NOT unique, deliberately. Two devices can rotate the
-- same identity concurrently -- neither has seen the other's op -- and both
-- transitions are real and both are retained, exactly as two concurrently
-- minted stream keys are. The fold picks the winner by
-- `(hlc_physical_ms, hlc_logical, emitter_device_id)`, which is the same
-- total order `device_revocations` uses, so the losing branch is a branch the
-- account did not take rather than a row that had to be deleted.
--
-- The HLC halves are columns rather than a decoded envelope field because the
-- fold has to ORDER BY them, and the envelope is sealed: a fold that had to
-- open every op to sort them could not run before the keys are loaded.
--
-- `payload` is the whole CBOR `IdentityTransitionPayload`. The four public
-- columns beside it are a denormalization of it and are what the fold reads;
-- the blob is kept because the roster and the shares are not in any column --
-- they are what a device absorbs to re-verify the digests the signatures
-- cover, and what a device that was offline during the rotation opens later to
-- obtain the successor secrets. Dropping it would make the signatures
-- unverifiable after the fact, which is the one thing this table exists to
-- keep true.
--
-- `meta_epoch` is the vault-meta Stream epoch the op was sealed under. It says
-- which key opens `payload`, and it orders a transition against the key
-- rotations around it without reference to any clock.
CREATE TABLE identity_transitions (
    to_identity_id    BLOB PRIMARY KEY,
    from_identity_id  BLOB NOT NULL,
    to_id_s_pub       BLOB NOT NULL,
    to_id_d_pub       BLOB NOT NULL,
    meta_epoch        INTEGER NOT NULL,
    hlc_physical_ms   INTEGER NOT NULL,
    hlc_logical       INTEGER NOT NULL,
    emitter_device_id BLOB NOT NULL,
    payload           BLOB NOT NULL,
    prev_sig          BLOB NOT NULL,
    next_sig          BLOB NOT NULL
) WITHOUT ROWID;

-- The fold's only access path: "what succeeded this identity?", asked once per
-- link from the genesis forward, with the tie-break already in the index so a
-- forked chain is resolved by a seek rather than by sorting the table.
CREATE INDEX identity_transitions_by_predecessor
    ON identity_transitions (from_identity_id, hlc_physical_ms, hlc_logical, emitter_device_id);

-- --- the fixed point ---
-- The identity this account *started* as, which no rotation changes.
--
-- `identity.identity_id` is the identity in force, so it moves on every
-- transition -- and a chain read from a moving anchor is not a chain: a
-- replica that had absorbed two of three transitions would fold from its own
-- second identity and conclude the third was a stranger's, while a replica
-- that had absorbed all three would fold from the first and agree with nobody.
-- The genesis is the one value every replica of an account shares no matter
-- how far along it is, so it is what the fold starts from and what a peer
-- names when it asks who this account is.
--
-- It is also the answer to a question rotation otherwise makes unanswerable:
-- an op signed under a retired identity is still that account's op, and the
-- only way to say so is to walk from a point both parties can name.
ALTER TABLE identity ADD COLUMN genesis_identity_id BLOB;

-- Nothing is deployed, so there is no vault that has rotated; every existing
-- `identity` row is therefore its own genesis by construction. Backfilled
-- rather than left NULL-and-coalesced, because "NULL means the current id"
-- is a rule every reader would have to know and one of them would not.
UPDATE identity SET genesis_identity_id = identity_id;
