-- Sunrise local storage schema, version 5: sync-local groundwork.
--
-- Adds the persistent device identity, the outbox, and per-(stream, device)
-- sync cursors. This is the local half of the sync machinery: every op in the
-- `ops` table is now a real signed+encrypted OpEnvelope, and the outbox tracks
-- which ops still need to be pushed to peers.
--
-- PRE-RELEASE MIGRATION NOTE: dev vaults created under STORAGE_V < 5 hold
-- PLAINTEXT inner-op CBOR in `ops.envelope` (the pre-sealing format). Those
-- rows are NOT converted to sealed OpEnvelopes by this migration — there is no
-- device identity to sign them with retroactively. Sunrise is pre-release:
-- WIPE dev vaults before upgrading to STORAGE_V = 5. New vaults seal from the
-- first op.

-- The single local device identity (id is pinned to 1). `signing_secret_wrapped`
-- is the device Ed25519 signing seed sealed under the vault root with
-- XChaCha20-Poly1305 (AAD = "sunrise.local_identity.v1" || device_id). `cert_blob`
-- is the self-issued DeviceCert (canonical CBOR).
CREATE TABLE local_identity (
    id                      INTEGER PRIMARY KEY CHECK (id = 1),
    device_id               BLOB NOT NULL,
    signing_secret_wrapped  BLOB NOT NULL,
    cert_blob               BLOB NOT NULL,
    created_at_ms           INTEGER NOT NULL
);

-- Ops awaiting push to peers. `acked_at_ms IS NULL` = still pending.
CREATE TABLE outbox (
    op_id           BLOB PRIMARY KEY REFERENCES ops (op_id),
    stream_id       BLOB NOT NULL,
    enqueued_at_ms  INTEGER NOT NULL,
    acked_at_ms     INTEGER
);
CREATE INDEX outbox_unacked ON outbox (enqueued_at_ms) WHERE acked_at_ms IS NULL;

-- Highest applied seq per (stream_id, device_id), for gap detection and pull
-- resumption.
CREATE TABLE sync_cursors (
    stream_id         BLOB NOT NULL,
    device_id         BLOB NOT NULL,
    last_applied_seq  INTEGER NOT NULL,
    PRIMARY KEY (stream_id, device_id)
);
