-- 0017_key_hierarchy.sql — ADR-0024. STORAGE_V = 17.
--
-- Stream keys stop being derived from the vault root and become independently
-- random per (stream_id, epoch), wrapped under the vault root at rest and
-- distributed between devices by HPKE `key_envelope` ops. That needs four
-- things this schema did not have: a persisted account identity, a stream-key
-- table keyed tightly enough to hold two concurrently minted keys for one
-- epoch, somewhere to park an op that arrived before the key that opens it,
-- and revocation columns on `devices`.

-- --- the account identity (ID_S / ID_D), one row ---
-- Independent of every device key: `identity_id` anchors to ID_S_pub, so the
-- device that happened to create the vault is no longer the identity.
CREATE TABLE identity (
    id                  INTEGER PRIMARY KEY CHECK (id = 1),
    identity_id         BLOB NOT NULL,
    id_s_pub            BLOB NOT NULL,
    id_d_pub            BLOB NOT NULL,
    -- nonce(24) || ct(32) || tag(16), AAD
    -- "sunrise.local_identity.identity.v1" || identity_id
    id_s_priv_wrapped   BLOB NOT NULL,
    id_d_priv_wrapped   BLOB NOT NULL,
    created_at_ms       INTEGER NOT NULL
);

-- The device X25519 key was generated at first open and immediately dropped:
-- only its public half survived, inside the cert. `key_envelope` ops seal to
-- exactly that key, so the private half now has to persist.
-- Nullable because a pre-0017 row has no such key; `adopt_legacy_vault` mints
-- one on the next open and re-issues the cert around it.
ALTER TABLE local_identity ADD COLUMN dh_secret_wrapped BLOB;

-- --- per-Stream wrapped key store, re-keyed ---
-- key_id = BLAKE3.derive_key("sunrise.stream_key_id.v1", stream_key, 8).
-- Two devices minting an epoch concurrently produce two rows at the same
-- (stream_id, epoch); both are retained and both are distributed, and the AEAD
-- tag picks the right one at decrypt time. A last-writer-wins on the minting
-- op would instead lose every op sealed under the losing key.
ALTER TABLE stream_keys RENAME TO stream_keys_old;
CREATE TABLE stream_keys (
    stream_id       BLOB NOT NULL,
    epoch           INTEGER NOT NULL,
    key_id          BLOB NOT NULL,
    wrapped         BLOB NOT NULL,
    -- 'local' | 'envelope' | 'pairing' | 'legacy'
    source          TEXT NOT NULL,
    created_at_ms   INTEGER NOT NULL,
    PRIMARY KEY (stream_id, epoch, key_id)
);
CREATE INDEX stream_keys_by_stream ON stream_keys (stream_id, epoch DESC);
-- Pre-0017 rows carry keys *derived* from the vault root, and the wrapped blob
-- is a wrap of exactly that derived key. Nothing is lost by dropping them:
-- `Keychain::open` re-derives every one of them on the next open and
-- re-inserts it with source 'legacy' (adopt_legacy_vault), which is also where
-- the identity that should have wrapped them gets minted.
DROP TABLE stream_keys_old;

-- --- ops that arrived before the key that opens them ---
-- A `key_envelope` op and the ops sealed under the epoch it carries have no
-- ordering guarantee across streams, so an op can legitimately land first.
-- Refusing it would lose it (the relay does not redeliver); a cursor barrier
-- would stall the whole stream. It is parked here and drained after every
-- absorbed key.
CREATE TABLE deferred_ops (
    op_id           BLOB PRIMARY KEY,
    stream_id       BLOB NOT NULL,
    epoch           INTEGER NOT NULL,
    envelope        BLOB NOT NULL,
    received_at_ms  INTEGER NOT NULL
);
CREATE INDEX deferred_ops_by_key ON deferred_ops (stream_id, epoch);

-- --- device identity binding + revocation ---
-- `revoked_at_ms` already exists (0013) and becomes the revocation's
-- effective_at; these say who revoked it and why.
ALTER TABLE devices ADD COLUMN identity_id BLOB;
ALTER TABLE devices ADD COLUMN d_d_pub BLOB;
ALTER TABLE devices ADD COLUMN revoked_by BLOB;
ALTER TABLE devices ADD COLUMN revoke_reason TEXT;
-- The revocation row is an LWW register on the declaring op's own
-- `(hlc, device_id)`: `revoked_at_ms` is that HLC's physical half — which is
-- also the cut — and this is its logical half. Comparing the physical alone
-- would drop the part that orders two ops inside one millisecond, and
-- `revoked_by` breaks the remaining tie, so all three revocation columns follow
-- the winning op rather than converging on the timestamp while the other two
-- stay decided by arrival order.
ALTER TABLE devices ADD COLUMN revoked_hlc_logical INTEGER;

-- --- the Inbox stops sharing the vault-meta stream id ---
-- Pre-0017 the Inbox was sixteen zero bytes, which is also the vault-meta
-- stream id, so an Inbox task and a Stream-lifecycle op shared a stream and a
-- key. Existing rows are re-pointed at the new id so a vault does not appear to
-- lose its Inbox on upgrade. The `streams` row is inserted first because
-- `tasks.stream_id` has a foreign key to it and `PRAGMA foreign_keys` is on.
--
-- The already-signed *envelopes* in `ops` still name the old id and cannot be
-- rewritten: the signature covers the payload. A device paired *after* this
-- upgrade replays that history and is the only other place these rows can be
-- born, so `Engine::remap_legacy_inbox` performs exactly the rewrite below on
-- any entity payload naming `[0u8; 16]` as it materializes. Without it that
-- device would `ensure_stream_row` the vault-meta stream — the one stream that
-- must never have a `streams` row — and the two replicas of one account would
-- disagree about where the user's oldest tasks live.
INSERT OR IGNORE INTO streams
    (stream_id, head_root, last_op_seq, name, created_at_ms, updated_at_ms)
SELECT X'00000073756E726973652E696E626F78',
       X'0000000000000000000000000000000000000000000000000000000000000000',
       0, '', 0, 0
WHERE EXISTS (
    SELECT 1 FROM tasks WHERE stream_id = X'00000000000000000000000000000000'
);
UPDATE tasks SET stream_id = X'00000073756E726973652E696E626F78'
WHERE stream_id = X'00000000000000000000000000000000';

