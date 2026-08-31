---
status: accepted
---

# Op Log

The op log is the canonical history. Materialized state is derivable from the log. The log is append-only (in v1; compaction is a controlled rewrite, see [`compaction.md`](./compaction.md)).

> **`deps` are specified, not implemented — and every section below that rests
> on them describes the target.** `OpLog::insert` in
> `crates/sunrise-storage/src/oplog.rs` takes a `deps: &[[u8; 16]]` and writes
> each entry into `op_dep`, and **every caller in the workspace passes `&[]`** —
> the twenty-seven `self.ops_insert` sites in `crates/sunrise-core/src/engine.rs`,
> the remote-apply path in the same file, and the storage crate's own tests. The
> loop therefore never runs and `op_dep` has **zero rows and zero readers**.
>
> Downstream: causal apply ordering (§Op application order) reduces to "apply on
> arrival", because "all deps present" is vacuously true; there are no orphans
> to hold, so §Orphan ops describes a state nothing can reach; and the
> `op_orphan_archive` table and the `db.dep.orphan` event **do not exist** —
> neither name appears anywhere in `crates/`.
>
> Convergence does not depend on any of it. Under
> [ADR-0014](../11-adr/0014-entity-level-lww-merge.md) every op is full-state
> and merges by `(hlc, device_id, seq)`, so out-of-order arrival converges
> regardless; what deps would buy is *intermediate* states a user would
> recognize, not a different final state.

## Schema

```sql
CREATE TABLE ops (
    op_id          BLOB PRIMARY KEY,
    stream_id      BLOB NOT NULL,
    device_id      BLOB NOT NULL,
    seq            INTEGER NOT NULL,
    ts_ms          INTEGER NOT NULL,
    envelope       BLOB NOT NULL,
    inner_kind     TEXT NOT NULL,
    target_kind    TEXT NOT NULL,
    target_id      BLOB,
    applied_at     INTEGER,
    received_from  BLOB,                  -- device_id of immediate sender; see below
    received_at    INTEGER NOT NULL,
    UNIQUE (stream_id, device_id, seq)
);
```

`deps` are stored in a separate `op_dep` table with an index for fast satisfaction queries; see [`local-database.md`](./local-database.md). The table and its `op_dep_reverse` index exist in `0013_baseline.sql`; nothing writes them — see the banner above.

`received_from` is the `device_id` of the immediate sender (the relay's own device id if forwarded from server, or the originating device id if directly synced). It is set once at first arrival on this device and never overwritten by intermediate hops. It is used only for diagnostics and to detect "this op came from a non-paired peer" anomalies.

## `ts_ms` vs ULID embedded timestamp

`op_id` is a ULID; its embedded timestamp is the device's `ts_ms` at emit time. They are equal by construction at emit. `ts_ms` is what gets serialized in the envelope; the ULID's embedded ts is for sortability only. Clock-skew handling is per [`../03-crypto/audit-and-tamper-evidence.md`](../03-crypto/audit-and-tamper-evidence.md).

## Access patterns

| Pattern | Index used |
|---|---|
| "Apply all unapplied ops in dep order." | scan WHERE `applied_at IS NULL`, sort by `(stream_id, deps resolved)` |
| "Send ops since cursor X for stream S to a peer." | `idx_ops_stream_device`, range from `(S, *, cursor+1)` |
| "Find op by ID." | PK |
| "Find all ops referring to entity E." | `idx_ops_target` (added below) |

```sql
CREATE INDEX idx_ops_target ON ops(target_id) WHERE target_id IS NOT NULL;
```

## Op application order

CRDTs allow concurrent ops; a strict total order isn't required for *merge correctness*. But applying ops in **causal order** (deps satisfied first) is required for the materialized state to be consistent with what the user expects.

Algorithm on receive:

```
for each new op O:
    insert into ops table with applied_at = NULL
    if all deps are present and applied:
        apply mutation to materialized state
        set applied_at = now
        for each op P with applied_at = NULL whose deps are now satisfied:
            recurse
```

Cycles in deps are impossible because deps are by op_id which depends on prior op_ids; a cycle would require time travel.

## Orphan ops

An op whose deps remain unsatisfied for > **48 hours** is logged at `warn` (`db.dep.orphan`) and the user is informed via a banner ("Some changes from `<device_h>` are waiting on data we haven't received."). After **30 days**, the orphan is moved to a `op_orphan_archive` table and removed from the active op-log; it is never auto-applied with missing deps. A user-initiated "force resync this Stream" operation re-fetches all ops for the Stream from the relay; if the missing dep is found, archived orphans become eligible.

## Outbox

Locally generated ops are inserted into `ops` with `applied_at = now` (locally applied immediately) AND into `outbox` for sync transmission. The sync layer reads from `outbox`, sends, and on ack removes the entry.

## Op log size

For a heavy user (10k tasks, 5 years), expect ~500k–1M ops, ~300 MB on disk before compaction. Compaction (after a configurable retention window) brings this to a fraction.

## Read snapshots

Querying current state is *not* done by replaying the log; it's done by reading the materialized tables. The log is for sync, audit, undo (within retention), and recovery.

## Undo

Undo is implemented by emitting an *inverse* op, not by deleting the original. This preserves the property that the log is append-only and that other devices converge to the same state.

The "undo stack" is a UI concept; it tracks a recent run of locally-emitted ops (not synced ones from peers) and emits inverses on user request.

## Op ID generation

ULID, generated client-side. Carries a millisecond-precision timestamp prefix for sortability without requiring it for correctness.

## Cross-stream ops

Ops belong to exactly one Stream — they live in that Stream's op log. Operations that *appear* cross-stream (moving a task) are modeled as a delete in the source Stream + a create in the destination Stream, both ops emitted atomically by the same device.

A move emits two ops: `delete_in(src_stream)` and `create_in(dst_stream)`. Both are submitted in one OpBatch; the destination op's `deps` includes the source op's id. A receiver that gets the destination op without the source op holds it as orphan (above) until the source arrives. The relay forwards both atomically; partial fan-out is a relay bug, not a normal condition.

> **A move is one op today, not two.** `Command::PromoteToStream` lowers to a
> `TaskPatch { stream_id: Some(dst), .. }` and goes through `update_task`, which
> emits a **single `task.update`** carrying the whole moved Task. Its `seq` is
> taken from the **destination** Stream (`self.next_seq(db, task.stream_id)`,
> read after the patch applied), so **the source Stream's log never records the
> departure** — there is no `delete_in(src)`, no `OpBatch`, and no dep to
> satisfy.
>
> What that costs is not convergence: the Task's `stream_id` is a field on a
> full-state entity, so a replica that applies the update moves the Task
> whatever order the ops arrive in. What it costs is **the source Stream's
> history**, which matters for two things this document already promises —
> §Read snapshots' "the log is for sync, audit, undo", and a per-Stream fetch,
> which now returns a log with no trace that a Task ever left. A peer subscribed
> to the source Stream and not the destination sees the Task simply stop being
> updated.
>
> The two-op form above is the target and needs `deps` first, since it is the
> whole reason a receiver must not apply the create before the delete.

## Compaction interaction

See [`compaction.md`](./compaction.md). Briefly: after retention window + ack from all known devices, ranges of ops are folded into a "snapshot op" that supersedes them. Other devices apply the snapshot directly without needing the folded history.
