---
status: accepted
---

# Sync — Overview

Sync moves encrypted ops between devices that participate in the same identity (or share the same Stream across identities). The server is a delivery relay; **all merging happens on the clients**.

## Properties we guarantee

1. **Eventual consistency.** Two devices that have received the same set of ops will produce the same materialized state.
2. **No lost ops.** A locally-committed op reaches all peer devices once both online (within a bounded window measured in seconds, given a working network).
3. **Convergent regardless of arrival order.** Receivers apply ops on arrival, and the merge state is a function of the ops applied, whatever order they came in ([ADR-0044](../11-adr/0044-per-field-ops.md)). A `Patch` names its causal `parents` ([ADR-0065](../11-adr/0065-copy-on-write-entity-versions.md)), so the fold tells an ordered write from a concurrent one without relying on delivery order. Until the set is complete, a device can show an op without its predecessors: applying in causal order is the target of `deps`, which no caller passes ([`op-log.md`](../04-storage/op-log.md)).
4. **Offline tolerance.** Devices may be offline indefinitely. On reconnect, they exchange diffs efficiently.
5. **Bandwidth proportional to changes.** Sync transfers only the new ops since the last cursor, not full state snapshots (except for compaction).

## Components

| Component | Spec |
|---|---|
| Merge engine (per-field ops, [ADR-0044](../11-adr/0044-per-field-ops.md); version DAGs, [ADR-0065](../11-adr/0065-copy-on-write-entity-versions.md)) | [`crdt-design.md`](./crdt-design.md) |
| Wire protocol | [`wire-protocol.md`](./wire-protocol.md) |
| Transport (SSE + typed POST, [ADR-0023](../11-adr/0023-sse-sync-transport.md)) | [`transports.md`](./transports.md) |
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

Two states sit outside the diagram. `Degraded` is connected but told by the
relay that ops it can no longer supply are missing. `Stopped` follows a relay
`Close` whose code the error catalogue marks not retryable (`AUTH_DEVICE_REVOKED`,
`AUTH_TOKEN_INVALID`, or a code it cannot read): unlike `Disconnected`
it does not retry, and leaves only when the credential is replaced or the app
restarts. `crates/sunrise-sync/src/state.rs` is the authority for the set.

## Latency targets

| Action | p50 | p95 |
|---|---|---|
| Local commit visible in UI | <16ms | <50ms |
| Op visible on a peer device that is online (≤ 50 ms RTT to the relay): from the authoring device's commit returning to the peer publishing the applied op on its change feed | <150ms | <300ms; **p99 <500ms**, the budget of record ([`performance-budgets.md`](../10-cross-cutting/performance-budgets.md) §Sync propagation), measured nightly by `crates/sunrise-e2e/tests/sync_latency.rs` |
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
