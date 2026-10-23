# 0065 — Entities are copy-on-write version DAGs; rows are a fold, and every conflict is kept and named

**Status:** accepted

**Amends** [ADR-0044](./0044-per-field-ops.md). Its op shape, its four field
types, its legacy reading (§7) and its unknown-field rules (§8) stand. This
record adds four things to them:

1. causal parents on every `Patch`;
2. a defined outcome for every concurrent write that cannot merge;
3. retention of the writes that lose;
4. the rule that a row is written only by the fold.

It also turns ADR-0044's emission on: from this record, commands write `Patch`
and nothing else (§7).

**Keeps** the comparison key of [ADR-0016](./0016-hlc-timestamps.md),
the parking and feature gate of
[ADR-0045](./0045-schema-identity-and-feature-gating.md), the per-device hash
chains of [ADR-0043](./0043-commit-tree.md) and the compaction floor of
[ADR-0059](./0059-client-op-log-compaction.md).

**Tracked by** [#447](https://github.com/justin13888/Sunrise/issues/447)
(commands write full-state ops), and closes the merge halves of
[#334](https://github.com/justin13888/Sunrise/issues/334) (concurrent defers),
[#331](https://github.com/justin13888/Sunrise/issues/331) (streak keys),
[#333](https://github.com/justin13888/Sunrise/issues/333) (dependency cycles)
and [#441](https://github.com/justin13888/Sunrise/issues/441) (rebuild from the
op log).

## Context

The owner's requirement is that **tasks are copy-on-write**. Tasks should be
conflict-free wherever the schema allows. Where the schema is too rich for
that, the outcome must be well defined *in the protocol*, not left to arrival
order or to whichever build happened to write last.

The tree falls short of that in three ways.

- **Rows are edited in place.** A local command reads the row, changes the
  struct, `UPDATE`s the row and appends a full-state op
  (`crates/sunrise-core/src/engine/task.rs#update_task`). The receive path
  folds into per-field state and projects. The two paths reach the row by
  different code, so "the row is a function of the ops" is a property of one
  path only.
- **A write that loses leaves no trace.** `write_register` keeps the winning
  value and stamp and discards the loser
  (`crates/sunrise-core/src/engine/merge/state.rs`). Two devices that retitle a
  task while offline get one title back, and nothing anywhere records that
  there were two.
- **Concurrency is guessed from clocks.** ADR-0044 compares
  `(hlc, device, seq)` and nothing else. So it cannot tell "B overwrote A's
  title having seen it" from "B wrote a title without knowing A had". The
  first is an edit. The second is a conflict. They need different outcomes:
  a status transition, a delete, a generated occurrence and a user's edit each
  mean something different depending on which case it is.

## Decision

### 1. A version is an op; an entity is a DAG of them

Every mutation of a mergeable entity (`Merge::Lww` in
`crates/sunrise-id/src/registry.rs`: Task, Stream, Context, Routine, Block,
Attachment, Preferences) is an immutable **version**. A version is one
`Patch` op as ADR-0044 §1 defines it, plus one key:

```cddl
patch = {
  "ref":     entity-ref,
  ? "create":  true,
  ? "origin":  "generated",
  ? "parents": [* op-ref],     ; the entity's heads the writer had applied (§2)
  ? "group":   group,          ; atomic multi-entity commit (§6)
  "fields":  { + field-name => field-op },
  unknown-fields
}
group = { "id": bstr .size 16, "size": uint .ge 2 }
```

- **The version id is the op's `op-ref`.** That is `(stream_id, device_id,
  seq)`, which is already unique and already hash-chained per device
  (ADR-0043). No second identifier is invented. The op log is the version
  store.
- **Nothing is ever updated in place.** A version, once sealed, is never
  edited, never replaced and never removed, except by compaction under §8.
- **An entity's *heads* are its versions that no other applied version names
  as a parent.** A device that has applied every version has one head after
  its own next write.

### 2. Parents make concurrency a fact, not a guess

A writer sets `parents` to the entity's heads *as it had applied them* when it
wrote. Two versions `a` and `b` of the same entity are:

- **ordered**, `a ≺ b`, when `a` is reachable from `b` through `parents`;
- **concurrent**, `a ∥ b`, otherwise.

`parents` is read and kept by every build. A build from before this record
knows `Patch` from ADR-0044 and stores `parents` as an unknown key, so it
round-trips losslessly.

A version with no `parents` key is concurrent with every version except its
own ancestors. That covers every legacy full-state op and every ADR-0044
`Patch` written before this record. They have none, and so none.

Ancestry is computed locally. The engine stores each version's parents in
`entity_versions`. It keeps a per-entity **causal frontier**: for each device,
the greatest `seq` of that device's versions reachable from a version. That
makes `a ≺ b` an O(devices) comparison. Both tables are rebuildable from
`ops`.

### 3. Every field class has one defined outcome under concurrency

The registry declares a **conflict class** next to each field's CRDT type.
The class is part of the schema fingerprint (ADR-0045 §1), so changing it is a
`DOC_SCHEMA_V` bump.

| Class | Fields (current entities) | Ordered writes | Concurrent writes |
|---|---|---|---|
| **merge** | OR-sets (`contexts`, `blocked_by`, `Block.tasks`, `skipped_keys`, `streak_keys`), PN-counters (`deferred_count`), map fields (`Preferences.values`, day schedule) | ADR-0044 §3 | ADR-0044 §3. All writes survive, so there is no conflict. |
| **quiet** | `priority`, `energy`, `estimate_minutes`, `sort_key`, `archived`, presentation-only fields | later wins | Register by `(hlc, device, seq)`. The loser is kept (§4) but not surfaced. |
| **tracked** | `title`, `body`, `stream_id`, every `SunriseTime` field, `scheduling_constraints`, `rrule`, `timezone`, template fields | later wins | Register by `(hlc, device, seq)`. The loser is kept (§4) **and surfaced** as a conflict on the entity. |
| **state** | `Task.state` (with `completed_at` coupled at read, ADR-0044 §6) | later wins | **Terminal over open:** `done` and `cancelled` beat `todo`, `in_progress`, `waiting` and any unknown non-terminal state. Two terminals are decided by `(hlc, device, seq)`. The loser is kept and surfaced. |
| **tombstone** | `deleted` | later wins (restore is an ordinary later write) | **Delete wins** over a concurrent `deleted = false`. A concurrent write to any *other* field never resurrects (ADR-0044 §5). The write is applied to the entity's state, and the entity carries an *edited while deleted* conflict naming it. |

**Generated writes yield.** A version with `"origin": "generated"` (routine
materialization, template propagation) never beats a concurrent user version
on any register. It still beats a user write it descends from: propagation
that has seen an edit and still writes the field is deliberate, and ADR-0044
§3's origin check already stops it touching fields a user changed. Two
generated creates of one deterministic id are equal by construction and fold
to one.

This replaces the clock race described in `conflict-resolution.md`. Under
it, a device that comes back online and materializes an occurrence after a
peer completed it could reset that occurrence to `todo`. Its generated create
is concurrent with the peer's completion, so it yields.

Unknown fields carry no declared class and are folded as **quiet**. A newer
build that declares one tracked keeps its own conflicts. The outcome is the
same on every build, because quiet and tracked pick the same winner. They
differ only in whether the loser is surfaced.

### 4. The losers are kept

For each register, the merge state keeps the winner **and every concurrent
loser that no later version descends from**. That is the register's
*unresolved set*.

- **When a write descends from every member of a field's unresolved set, the
  set is cleared.** The winner then simply stands. Editing a field on a device
  that has seen both sides is therefore a resolution by itself.
- **`Query::Conflicts { entity? }`** returns, per entity and field, the
  winning value and stamp and each unresolved loser's value, stamp, device and
  origin. Quiet fields are included only when asked for.
- **`Command::ResolveConflict { entity, field, value }`** writes a `Patch`
  whose `parents` are all current heads and which sets the field to `value`.
  `value` may be the winner, a loser or something new. By the rule above, this
  clears the set everywhere it is applied.
- **A tombstone conflict resolves** by `Restore` (a later `deleted = false`)
  or by `ResolveConflict { field: "deleted", value: true }`, which
  acknowledges the delete.

The unresolved set is a pure function of the versions applied. Every replica
that has applied the same versions reports the same conflicts.

### 5. A row is a fold, written only by the fold

`tasks`, `streams`, `contexts`, `routines`, `blocks`, `attachments` and
`preferences` rows are a **cache**: `row = project(fold(versions))`.

- A local command builds a `Patch`, appends it to the op log, and applies it
  **through the same merge path a received op takes**. No command writes an
  entity row directly.
- `Core::rebuild_projection()` truncates every projection and merge-state
  table and refolds from `ops` in canonical `(hlc, device, seq)` order. It must
  give byte-identical rows, merge state and conflict sets. The engine tests
  assert this after every scenario.
- Derived fields are computed by `project`, never stored by a write: `updated_at`
  (ADR-0044 §10), `Task.blocks`, and the streak counters derived from
  `streak_keys` and `skipped_keys` (#331).
- Read-time invariants (ADR-0044 §6) are applied in `project`, including
  `blocked_by` cycle suppression in oldest-edge-first order (#333) and the
  `Stream.parent_id` cycle rule.

### 6. A group is applied whole or not at all

Some commands write more than one entity and are only correct together:

- merging two calendar blocks into one;
- a routine re-key that keeps edited occurrences and tombstones untouched ones;
- a multi-task triage action (#335).

Every `Patch` such a command writes carries the same `group.id` and the
group's `size`.

- A receiver that has applied fewer than `size` members of a group holds the
  others **pending**: stored, sealed, not folded and not projected. When the
  last member arrives, all of them are folded in one transaction.
- Pending is not parked. A pending group needs no upgrade, only the rest of
  its members, and the sync driver keeps requesting them as it already does
  for gaps.
- A local group is folded in the transaction that appends it.

### 7. Rollout: `core.field_ops` turns on, and full-state writes stop

`core.field_ops` (ADR-0044 §9) is added to `FEATURES`. It now names the whole
model: `Patch` with `parents` and `group`, the conflict classes and the
precedence rules of §3. Before a device writes its first entity version into a
vault, it emits `VaultRequires { core.field_ops }` through
`Engine::require_features`.

- A build without the feature parks nothing it can read. It keeps applying
  `Patch` under ADR-0044. It refuses local writes (ADR-0045 §8) and shows
  *Update Sunrise to edit*. So it can never write a full-state op over a
  versioned entity.
- A build with the feature never emits a full-state `*Create`, `*Update` or
  `*Delete` again. Those kinds stay decodable forever (ADR-0044 §7).
- The bump is `DOC_SCHEMA_V` with a new fingerprint in
  `schemas/doc-schema/registry.json`, plus a `STORAGE_V` migration that adds
  `entity_versions`, `entity_frontier`, `register_losers` and `pending_groups`.
  It then rebuilds the projection. By §2 and ADR-0044 §7, a vault with only
  legacy history projects exactly as before.

### 8. Compaction keeps the frontier and the conflicts

ADR-0059 compaction folds ops below the acknowledged floor into a snapshot.
From this record, a snapshot of an entity carries:

- its merge state;
- its heads, as op-refs;
- its causal frontier;
- every unresolved loser.

A version above the floor whose parents were compacted away still orders
correctly against the frontier, and a conflict outlives the ops that caused
it.

## Alternatives considered

| Option | Why rejected |
|---|---|
| **Keep ADR-0044 as written: field ops, stamp-only LWW, losers discarded** | Conflict-free fields are handled. The rest are decided by clock skew, and nothing records what was lost. That does not meet the requirement that non-mergeable outcomes be defined in the protocol. |
| **A separate version table with its own ids beside the op log** | It duplicates the op log, which is already immutable, signed, hash-chained and uniquely keyed. Two stores of the same history can disagree. |
| **Whole-entity snapshot versions that fork on conflict** | Every concurrent write becomes a fork that needs a merge step, including the ones ADR-0044 already merges without loss. History is coarse, and older builds cannot read it. |
| **Version vectors on the wire instead of parent op-refs** | The vector grows with every device that ever wrote, and it does not survive a device's revocation cleanly. Parents are bounded by the number of concurrent heads, and the frontier is derived locally. |
| **Ask the user to resolve every tracked conflict before showing a value** | The UI would block on sync. A deterministic winner is shown immediately, and the conflict is a badge, not a gate. |
| **Open over terminal on state** | A completion is the stronger statement of intent. A concurrent reopen that loses is kept and surfaced, so the user can make it win in one action. |

## Consequences

- **Concurrent edits to different fields survive between two current
  devices** (#447). Concurrent defers count twice (#334). Concurrent
  completions of different routine occurrences each add their streak key
  (#331).
- **A conflict is a first-class, synced, deterministic thing.** The CLI lists
  and resolves conflicts (`sunrise conflicts`). The Apple client gets the same
  query through the bindings; its UI is follow-up work.
- **Undo of delete is a restore** (ADR-0044 §5). `crates/sunrise-client-core`
  undo offers it.
- **Every engine scenario test also asserts the rebuild property.** The
  convergence proptest adds three properties:
  1. conflict sets are equal across replicas;
  2. a generated version never beats a concurrent user version;
  3. a group is never half-visible.
- **Storage grows by one row per version in `entity_versions`**, plus losers
  that are not yet resolved. Compaction bounds both (§8).

## What would force revisiting this

1. **Collaborative rich text in `body`.** A sequence CRDT field-op kind would
   move `body` from *tracked* to *merge*, under its own ADR (ADR-0044
   §Revisit 1).
2. **Shared streams across users.** A conflict's "device" would need to
   become a person, and resolution might need permission.
3. **Unresolved sets that grow without bound in practice.** That would be
   telemetry showing users never resolve. It would call for an expiry rule
   that keeps the newest N losers, under its own ADR.
