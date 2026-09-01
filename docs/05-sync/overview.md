---
status: accepted
---

# Sync — Overview

Sync moves encrypted ops between devices that participate in the same identity (or share the same Stream across identities). The server is a delivery relay; **all merging happens on the clients**.

## Properties we guarantee

1. **Eventual consistency.** Two devices that have received the same set of ops will produce the same materialized state.
2. **No lost ops.** A locally-committed op reaches all peer devices once both online (within a bounded window measured in seconds, given a working network).
3. **Causal application.** Ops are applied in causal order on receivers; user-visible state never reflects a "future" op without its predecessors.
4. **Offline tolerance.** Devices may be offline indefinitely. On reconnect, they exchange diffs efficiently.
5. **Bandwidth proportional to changes.** Sync transfers only the new ops since the last cursor, not full state snapshots (except for compaction).

## Components

| Component | Spec |
|---|---|
| Merge engine (entity LWW) | [`conflict-resolution.md`](./conflict-resolution.md), [ADR-0014](../11-adr/0014-entity-level-lww-merge.md) |
| Per-field merge (deferred design) | [`crdt-design.md`](./crdt-design.md) *(proposed)* |
| Wire protocol | [`wire-protocol.md`](./wire-protocol.md) |
| Transport (SSE + typed POST per [ADR-0023](../11-adr/0023-sse-sync-transport.md); WebSocket is the implementation being replaced) | [`transports.md`](./transports.md) |
| Conflict resolution policies | [`conflict-resolution.md`](./conflict-resolution.md) |
| Presence | [`presence.md`](./presence.md) *(proposed)* |
| Offline outbox | [`offline-queue.md`](./offline-queue.md) |
| Single-user multi-device | [`multi-device.md`](./multi-device.md) |
| Cross-user shared docs | [`shared-documents.md`](./shared-documents.md) *(proposed)* |
| Backpressure & quotas | [`backpressure-and-quotas.md`](./backpressure-and-quotas.md) *(proposed)* |

## Sync state machine (per device, per Stream)

```
   ┌─────────────┐  connect ok    ┌─────────────┐
   │ Disconnected│ ────────────▶  │  Catching   │
   └─────────────┘                │     up      │
        ▲                         └──────┬──────┘
        │ network fail / quota           │ caught up
        │                                ▼
        │                         ┌─────────────┐
        └─────────────────────────│   Live      │
                                  └─────────────┘
                                       │
                                       │ disconnect
                                       ▼
                                Disconnected
```

`Catching up` and `Live` are both functional states from the user's perspective; the indicator differs.

## Latency targets

| Action | p50 | p95 |
|---|---|---|
| Local commit visible in UI | <16ms | <50ms |
| Op visible on a peer device on same network | <500ms | <2s |
| Op visible on a peer over LTE | <2s | <8s |
| Op visible on a phone awoken by push | <5s | <15s |

## Failure modes and behavior

| Failure | Behavior |
|---|---|
| Server unreachable | UI badges as "offline." Local commits still work. Outbox grows. |
| TLS / cert validation failure | Hard error; sync paused; user notified. |
| Auth token expired | Silent re-auth on next request; transparent to user. |
| Two devices propose conflicting LWW writes simultaneously | Merges deterministically on `(hlc, device_id, seq)`; no UI prompt. |
| A receiver finds a tampered envelope | Drop, log, surface red integrity badge. |
