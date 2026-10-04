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
>   `core.field_ops`, and `vault_requires` is
>   [#324](https://github.com/justin13888/Sunrise/issues/324). Every local
>   command still writes a full-state op, which merges as a write to every
>   field it carries (see [Legacy full-state ops](#legacy-full-state-ops)). So
>   between two devices running this build, a concurrent edit to a different
>   field still loses to a later full-state write. A `Patch` from a build that
>   emits one merges field by field. Undoing a delete as a restore (ADR-0044
>   §5) waits for the same gate.
> - **Streaks are still stored.** The streak fields are carried as registers.
>   Deriving them from `streak_keys` and `skipped_keys` is
>   [#331](https://github.com/justin13888/Sunrise/issues/331).
> - **The read-time invariant rules of ADR-0044 §6 are not applied**, except
>   that a tombstoned context is not shown on a task. A merged state that breaks
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
| Scalars and optional fields (`title`, `state`, `planned_at`, `target_at`, `hard_due_at`, `stream_id`, `sort_order`, `deleted`, …) | LWW register, compared by `(hlc, device_id, seq)`, with an origin (`generated` or `user`) |
| Nested values edited as a unit (`scheduling_constraints`, `rrule`, each `template.*` field) | One LWW register for the whole value |
| Map-valued fields (`Preferences.values`, day-schedule entries) | Map: one LWW register per key, tombstoned keys |
| `Task.contexts`, `Task.blocked_by`, `Block.tasks` | Observed-remove OR-set (add-wins) |
| `Routine.skipped_keys`, `Routine.streak_keys` | Observed-remove OR-set (add-wins) |
| `Task.blocks` | Derived from `Block.tasks` on read; not merged |
| `deferred_count` | PN-counter |
| Streak count, `streak_started_at`, `last_completed_at` | Derived at read time from `streak_keys` and `skipped_keys`; not stored |
| Ordering of a stream's children | `sort_order` fractional key, one register per entity |
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
| Register, map key | A write stamped below `L` reads as absent |
| OR-set | An add stamped below `L` reads as removed; `L` re-adds each element of its own value under its own tag |
| Counter | The base `L` carried, plus every `inc` stamped after `L` |

A history made only of legacy ops therefore projects exactly as entity-level
last-writer-wins did, in any delivery order.

The OR-set rule removes *every* add below `L`. ADR-0044 §7 removes only the
adds of elements `L` does not carry. The value is the same either way, since
`L` re-adds what it carries. They differ only for a later remove that observed
`L`'s tag but not an older one. Under this rule a replica that never saw the
older tags agrees with one that replayed every op. That is the case for any
replica that seeded an entity from its row (below), so the rule is the one
that converges.

### Seeding and local writes

An entity a vault held before migration 0033 has a row and no field state. The
first op to touch it seeds the state from the row, read as the legacy op that
wrote it, at the row's stamp. Under the rule above only `L` matters, so this is
what replaying every earlier op would give, up to what the row cannot hold, and
the row projects identically. A local command still writes its row directly.
The merge sees a row stamp it did not write and folds the row in the same way
before the next op.

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
| Device A renames a task; device B renames it | The `title` register takes the greater `(hlc, device_id, seq)` |
| Device A renames a task; device B sets its priority | Both survive: they are different registers |
| Device A adds context X; device B removes context X | The add survives if B had not observed A's add-tag; otherwise X is removed |
| Device A marks done; device B edits the title | Both apply; the task is done with the new title |
| Device A deletes a task; device B edits it | The task stays deleted. B's edit is kept inside the tombstoned entity, and a restore shows it (ADR-0044 §5) |
| Two devices complete the same routine occurrence | Both add the same key to `streak_keys`; the set counts it once, and the derived streak advances once |
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
state; its format is [`../04-storage/compaction.md`](../04-storage/compaction.md)'s
to define ([#330](https://github.com/justin13888/Sunrise/issues/330)).

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
