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
PRAGMA auto_vacuum    = INCREMENTAL;
PRAGMA journal_mode   = WAL;
PRAGMA synchronous    = NORMAL;
PRAGMA foreign_keys   = ON;
PRAGMA busy_timeout   = 5000;
```

`auto_vacuum` comes first. It takes effect only on a file with no header page yet, and setting `journal_mode = WAL` writes that header, so issued after it the pragma is silently ignored ([#461](https://github.com/justin13888/Sunrise/issues/461)).

All multi-row writes are wrapped in a single `BEGIN IMMEDIATE … COMMIT`. Reads outside transactions see snapshots at the time of statement start (SQLite default). The op-application loop holds `BEGIN IMMEDIATE` for the entire batch; concurrent reads continue to see the pre-batch snapshot until commit.

## Tables

> **The schema is
> [`crates/sunrise-storage/migrations/`](../../crates/sunrise-storage/migrations/);**
> the baseline,
> [`0013_baseline.sql`](../../crates/sunrise-storage/migrations/0013_baseline.sql),
> carries the rationale ([ADR-0018](../11-adr/0018-storage-baseline-reset.md)).
> This section gives shape and reasons only.

### Op log — `ops`, `op_dep`

`ops` is keyed by `op_id` with `UNIQUE (stream_id, device_id, seq)`, which is
the idempotence gate on the receive path: a re-delivered op is an
`INSERT OR IGNORE` that changes zero rows and stops there. `envelope` holds the
full sealed `OpEnvelope`; the server never sees anything else, and neither does
this table.

`deps` is **not** a column on `ops`. It is a separate join table so both
directions are indexed:

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

Inserting an op inserts its dep rows in the same transaction.

### Materialized entities — `streams`, `tasks`, `contexts`, `routines`, `blocks`, `saved_views`

All plural, all projections: **every one is rebuildable from the op log**, which
is what makes "wipe and re-materialize" a real recovery path rather than a
slogan.

Four things are worth knowing beyond the column list:

* **The LWW stamp.** `lww_hlc_ms`, `lww_hlc_logical`, `lww_seq`, `lww_device`
  carry the greatest stamp applied to the entity, in the `(hlc, device_id, seq)`
  key that legacy full-state ops and quiet registers compare. Which of two
  concurrent writes survives is decided per field by the version DAG and the
  field's conflict class
  ([ADR-0065](../11-adr/0065-copy-on-write-entity-versions.md)). See
  [`../05-sync/conflict-resolution.md`](../05-sync/conflict-resolution.md) and
  [ADR-0016](../11-adr/0016-hlc-timestamps.md). `lww_device` is NULL only for a
  placeholder row no real op has stamped yet.
* **`SunriseTime` projects onto three columns.** `*_at_ms` is the epoch-ms
  index key that every range query and `ORDER BY` reads; `*_at_kind` and
  `*_at_tz` carry the kind that the key alone cannot express. See
  [ADR-0017](../11-adr/0017-sunrise-time-representation.md).
* **`tasks.extra` is written.** It holds fields from a newer `DOC_SCHEMA_V`
  that this build does not model, preserved verbatim so a round trip through an
  older client does not destroy them
  ([§7](../10-cross-cutting/protocol-versioning.md#7-document-schema-forward-compat)).
* **`streams.head_root`** is reserved and written as zeros. Tamper evidence is
  the per-device chain
  ([`../03-crypto/audit-and-tamper-evidence.md`](../03-crypto/audit-and-tamper-evidence.md)).

### Edge tables — `task_contexts`, `task_blockers`, `block_tasks`

`task_blockers` covers **both** directions from one table: forward
(`blocked_by`) from its primary key, reverse (`blocks_others`) from the
`by_blocker` index. `blocks_others` is derived-only per
[`../02-domain/tasks.md`](../02-domain/tasks.md) — never a column, never on the
wire.

None of these carry **foreign keys**, deliberately. Ops arrive out of order: a
`task.update` naming a blocker can be materialized before that blocker's
`task.create` reaches this replica. An FK would abort the apply transaction and
wedge the receive path. An edge to an unknown task is a *fact*, and a task whose
blocker has not arrived counts as blocked until it does — which is the
convergent answer.

### Append-only records — `focus_sessions`, `focus_session_ends`, `focus_interruptions`, `review_snapshots`

None of these enter the LWW contest; each is keyed by its own `EntityRef` and
written once ([ADR-0013](../11-adr/0013-focus-session-op-representation.md)). A
focus session's start and end are **separate tables** so that a start with no
end is a valid state, and so an `end` that overtakes its `start` still lands
rather than updating zero rows. Nothing ticking is stored: elapsed time is
derived on read, and only the frozen `actual_focused_ms` is persisted.

### Merge state, version DAG and chain tables

Each migration's header comment gives the columns. `crates/sunrise-core/src/engine/rebuild.rs`
classifies every table as a projection or a source; a rebuild clears only the
projections.

* **Merge state** — `merge_entities`, `merge_registers`, `merge_map_entries`,
  `merge_orset_adds`, `merge_orset_removes`, `merge_counter_deltas` (migration
  0033, [ADR-0044](../11-adr/0044-per-field-ops.md)). The per-field state the
  entity rows are folded from: one register per field, one per map key, an
  add-wins set's adds and removes, and each counter increment. All projections.
  See [`../05-sync/crdt-design.md`](../05-sync/crdt-design.md).
* **Version DAG** — `entity_versions`, `entity_frontier`, `register_losers`,
  `pending_groups` (migration 0039,
  [ADR-0065](../11-adr/0065-copy-on-write-entity-versions.md)). One row per
  applied entity op with its parents, each version's causal frontier, the
  concurrent writes that lost a register, and a group's arrived members. All
  projections, and pruned as [`compaction.md`](./compaction.md) §Pruning version
  rows says.
* **Chain** — `chain_heads_sent`, `chain_expected`, `chain_claims`,
  `fork_evidence`, `chain_divergence` (migration 0034,
  [ADR-0043](../11-adr/0043-commit-tree.md)), beside `ops.op_hash` and
  `ops.chain_root`. What a device has listed, an op known to be missing, a peer's
  root not yet checkable, two signed claims about one position, and a peer's root
  that disagrees. All source: `fork_evidence` keeps a verbatim envelope that
  nothing can rebuild from `ops`. See
  [`../03-crypto/audit-and-tamper-evidence.md`](../03-crypto/audit-and-tamper-evidence.md).
* **Blob cache** — `blob_cache`, `blob_cache_backfill` (migration 0038). This
  device's index of its sealed blobs for the LRU, never an op and never synced.
  See [`blob-store.md`](./blob-store.md) §Cache eviction.
* **Devices** — `devices`, `device_revocations`, `device_revoke_ops`,
  `device_read_bounds`, `device_features`, `device_preferences`. The known
  devices, the revocation register, the ledger it is derived from, the monotone
  read bound, each device's advertised features, and the preferences scoped to
  this device. All source.

### Sync state — `local_identity`, `outbox`, `sync_cursors`

`local_identity` is pinned to one row (`CHECK (id = 1)`) and holds the device's
signing seed sealed under the vault root. `outbox` tracks ops still to push
(`acked_at_ms IS NULL` = pending). `sync_cursors` records the highest applied
`seq` per `(stream_id, device_id)` for gap detection and pull resumption.

### Wrapped stream keys — `stream_keys`

Keyed by `(stream_id, epoch)`, so a rotation adds a row rather than replacing
one and old epochs stay openable.

### FTS — `search_docs`, `search_words`, `search_tri`

Two FTS5 indexes over every searchable kind (migration 0040,
[ADR-0052](../11-adr/0052-search-v2.md),
[`../08-features/search.md`](../08-features/search.md)), keyed through one
table:

```sql
CREATE TABLE search_docs (
    doc INTEGER PRIMARY KEY, kind TEXT NOT NULL, id BLOB NOT NULL,
    UNIQUE (kind, id)
);
CREATE VIRTUAL TABLE search_words USING fts5 (
    title, aux, body,
    tokenize = 'porter unicode61 remove_diacritics 2'  -- `porter` only for English
);
CREATE VIRTUAL TABLE search_tri USING fts5 (title, body, tokenize = 'trigram');
CREATE TABLE search_meta (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    index_v INTEGER NOT NULL, stemmed INTEGER NOT NULL, language TEXT,
    reindex_stemmed INTEGER, reindex_cursor INTEGER NOT NULL DEFAULT 0
);
```

An entity's `doc` is the rowid of its row in both indexes, so replacing one
entity's text is a keyed delete and insert, not a scan of the index.
`search_words` is the ranked word index; FTS5 expects chained tokenizers
**outer-first**, so `porter` wraps `unicode61`, and the arguments after
`unicode61` configure that base tokenizer. It stems only when the vault's
content language (`search.content_language`) is English or unset, and a change
of language re-tokenizes it into `search_words_next` in the background before
swapping it in. `search_tri` holds text the core folded (NFKD, combining marks
dropped, lowercased) — titles in full and bodies to their first 4 KiB — because
the bundled SQLite's `trigram` tokenizer has no `remove_diacritics`.
`search_meta` records how the two were built. The first three tables are
projections; `search_meta` is source.

| Search input | Indexed text | Match? | Through |
|---|---|---|---|
| `cafe` | `café` | yes | both: diacritics folded |
| `running` | `run` | yes, in English | the word index's Porter stem |
| `pass` | `passport` | yes | the trigram index |
| `東京出張` | `東京出張の準備` | yes | the trigram index; `unicode61` keeps the CJK run as one token |
| `東京` | `東京出張の準備` | yes, in titles | a bounded `LIKE` scan: two characters are no trigram |
| `pasport` | `passport` | no | no typo tolerance |

## Indexes

Defined alongside their tables in the baseline, except where a later migration
is named. The ones that carry a read path rather than merely a foreign key:

| Index | What it serves |
|---|---|
| `ops_by_stream (stream_id, seq)` | Replay and backfill in causal order. |
| `ops_unapplied (applied_at) WHERE applied_at IS NULL` | The reconciliation pass. |
| `ops_by_ts (ts_ms)` (migration 0021) | The HLC restore at open. Without it the two queries `Engine::prime_hlc` runs are full scans of `ops`, which is 2.0 s on a million-op vault. |
| `tasks_by_due (hard_due_at_ms) WHERE hard_due_at_ms IS NOT NULL` | Today, lateness and the deadline views (the hard deadline; migration 0045 renamed its column). |
| `tasks_by_target (target_at_ms) WHERE target_at_ms IS NOT NULL` | The same, for the soft deadline (migration 0045). |
| `task_blockers_by_blocker` | The reverse dependency direction. |
| `contexts_by_name (name COLLATE NOCASE) WHERE deleted = 0` | `@name` resolution during capture. Deliberately **not** UNIQUE — see the baseline's comment. |
| `review_snapshots_by_window (window_start_ms DESC, created_at_ms DESC)` | History, newest first. |

## Materialization

Applying an op runs the op's mutation against the materialized table(s) **inside the same SQL transaction** that inserts the op into `ops`. This keeps op log and state mutually consistent at every commit point.

If an op cannot be applied (e.g. dep not yet present), it is inserted with `applied_at = NULL` and processed later when its deps arrive. A reconciliation pass runs at sync end.

## SQLCipher configuration

We ship the SQLCipher v4 default cipher suite to leverage the well-tested upstream:

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

Using a pre-derived key bypasses SQLCipher's internal PBKDF2: we set `PRAGMA cipher_kdf_algorithm = PBKDF2_HMAC_SHA512` and `PRAGMA kdf_iter = 1`.

**The first key in a process waits for SQLCipher to start.** `sqlite3_initialize` marks SQLite initialised before it runs SQLCipher's own initialisation, so a thread that opens a database in that window has `PRAGMA key` refused with a message about an empty key (issue #521). Before the first key it applies, `Db` keys a throwaway in-memory database until that succeeds, and other first openers wait on it. A process whose SQLCipher never initialises gets `DbError::CipherUnavailable` carrying the real error. The relay's store keys its own connections and waits on the same gate, `sunrise_storage::ensure_cipher_ready`, before each key (`StoreError::CipherUnavailable`).

**What `kdf_iter = 1` rests on, precisely.** It rests on the input already being a uniformly random 32-byte key — not on a prior Argon2id gate, because there is no passphrase unlock. `Unlock::Passphrase` exists as a variant (`crates/sunrise-core/src/unlock.rs:70#Unlock`) and **no caller constructs it**; every `Core::open` in the tree passes `Unlock::DevicePaired` or `Unlock::RecoveryCode`. The root is generated once at random per vault and held by the OS: `SecRandomCopyBytes` into the login Keychain on macOS, or a `0600` key file in the CLI's keystore (`crates/sunrise-cli/src/vault.rs:20-28`). A second device gets it by pairing, not by the user retyping anything. Argon2id does appear in `sunrise-crypto`, but for the **recovery** key derived from the BIP-39 seed (`crates/sunrise-crypto/src/recovery.rs`), which is a different path.

