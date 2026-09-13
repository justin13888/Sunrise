---
status: accepted
---

# Offline Queue (Outbox)

Locally-generated ops are queued for transmission while offline. This document
describes what is built; where it describes something that is not, it says so on
the line.

## Model

The outbox is four columns (`crates/sunrise-storage/migrations/0013_baseline.sql:79-85`):

```sql
CREATE TABLE outbox (
    op_id           BLOB PRIMARY KEY REFERENCES ops (op_id),
    stream_id       BLOB NOT NULL,
    enqueued_at_ms  INTEGER NOT NULL,
    acked_at_ms     INTEGER
);
CREATE INDEX outbox_unacked ON outbox (enqueued_at_ms) WHERE acked_at_ms IS NULL;
```

There is **no `attempts` column and no `next_retry_at` column.** Retry state is
in memory only (§Backoff), which is the single most important thing to know
about this design: a restart forgets every retry and re-drains everything
unacked.

- On every locally-emitted op: insert into `ops` (immediately applied to
  materialized state) and `INSERT OR IGNORE` into `outbox`, in the caller's
  transaction (`crates/sunrise-storage/src/sync_local.rs:42-51`). The
  `OR IGNORE` makes enqueue idempotent.
- Pending means `acked_at_ms IS NULL` (`sync_local.rs:61-62,95`), which the
  partial index above serves directly.
- An ack is a **soft** update — `UPDATE outbox SET acked_at_ms = ?`
  (`sync_local.rs:79-87`) — not a delete. The row survives, so the outbox is an
  append-only record of what was sent and when it was acknowledged.

## Backoff

One policy, in memory, used in two places
(`crates/sunrise-sync/src/backoff.rs:31-63`): initial 100 ms, doubling, capped
at 30 000 ms, jittered ×[0.8, 1.2], `max_retries = 5`.

**Reconnect.** The session loop backs off between connection attempts
(`crates/sunrise-core/src/sync_driver.rs:539` and `:571`, `ev = "sync.backoff"`).
Exhausting the policy here does *not* give up — it **cycles**. Five jittered
delays of 100, 200, 400, 800 and 1600 ms; on the sixth call `next_delay` returns
`None`, so `backoff_sleep` resets the policy and sleeps a flat, un-jittered 30 s
(`sync_driver.rs:588-594`); the attempt counter is then back at zero and the
sequence starts again at 100 ms. A client that cannot reach its relay for an hour
therefore retries roughly every 30 s in bursts of five, forever, on the reasoning
that a long-lived client should never stop trying.

One consequence worth naming because it looks like a bug and is not: the
`min(30_000)` cap inside `next_delay` (`crates/sunrise-sync/src/backoff.rs:52`)
is **unreachable on this policy**. With `max_retries = 5` the largest base is
1600 ms, so the cap never binds; the only 30 s that ever elapses is the flat
sleep on the exhausted branch.

**Per-batch retransmit.** Each in-flight `OpBatch` carries its own timer and
`Backoff`; when the deadline passes the batch frame is sent again
(`sync_driver.rs:858-887`, `ev = "sync.op.retransmit"`). Exhausting the policy
here *does* escalate: the driver tears the session down and reconnects
(`sync_driver.rs:740-755`, `SYNC_NETWORK_UNAVAILABLE`), because a batch that
went unacked through the full policy is evidence the link is not carrying ops at
all. A fresh session re-drains the outbox from scratch.

Neither timer is persisted. After a restart every unacked row is simply drained
again, at attempt zero.

## Idempotency

`op_id` is the primary key of both `ops` and `outbox`, so enqueue is idempotent
locally. Receivers dedup applied ops by `op_id`, and the
`UNIQUE (stream_id, device_id, seq)` constraint on `ops` makes re-delivery a
no-op ([`../03-crypto/audit-and-tamper-evidence.md`](../03-crypto/audit-and-tamper-evidence.md)
§Identity and replay invariants).

*Target state:* batch-level dedup by `batch_id`. `batch_id` is a client-minted
`u64` correlation id ([`wire-protocol.md`](./wire-protocol.md) §OpBatch), and the
relay does not dedup on it — it appends the frame bytes verbatim to the relay log
whether or not it has seen that `batch_id` before. Relay-side batch dedup is a
code change that has not been made.

## Ack handling

The ack is `Ack { batch_id, stream_id, server_first_seen_ms }`
(`crates/sunrise-wire-protocol/src/payloads.rs:85-94`). There is **no
`applied_seq_range` field**; earlier revisions of this file specified one.

On receipt the client stamps `acked_at_ms` on each of the batch's outbox rows.
`server_first_seen_ms` is advisory and is not persisted ([`wire-protocol.md`](./wire-protocol.md)
§Server timestamp annotation).

A crash between the relay's durable append and the local ack stamp costs
nothing: the rows are still unacked, the next session re-sends them, and the
receiving side drops the duplicates. Durability is ordered so this is the only
failure shape — the relay appends before it acks
([`wire-protocol.md`](./wire-protocol.md) §Partial OpBatch on disconnect), so an
ack never outruns the data.

## Bounded outbox

There is no hard cap on outbox size; a user can be offline for months and accrue
many ops.

- Local writes are not throttled by outbox size.
- *Target state:* a daily housekeeping job warning the user (in-app banner) if
  the outbox exceeds 10k ops or 50 MB. No such job exists; `sync_pending()`
  reports the count (`sync_driver.rs:687-688`) and nothing thresholds it.

## Optimistic UI implications

Because local commit is the source of truth from the user's POV, *every* feature
behaves as if the op were already synced. There is no "this hasn't been saved to
the cloud yet" warning shown to the user — a misleading framing for local-first.

The exception: when an op references a peer-side resource that hasn't propagated
yet (e.g. accepting a share that the granter hasn't yet pushed), the UI shows
"waiting for peer." Sharing is post-v1
([ADR-0027](../11-adr/0027-v1-self-host-first.md)), so this path is unreachable
today.

## Crash safety

The outbox row commits in the same SQLite transaction as the op insert
(`sync_local.rs:42-51`). A crash mid-commit either commits both `ops` and
`outbox` rows, or neither.

## Network changes

*Target state.* The OS can hint network changes — Wi-Fi connect/disconnect, cell
reachability — and the sync layer would subscribe to them (per-platform,
provided by the UI shell to the core via callbacks) and trigger an immediate
drain rather than waiting for the next backoff tick. No such callback exists on
the seam today; reconnection is driven entirely by the backoff timer above.
