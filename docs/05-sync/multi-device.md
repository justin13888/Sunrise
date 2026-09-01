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

Cursors are the part of this page that is real: `sync_cursors` is a table in
`0013_baseline.sql`, cursors ride in `SubscribePayload`, and the relay filters
retained frames against them.

A cursor is considered **stale** if its device has not heartbeated within 30 days, matching the compaction "known device" threshold.

### Device revocation

*Partly implemented, and the two halves are worth separating.*

**Relay-level revocation works.** `DELETE`ing a device through the API sets
`devices.revoked = 1` and drops its push tokens
(`Store::revoke_device`, `crates/sunrise-server/src/store.rs`), and every
authenticated lookup selects `WHERE revoked = 0`, so a revoked device stops
being able to reach the relay. A device cannot revoke itself — the route
refuses it — because a device that could would be a device a thief can use to
erase the evidence of its own theft.

**Cryptographic revocation does not.** Under the implemented key schedule every
paired device holds the vault root, and the root *is* the whole key schedule, so
a revoked device that kept a copy of the ciphertext can still decrypt it.
[ADR-0024](../11-adr/0024-key-hierarchy.md) is the slice that makes revocation
real: per-`(stream, epoch)` random keys wrapped under the vault root and read
from the `stream_keys` table, plus a `device_revoke` op family, so a revocation
rotates to an epoch the revoked device cannot unwrap. It bumps `CRYPTO_SUITE_V`
and `DOC_SCHEMA_V`. Until it lands, treat revocation as "cut off from the
relay", not as "can no longer read".

### Cursor cleanup on device revoke — target state

None of this is implemented. There is no cursor row on the server to delete —
the relay holds retained frames and per-device `evicted_through` watermarks,
not subscriber cursors, which live on each client. Nothing observes a revoke and
prunes a local cursor, and the local `devices` table's `revoked_at_ms` column
(`0013_baseline.sql`) is only ever written `NULL` — both insert sites in
`sunrise-core` hard-code it and no statement anywhere sets it. The intent:

- The server deletes the cursor row immediately.
- Other devices observe the revoke and remove their local cached cursor for the revoked device on the next compaction-eligibility evaluation.
- The revoked device's `device_id` remains as `revoked_at_ms` for audit; never reused.

The `device_revoke` op ADR-0024 adds is the event the second bullet needs.

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

## Selective sync — target state

*Not implemented.* A client subscribes to the Streams it is told to and the
relay serves what it is asked for; there is no per-device Stream subset, no
Lite mode, and no backfill-on-subscribe distinct from ordinary cursor replay.
The intent:

By default, every device gets every Stream. Exceptions:

- **CLI** can be configured to subscribe only to a subset of Streams (e.g. only "work").
- **Web** can be configured similarly for users who don't want their full vault in browser storage.
- **Mobile** has a "Lite mode" that excludes Streams marked as desktop-only (e.g. an archived Stream the user keeps for reference).

Ops for unsubscribed Streams are not delivered; if the user later subscribes, the server backfills.

## Device limits — target state

*Not enforced.* `Store::active_device_count` exists and is reported as
`device_count` on the account resource, but nothing compares it to a threshold;
device registration never refuses. The intent:

- Soft limit: 10 devices per identity.
- Hard limit: 50 devices per identity.

Beyond that, the user is asked to revoke unused devices. Reason: cursor tracking and key-envelope re-wrapping costs are O(devices) — a cost that only becomes real with ADR-0024's per-device key envelopes.
