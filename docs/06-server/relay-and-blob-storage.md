---
status: accepted
---

# Relay and Blob Storage

## Relay model

The server stores op envelopes in a queue per `(stream_id, target_device_id)`. Devices subscribe; the server delivers in arrival order. After ack, the per-target queue entry is removed; the envelope itself stays in the canonical op store until eligible for compaction.

## Storage layout (managed)

| Storage | Contents | Backend |
|---|---|---|
| Op metadata | `(op_id, stream_id, originating_device_id, seq, size, created_at, …)` | Postgres |
| Op envelopes | encrypted bytes, large | S3-compatible object store (key: `ops/<stream>/<op_id>`) |
| Per-device cursors | `(stream_id, device_id, last_acked_seq)` | Postgres |
| Account & device records | `(account_id, email, devices, …)` | Postgres |
| Blobs | encrypted attachment chunks | S3-compatible object store (key: `blobs/<blob_id>/<chunk>`) |
| Push tokens | encrypted at rest, per device | Postgres |

## Storage layout (self-host single-binary)

- Op metadata, accounts, cursors → SQLite.
- Op envelopes and blobs → local filesystem under `data/`.
- No S3, no Postgres, no Redis.

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

## Op write path

1. Client `OpBatch` arrives over WS.
2. Server validates: signature, size, rate limit, quota.
3. Server writes to object store (envelope) and Postgres (metadata) inside a 2PC-style outbox pattern (Postgres row written, then S3 PUT, then Postgres confirmation; failures retried).
4. Server publishes "new op" via internal pub/sub.
5. Server sends `Ack` to originating client.
6. Other connected receivers get `Push`. Disconnected receivers get a wakeup push if a token is registered.

### Blob 2PC outbox

The blob upload flow is a textbook 2PC outbox:

```
1. Begin Postgres tx.
2. Insert op row with status='pending', blob_ref=presigned_url, blob_id=<random>.
3. Commit Postgres tx.
4. Server returns presigned URL to client.
5. Client uploads blob to object store (PUT to presigned URL).
6. Client calls POST /api/v1/blobs/<blob_id>/finalize { chunk_hashes }.
7. Server verifies hashes, sets status='ready'.
```

A `pending` row older than **24 h** is GC'd by a cron job; the blob, if uploaded, is garbage-collected by the object-store lifecycle policy. (This 24-hour pending-blob TTL is unchanged from earlier drafts; the broader retention numbers — see "Retention" below — are unified to 30 days.)

### Concurrent same-`blob_id`

`blob_id` is 16 random bytes; collision probability is cryptographically negligible. If two finalize calls arrive for the same `blob_id` with non-matching `chunk_hashes`, the second returns `409 BLOB_FINALIZE_CONFLICT`. Identical hashes → idempotent `200 OK`.

## Op read path (on subscribe / catch-up)

1. Receiver sends `Subscribe` with cursors.
2. Server queries Postgres for ops since cursor, in order.
3. Server streams envelopes, fetched on-demand from object store, in `OpBatch`es.

## Durability

- Postgres durability on commit.
- S3 PUT confirmed before ack (no async fire-and-forget).
- Object store SHOULD be configured for cross-AZ replication.

## Retention

All retention numbers are unified to **30 days** (or shorter) in v1:

- Op envelopes: retained until eligible for compaction; post-compaction retention is **30 days** (see [`../04-storage/compaction.md`](../04-storage/compaction.md)).
- Op metadata rows: retained at least **30 days** regardless of compaction state, providing a recovery window if compaction logic produces a defective snapshot.
- Blob GC grace: `gc_grace_days = 30` (configurable). Tombstoned blobs are deleted from the object store on day 31. This is independent of post-compaction retention.
- Pending-blob outbox rows: 24 h (described above).
- Cursors: retained for the life of the device.
- Audit (managed): **30 days** (see [`observability.md`](./observability.md)). Self-host default: also 30 days, configurable.

Postgres durability for self-hosters: `fsync = on` is the default and required for production. The Doctor subcommand verifies it (see [`self-hosting.md`](./self-hosting.md)). Self-hosters who change it accept data-loss risk; this is documented in operator notes.

## Blob storage

| Operation | Detail |
|---|---|
| Init upload | Server returns N presigned PUT URLs, one per chunk. Upload URLs expire **1 hour** after issuance. |
| Upload chunks | Direct from client to object store. |
| Finalize | Client posts `chunk_hashes` (one `BLAKE3(ciphertext_chunk, 32)` per entry, lowercase hex, 32 chars). Server validates a single hash per chunk: for the S3 backend, via the storage backend's native ETag equivalent; for the local backend, by computing BLAKE3-of-ciphertext on receipt. Mismatch → `400 BLOB_HASH_MISMATCH` (see [`api.md`](./api.md)). There is no `plaintext_hash` server-side; that's a client-only concept. |
| Download | Server returns presigned GET URL (expires **24 hours** after issuance; clients re-request on expiry); client decrypts. |
| Delete | Client requests delete; server tombstones; GC removes after the 30-day grace period. |

Persisted blob payloads carry the uniform 5-byte magic prefix from [`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md) §3, but the server stores opaque bytes and does not introspect.

## Privacy / metadata

The server sees: blob IDs, sizes, upload/download times, per-device access patterns. It does *not* see plaintext, filename, MIME type beyond what the client explicitly sends in opaque metadata ops.

## Failure modes

| Failure | Behavior |
|---|---|
| S3 outage during write | Op rejected with `RETRY_AFTER`; client retries via outbox |
| S3 outage during read | Server returns a transient error to the relay subscriber; client retries |
| Postgres outage | Hard failure for new connections; existing connections drained |
| Push provider outage | Push degrades; clients still pull on next foreground |

## Self-host filesystem layout

```
data/
├── meta.db              # SQLite
├── ops/<stream>/<op>    # envelope files
└── blobs/<blob>/<chunk> # blob files
```

Backup is `tar` of the `data/` dir while paused (or with a snapshot mechanism).
