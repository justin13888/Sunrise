---
status: accepted
---

# Relay and Blob Storage

> **Implementation status.** The self-host single-binary path is built and is
> the only one: one SQLite database (`store.rs` + `relay_log.rs`) and a
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

`Store::open` runs `PRAGMA foreign_keys = ON` and the schema, and **nothing
else**. In particular it issues no `PRAGMA key`, so **the relay database is not
SQLCipher-encrypted** — `rusqlite` is built with the workspace's
`bundled-sqlcipher` feature, but only the *client* vault applies a key
(`sunrise_storage::db` derives it as
`BLAKE3.derive_key("sunrise.sqlcipher_key.v1", vault_root)`). On the relay the
file is a plain SQLite database, readable by anyone who can read the data dir.

What that file exposes, stated plainly rather than left to inference:

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

So the channel namespace is hashed and the routing ids inside it are not. The
`id_h` truncation in `logging/mod.rs` is applied to **log output**, not to
storage; do not read a log line's 8-hex `stream_h` as the stored form.

`relay_frames.bytes` is stored exactly as received: the relay parses the
cleartext routing header via `sunrise_cbor::decode_envelope_header` and nothing
more. It cannot do more — `sunrise-server` has **no `sunrise-crypto`
dependency**, and `EnvelopeHeader` has no payload field, so the
non-responsibility is enforced by the dependency graph.

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
   **no rate limit and no quota**; neither exists. Frame size is bounded by the
   wire protocol's frame cap, not by a per-account budget.
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
     d. best-effort delete of the pending area.
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
`pending` status row and no cron job; an abandoned upload leaves a pending
directory that costs disk and never correctness.

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
  together — one transaction, one file.
- Blob chunks are written to a `.bin.tmp`, `sync_all`'d, then renamed into
  place, and the manifest is written last (unsynced), so an interrupted
  finalize leaves an invisible partial rather than a short read.
- The Postgres/S3 durability rules below apply to the unbuilt managed
  deployment: S3 PUT confirmed before ack (no async fire-and-forget); object
  store SHOULD be configured for cross-AZ replication.

## Retention

All retention numbers are unified to **30 days** (or shorter) in v1:

- Op envelopes: retained until eligible for compaction; post-compaction retention is **30 days** (see [`../04-storage/compaction.md`](../04-storage/compaction.md)).
- Op metadata rows: retained at least **30 days** regardless of compaction state, providing a recovery window if compaction logic produces a defective snapshot.
- Blob GC grace: `gc_grace_days = 30` (configurable). Tombstoned blobs are deleted from the object store on day 31. This is independent of post-compaction retention.
- Pending-blob outbox rows: 24 h (described above).
- Cursors: retained for the life of the device.
- Audit (managed): **30 days** (see [`observability.md`](./observability.md)). Self-host default: also 30 days, configurable.

Retention **enforcement** is implemented for the relay op log only: `relay_log.rs`
applies both bounds on every append. Blob GC, compaction retention, op-metadata
retention and audit retention have no implementation — no GC job, no cron, and
no `gc_grace_days` setting the parser will accept.

There is no Postgres, so there is no `fsync = on` to check, and the
`sunrise-server doctor` subcommand that would check it does not exist (see
[`self-hosting.md`](./self-hosting.md) §"Not yet wired"). SQLite's own default
`synchronous` setting governs relay durability; the server sets no `PRAGMA
synchronous` of its own.

## Blob storage

| Operation | Detail |
|---|---|
| Init upload | **As built:** server returns N *relative* URLs, `/api/v1/blobs/<upload_id>/<i>`, which it serves itself. Presigned PUT URLs and their 1-hour expiry are the unbuilt managed shape. |
| Upload chunks | **As built:** `PUT` to the relay, 1..=1 MiB per chunk, at most 4096 chunks, 100 MB per blob. |
| Finalize | Client posts `chunk_hashes` — one `BLAKE3(ciphertext_chunk)` per entry, lowercase hex, **64 characters** (32 bytes), plus a `content_hash` over the concatenation. The server re-reads every stored chunk and re-hashes it; a mismatch on any chunk or on the concatenation is `400 BLOB_HASH_MISMATCH`, and a chunk that never arrived is `409 BLOB_CHUNK_MISSING` (see [`api.md`](./api.md)). No ETag shortcut is taken — the check is always a real re-hash. There is no `plaintext_hash` server-side; that's a client-only concept. |
| Download | **As built:** `GET /api/v1/blobs/<blob_id>` reassembles the chunks and returns `application/octet-stream` from the relay. Presigned GET URLs and their 24-hour expiry are the unbuilt managed shape. |
| Delete | **Not implemented.** No `DELETE` route, no tombstone, and no GC job. See [`api.md`](./api.md) §Blobs. |

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
│                                     #   relay_frames, relay_frame_heads, relay_evicted
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
path. Backup is `tar` of the data dir
while paused (or with a snapshot mechanism); because the op log lives in the
database, that single file is the whole relay state.
