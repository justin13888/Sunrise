---
status: draft
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

## Op write path

1. Client `OpBatch` arrives over WS.
2. Server validates: signature, size, rate limit, quota.
3. Server writes to object store (envelope) and Postgres (metadata) inside a 2PC-style outbox pattern (Postgres row written, then S3 PUT, then Postgres confirmation; failures retried).
4. Server publishes "new op" via internal pub/sub.
5. Server sends `Ack` to originating client.
6. Other connected receivers get `Push`. Disconnected receivers get a wakeup push if a token is registered.

## Op read path (on subscribe / catch-up)

1. Receiver sends `Subscribe` with cursors.
2. Server queries Postgres for ops since cursor, in order.
3. Server streams envelopes, fetched on-demand from object store, in `OpBatch`es.

## Durability

- Postgres durability on commit.
- S3 PUT confirmed before ack (no async fire-and-forget).
- Object store SHOULD be configured for cross-AZ replication.

## Retention

- Op envelopes: retained until eligible for compaction (see [`../04-storage/compaction.md`](../04-storage/compaction.md)).
- Cursors: retained for the life of the device.
- Audit: minimal; we keep aggregate metrics, not per-op access logs, beyond 14 days.

## Blob storage

| Operation | Detail |
|---|---|
| Init upload | Server returns N presigned PUT URLs, one per chunk |
| Upload chunks | Direct from client to object store |
| Finalize | Client posts hash list; server verifies via HEAD on each chunk; activates blob |
| Download | Server returns presigned GET URL; client decrypts |
| Delete | Client requests delete; server tombstones; GC removes after grace period |

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
