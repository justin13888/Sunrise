---
status: accepted
---

# Conflict Resolution

> **Target state.** v1 resolves *every* field by entity-level LWW over
> `(hlc, device_id, seq)` — the "Scalar" row below applied to the whole entity.
> The OR-Set, PN-counter, list, and RichText policies are not implemented; see
> [ADR-0014](../11-adr/0014-entity-level-lww-merge.md).

The merge layer resolves every concurrent edit deterministically, with no user prompt. This spec documents the *deliberate* policy choices; in v1 the single policy in force is the entity-level LWW described in the banner above.

## Default policies

| Field type | Policy |
|---|---|
| Scalar (title, due_at, priority, …) | LWW with `(hlc, device_id, seq)` tiebreak |
| Set membership (contexts, blocks↔tasks) | OR-Set: add wins over concurrent remove of an *earlier* add |
| Counter (deferred_count, streak) | PN-counter (commutative add/sub) |
| List (ordered children of a Stream) | Loro List with fractional indices; concurrent inserts at the same anchor get tiebroken by `device_id` |
| Rich text (notes) | Loro RichText (CRDT character merge) |
| Map (entity itself) | Map of the above types |

## The comparison key

The winner of any two writes to one entity is the greater of

```
(hlc, device_id, seq)
```

compared left to right, where

