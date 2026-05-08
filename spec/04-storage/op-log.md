---
status: accepted
---

# Op Log

The op log is the canonical history. Materialized state is derivable from the log. The log is append-only (in v1; compaction is a controlled rewrite, see [`compaction.md`](./compaction.md)).

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
    deps           BLOB,
    applied_at     INTEGER,
    received_from  BLOB,                  -- device_id of relay peer (could be self)
    received_at    INTEGER NOT NULL,
    UNIQUE (stream_id, device_id, seq)
);
```

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

## Compaction interaction

See [`compaction.md`](./compaction.md). Briefly: after retention window + ack from all known devices, ranges of ops are folded into a "snapshot op" that supersedes them. Other devices apply the snapshot directly without needing the folded history.
