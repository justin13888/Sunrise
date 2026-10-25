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
  that are not yet resolved. Compaction bounds both (§8, D25): once every
  device that can still write has passed a version, an entity keeps its heads,
  each device's newest version, its registers' winners and losers, and the
  versions whose ops are not yet folded. A device that never acknowledges,
  until it is revoked, holds the versions it may still name.

## What would force revisiting this

1. **Collaborative rich text in `body`.** A sequence CRDT field-op kind would
   move `body` from *tracked* to *merge*, under its own ADR (ADR-0044
   §Revisit 1).
2. **Shared streams across users.** A conflict's "device" would need to
   become a person, and resolution might need permission.
3. **Unresolved sets that grow without bound in practice.** That would be
   telemetry showing users never resolve. It would call for an expiry rule
   that keeps the newest N losers, under its own ADR.

## Clarified by implementation (2026-10)

Building this record raised twenty-five questions it leaves open. Each answer
below is binding on the implementation, and none changes a decision above
except D10, which narrowed §8 and the last consequence until D25 retired it, D11,
which says how §4's two commands write before §7's rollout, as D9 does for
every other command, and D13, which says how §6's groups are marked in that
mode. D16 brought two of §7's `Patch` writes forward for devices that all
fold them; D21 retired it once §7 landed. D11 and D13 hold only in the
legacy mode D9 defines (D22).