| Term | What it is | Why it is there |
|---|---|---|
| `hlc` | `(physical_ms, logical)` — a hybrid logical clock, envelope field 5 | An unbounded device wall clock let one skewed device win every conflict it ever entered, permanently and silently (issue #21). It also could not order two writes inside one millisecond at all. |
| `device_id` | the raw 16-byte id, memcmp, higher wins | Breaks *cross-device* ties deterministically, so every replica picks the same winner. |
| `seq` | the writer's per-`(stream, device)` counter, envelope field 4 | Reached only when two ops from the SAME device carry an equal `hlc`. The HLC's send rule makes that impossible while a device's clock state lives; it becomes possible across a process restart, when the logical counter resets. |

### The HLC rules

- **Send.** `hlc = max(local, wall_clock) + 1` in the lexicographic sense: the
  physical component takes the wall clock when the wall clock is ahead and the
  logical counter resets; otherwise the logical counter increments. A device's
  own ops are therefore strictly ordered even if its clock stalls or jumps
  backwards.
- **Receive.** `local = max(local, received, now) + 1`. After observing a peer's
  op, this device sits above it, so everything it emits afterwards sorts after
  the op that caused it. Causality without a vector clock.
- **The receiver stores the SENDER's value**, not its own post-merge reading.
  That is what makes the order replica-independent: two replicas that receive
  the same op in different orders record the same stamp for it. Storing the
  local post-merge value would make the winner depend on delivery order — the
  exact divergence LWW exists to prevent.
- **Drift bound.** An op whose `physical_ms` is more than `MAX_DRIFT_MS`
  (5 minutes) beyond the receiver's own clock is REFUSED — not applied, not
  logged, and not absorbed. Accepting it would drag the receiver's clock forward
  with the bad one and propagate the skew to every peer it talks to next. An op
  from the *past* is always accepted: that is a device coming back from a week
  offline, not a clock fault.

A fast clock therefore still wins a genuinely concurrent race, and that is
correct — someone has to. What it can no longer do is win *forever*: a peer
cannot edit an entity it has never seen, so by the time it edits, it has already
absorbed the fast device's stamp and sits above it.

See [ADR-0016](../11-adr/0016-hlc-timestamps.md).

## Specific decisions

### Concurrent state changes

A and B both transition a task on the same op-window:

| A → | B → | Result |
|---|---|---|
| `done` | `cancelled` | LWW: later timestamp wins. UI may show "transitioned through cancelled." |
| `done` | `done` | Same value. Idempotent. `completed_at` is LWW. |
| `done` | `in_progress` | Later wins; user can "redo" it as `done` if they intended that. |

### Concurrent due_at edits

Both set `due_at`. LWW, on the whole entity. Nothing is logged: the merge journal that would have recorded it was removed (see below).

### Concurrent task moves across streams

**In v1 a move is not delete-plus-create — it is one op.** `Task.stream_id`
(`crates/sunrise-domain/src/task.rs`) is a field on the entity, and moving a
task emits a single `InnerOp::TaskUpdate` carrying the whole task with its new
`stream_id`. There is no `TaskMove` variant and no delete/create pair in
`crates/sunrise-core/src/inner_op.rs`.

So two concurrent moves are just two concurrent writes to one entity, and
entity-level LWW settles them with the comparison key above: the later
`(hlc, device_id, seq)` wins, the task ends up in exactly one stream, and every
replica picks the same one. **No duplicate is ever created, so nothing needs
tombstoning.**

*Target state.* Duplicate-and-tombstone is what would be needed
if a move ever becomes a delete-source + create-destination pair — sort copies
by `(create_op.hlc, create_op.device_id_lex, create_op.seq)`, keep the last,
auto-tombstone the rest, extending to N-way. It is **not implemented**, because
the situation it resolves cannot arise. (A "moved" relation was also considered
and rejected as an explosion in scope.)

`device_id_lex` is the lex byte order of the raw 16-byte `device_id` (memcmp). No base-encoding is involved.

The `device_id` tiebreak resolves **cross-device** ties only. Two ops from the
*same* device are not concurrent — they are causally ordered by their
per-`(stream, device)` `seq`, which is why `seq` is the third term of the
comparison key. Applying the memcmp to a device's own ops would make its later
op lose to its own earlier one (`dev > dev` is false), silently discarding it on
every remote replica while the originating replica kept it.

### Concurrent same-routine completion

Two devices complete occurrence O of a routine R at nearly the same time:

- Both emit `complete(occurrence_id)` op with the same `occurrence_id` (deterministically derived from `routine_id || occurrence_date`).
- Receivers see both; idempotent; PN-counter for streak increments by 1 (using op_id dedup), not by 2.

### Concurrent list reorders

*Target state:* a fractional-index list handles this, and concurrent moves of the same item produce one final position, deterministic across replicas. In v1 the containing entity merges as a unit, so the later writer's ordering wins wholesale.

### Concurrent share-then-revoke

A grants share to peer P; B revokes the same share concurrently. We treat share grants and revokes as ordered by the same `(hlc, device_id, seq)` key; the later wins. Edge case: if grant wins after revoke, peer P briefly has access until A's revoke arrives — minimal exposure.

## Merge journal — removed

**There is no merge journal.** `merge_journal` was dropped by
[ADR-0018](../11-adr/0018-storage-baseline-reset.md) and is absent from
`0013_baseline.sql`, which records why: it was "a per-**FIELD** conflict journal
for a merge model that is entity-level … Zero writers, zero readers, one index."
Under [ADR-0014](../11-adr/0014-entity-level-lww-merge.md) there is no losing
*field* to record — the losing write is the whole entity — so the table had
nothing to say.

Its absence is asserted, not merely current: `baseline_omits_the_dead_schema`
in `crates/sunrise-storage/src/db.rs` fails if `merge_journal` reappears, so a
future migration cannot reintroduce it by copy-paste.

The consequence for the product: **an automatic merge is currently invisible.**
Nothing records that a concurrent edit lost, so the "X edits merged
automatically this week" review surface has no data behind it. Reinstating that
UI means designing a journal for an entity-level model — one row per losing
*entity version*, not per field — and an ADR superseding 0018's removal, not
restoring the schema above.

## Atomic batches

> **Target state.** The receiver applies **one envelope at a time**. The sync
> driver decodes an inbound `OpBatchPayload` and calls `Core::apply_remote`
> per envelope, each in its own vault transaction
> (`crates/sunrise-core/src/sync_driver.rs`), so a batch **can** be half-applied
> if the process dies mid-loop. `OpBatchPayload` itself is only exercised
> end-to-end by the server's own tests. Convergence survives — every apply is
> idempotent and entity-level LWW — but the all-or-nothing guarantee below is
> not one v1 provides.

`OpBatch` (see [`wire-protocol.md`](./wire-protocol.md)) carries multiple ops as one transactional unit. Intended receiver behavior:

1. Verify and decrypt every envelope in the batch before applying any.
2. If every op's `deps` are present and applied, apply all ops in the batch in a single SQL transaction; set `applied_at` for all.
3. If any op has unsatisfied `deps`, persist the entire batch with `applied_at = NULL` and defer; reattempt on each subsequent dep arrival.
4. If any envelope fails verification (signature or AEAD), reject the entire batch; emit a sync warning.

A batch MUST never be partially applied. Today one can be.

## What we never do

- Show a "conflict resolution" modal in the editing flow. Modals interrupt. Modals lose.
- Refuse a write because of a concurrent edit. The system is local-first; refusal is a sync problem, not a user problem.
