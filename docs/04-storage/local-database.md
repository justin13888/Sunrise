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

> **The schema itself lives in
> [`crates/sunrise-storage/migrations/0013_baseline.sql`](../../crates/sunrise-storage/migrations/0013_baseline.sql),
> and that file is the source of truth**, together with the migrations appended
> after it, listed in
> [`crates/sunrise-storage/migrations/`](../../crates/sunrise-storage/migrations/).
> Naming them here is what rotted last time, and so is counting them — this
> sentence said "four files today" through four appends. The directory is the
> list. The baseline is a collapse, not a sequence
> ([ADR-0018](../11-adr/0018-storage-baseline-reset.md)), and it carries the
> design rationale for each table on the table. This section describes the
> *shape and the reasons*; it deliberately does not restate every column, because
> a second copy of a schema is a second thing to be wrong. An earlier version of
> this document did restate them, and drifted: it named six tables in the
> singular (`stream`, `context`, `routine`, `block`, `attachment`, `person`)
> that never existed, columns that were never created (`policy_blob`,
> `rrule_blob`, `config_blob`, `meta_blob`, `task_contexts.added_at`), and Loro
> `doc_blob` columns that ADR-0014 deleted.

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

### Materialized entities — `streams`, `tasks`, `contexts`, `routines`, `blocks`

All plural, all projections: **every one is rebuildable from the op log**, which
is what makes "wipe and re-materialize" a real recovery path rather than a
slogan.

Four things are worth knowing beyond the column list:

* **The LWW stamp.** `lww_hlc_ms`, `lww_hlc_logical`, `lww_seq`, `lww_device`
  carry the `(hlc, device_id, seq)` key that decides which of two concurrent
  writes survives. See
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
* **`streams.head_root`** is the per-Stream Merkle root defined in
  [`../03-crypto/audit-and-tamper-evidence.md`](../03-crypto/audit-and-tamper-evidence.md),
  recomputed on every apply. On startup it is verified against a fresh
  recomputation over the last 1 000 ops; a mismatch raises
  `STORAGE_TAMPER_DETECTED` and the user is prompted to wipe and resync.

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

### Sync state — `local_identity`, `outbox`, `sync_cursors`

`local_identity` is pinned to one row (`CHECK (id = 1)`) and holds the device's
signing seed sealed under the vault root. `outbox` tracks ops still to push
(`acked_at_ms IS NULL` = pending). `sync_cursors` records the highest applied
`seq` per `(stream_id, device_id)` for gap detection and pull resumption.

### Wrapped stream keys — `stream_keys`

Keyed by `(stream_id, epoch)`, so a rotation adds a row rather than replacing
one and old epochs stay openable.

### FTS — `search_idx`

```sql
CREATE VIRTUAL TABLE search_idx USING fts5(
    kind, id UNINDEXED, stream_id UNINDEXED,
    title, body, contexts,
    tokenize = 'porter unicode61 remove_diacritics 2'
);
```

FTS5 expects chained tokenizers **outer-first**, so `porter` wraps `unicode61`
and the arguments after `unicode61` configure that base tokenizer. (This
document previously wrote the pair in the other order, which FTS5 does not
accept.) The tokenizer applies to both the index and queries:

| Search input | Index entry | Match? |
|---|---|---|
| `cafe` | `café` | yes (diacritic stripped) |
| `running` | `run` | yes (porter stem) |
| `runs` | `running` | yes (both stem to `run`) |
| `日本語` | `日本語` | unicode61 segments per character; CJK match works as substring |

CJK quality is acknowledged as basic; v1.1 introduces a per-locale CJK tokenizer (capability bit `CLI_FTS5_CJK`).

## Indexes

Defined alongside their tables in the baseline. The ones that carry a read path
rather than merely a foreign key:

| Index | What it serves |
|---|---|
| `ops_by_stream (stream_id, seq)` | Replay and backfill in causal order. |
| `ops_unapplied (applied_at) WHERE applied_at IS NULL` | The reconciliation pass. |
| `tasks_by_due (due_at_ms) WHERE due_at_ms IS NOT NULL` | Today and the deadline views. |
| `task_blockers_by_blocker` | The reverse dependency direction. |
| `contexts_by_name (name COLLATE NOCASE) WHERE deleted = 0` | `@name` resolution during capture. Deliberately **not** UNIQUE — see the baseline's comment. |
| `review_snapshots_by_window (window_start_ms DESC, created_at_ms DESC)` | History, newest first. |

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

Using a pre-derived key bypasses SQLCipher's internal PBKDF2: we set `PRAGMA cipher_kdf_algorithm = PBKDF2_HMAC_SHA512` and `PRAGMA kdf_iter = 1`.

**What `kdf_iter = 1` rests on, precisely.** It rests on the input already being a uniformly random 32-byte key — not on a prior Argon2id gate, because there is no passphrase unlock. `Unlock::Passphrase` exists as a variant (`crates/sunrise-core/src/unlock.rs:11`) and **no caller constructs it**; every `Core::open` in the tree passes `Unlock::DevicePaired`. The root is generated once at random per vault and held by the OS: `SecRandomCopyBytes` into the login Keychain on macOS, or a `0600` key file in the CLI's keystore (`crates/sunrise-cli/src/vault.rs:20-28`). A second device gets it by pairing, not by the user retyping anything. Argon2id does appear in `sunrise-crypto`, but for the **recovery** key derived from the BIP-39 seed (`crates/sunrise-crypto/src/recovery.rs`), which is a different path.

The choice of AES here is a deliberate divergence from XChaCha20-Poly1305 used elsewhere; rationale:

- SQLCipher's AES-CBC + HMAC mode is shipped as a single audited library on every platform v1 targets. (The WASM story would go through `wa-sqlite`, which nothing in the tree builds — the web core is deferred, [ADR-0012](../11-adr/0012-web-wasm-deferred.md).)
- Page-level encryption inside SQLite has its own design (per-page IVs, MAC over `(page_no, ciphertext)`). Replacing the cipher would mean shipping a custom SQLCipher fork, which is outside our maintenance budget.
- The vault DB at rest is protected by the OS keystore and the application unlock; the cipher choice here is defense-in-depth, not the primary trust boundary.

## Backups

The vault DB is suitable for binary backup (file copy while not actively writing). Encrypted at rest, so backups carry no plaintext. UI offers "create backup" that performs a checkpoint and copies the DB to a user-chosen location.

## Vacuum / maintenance

- `PRAGMA auto_vacuum = INCREMENTAL` set at vault creation.
- Background incremental vacuum runs after compaction (see [`compaction.md`](./compaction.md)).
- WAL checkpointing is automatic.