| # | Clarification | Why |
|---|---|---|
| D1 | The causal frontier (§2) is keyed by **device**, and each entry is that device's greatest full stamp `(hlc, device, seq, stream)`, not its greatest `seq`. | `seq` counts per `(stream, device)` (ADR-0044 §1). A task that changes stream has versions in two sequences, so `seq` alone cannot order them. |
| D2 | A device's own versions of an entity are always ordered by stamp: each has an implicit parent edge to the same device's previous version. | ADR-0043's hash chains and the HLC send rule already make a device's ops a chain. Without the edge, two consecutive legacy edits from one device would count as concurrent, and every entity with legacy history would show conflicts. |
| D3 | An explicit parent whose known stamp is not below the child's is ignored. | It keeps ≺ acyclic and monotone in the stamp, which the order-independence of the fold needs. An honest writer never produces one, because of the HLC receive rule. |
| D4 | The legacy floor of ADR-0044 §7 filters candidates **before** the class rules apply. A loser that came from a legacy version is kept but never surfaced. | A full-state op cannot tell a field it changed from one it carried unchanged (ADR-0014), so surfacing its losers would show conflicts nobody made, and a legacy-only history would not project exactly as before (§7). |
| D5 | A loser whose canonical-CBOR value equals the winner's is not a conflict. | Two devices both completing a task, or both generating one occurrence, must not raise a conflict. |
| D6 | `Task.completed_at` is **quiet**, not tracked. | It is coupled to `state` at read time (ADR-0044 §6). Tracked, every concurrent double completion would be a conflict twice over. |
| D7 | Inside a set of concurrent writes, precedence is **origin first, then the class rule, then stamp**. | "Generated writes yield" (§3) is stated without exception, so it must outrank the class rules as well as the stamp. |
| D8 | A group (§6) is identified by `(id, size)`. It is complete once at least `size` members are present; then every present member applies, and any later member applies at once. | The result stays a pure function of the op set, whatever order members arrive in. |
| D9 | Until the vault requires `core.field_ops`, commands keep writing full-state ops, through the same merge path (§5). "Never emits a full-state op again" (§7) applies from the moment the feature is required. | ADR-0044 §9: the feature cannot be required while a paired device lacks it, and `Engine::require_features` refuses with `FeatureUnsupportedByDevices`. |
| D10 | *Retired by D25.* Compaction still folds the ops below the floor, but does **not** prune `entity_versions` or `entity_frontier` in this rollout. | A late version that names a pruned parent needs that parent's frontier, and any approximation of it diverges between replicas. A safe pruning rule is follow-up work. Until it lands, "Compaction bounds both" in §Consequences holds for the op log and the losers, not for the version rows. |
| D11 | Until the vault requires `core.field_ops`, `ResolveConflict` and `Restore` write a full-state op of the entity as it projects, with the one field set, rather than being refused. | It is stamped after every version the writer applied and becomes the entity's legacy floor, so by D4 it clears every set below it on every replica that applies it. That settles every field of the entity at once, which is what any edit in that mode already does, and it keeps restore, the undo of a delete, available before the rollout. Once the feature is required, both write the `Patch` of §4. |
| D12 | "Edited while deleted" (§3) is reported for the heads the winning delete does not precede, leaving out a head written by a full-state op or below the legacy floor. | A head is a branch's newest version, so a branch is reported once. A full-state head is D4's case: it cannot say which fields it meant to write. |
| D13 | Until the vault requires `core.field_ops`, a group's members are full-state ops (D9), and each carries the top-level mark `"op_group": {"id", "size", "op"}` beside its entity's fields, where `op` is the member's own op-ref. A build that reads the mark holds the member as §6 says; the fold never reads the mark as a field. A build from before this record reads it as an unknown field, applies each member as it arrives, and may write the mark back in a later op of its own, which marks nothing because its `op` names another op. Once the feature is required, members are `Patch`es with `group`. | A full-state op has no key of its own, and a new op kind or envelope field would be parked by, or visible to, builds that need neither. Binding the mark to its op makes a copy inert, so the result stays a function of the ops (D8). Atomicity on older builds is what it was before groups: per op. |
| D14 | A member counts as present once its op is logged or a joined `doc_state` carries its version (§8); `pending_groups` names it by its deterministic op id. Compaction never deletes a member that is still held, and a rebuild keeps the rows of members whose ops it deleted. | A member compacted away at the writer must still complete the group on a replica that joins the writer's snapshot and receives the rest as ops. |
| D15 | `doc_state` v2 carries the versions, frontiers and losers of the ops it covers. A joined frontier entry keeps the greater of the two stamps, and recomputing an entity's frontiers starts from what is stored, so a frontier a snapshot carried, which can reach versions this replica never holds, survives a later recompute. | A frontier only grows (§2), so a stored one is a floor, never more than the exact frontier, and taking the larger keeps the join order-independent. |
| D16 | *Retired by D21.* Before the vault requires `core.field_ops`, two writes are already `Patch`es naming the writer's heads, where this build has `routine.civil_times`, at least one other device is paired, and every paired device has advertised `routine.civil_times` (`Engine::writes_field_ops`): a defer, as `{scheduled_at: set, deferred_count: inc 1}`, and a completion's streak advance, as `{streak_keys: add [key]}` on the routine. Every other write, and these two in any other vault, stays the full-state op of D9. | They are the two writes a full-state op cannot keep under concurrency: a read-increment-write loses one of two defers (#334), and the legacy floor drops the other device's streak key (#331). Every build that advertises `routine.civil_times` folds a `Patch` with parents, an `inc` and an `add` as this one does, since ADR-0065 PR 1–5 came first, so no paired device can lose these ops, and `core.field_ops` (PR 8) does not exist to be required yet. A vault with no peer has no concurrent write to lose and keeps the streak rule its full-state ops carry. A device paired later on a build before `DOC_SCHEMA_V` 15 can still receive one, which is the case PR 8's requirement closes. |
| D17 | A routine's `streak_counter`, `last_completed_at`, `streak_started_at` and `forgivenesses_in_window` are derived in `project` from `streak_keys`, `skipped_keys` (and `skip_dates`), the rule and `forgiveness_enabled` once a `Patch` stamped above the legacy floor has written `streak_keys` or `skipped_keys`, and are the registers the newest full-state op carried until then. The derivation walks the rule's occurrences up to the newest key in key order: a keyed one counts, a skipped one is neutral, any other is a miss that spends the 30-day forgiveness allowance or ends the run; `last_completed_at` is the newest counted occurrence's due instant. The grace window applies at read time: `Query::Routine(s)`, the weekly review and the streak export apply as misses the occurrences after the newest key whose grace closed before now (`Routine::streak_as_of`). | No key records when its completion happened, so the projection cannot apply the grace window to a completion without a clock, and "as of now" is a function of the clock. Keeping the registers until a `Patch` writes a key leaves a history of full-state ops projecting exactly as before. A late completion therefore counts once derived, and an occurrence completed after a later one first reads as a miss and stops being one when its key arrives. |
| D18 | The read-time cycle rules (§5) read the projection, never write it. `blocked_by`: the edges are taken oldest first by the stamp of each edge's oldest surviving add in `merge_orset_adds`, and an edge that would close a cycle among those already kept is suppressed (`sunrise_domain::resolve_cycles`), in `read_dependency_graph`, `actionable_scan` and the unblock cascade. `Stream.parent_id`: in each loop, the stream whose `parent_id` winner has the greatest stamp reads as a root (`resolve_parent_cycles`), in `read_stream` and in search's stream nesting. Each set-aside edge is one `Query::Conflicts` entry of class `dependency_cycle` on the task or stream holding it, whose `winner` is that edge's own write and whose `cycle` lists the loop; asking for any member's conflicts returns it. | Both rules are pure functions of the merged sets and registers, so every replica that holds them reads the same graph, and one task's write never re-projects another's row. An edge is never dropped from its set, so removing another edge of the loop brings it back. |
| D19 | The write mode (§7) is decided inside each command's transaction (`Engine::write_mode`): `Patch` when the vault requires `core.field_ops`; otherwise, when this build has the feature and no other non-revoked device lacks it, the `VaultRequires` is sealed in that transaction before its first `Patch`, and the mode is `Patch`; otherwise legacy. A device that never advertised lacks every feature. A command that writes no entity requires nothing, and `Core::enable_field_ops(confirmed)` requires the feature over a device that lacks it. | The requirement commits with the first op that uses it or not at all (ADR-0045 §7), one gate serves every command, and opening a vault that generates no occurrence emits nothing. |
| D20 | A command builds its write as the full-state op of the entity it leaves behind. In `Patch` mode that op is rewritten as the `Patch` of the fields that differ from the entity as the command's own reader returns it inside the transaction: a register's new value (one cleared writes what its record reads the absent key as: `null`, the empty list, or `true` for a flag that defaults to it), a nested record's changed sub-fields (`template.title`), an OR-set's gained elements as `add` and lost ones as `remove` with the add-tags this replica observed, and a counter's difference as `inc`. A key this build does not know is never written: the command carries it back as it read it. A create writes every field it sets to other than its default, and a routine's generated occurrence, a re-key's tombstone and a zone change's re-pin carry `"origin": "generated"`. A defer always writes `scheduled_at`, and `ResolveConflict` and `Restore` always write their field; any other write that changes nothing writes no op, and reports an all-zero op id. | One rewrite serves every command, so "writes only the fields it changed" cannot drift command by command. Reading through the command's own reader keeps a value a read filters (a deleted context on a task, a stream a nesting loop roots) from reading as a change. A defer and a settle are statements of the value, so writing it even unchanged is what counts the defer and settles the set. |
| D21 | D16 is retired: §7's write mode replaces `Engine::writes_field_ops`. A vault in legacy mode, one of whose devices lacks `core.field_ops`, a `DOC_SCHEMA_V` 15–17 build included, writes defers and streak advances as full-state ops again. The D16 `Patch`es already in a log keep folding, and D17 keeps deriving the streak of a routine they wrote keys for. | A `Patch` of an entity that has full-state ops now uses `core.field_ops` (`feature::features_used`), so the seal guard refuses one in a vault that does not require it: D16's two writes would need a gate of their own. The mixed state lasts until the older device updates. |
| D22 | D11 and D13 hold only in legacy mode. In `Patch` mode `ResolveConflict` and `Restore` write §4's `Patch`, naming every head, and a group's members are `Patch`es carrying `group`, with no `op_group` mark. A completion and its streak key (from a command or a focus session), a skip and its occurrence's tombstone, and a routine re-key and its tombstones are groups in `Patch` mode, and stay separate ops of one transaction in legacy mode. | Every member's reader folds `group` in `Patch` mode. In legacy mode an older paired device would keep the mark as an unknown field, and these four writes were never grouped before. |
| D23 | A placeholder `streams` row (a stream named by an entity before its create arrived, such as the Inbox) is stamped with the earliest `created_at` of the entities that named it, not with the head stamp of whichever one was folded first. Narrowed by [ADR-0046](./0046-optional-stream.md) C12: only a focus session still makes one. | The head stamp depends on arrival order, so two replicas, or a replica and its rebuild, held different placeholder rows. |
| D24 | Materialization generates an occurrence this replica holds no row and no create for, even when an edit of it has arrived first (`occurrence_exists`); it does not wait for an id to have no versions at all. | Two generated creates of one id fold to one (D5), and a generated create yields to every concurrent user write (§3), so generating beside an overtaking edit loses nothing, while waiting would hide the occurrence until its creator's op arrives. |
| D25 | D10 is retired: compaction prunes a version's `entity_versions` row and its `entity_frontier` rows once the version is **passed** (`engine::merge::prune`). It is passed when (a) its op is at or below its prefix's floor and gone from the log; (b) it is not a head, not its device's newest version of the entity, not a register's winner or loser, and not the newest `Patch` that wrote `streak_keys` or `skipped_keys`; (c) every version that names it as a parent, and its device's next version of the entity, is in its stream and compacted by (a); (d) every op-ref a version of the entity names as a parent is a version here or is at or below its prefix's floor; and (e) every device in `devices` but this one that is not revoked, or was revoked less than `retention_ms` ago, has acknowledged a version that descends from it, by writing it or by publishing a frontier in its stream that reaches it, with this replica holding that device's own prefix there as far as that acknowledgement states, and holds no op here unapplied (waiting for its key, parked, or held in a group). The stream's `doc_state` then names each group member whose version was pruned (`members`), and a joiner counts it present (D14). | A version a replica applies after the prune was written by a device that had applied a descendant of every pruned version (e), or before that and is already applied here (e), so it names no pruned version. Its device's previous version is not a pruned one either: (b) keeps each device's newest, and a version older than that whose previous one was pruned would sit below that one's next version, compacted in its stream (c), and so would be applied already. By (d), and because a covered op is never delivered again, it is no pruned version's ancestor. So every frontier it reads is still stored and is the one a replica that kept every version reads, and the registers, losers, heads and conflicts that read them are the same. The versions whose stored frontiers were computed from a pruned one are never folded again (c): a rebuild's base carries their frontiers, and a joiner's floor rises past them. A silent device holds pruning back, unlike the op-log floor's known devices, because its unsent versions may name anything it held; revoking it releases it. Three things the rule cannot see: a device paired elsewhere whose cert has not arrived, an op sealed in another stream before the acknowledgement (e) read and never received here, and a writer that names a parent it did not hold as a head. A version of those that names a pruned parent folds it as a parent never applied, which adds nothing to its frontier: its writes stay in the unresolved set beside the ones it had seen there, where a replica that kept the parent lets them replace those, until the next write naming the heads settles both alike. No write is lost. |

The conflict classes are declared on each field in the registry's
`for_each_entity!` (`crates/sunrise-id/src/registry.rs`), with the names the
code uses: the ADR's `estimate_minutes` and `sort_key` are
`estimated_duration_s` and `Stream.sort_order`, and `planned_at` and
`hard_due_at` keep the wire names `scheduled_at` and `due_at` (ADR-0047 C1);
`target_at` is its own tracked register from `DOC_SCHEMA_V` 20.
The terminal states, the D7 precedence and the yield rule are hashed into the
schema fingerprint beside the classes, from `DOC_SCHEMA_V` 14.

## Delivered (2026-10)

The record is fully delivered, in eight pull requests:

1. [#532](https://github.com/justin13888/Sunrise/pull/532): conflict classes
   in the registry and the fingerprint, and `parents` and `group` on `Patch`
   (`DOC_SCHEMA_V` 14).
2. [#537](https://github.com/justin13888/Sunrise/pull/537):
   `Core::rebuild_projection` and the rebuild-identity check (#441).
3. [#541](https://github.com/justin13888/Sunrise/pull/541): one merge path
   for every local write (§5).
4. [#564](https://github.com/justin13888/Sunrise/pull/564): the version DAG
   (§1–§2, migration 0039), and a rebuild at open that replaced row seeding.
5. [#568](https://github.com/justin13888/Sunrise/pull/568): registers fold by
   class and keep their losers (§3–§4); `Query::Conflicts`,
   `Command::ResolveConflict` and `Command::Restore`.
6. [#575](https://github.com/justin13888/Sunrise/pull/575): groups (§6) and
   compaction that keeps the frontier and the conflicts (§8, `doc_state` v2).
7. [#578](https://github.com/justin13888/Sunrise/pull/578): values derived in
   `project` and the read-time cycle rules (§5), and history readers that
   understand `Patch`.
8. The rollout (§7): `core.field_ops` (`DOC_SCHEMA_V` 18), the write mode
   (D19), every command writing the `Patch` of the fields it changed (D20),
   `Core::enable_field_ops`, and the four commands that became groups (D22).
   The registry also declares each field's default now (ADR-0044 §2), and the
   fingerprint hashes it. Search's `is:blocked` and `has:dependency` read the
   cycle rule's graph (D18). It closes
   [#447](https://github.com/justin13888/Sunrise/issues/447).

Then, as a clarification: pruning version rows below what every device has
passed (D25, [#581](https://github.com/justin13888/Sunrise/issues/581)),
which retires D10.

What is left, each tracked:

- **The Apple client's conflicts UI**, and its confirmation before
  `enable_field_ops(confirmed: true)` locks an older device out of editing.
  The bindings carry both; the screens are a Mac pass.
- **Pending groups are not surfaced** (plan risk R10): a member whose stream
  key a replica never gets holds its group pending, and no query reports it.
  It belongs with shared streams (§What would force revisiting this, 2).
- **#333 and #334** keep the boxes of theirs this record does not cover:
  the plan and deadline split and lateness (#334). #331's (edit scopes,
  template propagation and catch-up) were delivered on this record: a split is
  one group (§6), and an occurrence is untouched while every version of it is
  `"origin": "generated"` (§1, §3).
