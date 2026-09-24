# 0044 — Entity ops write fields, not whole entities, and each field merges by its own CRDT type

**Status:** accepted

**Supersedes** [ADR-0014](./0014-entity-level-lww-merge.md) on the merge model,
which was full-state entity ops with one last-writer-wins survivor per row.
ADR-0014's other two decisions stand: the deletion of `sunrise-crdt` and Loro
(no CRDT library enters the workspace here either), and its record of why a
delete op must not be a bare id. ADR-0014 §"Per-field LWW, revisited" set the
trigger for this record as "the first partial-update command path", and asked
that the ADR cover op shape, `updated_at` semantics and what replay means.
Those are §Decision 1 (op shape) and §Decision 10 (`updated_at` and replay)
below.

**Keeps** the comparison key from [ADR-0016](./0016-hlc-timestamps.md),
`(hlc, device_id, seq)`, and the HLC restore from
[ADR-0036](./0036-hlc-restored-at-open.md).

**Depends on** [ADR-0045](./0045-schema-identity-and-feature-gating.md). A new
op family is only safe to ship once parking, lossless unknowns and
`vault_requires` exist. Also depends on the entity registry ([#328](https://github.com/justin13888/Sunrise/issues/328)), which
gives each field a stable name and a declared CRDT type.

**Tracked by** [#319](https://github.com/justin13888/Sunrise/issues/319). It is ranked in phase P1 of
[`../roadmap.md`](../roadmap.md).

## Context

The owner's invariant is:

> **Merging vaults across client versions MUST NEVER break and MUST NEVER lose
> data.**

Full-state LWW breaks the "never lose" half on ordinary use, even between two
devices running the same build:

- `TaskCreate`, `TaskUpdate` and `TaskDelete` carry the whole `Task`
  (`crates/sunrise-core/src/inner_op.rs#InnerOp`).
  `crates/sunrise-core/src/engine/lww.rs#lww_wins` compares one
  `LwwStamp` per row, and
  `crates/sunrise-core/src/engine/lww.rs#materialize_remote` writes the
  winner's full state over the row.
- **Concurrent edits to different fields lose one side.** If device A renames a
  task while device B sets its priority, one of the two edits is gone from
  every projection.
- **Concurrent additions to a set lose one side.** `Task.contexts` and
  `Task.blocked_by` (`crates/sunrise-domain/src/task.rs#Task`) and
  `Block.tasks` (`crates/sunrise-domain/src/block.rs#Block`) are documented
  in the code as OR-Sets. The routine's `skipped_keys` and `streak_keys` are
  specified as OR-sets in
  [`../02-domain/routines-and-recurrence.md`](../02-domain/routines-and-recurrence.md)
  §Merge mapping. All of them are materialized as one value, so two devices
  that each add a context keep only one of the two.
- **Concurrent increments count once.** `deferred_count` is documented as a
  PN-counter and stored as an integer the winner overwrites. Two devices each
  deferring the same task leave it at `+1`.
- **An older build overwrites what it cannot represent.** Every write is
  full-state, so an older build that re-saves an entity re-emits it without
  anything it failed to preserve. The preservation failures are an enum value
  it degraded, a nested field it dropped, or an unknown time kind it coerced
  ([#321](https://github.com/justin13888/Sunrise/issues/321), [#322](https://github.com/justin13888/Sunrise/issues/322)). That op then wins on every replica. Per-field ops shrink
  this blast radius to the fields the older build actually edited.

ADR-0014 explained why per-field LWW over *full-state* ops is meaningless: a
per-field stamp cannot tell "B did not change the title" from "B wrote the
title it already had". The op shape has to change first. That is the core of
this decision.

## Decision

### 1. One new op family: `Patch`, which carries only the fields a command wrote

```cddl
; InnerOp variant, externally tagged like every other: { "Patch": patch }
patch = {
  "ref":     entity-ref,            ; the entity; its kind comes from the id prefix
  ? "create": true,                 ; this op creates the entity (§4)
  ? "origin": "generated",          ; written by generation or template propagation; absent = user (§3)
  "fields":  { + field-name => field-op },
  unknown-fields                    ; preserved, per ADR-0045
}

field-name = tstr                   ; the registry's stable name; never reused (§2)

field-op = set-op / set-edit / inc-op / map-op

set-op   = { "set": any }           ; LWW register write; null clears an optional field
set-edit = {                        ; OR-set edit; at least one of the two keys
  ? "add":    [+ any],              ; each element is tagged with THIS op's op-ref
  ? "remove": [+ [any, [+ op-ref]]] ; element, plus the add-tags of it that the writer had observed
}
inc-op   = { "inc": int .ne 0 }     ; PN-counter delta
map-op   = { "map": { + tstr => set-op } } ; per-key register writes; a null set tombstones the key

op-ref = [ bstr .size 16,           ; stream_id of the op
           bstr .size 16,           ; device_id
           uint ]                   ; seq
```

`entity-ref` and `unknown-fields` are defined once in
[`../02-domain/overview.md`](../02-domain/overview.md) §Common CDDL types.

- **Every op carries only the fields its command wrote.** "Set the priority"
  emits `{"priority": {"set": 3}}` and nothing else. A command MUST NOT include
  a field it did not change. Doing so re-creates exactly the overwrite this
  record exists to remove.
- **The stamp is the envelope's.** Every field write in one op is stamped with
  the enclosing envelope's `(hlc, device_id, seq)`. The receiver stores the
  sender's stamp, never its own post-merge reading, per ADR-0016. The one
  exception is not a write: moving an entity between key domains re-seals its
  existing field state under the destination with each field's original stamp
  ([ADR-0046](./0046-optional-stream.md) §2), and a receiver merges that state
  as the original writes.
- **The tag of an OR-set add is the op's `op-ref`.** The triple
  `(stream_id, device_id, seq)` is unique per op, because the replay invariant
  in [`../03-crypto/audit-and-tamper-evidence.md`](../03-crypto/audit-and-tamper-evidence.md)
  makes it so. `seq` alone is not unique, because a task that moves between
  streams has ops in more than one `(stream, device)` sequence.
- **Field ops are self-describing.** A reader can merge a field it has never
  heard of, because the op says whether it is a register write, a set edit or a
  counter delta, or a map of per-key register writes. This is what makes
  unknown fields safe (§8).
- **The payload is sealed exactly as today.** Envelope, AAD and signature are
  unchanged. The new variant is a `DOC_SCHEMA_V` bump and the structural
  feature `core.field_ops` (§9).

### 2. Field identity comes from the entity registry

A field is named by the string key it already has in the entity's CBOR map.
Examples are `"title"`, `"contexts"` and `"deferred_count"`. The entity
registry ([#328](https://github.com/justin13888/Sunrise/issues/328)) declares, for each field:

- its name
- its value type
- its CRDT type: `register`, `map`, `orset` or `counter`
- its default

The schema fingerprint (ADR-0045) hashes those declarations. Two rules follow:

- **A name is never reused.** A rename is an add plus a removal, as
  [`../02-domain/schema-versioning.md`](../02-domain/schema-versioning.md)
  already requires. A removed name stays reserved in the registry forever.
- **A field's CRDT type never changes.** Moving `scheduling_constraints` from
  one whole-list register to an OR-set of constraints is a new field with a new
  name. It is not a retype.

String names were chosen over integer ids for two reasons. Legacy full-state
ops (§7) and the existing `unknown` maps are already keyed by these strings, so
the legacy mapping is the identity function. And the bytes saved by integer ids
are small against the payload.

### 3. The four field types and their merge rules

| CRDT type | Fields (current entities) | State | Value |
|---|---|---|---|
| **LWW register** | every scalar and optional field, e.g. `title`, `state`, `priority`, `energy`, `planned_at`, `target_at`, `hard_due_at`, `stream_id`, `parent_id`, `sort_order`, `archived`, `deleted`. Also every nested value type as one register, e.g. `scheduling_constraints`, `rrule`, and each `template.*` field | `(value, stamp, origin)` | the value with the greatest stamp |
| **Map** (per-key registers) | map-valued fields whose entries are edited independently, e.g. `Preferences.values` ([`../02-domain/preferences.md`](../02-domain/preferences.md)) and the day schedule's per-weekday and per-date entries ([`../02-domain/day-schedule.md`](../02-domain/day-schedule.md)) | one `(value, stamp, origin)` register per key ever written | every key whose register holds a non-null value |
| **OR-set** (add-wins) | `Task.contexts`, `Task.blocked_by`, `Block.tasks`, `Routine.skipped_keys`, `Routine.streak_keys` | the set of `(element, tag)` adds, and the set of removed tags | every element that has at least one add-tag not in the removed set |
| **PN-counter** | `Task.deferred_count` | the multiset of applied deltas | the sum of deltas, plus the legacy base (§7) |

- **Registers.** The write with the greatest `(hlc, device_id, seq)` wins,
  field by field. Two concurrent writes to different fields both survive.
- **Every register records its origin.** `origin` is `generated` when the
  winning write was made by the system on the user's behalf (routine
  generation, or a template propagation) and `user` when it came from a user
  command. It travels with the value: the `Patch` carries it per op (a
  generation or propagation op is marked as such), and the winning write's
  origin is the register's. Template propagation reads it to leave a field the
  user edited on one occurrence alone
  ([`../02-domain/routines-and-recurrence.md`](../02-domain/routines-and-recurrence.md)
  §Template propagation). A legacy full-state op (§7) has origin `user`, except
  the routine engine's own occurrence creates, which are `generated`.
- **Maps are registers per key.** Each key of a map field is its own LWW
  register, with its own stamp and origin, so two devices editing different
  keys both survive. The key set only grows: a key is removed by writing
  `null` to it, which tombstones the key's register; it is never forgotten, so
  a stale write to the key still competes by stamp and loses to a newer
  tombstone. A map key is a string; a field that needs add-wins membership
  rather than per-key values is an OR-set instead.
- **OR-sets are observed-remove and add-wins.** A remove carries the tags the
  writer had observed. So a concurrent add, whose tag the remover never saw,
  survives. Removing an element a device never saw added is a no-op. The order
  of application is irrelevant: the value is a pure function of the two sets.
- **Counters are PN-counters.** Each `inc` is recorded once under its op
  identity. Re-delivery is already idempotent on
  `UNIQUE (stream_id, device_id, seq)`, so the value is the plain sum.
  `deferred_count` only ever receives `+1` from defer and `-1` from undoing a
  defer.
- **Streak counts are derived, not counters.** `Routine.streak_counter`,
  `streak_started_at`, `last_completed_at` and `forgivenesses_in_window` are
  functions of `streak_keys` and `skipped_keys`, the routine's schedule and its
  grace settings. A streak resets to zero on a miss, and a PN-counter cannot
  express a reset commutatively. So these fields are computed at read time from
  the two OR-sets and are not written by `Patch` ops ([#331](https://github.com/justin13888/Sunrise/issues/331)). Two devices
  completing the same occurrence add the same key, and add-wins set semantics
  count it once.
- **The task–block binding has one side.** `Block.tasks` is the only merged
  set. `Task.blocks` is not a field of the merge: it is derived on read from
  every live block whose `tasks` names the task
  ([`../02-domain/time-blocks.md`](../02-domain/time-blocks.md) §Symmetry with
  `Task.blocks`, [`../02-domain/tasks.md`](../02-domain/tasks.md)). A bind or
  unbind command edits `Block.tasks` only. A `blocks` value carried by a legacy
  full-state `Task` op is ignored by the merge, as the engine already ignores
  it (`crates/sunrise-core/src/engine/block.rs`).

### 4. Creation carries initial fields

A `Patch` with `"create": true` creates the entity and carries every field the
command set. A field it does not carry takes the registry default at read time.

- **An entity is visible only once a create is applied.** A create can arrive
  after later field writes, from another device or through another stream.
  Those field writes merge into the entity's state normally, but the entity is
  not returned by any query until a create, or a legacy full-state op (§7), has
  been applied. Nothing is dropped while it waits.
- **Two creates of the same id merge.** Routine occurrences have deterministic
  ids ([`../05-sync/conflict-resolution.md`](../05-sync/conflict-resolution.md)
  §Concurrent same-routine completion), so two devices can both create one.
  Each create's fields merge as ordinary writes under its own stamp.
  `created_at` is the `hlc.physical_ms` of the create op with the least stamp.
- **A singleton entity is created by an idempotent create with a deterministic
  id, then patched.** An entity kind with one instance per vault, such as
  `Preferences` ([`../02-domain/preferences.md`](../02-domain/preferences.md)),
  has a fixed id. The first write on a device that has not seen the entity's
  create emits a `Patch` with `"create": true` under that id, carrying only the
  fields it sets. Every later write is a plain `Patch`. Two devices that each
  create it merge by the rule above, so the create is idempotent and no device
  has to know whether another already made it.

### 5. Delete is a tombstone register with an explicit resurrect rule

`deleted` is an LWW register like any other.

- **Delete** is `{"deleted": {"set": true}}`. **Restore** is
  `{"deleted": {"set": false}}`. Both compete under the ordinary comparison
  key.
- **No other field write resurrects an entity.** If a device that has not yet
  seen a delete edits the title, the title register takes the edit, and the
  entity stays deleted. The edit is not lost. It is part of the entity's state,
  and a restore shows it. The alternative ("an edit after a delete
  resurrects") would let any stale device undo a user's delete as a side effect
  of an unrelated edit.
- **The undo of a delete is a restore op**, not the removal of the delete op.
  This changes the client: `crates/sunrise-client-core/src/undo.rs` today
  says a delete cannot be undone, because a legacy full-state tombstone has no
  inverse that survives a concurrent edit. Under `Patch`, the inverse of a
  delete is `{"deleted": {"set": false}}`, and undo MUST offer it.
- **Referential cleanup is read-time**, not an op (§6). `ContextDelete` no
  longer rewrites every task that carries the context. A tombstoned context is
  simply not shown on any task, and a restored one reappears. This matters
  because the rewrite was itself a full-state write of every such task, and so
  an overwrite.

Physically removing a tombstone is compaction's decision
([`../04-storage/compaction.md`](../04-storage/compaction.md), [#330](https://github.com/justin13888/Sunrise/issues/330)). It is
never the merge's.

### 6. Invariants that span fields are derived read-time validity

**A merged op is never rejected, and never rewritten, because the merged state
violates an invariant.** Each field converges on its own. A rule that relates
two fields, or two entities, is evaluated when the state is read. It yields a
*derived* value, or a flag the UI surfaces. That derived result is never stored
as an op, so every replica derives the same thing from the same state, without
coordination.

Local commands still validate. A command that would create a violation from
user input is refused with a typed error, exactly as today. A merged state that
violates the same rule is accepted and read through these rules.

| Invariant | A merge can violate it by | Read-time rule |
|---|---|---|
| Title is non-empty | an older or foreign writer setting `""` | Shown as *Untitled*. The stored value is untouched. |
| `state = done` ⇔ `completed_at` is set | device A completes, while device B concurrently clears `completed_at` or reopens | `state` is authoritative. If it is `done` and `completed_at` is null, the completion time reads as the physical time of the `state` register's stamp. If `state` is not `done`, `completed_at` is ignored by every "done" read and kept in storage. |
| `blocked_by` is acyclic | A adds `x → y` while B adds `y → x` | Each live edge is stamped with the earliest of its surviving add-tags. Edges are added to the graph oldest first, by that stamp; an edge that would close a cycle is suppressed at read time and stays in the OR-set. The suppressed edge is reported as a conflict ([`../02-domain/scheduling-constraints.md`](../02-domain/scheduling-constraints.md) §Dependencies, [#333](https://github.com/justin13888/Sunrise/issues/333)). |
| `Stream.parent_id` is acyclic | two concurrent re-parents | The stream whose `parent_id` stamp is greatest in the cycle reads as a root. |
| References name live entities (`stream_id`, `contexts`, `blocked_by`, `parent_id`, `default_context`, bindings) | a concurrent delete of the target | A reference to a tombstoned or not-yet-created entity is not shown. A task whose stream is tombstoned reads as having no stream, and it is re-homed per [#332](https://github.com/justin13888/Sunrise/issues/332). |
| `planned_at` and `target_at` precede `hard_due_at`; hard scheduling constraints hold | independent writes to each field, or a time-zone change | The task is flagged late or in violation per [#334](https://github.com/justin13888/Sunrise/issues/334) and [#333](https://github.com/justin13888/Sunrise/issues/333). Both values are kept. |
| `priority ∈ 1..=5`, other bounded scalars | a newer schema widening the range | An out-of-range value reads as the nearest safe reading, and it is preserved (ADR-0045 §Lossless enums applies the same rule to enums). |
| A block's `ends_at` is after `starts_at` | independent writes | The block reads as zero-length at `starts_at` and is flagged. |

The engine SHOULD emit a structured `core.merge.invariant_derived` event at
`debug` whenever a read-time rule changes what a reader sees. That is the
signal
[`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md)
§7 notes is missing today.

### 7. Legacy full-state ops keep applying, as writes to every field they carry

Every `*Create`, `*Update` and `*Delete` op already in logs and on relays stays
readable, forever. A legacy full-state op at stamp `S` is read as follows:

- **Register fields.** Each field it carries is a register write at `S`. That
  includes every key in its `unknown` map, which becomes a register keyed by
  that name.
- **OR-set fields.** It *assigns* the set at `S`. Every element in its value
  gains an add-tag at `S`. Every add-tag with a stamp less than `S` whose
  element is not in the value counts as removed. The rule is evaluated over the
  whole op set, not at arrival, so it does not depend on delivery order.
- **Map fields.** It *assigns* the map at `S`. Each key in its value is a
  register write at `S`. Every key register with a stamp less than `S` whose
  key is not in the value reads as tombstoned.
- **Counter fields.** It *assigns* a base at `S`. The counter reads as the base
  from the legacy op with the greatest stamp, plus every `inc` stamped after
  it.
- **Create and delete.** A legacy create counts as a create for §4. A legacy
  delete counts as `deleted = true` at `S`.

This reproduces entity-level LWW exactly for any history made only of legacy
ops, so an existing vault projects the same before and after. Once a history
mixes legacy ops and `Patch` ops, every legacy field write simply competes by
stamp.

Once `core.field_ops` is in `vault_requires` (§9), a build that has the feature
MUST NOT emit a legacy full-state op for that vault.

### 8. Unknown fields merge, and are never overwritten by a writer that does not know them

- **A `Patch` naming a field this build does not know is applied.** The
  field-op kind says how to merge it, so the build keeps the field's merged
  state in the entity's unknown state (the `extra` column). It is not
  surfaced, and it survives rebuild. A later `Patch` from this build never
  mentions the field, so the field cannot be overwritten by a writer that does
  not know it. That is the structural fix for the overwrite described in
  §Context.
- **A `Patch` using a field-op kind this build does not know**, such as a
  future text CRDT, cannot be merged correctly. The *whole op* is parked, per
  ADR-0045, and replayed after upgrade. It is never applied partially.
- A `Patch` whose own top-level map carries unknown keys keeps them, per
  ADR-0045 §Unknown maps.

### 9. Rollout is feature-gated

`core.field_ops` is a **structural** feature (ADR-0045 §`vault_requires`).

- A build that has it MUST NOT emit its first `Patch` into a vault until
  `vault_requires` lists the feature.
- It MUST NOT add the feature to `vault_requires` until every non-revoked
  device has advertised support. If one has not, the user decides.
- A device without the feature keeps syncing and parks the `Patch` ops. It
  refuses every local write to the vault and shows *Update Sunrise to edit*.
- Existing vaults are re-projected under these rules by the local rebuild from
  the op log ([`../04-storage/migrations.md`](../04-storage/migrations.md)
  §Migration rigor, [#327](https://github.com/justin13888/Sunrise/issues/327)). By §7, that rebuild yields the same projection
  before the first `Patch` arrives.

### 10. `updated_at` and replay

- **`updated_at` is derived.** It is the greatest `hlc.physical_ms` among
  applied ops that touched the entity, which the `Task` doc comment already
  claims (`crates/sunrise-domain/src/task.rs#Task`). It is no longer a stored
  register, and a `Patch` never writes it. The same holds for every entity with
  an `updated_at`.
- **Replay is a fold, and the order does not matter.** Every field type above
  is a pure function of the set of applied ops. So rebuilding the projection
  from `ops` in any order yields byte-identical rows. That is the property
  [#327](https://github.com/justin13888/Sunrise/issues/327)'s rebuild relies on and [#326](https://github.com/justin13888/Sunrise/issues/326)'s harness asserts. Canonical
  `(hlc, device_id, seq)` order remains the rebuild's order for determinism of
  side tables, not for correctness.
- **Undo emits inverse field ops.**
  - A register undo is a `set` of the prior value at a new stamp.
  - A set undo removes the tags the undone op added, or re-adds the elements it
    removed.
  - A counter undo is the negated `inc`.
  - A delete undo is a restore (§5).

  Undo never deletes an op
  (`crates/sunrise-client-core/src/undo.rs`), and it keeps a register's
  origin as `user`.

## Alternatives considered

| Option | Why rejected |
|---|---|
| **Keep full-state ops with per-field stamps** | ADR-0014 already showed this is meaningless. An unchanged field and a rewritten one are the same bytes. |
| **Adopt a CRDT library (Loro, Automerge)** | It would bring back the dependency ADR-0014 removed, and its unmaintained transitive advisories, for four merge types that are each a few dozen lines. It would also put the merge state in a format the schema fingerprint cannot describe field by field. Rich text in notes is the case that could still justify a library. It would arrive as a new field-op kind, parked by older builds (§8), under its own ADR. |
| **LWW for sets and counters too, with smaller ops** | Two devices adding different contexts would still lose one. The invariant is about data, not about fields. |
| **Resurrect on edit-after-delete** | A stale device could undo any delete as a side effect. Keeping the edit inside the tombstoned entity loses nothing and surprises nobody. |
| **Reject or repair merged states that violate invariants** | Rejecting diverges replicas permanently. Repairing by emitting an op races between replicas that each repair differently. A read-time rule is a pure function, so every replica agrees. |
| **Integer field ids** | They save bytes but need a mapping for every legacy op and unknown key. Names are already the identity on the wire. |

## Consequences

- **Concurrent edits to different fields, and concurrent adds and increments,
  all survive.** The loss [#319](https://github.com/justin13888/Sunrise/issues/319) records is closed for every entity migrated
  to `Patch`.
- **An older writer's blast radius is the fields it edits.** An older build
  that has not learned a field never writes it. Enum and nested-field
  preservation (ADR-0045) still matter for the fields it does write.
- **The merge journal becomes possible.** A losing *field* write is now a
  well-defined thing. Reinstating the journal
  [`../05-sync/conflict-resolution.md`](../05-sync/conflict-resolution.md)
  records as removed is future work with its own ADR, but it is no longer
  blocked by the model.
- **Storage gains per-field stamps and origins, map-key registers, OR-set tag tables and counter deltas.** All
  of them are rebuildable from `ops`. The row columns stay as the read
  projection. The exact tables are a `STORAGE_V` migration under [#319](https://github.com/justin13888/Sunrise/issues/319).
- **The convergence proptest changes shape.** It compares canonical
  projections after applying random interleavings, duplications and reorders
  of mixed legacy and `Patch` ops. It adds one property per field type: no
  concurrent write to a *different* field, and no concurrent add or increment,
  is lost.
- **`docs/05-sync/conflict-resolution.md` and `crdt-design.md` must be brought
  to this model** by [#319](https://github.com/justin13888/Sunrise/issues/319). Until then, where they disagree, this record is
  authoritative.
- **Capability bit 32 (`CLI_ENTITY_LWW`) keeps its position**, and its meaning
  is unchanged for sessions. The merge model is now signalled per vault by
  `core.field_ops`, not per session.

## What would force revisiting this

1. **Collaborative rich text in notes.** This needs a sequence CRDT field-op
   kind. It fits §8's parking rule, but it needs its own ADR and probably a
   library.
2. **Shared streams across users.** OR-set grant lists and per-user
   attribution may need tags carrying more than `op-ref`.
3. **A cross-field invariant that cannot be expressed as a pure read-time
   function**, for example one that needs to allocate something. That would
   need a coordination mechanism this system deliberately does not have.
