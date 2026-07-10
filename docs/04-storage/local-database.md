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

## SQLite configuration and transaction isolation

SQLite (via SQLCipher) is opened in WAL mode with:

```
PRAGMA journal_mode   = WAL;
PRAGMA synchronous    = NORMAL;
PRAGMA foreign_keys   = ON;
PRAGMA busy_timeout   = 5000;
PRAGMA auto_vacuum    = INCREMENTAL;
```

All multi-row writes are wrapped in a single `BEGIN IMMEDIATE … COMMIT`. Reads outside transactions see snapshots at the time of statement start (SQLite default). The op-application loop holds `BEGIN IMMEDIATE` for the entire batch; concurrent reads continue to see the pre-batch snapshot until commit.

## Tables

### Op log

```sql
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
    applied_at     INTEGER,              -- nullable until applied to state
    UNIQUE (stream_id, device_id, seq)
);
```

`deps` is **not** a column on `ops`; it is materialized into a separate join table for indexed lookup:

```sql
CREATE TABLE op_dep (
    op_id   BLOB NOT NULL,
    dep_id  BLOB NOT NULL,
    PRIMARY KEY (op_id, dep_id)
);
CREATE INDEX op_dep_dep_idx ON op_dep (dep_id);
```

Inserting an op also inserts its dep rows in the same transaction. The "find ops whose deps just became satisfied" query is:

```sql
SELECT op_id FROM op_dep
 WHERE dep_id = ?
   AND NOT EXISTS (
       SELECT 1 FROM op_dep d2
            LEFT JOIN ops o ON o.op_id = d2.dep_id
        WHERE d2.op_id = op_dep.op_id
          AND o.applied_at IS NULL
   );
```

### Stream (materialized)

```sql
CREATE TABLE stream (
    stream_id      BLOB PRIMARY KEY,           -- 16 bytes
    doc_blob       BLOB NOT NULL,              -- Loro snapshot bytes
    doc_blob_v     INTEGER NOT NULL,           -- Loro encoding version
    head_root      BLOB NOT NULL,              -- 32 bytes (BLAKE3)
    last_op_seq    INTEGER NOT NULL,
    parent_id      BLOB REFERENCES stream(stream_id),
    deleted        INTEGER NOT NULL DEFAULT 0,
    created_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL
);
CREATE INDEX stream_parent_idx  ON stream(parent_id);
CREATE INDEX stream_deleted_idx ON stream(deleted, updated_at_ms);
```

`head_root` is the **per-Stream Merkle root** as defined in [`../03-crypto/audit-and-tamper-evidence.md`](../03-crypto/audit-and-tamper-evidence.md). It is recomputed on every op apply and stored on the `stream` row. On startup, the value is verified against a fresh recomputation over the last 1 000 ops; mismatch raises `STORAGE_TAMPER_DETECTED` and the user is prompted to wipe + resync.

### Context

```sql
CREATE TABLE context (
    context_id     BLOB PRIMARY KEY,           -- 16 bytes
    name           TEXT NOT NULL,              -- plaintext only in this local row; encrypted in op-log
    active         INTEGER NOT NULL DEFAULT 1,
    policy_blob    BLOB NOT NULL,              -- CBOR; see 02-domain/contexts-and-tags
    created_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL
);
```

### Routine

```sql
CREATE TABLE routine (
    routine_id     BLOB PRIMARY KEY,           -- 16 bytes
    stream_id      BLOB NOT NULL REFERENCES stream(stream_id),
    rrule_blob     BLOB NOT NULL,              -- CBOR-encoded RRULE subset
    config_blob    BLOB NOT NULL,              -- CBOR; horizon, catchup, streak rules
    scheduling_constraints BLOB,               -- canonical CBOR, NULL = empty; projection (arrives in migration 0003)
    next_gen_at_ms INTEGER NOT NULL,
    created_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL
);
CREATE INDEX routine_next_gen_idx ON routine(next_gen_at_ms) WHERE next_gen_at_ms IS NOT NULL;
```

