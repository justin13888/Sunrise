---
status: accepted
---

# Relay and Blob Storage

> **Implementation status.** The self-host single-binary path is built and is
> the only one: one SQLite database (`store/` + `relay_log.rs`) and a
> local-filesystem blob root (`api/blobs.rs` over `sunrise_storage::BlobStore`).
> There is no Postgres, no object store, and no pub/sub; the "managed" sections
> below describe a deployment that does not exist. Sections that describe built
> behaviour are marked **(implemented)**.

## Relay model

As built, the server stores op envelopes per **`(account_h, stream_id)` channel**
— not per `(stream_id, target_device_id)` — and there is no per-target queue and
no ack-driven removal. Devices subscribe with per-originating-device cursors;
the server replays what the cursors do not already cover, in `relay_frames.id`
order, and drops a frame from the log only when a retention bound evicts it.
The account half of the channel key is derived server-side from the verified
token, never taken from the client, which is what makes cross-tenant
subscription impossible rather than merely discouraged.

## Storage layout (managed — design target, not implemented)

| Storage | Contents | Backend |
|---|---|---|
| Op metadata | `(op_id, stream_id, originating_device_id, seq, size, created_at, …)` | Postgres |
| Op envelopes | encrypted bytes, large | S3-compatible object store (key: `ops/<stream>/<op_id>`) |
| Per-device cursors | `(stream_id, device_id, last_acked_seq)` | Postgres |
| Account & device records | `(account_id, email, devices, …)` | Postgres |
| Blobs | encrypted attachment chunks | S3-compatible object store (key: `blobs/<blob_id>/<chunk>`) |
| Push tokens | encrypted at rest, per device | Postgres |

## Storage layout (self-host single-binary — implemented)

- Accounts, devices, push tokens, **and the op envelopes themselves** → one
  SQLite database. Op frames are rows in `relay_frames`, not files: see the
  next section.
- Blobs → local filesystem under the blob root, per account.
- No S3, no Postgres, no Redis.

### What the relay database actually holds (implemented)

