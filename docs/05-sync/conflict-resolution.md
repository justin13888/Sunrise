---
status: accepted
---

# Conflict Resolution

> **Target state.** v1 resolves *every* field by entity-level LWW over
> `(ts_ms, device_id)` — the "Scalar" row below applied to the whole entity.
> The OR-Set, PN-counter, list, and RichText policies are not implemented; see
> [ADR-0014](../11-adr/0014-entity-level-lww-merge.md).

The CRDT framework deterministically merges most concurrent edits. This spec documents the *deliberate* policy choices for cases where the framework offers options.

## Default policies

| Field type | Policy |
|---|---|
| Scalar (title, due_at, priority, …) | LWW with `(ts_ms, device_id)` tiebreak |
| Set membership (contexts, blocks↔tasks) | OR-Set: add wins over concurrent remove of an *earlier* add |
| Counter (deferred_count, streak) | PN-counter (commutative add/sub) |
| List (ordered children of a Stream) | Loro List with fractional indices; concurrent inserts at the same anchor get tiebroken by `device_id` |
| Rich text (notes) | Loro RichText (CRDT character merge) |
| Map (entity itself) | Map of the above types |

## Specific decisions

### Concurrent state changes

A and B both transition a task on the same op-window:

| A → | B → | Result |
|---|---|---|
| `done` | `cancelled` | LWW: later timestamp wins. UI may show "transitioned through cancelled." |
| `done` | `done` | Same value. Idempotent. `completed_at` is LWW. |
| `done` | `in_progress` | Later wins; user can "redo" it as `done` if they intended that. |

### Concurrent due_at edits

Both set `due_at`. LWW. We log to a "merge journal" (see below) so the user can review if needed.

### Concurrent task moves across streams

Move = delete-source + create-destination. Ordering matters:

- A moves task T from S1 to S2.
- B moves task T from S1 to S3.

Result after both apply: T has been deleted from S1 (idempotent), and exists in *both* S2 and S3 — both as legitimate creations. **One is a duplicate.**

Resolution: deterministic — sort copies by `(create_op.ts_ms, create_op.device_id_lex, create_op.seq)` and keep the **last** entry; all earlier entries are auto-tombstoned. The same rule extends to N-way concurrent moves. Total ordering guarantees determinism. The merge journal records this.

`device_id_lex` is the lex byte order of the raw 16-byte `device_id` (memcmp). No base-encoding is involved.

(We considered a "moved" relation, but it explodes in scope. The duplicate-and-tombstone path is simple and correct.)

### Concurrent same-routine completion

Two devices complete occurrence O of a routine R at nearly the same time:

- Both emit `complete(occurrence_id)` op with the same `occurrence_id` (deterministically derived from `routine_id || occurrence_date`).
- Receivers see both; idempotent; PN-counter for streak increments by 1 (using op_id dedup), not by 2.

### Concurrent list reorders

Loro List handles this. Concurrent moves of the same item produce one final position, deterministic across replicas.

### Concurrent share-then-revoke

A grants share to peer P; B revokes the same share concurrently. We treat share grants and revokes as ordered by `(ts_ms, device_id)`; the later wins. Edge case: if grant wins after revoke, peer P briefly has access until A's revoke arrives — minimal exposure.

## Merge journal

For the small set of fields where LWW silently picks a winner over a non-trivial concurrent edit, we write a row to `merge_journal`:

```sql
CREATE TABLE merge_journal (
    journal_id   BLOB PRIMARY KEY,
    created_at   INTEGER NOT NULL,
    entity_ref   TEXT NOT NULL,
    field        TEXT NOT NULL,
    losing_op_id BLOB NOT NULL,
    winning_op_id BLOB NOT NULL,
    summary      TEXT
);
```

UI surfaces "X edits merged automatically this week" in the weekly review. Power users can drill in. We do **not** show every merge in real time — that's a worse UX than letting the CRDT do its job.

Cardinality: one row per `(entity_id, field, op_id_at_merge)` triple. Same field merged again creates a new row. The journal is capped at **5 000 rows** per device (FIFO eviction); it is diagnostic-only.

## Atomic batches

`OpBatch` (see [`wire-protocol.md`](./wire-protocol.md)) carries multiple ops as one transactional unit. Receiver behavior:

1. Verify and decrypt every envelope in the batch before applying any.
2. If every op's `deps` are present and applied, apply all ops in the batch in a single SQL transaction; set `applied_at` for all.
3. If any op has unsatisfied `deps`, persist the entire batch with `applied_at = NULL` and defer; reattempt on each subsequent dep arrival.
4. If any envelope fails verification (signature or AEAD), reject the entire batch; emit a sync warning.

A batch is never partially applied.

## What we never do

- Show a "conflict resolution" modal in the editing flow. Modals interrupt. Modals lose.
- Refuse a write because of a concurrent edit. The system is local-first; refusal is a sync problem, not a user problem.