### Block

```sql
CREATE TABLE block (
    block_id       BLOB PRIMARY KEY,           -- 16 bytes
    stream_id      BLOB NOT NULL REFERENCES stream(stream_id),
    start_ms       INTEGER NOT NULL,
    end_ms         INTEGER NOT NULL,
    task_ids_blob  BLOB NOT NULL,              -- CBOR array of task ids
    rrule_blob     BLOB,                       -- nullable
    title          TEXT NOT NULL,              -- shadow copy
    title_track    INTEGER NOT NULL DEFAULT 0,
    created_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL
);
CREATE INDEX block_time_idx   ON block(start_ms, end_ms);
CREATE INDEX block_stream_idx ON block(stream_id);
```

### Attachment

```sql
CREATE TABLE attachment (
    attachment_id  BLOB PRIMARY KEY,           -- 16 bytes
    blob_id        BLOB NOT NULL,              -- 16 bytes; opaque server-side id
    meta_blob      BLOB NOT NULL,              -- CBOR: mime, size, chunks, hashes (see blob-store.md)
    cache_state    TEXT NOT NULL DEFAULT 'absent',  -- 'absent'|'partial'|'cached'
    cache_path     TEXT,                       -- relative path under blob-cache dir
    created_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL
);
CREATE INDEX attachment_cache_idx ON attachment(cache_state, updated_at_ms);
```

### Person

```sql
CREATE TABLE person (
    person_id      BLOB PRIMARY KEY,           -- 16 bytes; LOCAL identifier
    identity_id    BLOB,                       -- idn_… raw bytes; nullable
    display_name   TEXT NOT NULL,              -- local-only; never synced as plaintext
    email_lc       TEXT,                       -- nullable
    meta_blob      BLOB NOT NULL,              -- CBOR
    created_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL
);
CREATE INDEX person_identity_idx ON person(identity_id) WHERE identity_id IS NOT NULL;
CREATE UNIQUE INDEX person_email_idx ON person(email_lc) WHERE email_lc IS NOT NULL;
```

The `*_blob` columns hold the canonical CRDT or CBOR representation; the scalar columns are denormalized projections for query performance and are rebuilt from the blob on migration.

### Tasks (materialized projection)

```sql
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
    scheduling_constraints BLOB,           -- canonical CBOR, NULL = empty; projection (arrives in migration 0003)
    body             BLOB,                 -- CRDT doc snapshot
    extra            BLOB,                 -- forward-compat unknown fields
    head_root        BLOB                  -- per-task tamper-detection snapshot
);

-- Many-to-many task ↔ context
CREATE TABLE task_contexts (
    task_id    BLOB NOT NULL,
    context_id BLOB NOT NULL,
    added_at   INTEGER NOT NULL,
    PRIMARY KEY (task_id, context_id)
);
```

### Sync state

```sql
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
```

### Wrapped stream keys

```sql
CREATE TABLE stream_keys (
    stream_id    BLOB PRIMARY KEY,
    epoch        INTEGER NOT NULL,
    wrapped      BLOB NOT NULL,        -- AEAD(vault_root, stream_key)
    created_at   INTEGER NOT NULL
);
```

### FTS

```sql
CREATE VIRTUAL TABLE search_idx USING fts5(
    kind, id UNINDEXED, stream_id UNINDEXED,
    title, body, contexts,
    tokenize = 'unicode61 remove_diacritics 2 porter'
);
```

The `unicode61 remove_diacritics 2 porter` tokenizer applies to both index AND queries. Examples:

| Search input | Index entry | Match? |
|---|---|---|
| `cafe` | `café` | yes (diacritic stripped) |
| `running` | `run` | yes (porter stem) |
| `runs` | `running` | yes (both stem to `run`) |
| `日本語` | `日本語` | unicode61 segments per character; CJK match works as substring |

CJK quality is acknowledged as basic; v1.1 introduces a per-locale CJK tokenizer (capability bit `CLI_FTS5_CJK`).

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
