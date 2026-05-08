---
status: draft
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

Per-op retry uses exponential backoff with jitter:

```
delay = min(60s, base * 2^attempts) + jitter(0..base)
base = 1s
```

But: outbox retries are *connection-level*, not per-op. We don't run 1000 individual retry timers. The sync task runs with one connection; a failed connection retries on its own backoff schedule, and on success the entire outbox is drained.

## Idempotency

`op_id` is unique. If a connection acks an op then we lose the ack and re-send, the server sees the duplicate and re-acks. Receivers also see the duplicate and dedup by `op_id`.

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
