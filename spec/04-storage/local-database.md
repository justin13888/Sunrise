---
status: accepted
---

# Local Database

SQLite (via SQLCipher) holds:

1. The op log (`ops` table).
2. Materialized state for each domain entity.
3. Sync state.
4. FTS index.
5. Wrapped stream keys.

## Tables (overview)

```sql
-- Op log (see op-log.md for full schema)
CREATE TABLE ops (
    op_id          BLOB PRIMARY KEY,
    stream_id      BLOB NOT NULL,
    device_id      BLOB NOT NULL,
    seq            INTEGER NOT NULL,
    ts_ms          INTEGER NOT NULL,
    envelope       BLOB NOT NULL,        -- the full encrypted envelope
    inner_kind     TEXT NOT NULL,
    target_kind    TEXT NOT NULL,
    target_id      BLOB,
    deps           BLOB,                 -- CBOR-encoded list of op_ids
    applied_at     INTEGER,              -- nullable until applied to state
    UNIQUE (stream_id, device_id, seq)
);

-- Materialized entities (one table per entity kind)
CREATE TABLE tasks (
    id               BLOB PRIMARY KEY,
    stream_id        BLOB NOT NULL,
    title            TEXT NOT NULL,
    state            TEXT NOT NULL,
    priority         INTEGER,
    energy           TEXT,
    estimated_min    INTEGER,
    scheduled_at     INTEGER,
    due_at           INTEGER,
    completed_at     INTEGER,
    deferred_count   INTEGER NOT NULL DEFAULT 0,
    routine_id       BLOB,
    routine_occurrence INTEGER,
    archived         INTEGER NOT NULL DEFAULT 0,
    deleted          INTEGER NOT NULL DEFAULT 0,
    body             BLOB,                 -- CRDT doc snapshot
    extra            BLOB,                 -- forward-compat unknown fields
    -- Snapshot of HEAD root for tamper detection
    head_root        BLOB
);

CREATE TABLE streams ( /* … */ );
CREATE TABLE contexts ( /* … */ );
CREATE TABLE routines ( /* … */ );
CREATE TABLE blocks ( /* … */ );
CREATE TABLE attachments ( /* … */ );
CREATE TABLE persons ( /* … */ );

-- Many-to-many task ↔ context
CREATE TABLE task_contexts (
    task_id    BLOB NOT NULL,
    context_id BLOB NOT NULL,
    added_at   INTEGER NOT NULL,
    PRIMARY KEY (task_id, context_id)
);

-- Sync state
CREATE TABLE sync_cursors (
    stream_id  BLOB NOT NULL,
    device_id  BLOB NOT NULL,
    last_seq   INTEGER NOT NULL,
    PRIMARY KEY (stream_id, device_id)
);

CREATE TABLE outbox (
    op_id        BLOB PRIMARY KEY,
    queued_at    INTEGER NOT NULL,
    attempts     INTEGER NOT NULL DEFAULT 0,
    next_retry_at INTEGER
);

-- Wrapped stream keys
CREATE TABLE stream_keys (
    stream_id    BLOB PRIMARY KEY,
    epoch        INTEGER NOT NULL,
    wrapped      BLOB NOT NULL,        -- AEAD(vault_root, stream_key)
    created_at   INTEGER NOT NULL
);

-- FTS5
CREATE VIRTUAL TABLE search_idx USING fts5(
    kind, id UNINDEXED, stream_id UNINDEXED,
    title, body, contexts,
    tokenize = 'unicode61 remove_diacritics 2 porter'
);
```

## Indexes

```sql
CREATE INDEX idx_tasks_stream ON tasks(stream_id) WHERE deleted = 0 AND archived = 0;
CREATE INDEX idx_tasks_today ON tasks(scheduled_at) WHERE deleted = 0 AND state IN ('todo','in_progress','blocked');
CREATE INDEX idx_tasks_due   ON tasks(due_at)       WHERE deleted = 0 AND state IN ('todo','in_progress','blocked');
CREATE INDEX idx_tasks_routine ON tasks(routine_id, routine_occurrence) WHERE routine_id IS NOT NULL;

CREATE INDEX idx_blocks_time ON blocks(starts_at, ends_at) WHERE deleted = 0;
CREATE INDEX idx_ops_stream_device ON ops(stream_id, device_id, seq);
CREATE INDEX idx_outbox_retry ON outbox(next_retry_at) WHERE next_retry_at IS NOT NULL;
```

## Materialization

Applying an op runs the op's mutation against the materialized table(s) **inside the same SQL transaction** that inserts the op into `ops`. This keeps op log and state mutually consistent at every commit point.

If an op cannot be applied (e.g. dep not yet present), it is inserted with `applied_at = NULL` and processed later when its deps arrive. A reconciliation pass runs at sync end.

## SQLCipher configuration

For v1 we ship the SQLCipher v4 default cipher suite to leverage the well-tested upstream:

- Cipher: AES-256-CBC with HMAC-SHA-512 page MAC (SQLCipher v4 default).
- KDF: PBKDF2-HMAC-SHA-512, 256 000 iterations.
- Page MAC algorithm: HMAC-SHA-512.
- Page size: 4096 bytes.

The SQLCipher key is derived from `vault_root` (see [`../03-crypto/identity-and-device-keys.md`](../03-crypto/identity-and-device-keys.md)):

```
sqlcipher_raw_key = BLAKE3.derive_key(
    context = "sunrise.sqlcipher_key.v1",
    key_material = vault_root
)   // 32 bytes; passed to SQLCipher as a 64-char hex blob via PRAGMA key
```

Using a pre-derived key bypasses SQLCipher's internal PBKDF2 (we already gated entry through Argon2id at unlock); we set `PRAGMA cipher_kdf_algorithm = PBKDF2_HMAC_SHA512` and `PRAGMA kdf_iter = 1` since the input is already a uniformly random 32-byte key.

The choice of AES here is a deliberate divergence from XChaCha20-Poly1305 used elsewhere; rationale:

- SQLCipher's AES-CBC + HMAC mode is shipped as a single audited library on every supported platform (including iOS, Android, WASM via `wa-sqlite`).
- Page-level encryption inside SQLite has its own design (per-page IVs, MAC over `(page_no, ciphertext)`). Replacing the cipher would mean shipping a custom SQLCipher fork, which is outside our maintenance budget.
- The vault DB at rest is protected by the OS keystore and the application unlock; the cipher choice here is defense-in-depth, not the primary trust boundary.

## Backups

The vault DB is suitable for binary backup (file copy while not actively writing). Encrypted at rest, so backups carry no plaintext. UI offers "create backup" that performs a checkpoint and copies the DB to a user-chosen location.

## Vacuum / maintenance

- `PRAGMA auto_vacuum = INCREMENTAL` set at vault creation.
- Background incremental vacuum runs after compaction (see [`compaction.md`](./compaction.md)).
- WAL checkpointing is automatic.
