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
> **Status: proposed.** Not yet built; ranked on the roadmap
> ([`../roadmap.md`](../roadmap.md)) and tracked by
> [#319](https://github.com/justin13888/Sunrise/issues/319).
>
> **What exists in the tree:** entity-level last-writer-wins
> ([ADR-0014](../11-adr/0014-entity-level-lww-merge.md), superseded by
> ADR-0044). `sunrise-core::engine` merges whole entities by
> `(hlc, device_id, seq)`, with the `lww_*` columns arriving in
> `0013_baseline.sql` ([ADR-0018](../11-adr/0018-storage-baseline-reset.md)'s
> collapse, which later migrations append to rather than replace). `loro`
> appears in no `Cargo.toml` in the workspace. The rules in force today are
> [`conflict-resolution.md`](./conflict-resolution.md)'s banner.

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

- **Convergence.** Random interleavings, duplications and reorders of mixed
  legacy and `Patch` ops across N simulated devices produce identical
  canonical projections.
- **No lost concurrent write.** One property per field type: no concurrent
  write to a different field, and no concurrent add or increment, is lost.
- **Determinism.** Same op set, same final state, byte for byte.
