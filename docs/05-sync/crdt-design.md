---
status: proposed
---

# CRDT Design

> **Superseded in part by [ADR-0044](../11-adr/0044-per-field-ops.md) (per-field
> ops).** ADR-0044 is the design of record for per-field merge. It adopts no
> CRDT library: ops carry only the fields a command wrote, and each field
> merges as a register, a map of registers, an OR-set or a PN-counter. This page
> restates that model as a type catalogue. Where it and ADR-0044 disagree,
> ADR-0044 wins. The earlier Loro-based design (one Loro doc per Stream, Loro
> lists and rich text, Loro snapshots) is withdrawn; it survives only in
> [ADR-0003](../11-adr/0003-crdt-loro-vs-automerge.md) as history.
>
> **Status: proposed.** The merge is built; no command emits a `Patch` yet.
> Tracked by [#319](https://github.com/justin13888/Sunrise/issues/319).
>
> **What exists in the tree.** `crates/sunrise-core/src/engine/merge/` folds
> every received entity op into per-field state, in migration
> `0033_field_merge_state.sql`'s `merge_*` tables, and projects the merged
> entity to its row. It reads both op shapes: `Patch` (`DOC_SCHEMA_V` 8) and
> the full-state ops every earlier build wrote. Every field type in the table
> below is implemented for Task, Stream, Context, Routine, Block and Attachment.
> `loro` appears in no `Cargo.toml` in the workspace.
>
> What is not built yet:
>
> - **No command emits a `Patch`.** ADR-0044 §9 forbids a build from emitting
>   its first `Patch` into a vault until the vault's `vault_requires` lists
>   `core.field_ops`, and the feature registry `vault_requires` reads
>   ([#324](https://github.com/justin13888/Sunrise/issues/324)) does not list
>   it yet. Every local
>   command still writes a full-state op, which merges as a write to every
>   field it carries (see [Legacy full-state ops](#legacy-full-state-ops)). So
>   between two devices running this build, a concurrent edit to a different
>   field still loses to a later full-state write. A `Patch` from a build that
>   emits one merges field by field. Undoing a delete as a restore (ADR-0044
>   §5) waits for the same gate.
> - **Streaks are stored, and derived once a `Patch` writes a key.** The
>   streak fields are registers a full-state routine op carries. Once a
>   `Patch` stamped above the legacy floor has written `streak_keys` or
>   `skipped_keys`, `project` derives them from the keys instead
>   ([#331](https://github.com/justin13888/Sunrise/issues/331), ADR-0065
>   D17). Between devices that all fold field ops, a defer and a streak
>   advance are already such `Patch`es (D16).
> - **The read-time invariant rules of ADR-0044 §6 are not applied**, except
>   that a tombstoned context is not shown on a task, and the two cycle rules
>   of ADR-0065 §5: a `blocked_by` edge that closes a cycle, newest first, and
>   the newest `parent_id` of a stream nesting loop read as absent, and are
>   reported as `dependency_cycle` conflicts (D18). A merged state that breaks
>   another invariant is stored and read as it is, and
>   `core.merge.invariant_derived` is not emitted.
> - **A field's default is not declared in the registry** (ADR-0044 §2). The
>   merge gives a default to the required fields that have no serde default
>   (`Task.title`, `state`, `stream_id`, `Context.name`). An entity created by a
>   `Patch` that leaves out any other required field is not projected until a
>   write supplies it.

## No CRDT library

ADR-0044 §Alternatives considered rejects Loro and Automerge: the four merge
types are each a few dozen lines, and a library would bring back the
dependency and advisories ADR-0014 removed, and put merge state in a format the
schema fingerprint ([ADR-0045](../11-adr/0045-schema-identity-and-feature-gating.md))
cannot describe field by field. Collaborative rich text in notes is the one
case that could still justify a library. It would arrive as a new field-op kind
under its own ADR, parked by older builds (ADR-0044 §8).

## Key domains, not documents

There is no per-Stream document. Every entity is a row whose ops are sealed
under one key domain: a Stream, the vault's private domain for stream-less
content, or vault-meta
([ADR-0046](../11-adr/0046-optional-stream.md) §2 lists which entity kind goes
where). Sharing, per-stream sync and compaction follow the key domain.

## Mapping domain entities to field types

| Domain field | Field type (ADR-0044 §3) |
|---|---|
| Scalars and optional fields (`title`, `state`, `planned_at`, `target_at`, `hard_due_at`, `stream_id`, `sort_order`, `deleted`, …) | Register: the concurrent writes no other write descends from, with a winner picked by the field's conflict class, each write with an origin (`generated` or `user`). See §Registers and conflict classes |
| Nested values edited as a unit (`scheduling_constraints`, `rrule`, each `template.*` field) | One LWW register for the whole value |
| Map-valued fields (`Preferences.values`, day-schedule entries) | Map: one LWW register per key, tombstoned keys |
| `Task.contexts`, `Task.blocked_by`, `Block.tasks` | Observed-remove OR-set (add-wins) |
| `Routine.skipped_keys`, `Routine.streak_keys` | Observed-remove OR-set (add-wins) |
| `Task.blocks` | Derived from `Block.tasks` on read; not merged |
| `deferred_count` | PN-counter |
| Streak count, `streak_started_at`, `last_completed_at` | Derived at read time from `streak_keys` and `skipped_keys`; not stored |
| Ordering of a stream's children, and of a stream's tasks | `sort_order` fractional key, one register per entity (`Stream.sort_order`, `Task.sort_order`); equal keys break by id |
| `updated_at` | Derived: the greatest `hlc.physical_ms` among applied ops that touched the entity |

### OR-set merge rules

- An add carries the element, and is tagged with the adding op's `op-ref`,
  `(stream_id, device_id, seq)`.
- A remove carries the element and the add-tags the removing device had
  observed for it.
- A remove removes only the listed tags. A concurrent add, whose tag the
  remover never saw, survives.
- The value is every element with at least one tag not removed, so it is a pure
  function of the two sets, whatever the order of application.
- A remove is recorded per `(element, tag)` pair: one op's adds share one tag,
  so the tag alone does not name an add.

### Legacy full-state ops

A `TaskUpdate` and its siblings carry the whole entity. The merge reads one,
at its own stamp, as a write to every field this build registers for the
entity and to every unknown key it carries (ADR-0044 §7). A registered field
the op leaves out, because serde skipped an empty list or a `None`, is written
as absent and reads as the field's default. Every rule involving a legacy op is
evaluated against `L`, the greatest stamp among the legacy ops applied to the
entity:

| Field type | Rule |
|---|---|
| Register | A write stamped below `L` is not a candidate: it leaves the register's antichain and is deleted, winner or loser (ADR-0065 D4) |
| Map key | A write stamped below `L` reads as absent |
| OR-set | An add stamped below `L` reads as removed; `L` re-adds each element of its own value under its own tag |
| Counter | The base `L` carried, plus every `inc` stamped after `L` |

A history made only of legacy ops therefore projects exactly as entity-level
last-writer-wins did, in any delivery order.

The OR-set rule removes *every* add below `L`. ADR-0044 §7 removes only the
adds of elements `L` does not carry. The value is the same either way, since
`L` re-adds what it carries. They differ only for a later remove that observed
`L`'s tag but not an older one. Under this rule a replica that never saw the
older tags agrees with one that replayed every op. That was the case for any
replica that seeded an entity from its row (below), so the rule is the one
that converges.

`L` and every register compare the same full stamp, `(hlc, device_id, seq,
stream)`: the stream breaks a tie between two ops one device made in two
streams with an equal clock and `seq`, so the register that holds the
greatest stamp is never read as below `L`. When `L` rises, what it makes
unreadable is deleted: the adds below it, the counter deltas at or below it,
and every register write below it, a kept loser included
(`state::prune_below_floor`). An add, delta or register write that arrives
below `L` is not stored. So a set written only
by legacy ops holds one add per element, not one per element per op.

The removes that named a pruned add are kept, inert. Deleting them with their
add would make the stored set depend on arrival order: a remove folded before
`L` rose would go with its add, and the same remove folded after would be
stored, because nothing tells an add `L` pruned from one that has not arrived.
Only a `Patch` writes a remove, so the set grows with field ops and not with
full-state ones. Bounding it is compaction's
([ADR-0065](../11-adr/0065-copy-on-write-entity-versions.md) §8).

A field this build does not register can be written both ways: key by key by
a newer build's map `Patch`, and whole, as one unknown value, by a full-state
op that carried it. The whole write is the field's value as of its stamp, as
`L` is the entity's: a map entry stamped before it is superseded, one stamped
after it applies on top, and where the whole value is not a map the newer
entries replace it. So a map an unknown field held survives a full-state op
from a build that cannot read it, as a registered map does.

### Local writes, and rows written another way

A local command builds its op, appends it to the op log, and folds it through
the call a received op takes (`Engine::append_and_fold`, then
`materialize_remote`), in one transaction
([ADR-0065](../11-adr/0065-copy-on-write-entity-versions.md) §5). No command
writes an entity row: the row writers are private to the merge module, so a
command that tried would not compile. The writer and every peer derive a row
from one function of the same ops, and a rebuild of the writer's own log lands
on the rows its commands left. Each local op is logged under the
`inner_kind` and `target_kind` the receive path reads off it, so one op has
one kind on every replica. A defer had been logged locally as `task.defer`
and is now the `task.update` it is; compaction still recognises the older
kind.

Routine materialization is a local write like any other: each occurrence is a
`task.create` appended and folded, with `generated` origin, and an occurrence
the replica already holds, as a row or as a created entity waiting for a
field, is not generated again.

Two kinds of row were written another way. An entity a vault held before
migration 0033 has a row and no field state, and a build before ADR-0065's
third pull request wrote each command's row directly and left the merge to
fold it before the next op. Until ADR-0065's fourth pull request, the first op
to touch such an entity seeded the state from the row, read as the legacy op
that wrote it. Now `Core::open` rebuilds the projection from the op log
whenever the vault was last rebuilt by another version of the fold
(`FOLD_V`), which includes every vault opened for the first time after
migration 0039, and before anything reads the projection or folds an op into
it. The log holds the op behind every such row, so each is folded from its op,
and a legacy-only history projects exactly as it did. No row is read back as
an op any more.

### Versions

Every op folded into an entity is also admitted as one of its *versions*
([ADR-0065](../11-adr/0065-copy-on-write-entity-versions.md) §1–§2), in
`entity_versions` (migration 0039), keyed by its op-ref. A version's causal
frontier, in `entity_frontier`, maps each device to the greatest stamp of that
device's versions it reaches through the `parents` it names and through its
own device's previous version (clarifications D1–D3). `a` precedes `b` when
`b`'s frontier at `a`'s device is at least `a`'s stamp; otherwise they are
concurrent. The heads, the versions nothing else descends from, are flagged
`is_head`. A version that arrives after one that reaches it, by naming it or
by being its device's later version, recomputes every frontier of the entity
in stamp order, so the stored DAG is a function of the versions applied,
whatever order they came in. The register fold reads it (below), and the
heads are what the conflicts query reports as branches.

### Registers and conflict classes

A register keeps the **antichain** of its writes (ADR-0065 §3–§4,
`engine::merge::register`): every write at or above `L` that no other such
write descends from. `merge_registers` holds the winner and `register_losers`
the rest. The winner is the member with the greatest rank, which is origin
first, then the class rule, then the stamp (clarification D7):

| Class | Fields | Rank among concurrent writes |
|---|---|---|
| quiet | `priority`, `energy`, `archived`, `completed_at` (D6), `Stream.sort_order` and `Task.sort_order` (#340), `SavedView.sort` and `SavedView.sort_order` (#341), presentation fields, and every field this build does not know | `(user, stamp)` |
| tracked | `title`, `body`, `stream_id`, every time field, `scheduling_constraints`, `rrule`, `timezone`, template fields, `SavedView.name`, `view` and `filter` (#341) | `(user, stamp)` |
| state | `Task.state` | `(user, state is done or cancelled, stamp)` |
| tombstone | `deleted` | `(user, value is true, stamp)` |

`user` is 1 for a user write and 0 for a generated one, so a generated write
never beats a concurrent user write and still replaces one it descends from,
which is not in the antichain. Ordered writes are decided by the order alone:
the later one is the only candidate left. The registry declares each class
(`crates/sunrise-id/src/registry.rs`), and quiet and tracked differ only in
whether a loser is reported.

The winner is an argmax over a set, so no two writes are ever merged pair by
pair and no order of evaluation is involved. The stored antichain is kept
incrementally and is still a function of the versions applied: a write is left
out only when another descends from it or it is below `L`, and both are
permanent once true. A version that arrives late can order two writes kept as
concurrent; the entity's frontiers are then recomputed and every register is
filtered again.

Two full-state ops name no parents, so they are concurrent, but only `L`
survives the floor: a history of full-state ops alone keeps no loser and
projects exactly as entity-level last-writer-wins did. A full-state op that
loses to a concurrent `Patch` is kept, marked `legacy`, and never reported
(D4).

**Conflicts.** `Query::Conflicts` reports a field whose kept losers include
one written by a `Patch` with a value other than the winner's (D5), for a
tracked, state or tombstone field, and for a quiet one when asked. A winning
delete also reports the heads it does not precede: branches that edited the
entity without seeing the delete. A write that descends from every member of
a field's antichain clears it, which is what `Command::ResolveConflict` and
`Command::Restore` write. `conflict-resolution.md` is the user-facing
account.

### Groups

Some commands write several entities that are only correct together: merging
two blocks (`Command::MergeBlocks`), and a batch of task actions
(`Command::BulkTasks`). Each writes one op per entity, as one **group**
(ADR-0065 §6): every member names the group's id and size, a `Patch` in its
`group` key and a full-state op in a top-level `op_group` mark that names the
member's own op-ref (D13). A receiver stores a member and counts it toward the
cursor, but holds it out of the fold until at least `size` members of the
group are present; then it folds every held member in one transaction, and a
later member at once (D8). So no replica ever shows part of a group, and what
it shows is still a function of the ops it holds, whatever order they came in.
The writer folds its own group in the transaction that appends it.

A build from before groups reads the mark as an unknown field and applies each
member as it arrives. A copy of the mark it writes back names another op, and
marks nothing.

Compaction never deletes a member still held, and a snapshot retains it. A
member whose op compaction deleted counts as present through the version a
snapshot or a rebuild's base carries (D14).

### Visibility, timestamps and validation

- An entity is projected once a create is applied: a `Patch` with `create`,
  or any legacy op.
- `created_at` is the legacy register where one wrote it, otherwise the
  physical time of the least create stamp. `updated_at` is the later of the
  legacy register and the newest `Patch`'s physical time. A `Patch` may write
  neither, nor `id`, nor a derived or nested field.
- A `Patch` is checked before anything is written. A field-op kind this build
  does not know parks the whole op (ADR-0044 §8). Writing a known field with
  the wrong op kind, or with a value its type cannot hold, refuses it as
  malformed. Fit is checked against a fixed sample entity, so the verdict never
  depends on what a replica holds.
- A merged set is written in a fixed order: text by its string, so a
  `Vec<String>` set such as `streak_keys` reads back sorted, then anything else
  by its canonical CBOR.

## Concurrent writes — concrete cases

| Scenario | Result |
|---|---|
| Device A renames a task; device B renames it | As `Patch` versions: the `title` register shows the greater stamp and keeps the other, reported as a tracked conflict. As full-state ops, which every command writes until `core.field_ops`: the later op wins whole and nothing is kept (D4) |
| Device A completes a task; device B, not having seen it, starts it | As `Patch` versions: `done` wins whatever the stamps, and `in_progress` is kept and reported (state class). A reopen written after seeing the completion is an ordinary later write and wins |
| Device A renames a task; device B sets its priority | Both survive: they are different registers |
| Device A adds context X; device B removes context X | The add survives if B had not observed A's add-tag; otherwise X is removed |
| Device A marks done; device B edits the title | Both apply; the task is done with the new title |
| Device A deletes a task; device B edits it | As `Patch` versions: the task stays deleted, B's edit is applied inside the tombstone and reported as edited while deleted, and a restore shows it (ADR-0044 §5, ADR-0065 §3). As full-state ops: the later op wins whole, so a later edit brings the task back |
| Two devices complete the same routine occurrence | Both add the same key to `streak_keys`; the set counts it once, and the derived streak advances once |
| Device A merges two blocks; device B has received one of the three writes | B shows the two blocks as they were until it has all three, then the union alone |
| Device A adds `x → y` to `blocked_by`; device B adds `y → x` | Both edges stay in the OR-sets. The edge whose earliest surviving add-tag is newer is suppressed at read time ([`../02-domain/scheduling-constraints.md`](../02-domain/scheduling-constraints.md) §Dependencies) |

## Op encoding

A per-field op is the `Patch` inner-op variant (ADR-0044 §1), sealed in the
ordinary Sunrise envelope — see
[`../03-crypto/data-encryption-format.md`](../03-crypto/data-encryption-format.md).
Envelope, AAD and signature are unchanged. Legacy full-state ops stay readable
forever, as writes to every field they carry (ADR-0044 §7).

## Snapshots

Every field type is a pure function of the set of applied ops, so a projection
is rebuildable from `ops` in any order. A compaction snapshot carries that
state, the version DAG and the register losers with it (ADR-0065 §8), and
applying one is a join of it into the local state
([`../04-storage/compaction.md`](../04-storage/compaction.md) §Snapshot record,
[ADR-0059](../11-adr/0059-client-op-log-compaction.md) §1).

## Properties tested

In `crates/sunrise-core/src/engine/tests/field_merge.rs`:

- **Convergence, and no lost concurrent write.**
  `any_delivery_order_converges_and_loses_no_concurrent_write`: three devices
  write to one task without seeing each other. The writes are `Patch` ops
  covering registers, an OR-set with observed removes, a counter and a map,
  mixed with full-state renames from the command path. Receivers take every op
  forward, reversed and in a random order with repeats, the create included at
  any position. All of them must project the same task and row stamp. Where no
  full-state op was written, the projection must also match an engine-free
  model: the greatest write of each register and map key, every add no remove
  observed, and the sum of the deltas.
- **Each field type alone**, against two replicas: concurrent register writes
  to different fields, concurrent set adds, an observed remove against a
  concurrent re-add, concurrent increments, concurrent map keys.
- **Legacy ops.** A later full-state op writes every field it carries. A
  history of full-state ops alone reproduces entity-level LWW. A `Patch` to an
  entity with no field state keeps the fields its row held.

Every existing engine test of the full-state merge also runs through this path
unchanged. The model checks only the `Patch`-only cases. A case with a
full-state op is checked for convergence alone, because that op writes every
field from its writer's own view, which the model does not track.
