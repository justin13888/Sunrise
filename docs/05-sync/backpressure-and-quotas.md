---
status: proposed
---

# Backpressure and Quotas

> **Status: proposed. Not scheduled for v1.**
> [ADR-0027](../11-adr/0027-v1-self-host-first.md) places per-account quotas
> after v1. This document is the design of record for that work, not a
> description of anything that ships.
>
> **What exists in the tree:** nothing. No quota accounting, no `Throttle`
> frame, no rate-limiting middleware
> ([`../06-server/api.md`](../06-server/api.md) §Rate limits), and no
> `AUTH_RATE_LIMITED` on the typed error surface —
> [`wire-protocol.md`](./wire-protocol.md)`:233-238` lists it among the nine
> names the enum does not contain.
>
> **Why it is not v1:** quotas presuppose plan tiers, and plan tiers presuppose
> billing; ADR-0027 defers all three. What v1 enforces instead is a small set of
> fixed operator constants that need no per-account state: a 2 MiB request body
> (`crates/sunrise-server/src/config.rs:76-77`), a 1 MiB ciphertext chunk /
> 4096 chunks / 100 MB blob (`api/blobs.rs:53,57,61`), and 30-day / 256 MiB
> per-channel relay-log retention (`relay_log.rs:54,63`).
>
> **What holds regardless:** the backpressure *shape* below — clients respecting
> a server signal rather than retrying blind — is the design any future limit
> would use. The numbers are not citable from an `accepted` spec.

Sync must not become a denial-of-service vector. The server enforces quotas; clients respect backpressure signals.

## Server-to-client backpressure

If a client outpaces the server's processing, the server sends a `Throttle` control message:

```
Throttle { retry_after_ms, scope: "stream" / "account", reason }
```

Client pauses outbound `OpBatch` sends for `retry_after_ms`, optionally drops compression to lighter mode, and notes the throttle in diagnostics.

## Client-to-server backpressure

If the server is sending more ops than the client can apply (rare but possible during initial sync of a heavy Stream), the client throttles by:

- Slow-acking `OpBatch` (server waits for ack before sending more).
- Optionally requesting a snapshot instead of full op history.

## Quota exceeded behavior

- Local writes still succeed. The user's experience does not degrade.
- The outbox drains to a stop; UI shows "Sync paused: storage limit reached."
- The user is offered: upgrade plan (managed), self-host migration, prune attachments, archive old Streams.
- Pruning attachments drops blobs; the metadata remains; pruning is reversible if the user re-uploads.

## Abuse handling

- A misbehaving (or compromised) client that floods ops gets rate-limited at the connection level after thresholds.
- Per-device signed op-rate would have a hard limit of **50 signed ops/sec per device** (averaged over a 10 s window), with the client backing off on refusal. The error code and the log event this rule used to name do not exist and are not reserved: the typed enum has no rate-limit code, and no source file emits a quota event. Whatever carries the refusal has to be chosen when the rule is built.

## Stream-level prioritization

Clients **SHOULD** prioritize foreground-Stream sync. When the outbox has ops for multiple Streams and bandwidth is tight, the client prioritizes:

1. Today-related Streams (where the user is currently focused).
2. Streams with shared peers active (presence indicates someone is waiting).
3. Other Streams.

Implementation: outbound OpBatch order favors the Stream the user is currently viewing. Servers apply no special prioritization. Failure to prioritize is a UX issue, not a correctness issue.

## Push priority

Pushes use APNs/FCM priority tiers. There are exactly **two** tiers, matching [`../06-server/push-notifications.md`](../06-server/push-notifications.md):

| Sunrise tier | APNs | FCM | Web Push |
|---|---|---|---|
| `alert` (block-start reminder, mention hand-off) | `apns-priority: 10`, `apns-push-type: alert` | `priority: high` | `Urgency: high` |
| `silent` (sync wakeup, background re-sync) | `apns-priority: 5`, `apns-push-type: background` | `priority: normal`, `time_to_live: 86400` | `Urgency: normal` |

## Self-host considerations

A self-hosted server runs at the operator's pace. It defaults to *no* per-account quotas but enforces:

- Connection rate limits (anti-DoS).
- Reasonable per-request size caps.
