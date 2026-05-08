---
status: accepted
---

# Backpressure and Quotas

Sync must not become a denial-of-service vector. The server enforces quotas; clients respect backpressure signals.

## Per-account quotas (managed cloud)

| Resource | Free tier | Paid tier |
|---|---|---|
| Total stored ops | 100 MB ciphertext | 5 GB ciphertext |
| Op rate | 5 ops/sec sustained, 50 ops/sec burst | 50 ops/sec sustained, 500 ops/sec burst |
| Devices per account | 5 | 50 |
| Shared Streams (in + out) | 5 | unlimited |
| Attachment storage | 1 GB | 50 GB |
| Push notifications | 100/day | 10k/day |

Self-hosted servers can configure their own limits or disable them.

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
- Per-device signed op-rate is bounded; the server drops ops above per-device limits and surfaces the violation to the client.

## Stream-level prioritization

When the outbox has ops for multiple Streams and bandwidth is tight, the client prioritizes:

1. Today-related Streams (where the user is currently focused).
2. Streams with shared peers active (presence indicates someone is waiting).
3. Other Streams.

Ordering is best-effort; the server sees ops in the order the client sends.

## Push priority

Pushes use APNs/FCM priority tiers. Critical (an op marking a task done that the user expects to be reflected on another device immediately) gets high priority; bulk backfill uses normal/low priority.

## Self-host considerations

A self-hosted server runs at the operator's pace. It defaults to *no* per-account quotas but enforces:

- Connection rate limits (anti-DoS).
- Reasonable per-request size caps.
