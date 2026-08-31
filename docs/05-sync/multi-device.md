---
status: accepted
---

# Multi-Device (Single User)

A user runs Sunrise on N devices. All N see the same data and converge.

## Topology

```
                ┌─────────────┐
                │   Server    │ relay
                └──────┬──────┘
                       │
       ┌───────────────┼───────────────┐
       ▼               ▼               ▼
   ┌───────┐       ┌───────┐       ┌───────┐
   │Phone A│       │Laptop │       │  CLI  │
   └───────┘       └───────┘       └───────┘
```

All sync goes through the server in v1. LAN / mDNS direct sync is deferred to v2+ (see [`transports.md`](./transports.md) non-goals): partition-tolerance complexity, peer-discovery security, key-distribution to peers without a central relay, and NAT traversal all add risk for marginal user benefit.

## Bootstrapping a new device

See [`../03-crypto/pairing-and-onboarding.md`](../03-crypto/pairing-and-onboarding.md). After pairing, the new device subscribes to all of the user's Streams.

## Sync cursors

Each device maintains, per (Stream, originating-device) pair, a cursor meaning "I have applied every op through this `seq`, with no holes" — a contiguous prefix, *not* the highest `seq` seen. On connect, it sends all cursors and the server delivers ops since. The distinction is a correctness invariant once the relay filters on cursors; see [`wire-protocol.md`](wire-protocol.md#a-cursor-is-a-contiguous-prefix-not-a-high-water-mark).

This means: with N devices each producing ops, every other device tracks N cursors per Stream. Manageable; cursors are tiny (16 bytes ID + 8 bytes seq).

A cursor is considered **stale** if its device has not heartbeated within 30 days, matching the compaction "known device" threshold.

### Cursor cleanup on device revoke

On `device_revoke`:

- The server deletes the cursor row immediately.
- Other devices observe the revoke and remove their local cached cursor for the revoked device on the next compaction-eligibility evaluation.
- The revoked device's `device_id` remains in `vault_meta.devices` as `revoked_at_ms` for audit; never reused.

## Per-device settings

Some user preferences are per-device, not per-identity. Examples:

- Notification preferences (each device has different OS).
- Default keyboard shortcuts (per-GUI-platform conventions differ).
- Cache size limits.
- Local-only diagnostic logging level.

These live in a per-device map within the vault, in their own subdoc, not synced. Synced settings are in a separate "settings" subdoc.

## Per-device performance characteristics

| Device class | Op apply rate | Initial sync target |
|---|---|---|
| Desktop (Apple Silicon, modern x86) | >50k ops/sec | <5s for 100k ops |
| Modern phone | >10k ops/sec | <30s for 100k ops |
| 2018-era phone | >2k ops/sec | <2 min for 100k ops |
| Web (V8, WASM) | >5k ops/sec | <60s for 100k ops |
| CLI | similar to desktop | similar to desktop |

We treat any op-apply path slower than these benchmarks as a perf bug.

## Selective sync

By default, every device gets every Stream. Exceptions:

- **CLI** can be configured to subscribe only to a subset of Streams (e.g. only "work").
- **Web** can be configured similarly for users who don't want their full vault in browser storage.
- **Mobile** has a "Lite mode" that excludes Streams marked as desktop-only (e.g. an archived Stream the user keeps for reference).

Ops for unsubscribed Streams are not delivered; if the user later subscribes, the server backfills.

## Device limits

- Soft limit: 10 devices per identity.
- Hard limit: 50 devices per identity.

Beyond that, the user is asked to revoke unused devices. Reason: cursor tracking and key-envelope re-wrapping costs are O(devices).