The choice of AES here is a deliberate divergence from XChaCha20-Poly1305 used elsewhere; rationale:

- SQLCipher's AES-CBC + HMAC mode is shipped as a single audited library on every native platform Sunrise targets. The web vault is the exception: it is plaintext SQLite in OPFS, without SQLCipher ([ADR-0055](../11-adr/0055-web-wasm-core.md)).
- Page-level encryption inside SQLite has its own design (per-page IVs, MAC over `(page_no, ciphertext)`). Replacing the cipher would mean shipping a custom SQLCipher fork, which is outside our maintenance budget.
- The vault DB at rest is protected by the OS keystore and the application unlock; the cipher choice here is defense-in-depth, not the primary trust boundary.

## Backups

The vault DB is suitable for binary backup (file copy while not actively writing). Encrypted at rest, so backups carry no plaintext. UI offers "create backup" that performs a checkpoint and copies the DB to a user-chosen location.

## Vacuum / maintenance

- A new vault is created with `auto_vacuum = INCREMENTAL` in effect. A vault created before [#461](https://github.com/justin13888/Sunrise/issues/461) reports `auto_vacuum = 0`; `Db::open` rewrites it once with a full `VACUUM` (logged as `db.auto_vacuum.convert`), after any migration and its backup, and every later open finds the mode set. The rewrite needs free disk space about the size of the vault. One that fails changes nothing: it is logged as `db.auto_vacuum.convert_failed`, the open goes on with the vault as it was, and the next open tries again.
- `Db::incremental_vacuum` returns every page on the free list to the filesystem. A compaction run that deleted any row ([`compaction.md`](./compaction.md)) ends with it, so the pages the fold freed leave the file rather than waiting on the free list for later writes.
- WAL checkpointing is automatic.