`Store::open` applies the database key when one is configured, sets the
connection pragmas described under
[Schema versions and pragmas](#schema-versions-and-pragmas-implemented), runs a
bounded `PRAGMA quick_check`, and migrates the schema, and **nothing else**.

**Encryption at rest is the operator's choice, off by default.** With
`[storage] encrypt = true` and a `key_file`, the whole file is
SQLCipher-encrypted under that 32-byte raw key, header included
([ADR-0060](../11-adr/0060-relay-database-encryption-at-rest.md),
`crates/sunrise-server/src/store/cipher.rs`). The key comes from a file the
operator holds outside the data dir, never from anything inside it, unlike the
*client* vault, whose key `sunrise_storage::db` derives as
`BLAKE3.derive_key("sunrise.sqlcipher_key.v1", vault_root)`. Without it, the
file is a plain SQLite database, readable by anyone who can read the data dir.
Either way the running relay reads every column below: encryption protects the
file and its backups, not the data from the process that serves it.

What the database holds, stated plainly rather than left to inference. The
**Form** column is the value as the relay stores it; with encryption on, all of
it is under the database key at rest:

| Stored | Form | Notes |
|---|---|---|
| `accounts.email` | **plaintext** | the IdP's `email` claim, refreshed on each login |
| `accounts.oidc_iss` / `oidc_sub` | plaintext | the IdP's identifiers for the user |
| `accounts.recovery_blob` | ciphertext | opaque; written by `POST /accounts`, served back by `GET /accounts/me/recovery_blob` behind an OIDC step-up. Write-once: a differing blob is `409`, not a silent overwrite |
| `devices.nickname`, `devices.platform`, `devices.app_version` | **plaintext** | user-set name and client-reported platform/version |
| `devices.device_pub_s` / `device_pub_d` / `device_cert` | public keys | |
| `push_tokens.token` | **plaintext** | see [`push-notifications.md`](./push-notifications.md) |
| `relay_frames.bytes` | **verbatim ciphertext** | the wire frame as received, alongside `n_bytes` and `created_ms` |
| `relay_frames.account_h` | BLAKE3-truncated | first 16 bytes of `BLAKE3(account_id)`, derived server-side from the verified token — never from the client |
| `relay_frames.stream_id` | **raw 16-byte id** | as the subscriber named it |
| `relay_frame_heads.device_id` | **raw 16-byte id** | read from the envelope's cleartext routing header |
| `account_delete_tokens.token_h` | BLAKE3 | of a pending deletion phrase; the phrase itself is never stored |
| `blob_tombstones` | **raw ids** | blob key, stream id, publishing device and `seq` of the detaching op, as `DELETE /blobs/{blob_id}` named them |
| `device_cursors` | **raw ids** | per device, stream and originating device, the `seq` it declared applied on `POST /sync/subscribe` |

So the channel namespace is hashed and the routing ids inside it are not. The
`id_h` truncation in `logging/mod.rs` is applied to **log output**, not to
storage; do not read a log line's 8-hex `stream_h` as the stored form.

`relay_frames.bytes` is stored exactly as received: the relay parses the
cleartext routing header via `sunrise_cbor::decode_envelope_header` and nothing
more. It cannot do more — `sunrise-server` has **no `sunrise-crypto`
dependency**, and `EnvelopeHeader` has no payload field, so the
non-responsibility is enforced by the dependency graph.

### Schema versions and pragmas (implemented)

The schema is a numbered, append-only list of migrations in
`crates/sunrise-server/src/store/migrations/mod.rs`, and the database records
how far along it is in `PRAGMA user_version`. On open, `Store::open`:

1. with a key configured, encrypts a plaintext file in place, once, keeping
   the original as `sunrise.db.pre-encryption` (ADR-0060 §4); then applies the
   key before any other statement, and refuses a file it does not open
   (`WrongKey`), or an encrypted file with no key configured (`KeyRequired`),
   both exit 78;
2. sets `busy_timeout` (`[storage] busy_timeout_ms`, default 5 s), so even the
   next read waits out another process's lock rather than failing on it;
3. reads `user_version` and refuses a database above the binary's newest
   migration, or below zero, with `StoreError::SchemaTooNew`. Nothing has
   written to the file at this point, and nothing does: the binary exits 78;
4. sets `journal_mode = WAL`, then `synchronous = NORMAL` if WAL took, or
   `FULL` if it did not (a filesystem without shared memory keeps its rollback
   journal and logs `srv.store.wal_unavailable`), then `foreign_keys = ON`;
5. runs `PRAGMA quick_check` under a 10 s watchdog and logs
   `srv.store.quick_check`, starting regardless of the result;
6. applies every migration above the database's version, each in its own
   `BEGIN IMMEDIATE` transaction that also writes the new `user_version`, so a
   crash leaves the old schema at the old version or the new schema at the new
   one, and two processes opening one file cannot both apply a step.

| Migration | What it does |
|---|---|
| 0001 `baseline` | The `accounts`, `devices`, `push_tokens` and `relay_*` tables, as every pre-versioning release created them. Still `CREATE … IF NOT EXISTS`, so a database those releases wrote, which is at version 0, adopts it without change. |
| 0002 `devices_vault_device_id` | Adds `devices.vault_device_id` where a database predates it, then the `devices_by_vault_id` index. |
| 0003 `account_and_blob_deletion` | Adds `accounts.delete_requested_at_ms` and the tables deletion keeps: `account_delete_tokens`, `blob_tombstones`, and `device_cursors` (the cursors each device declared on subscribe, which a tombstone's quorum reads). Each cascades from its account or device row. |

A shipped migration is never edited: migration 0001 executes the tenants'
`SCHEMA` constants, so those are frozen with it, and a test builds a database
from a literal copy of the pre-versioning DDL, migrates it, and requires the
same schema a fresh database gets. A schema change is a new entry with the next
number. The list is `(id, name, step)` over one integer of state, which is what
a Postgres backend would record in a one-row table to share the numbering.

**The `synchronous = NORMAL` trade-off.** Under WAL, `NORMAL` syncs at
checkpoints rather than at every commit. A crash of the server process loses
nothing, and no crash can corrupt the database, but a power cut or a kernel
crash can roll back the transactions committed just before it. For the relay
that means a frame acknowledged in that window can be gone after a power cut,
when its sender has already dropped it from its outbox. Rollback-journal mode
uses `FULL` instead, because `NORMAL` there can corrupt the file on power loss.
`wal_autocheckpoint` stays at SQLite's default of 1000 pages (about 4 MiB); the
relay's writes are small frames, so the WAL never grows large enough to slow
reads.

All requests still share one connection behind a mutex. With WAL on, moving
reads onto a pool of read-only connections is possible, and is deferred until
the request-latency metrics show the shared connection is the bottleneck.

### Durable relay op log (implemented)

As built, the self-host relay keeps op frames in the **same SQLite database** as
accounts and devices (`relay_frames`, `relay_frame_heads`, `relay_evicted`)
rather than as files under `data/ops/`. One file is then the entire server
state, so a `tar` of it is a consistent backup, and an append plus its
retention eviction commit in one transaction — which a file tree plus a
separate metadata row could not give without the 2PC dance the managed
deployment needs.

The in-memory ring in front of it is live fan-out only. The durable log is the
authority for replay, which is what makes two things true that were not before:

- Passing the ring bound is no longer a loss event.
- A **relay restart** no longer loses history. Previously it dropped retained
  frames and eviction watermarks together, so a fresh process could not tell
  "never held it" from "evicted it" and reported *no gap* — a returning device
  was told it was caught up while ops were missing. No client-side change can
  detect that; only the server knows.

Retention is bounded per channel on two axes, enforced on every append:

| Bound | Default | Why |
|---|---|---|
| Age | 30 days | The same window every other retention number here uses. A device gone longer is a re-pair, not a resync. |
| Size | 256 MiB per channel | Bounds the disk by `channels × cap` instead of by uptime. Far above the 16 MiB memory ring, so it binds in practice only for genuinely long-abandoned devices. |

Eviction under either bound raises `evicted_through` exactly as the ring does,
so a cursor below it still produces the typed `SYNC_CURSOR_GAP`. That error is
now rare rather than routine — and because it is durable, it is *correct*: the
server only claims ops are gone when they actually are.

## Op write path (implemented)

1. Client `OpBatch` arrives as one `POST /api/v1/sync/ops`, and the handler rebuilds the wire frame from it.
2. Server reads each op's cleartext routing header for `(device_id, seq)`. It
   verifies **no signature** — it holds no key that could — and enforces
   **no quota**. Before that, the handler charges the batch's op count to the
   device's op budget and refuses `429 RATE_LIMITED` past it
   ([`api.md`](./api.md) §Rate limits), which bounds how fast a device writes,
   not how much. Frame size is bounded by the wire protocol's frame cap, not by
   a per-account budget.
3. Server appends the verbatim frame to `relay_frames` in the same transaction
   that enforces the channel's retention bounds and raises `evicted_through`
   watermarks. There is no object store and therefore no 2PC: the append and
   its eviction commit together, which is the point of putting the log in the
   same database.
4. Server fans the frame out over the in-process `RelayHub` to sessions
   subscribed to that `(account_h, stream_id)` channel. There is no pub/sub
   layer, so fan-out reaches only this process's sessions.
5. Server sends `Ack` to originating client.
6. Other connected receivers get the frame. **Disconnected receivers get
   nothing** — no wake-up push is constructed anywhere (see
   [`push-notifications.md`](./push-notifications.md)).

### Blob 2PC (implemented — and not an outbox)

The blob flow is a genuine two-phase commit, fully implemented and
hash-verified, but it is not the Postgres/S3 outbox an earlier draft described.
What `api/blobs.rs` does:

```
1. POST /api/v1/blobs/init { stream_id, chunk_count, size_bytes }
     → 200 { upload_id = "up_" + 16 random bytes hex,
             chunk_urls = ["/api/v1/blobs/<upload_id>/<i>", …] }
   The per-upload pending area is created eagerly, under
   <blob_root>/pending/<blake3(account_id)[..16] hex>/<upload_id>/.
2. PUT /api/v1/blobs/<upload_id>/<i>   raw ciphertext chunk → 204
3. POST /api/v1/blobs/finalize { upload_id, content_hash, chunk_hashes }
     a. re-read every pending chunk from disk and re-hash it with BLAKE3;
        a chunk whose digest differs from the client's claim → 400 BLOB_HASH_MISMATCH,
        a chunk that was never uploaded → 409 BLOB_CHUNK_MISSING;
     b. hash the concatenation and compare to content_hash → 400 on mismatch;
     c. write the chunks under the content address, then the manifest LAST;
     d. lift any tombstone on that content address, and best-effort delete
        of the upload's pending directory.
     → 200 { blob_id = "blb_" + first 16 bytes of content_hash hex,
             size_bytes, chunk_count }
```

The client's hashes are a **claim**; step 3a is the check. `blob_id` is
therefore a content address, not a random id — identical ciphertext converges on
one stored copy *within an account*, and both the pending and committed areas
are rooted at `blake3(account_id)[..16]` so content addressing can never become
a cross-tenant read primitive.

Writing the manifest last is what makes a crash safe: `fetch` reads the manifest
first, so a half-committed blob is invisible rather than short. There is no
`pending` status row. An abandoned upload leaves a pending directory that costs
disk and never correctness, and the maintenance pass (below) removes it once
nothing in it has been touched for `[storage] pending_upload_ttl_hours`
(default 24).

### Concurrent same-`blob_id` (implemented)

`blob_id` is the content address, so two finalizes agreeing on `content_hash`
are writing identical bytes and the second is idempotent by construction — the
same chunks and the same manifest are rewritten, and the response is the same
`200`. A `409 BLOB_FINALIZE_CONFLICT` code does not exist and cannot arise:
disagreeing `chunk_hashes` produce a different `content_hash`, hence a different
`blob_id`, and a `content_hash` that does not match the bytes on disk fails at
step 3b before any commit.

## Op read path (on subscribe / catch-up — implemented)

1. Receiver sends `Subscribe` with per-originating-device cursors.
2. Server calls `Store::relay_replay` on the channel, which selects the retained
   frames whose heads are not already covered and reports a `CursorGap` for any
   device whose cursor sits below `relay_evicted.evicted_through`.
3. Server streams the retained frames back verbatim; a gap becomes a typed
   `SYNC_CURSOR_GAP` rather than a silent under-delivery. Frames and their heads
   come from the same SQLite database, so there is no second store to fetch from.

## Durability

- SQLite durability on commit, for the frame and its retention eviction
  together — one transaction, one file. The database runs in WAL mode with
  `synchronous = NORMAL`, so a commit survives a crash of the server process
  but the last few may not survive a power cut; see
  [Schema versions and pragmas](#schema-versions-and-pragmas-implemented).
- Blob chunks are written to a `.bin.tmp`, `sync_all`'d, then renamed into
  place, and the manifest is written last (unsynced), so an interrupted
  finalize leaves an invisible partial rather than a short read.
- The Postgres/S3 durability rules below apply to the unbuilt managed
  deployment: S3 PUT confirmed before ack (no async fire-and-forget); object
  store SHOULD be configured for cross-AZ replication.

## Retention

All retention numbers are unified to **30 days** (or shorter):

- Op envelopes: the relay keeps a frame for at most **30 days** from arrival, or less when the channel's count bound evicts it first. This is the current relay retention, set in code by `DEFAULT_MAX_AGE_MS` (`crates/sunrise-server/src/relay_log.rs:123`) and applied on every append; it does not wait on compaction, which is a client-side fold the relay takes no part in ([`../04-storage/compaction.md`](../04-storage/compaction.md) §Server side).
- Op metadata rows: retained at least **30 days**, as above. The relay holds no snapshot: a client keeps its stream's latest snapshot record locally, and no transport carries one between devices yet ([#462](https://github.com/justin13888/Sunrise/issues/462)).
- Blob GC grace: `[storage] gc_grace_days`, default 30. A tombstoned blob is reclaimed by the first maintenance pass after its grace period at which every active device has acknowledged the tombstone. This is independent of post-compaction retention.
- Abandoned uploads: `[storage] pending_upload_ttl_hours`, default 24, measured from the newest file in the upload.
- Deleted accounts: `[storage] account_delete_grace_days`, default 30, from the confirmed deletion to the erasure.
- Cursors: retained for the life of the device.
- Audit (managed): **30 days** (see [`observability.md`](./observability.md)). Self-host default: also 30 days, configurable.

Retention **enforcement** is implemented for the relay op log, the blob store and
deleted accounts. `relay_log.rs` applies the log's two bounds on every append.
The rest is the **maintenance pass**
(`crates/sunrise-server/src/admin/maintenance.rs`). The serving binary runs it
at startup and then every `[storage] maintenance_interval_secs` (default 3600),
and `sunrise-server admin gc --now` runs it on demand. In order, it:

1. Erases accounts whose deletion was confirmed more than
   `account_delete_grace_days` ago (see [`api.md`](./api.md) §Account deletion).
2. Collects tombstoned blobs whose grace period has passed and whose quorum
   holds. It removes the manifest first, so a reader stops seeing the blob
   before any chunk goes.
3. Sweeps abandoned uploads.
4. Under a blob root the operator configured, sweeps per-account directories
   whose account no longer exists and which have been untouched for the upload
   TTL. A crash between an erasure's commit and its file deletion leaves those.

An item that fails is logged as `srv.maintenance.failed` and retried on the next
pass. The rest of the pass continues. Compaction retention, op-metadata retention
and audit retention have no implementation.

There is no Postgres, so there is no `fsync = on` to check;
`sunrise-server admin doctor` checks what applies instead (see
[`self-hosting.md`](./self-hosting.md) §Testing the install). Relay durability is
governed by the `synchronous = NORMAL` the server sets under WAL, described
under [Schema versions and pragmas](#schema-versions-and-pragmas-implemented).

## Blob storage

| Operation | Detail |
|---|---|
| Init upload | **As built:** server returns N *relative* URLs, `/api/v1/blobs/<upload_id>/<i>`, which it serves itself. Presigned PUT URLs and their 1-hour expiry are the unbuilt managed shape. |
| Upload chunks | **As built:** `PUT` to the relay, 1..=1 MiB per chunk, at most 4096 chunks, 100 MB per blob. |
| Finalize | Client posts `chunk_hashes` — one `BLAKE3(ciphertext_chunk)` per entry, lowercase hex, **64 characters** (32 bytes), plus a `content_hash` over the concatenation. The server re-reads every stored chunk and re-hashes it; a mismatch on any chunk or on the concatenation is `400 BLOB_HASH_MISMATCH`, and a chunk that never arrived is `409 BLOB_CHUNK_MISSING` (see [`api.md`](./api.md)). No ETag shortcut is taken — the check is always a real re-hash. There is no `plaintext_hash` server-side; that's a client-only concept. |
| Download | **As built:** `GET /api/v1/blobs/<blob_id>` reassembles the chunks and returns `application/octet-stream` from the relay. Presigned GET URLs and their 24-hour expiry are the unbuilt managed shape. |
| Delete | **As built:** `DELETE /api/v1/blobs/<blob_id>` writes a tombstone naming the op that detached the blob. The ciphertext stays readable until the maintenance pass collects it, after `gc_grace_days` and once every active device has acknowledged that op through its subscribe cursors. See [`api.md`](./api.md) §Blobs. |

Persisted blob payloads carry the uniform 5-byte magic prefix from [`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md) §3, but the server stores opaque bytes and does not introspect.

## Privacy / metadata

The server sees: blob IDs, sizes, upload/download times, per-device access patterns. It does *not* see plaintext, filename, MIME type beyond what the client explicitly sends in opaque metadata ops.

## Failure modes

Rows naming S3, Postgres or a push provider describe the unbuilt managed
deployment. As built, every one of these failures is a local disk or SQLite
failure: a write error on the relay log increments
`sunrise_relay_append_failed_total` and the frame is not retained, and a
`rusqlite` error on a REST route becomes `500 FATAL_INTERNAL` with the
underlying error deliberately not reflected to the caller.

| Failure | Behavior |
|---|---|
| S3 outage during write | Op rejected with `RETRY_AFTER`; client retries via outbox |
| S3 outage during read | Server returns a transient error to the relay subscriber; client retries |
| Postgres outage | Hard failure for new connections; existing connections drained |
| Push provider outage | Push degrades; clients still pull on next foreground |

## Self-host filesystem layout (implemented)

`[storage] data_dir` expands to `<dir>/sunrise.db` and `<dir>/blobs`. Ops are
**not** files — they are rows in `sunrise.db` — and the blob tree is rooted per
account under a BLAKE3 of the account id:

```
<data_dir>/
├── sunrise.db                        # SQLite: accounts, devices, push_tokens,
│                                     #   relay_frames, relay_frame_heads, relay_evicted,
│                                     #   account_delete_tokens, blob_tombstones, device_cursors
├── sunrise.db-wal, sunrise.db-shm    # WAL and its index, present while the server runs
├── sunrise.db.pre-encryption         # only after [storage] encrypt was turned on: the
│                                     #   plaintext original, until the operator deletes it
└── blobs/
    ├── pending/<acct_h>/<upload_id>/
    │   └── blobs/<xx>/<upload_hex>/<i>.bin
    └── committed/<acct_h>/
        ├── blobs/<xx>/<blob_hex>/<i>.bin   # xx = first 2 hex chars of the id
        └── manifests/<blob_hex>            # "<chunk_count> <size_bytes>"; written last
```

`<acct_h>` is `BLAKE3(account_id)[..16]` in hex; `<upload_hex>` and `<blob_hex>`
are the 32-hex bodies of `up_…` and `blb_…`. The inner `blobs/` segment is
`BlobStore`'s own root, which is why it appears under an already-blob-rooted
path. Backup is `sunrise-server admin backup` while the relay runs, or `tar`
of the data dir while it is stopped (or a snapshot mechanism); because the op
log lives in the database, that single file and `blobs/` are the whole relay
state. [`self-hosting.md`](./self-hosting.md) §Backup has the procedure and
§Restore the way back. Blob chunks are client-sealed ciphertext and are not
encrypted again at rest; with `[storage] encrypt = true`, `sunrise.db` is.
