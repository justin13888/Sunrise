---
status: accepted
---

# Offline Queue (Outbox)

Locally-generated ops are queued for transmission while offline.

## Model

- On every locally-emitted op:
  - Insert into `ops` (immediately applied to materialized state).
  - Insert into `outbox` with `attempts=0`, `next_retry_at=now`.
- Background sync task drains the outbox when connectivity is available.
- Acks remove rows from `outbox`. The op stays in `ops`.

## Backoff

Backoff is **per-batch** (`batch_id`), not per-op or global. The `outbox.attempts` column is per-row. Formula:

```
delay_ms = min(60_000, 1_000 * 2^(min(attempts, 6))) * jitter(0.8, 1.2)
```

A single failed batch does not block subsequent batches' first attempt; per-batch backoff serializes only that batch's retries.

Outbox retries are not per-op timers — the sync task runs with one connection, and on success the entire outbox is drained.

## Idempotency

`op_id` is unique. If a connection acks an op then we lose the ack and re-send, the server sees the duplicate and re-acks. Receivers also see the duplicate and dedup by `op_id`. The same dedup applies at the batch level via `batch_id`.

## Ack handling (transactional)

When the client receives `Ack { batch_id, applied_seq_range }`, it:

1. Deletes the matching outbox row.
2. Marks the corresponding op rows `applied_at = now` (if not already applied locally).

Both happen in one SQLite transaction. If the client crashes between server-side persistence and the local outbox-row delete, the next OpBatch send is dedup'd server-side by `batch_id`, returning the same Ack; the client deletes the row on the second pass.

## Bounded outbox

There is no hard cap on outbox size; a user can be offline for months and accrue many ops. However:

- Local writes are not throttled by outbox size.
- A daily housekeeping job warns the user (in-app banner) if outbox > 10k ops or > 50 MB.

## Optimistic UI implications

Because local commit is the source of truth from the user's POV, *every* feature behaves as if the op were already synced. There is no "this hasn't been saved to the cloud yet" warning shown to the user — a misleading framing for local-first.

The exception: when an op references a peer-side resource that hasn't propagated yet (e.g. accepting a share that the granter hasn't yet pushed), the UI shows "waiting for peer."

## Crash safety

The outbox is in SQLite, transactional with the op insert. A crash mid-commit either commits both `ops` and `outbox` rows, or neither.

## Network changes

The OS can hint network changes:

- Wi-Fi connect / disconnect
- Cell reachability change

The sync layer subscribes to these signals (per-platform; provided by the UI shell to the core via callbacks) and triggers an immediate drain attempt rather than waiting for the next backoff tick.
