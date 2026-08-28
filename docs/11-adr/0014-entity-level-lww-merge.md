# 0014 — Entity-level LWW in SQLite is the v1 merge model (supersedes 0003)

**Status:** accepted

**Supersedes:** [ADR-0003 — CRDT: Loro over Automerge](./0003-crdt-loro-vs-automerge.md)

**Amended by:** [ADR-0016 — Hybrid logical clocks](./0016-hlc-timestamps.md),
which replaces the comparison key `(ts_ms, device_id)` used throughout this
document with `(hlc, device_id, seq)` and fixes the skewed-clock defect recorded
under *Consequences*. The merge model itself — entity-level, one survivor — is
unchanged, and per-field LWW remains deferred; see *Per-field LWW, revisited*
at the end.

**Amends:** [ADR-0013 — Focus session op representation](./0013-focus-session-op-representation.md),
whose chosen representation named an OR-Set on a Loro Stream doc. Removing the
CRDT layer removed that mechanism; ADR-0013 now records the same requirement as
an append-only row keyed by its own `EntityRef`, which needs no merge type.

## Context

[ADR-0003](./0003-crdt-loro-vs-automerge.md) chose **Loro** as Sunrise's CRDT
library. **That choice was never realized.** A `crates/sunrise-crdt` crate was
written against `loro`, but nothing in the workspace ever depended on it: a
workspace-wide grep for `sunrise_crdt` found no `use`, and no member manifest
listed it. It compiled, its own tests passed, and no product path reached it.
`loro` was carried in `Cargo.lock` solely to build that orphan.

What actually merges Sunrise data is **entity-level last-writer-wins in
SQLite**, and it has been for the whole of the v1 build:

- `crates/sunrise-core/src/engine.rs` — `lww_wins(env_ts, env_dev, row_ts,
  row_dev)` compares the incoming envelope's `(ts_ms, device_id)` against the
  materialized row's, greater timestamp wins, ties broken by memcmp on the raw
  16-byte device id. A losing op is applied to the op log and then dropped from
  the projection; a winning op rewrites the row.
- `crates/sunrise-storage/migrations/0006_lww_metadata.sql` — adds `lww_ts_ms`
  and `lww_device` to `tasks`, `streams`, and `routines` so local and remote
  writes compete on the same footing. Both columns are projections, rebuildable
  from the op log. *(Since collapsed into `0013_baseline.sql` by
  [ADR-0018](./0018-storage-baseline-reset.md), and the columns renamed to
  `lww_hlc_ms` / `lww_hlc_logical` / `lww_seq` / `lww_device` by ADR-0016.)*
- Deletes are tombstones stamped with the same pair, so a concurrent
  delete/update resolves by the identical rule.

So the repository has had two answers to "how does Sunrise converge?" — one
decided and dead, one undecided and live. This ADR removes the ambiguity by
**making the de-facto state the decided state**, and deletes the unrealized
layer rather than leaving a plausible-looking crate that implies otherwise.

## Decision

**Entity-level LWW over `(ts_ms, device_id)` in SQLite is the v1 merge model.**

`crates/sunrise-crdt` is deleted, along with the `loro` entry in the root
`[workspace.dependencies]`. There is no CRDT library in the dependency graph.

The op log, the `OpEnvelope` codec, the sync protocol, and the storage layer are
unchanged by this — ADR-0003 already noted they are independent of the merge
engine, and that insulation is what makes this ADR a deletion rather than a
rewrite.

## What we give up

This is a real narrowing, not a free win. LWW at entity granularity means:

- **Concurrent edits to *different fields* of the same entity lose one of
  them.** If device A retitles a task at `t` and device B changes its due date
  at `t+1`, B's whole row wins and A's title edit is discarded from the
  projection. A per-field CRDT (or per-field LWW) would keep both. The losing op
  survives in the op log, so nothing is unrecoverable, but nothing surfaces it
  either.
- **Rich-text co-editing is impossible.** Character-level merge of a note body
  needs a text CRDT. Under entity LWW, two people typing in the same note body
  produce one survivor. `docs/02-domain/notes.md` describes `NoteBody` as a Loro
  `RichText` doc; that is now a **target-state** description, not an
  implementation. It is also not reachable in v1 — `Note` has no command path.
- **Set and counter semantics degrade to LWW.** The OR-Set (`blocked_by`,
  contexts), the PN-counter (streak, `deferred_count`), and the fractional-index
  list in `docs/05-sync/conflict-resolution.md` are likewise target-state. Under
  entity LWW, a concurrent add and remove resolves by timestamp, not add-wins.
- **The merge journal has nothing to record.** Entity LWW discards the loser
  wholesale rather than reconciling fields, so there is no per-field conflict to
  surface for review.

### A delete is a full-state op, not a flag

"The winning op's state replaces the entity" only converges if **every** op
carries a full state to replace it with. A delete is not exempt, and it is the
one place where the temptation to cut a corner is strongest: the id alone looks
like enough, because the tombstone is all anyone reads afterwards.

It is not enough. The delete ops originally carried only an `EntityRef`, so
when a delete won LWW the tombstone was set and the row stamped with the
winning stamp while every other column kept whatever *that replica* happened to
hold. Two replicas that had applied different updates before the delete then
disagreed permanently — and because both carried the same winning stamp,
neither would ever accept a correction. The divergence was stable, not
transient, and invisible to the UI because tombstoned rows are filtered from
every read. It surfaced only in byte-identical convergence checks.

So: **delete ops carry the entity's full state with `deleted` set**, and apply
through the same path as the corresponding update, so the whole row is
replaced. `DOC_SCHEMA_V = 4` is that change for `TaskDelete`, `StreamDelete`,
`ContextDelete`, and `RoutineDelete`.

`BlockDelete` and `AttachmentDelete` still carry an `EntityRef` and retain the
same latent defect. They are not covered by the convergence suites and have not
been converted; doing so is the same mechanical change.

### A note on measuring this class of bug

The divergence rate is a trap, and it is worth recording how it misleads,
because the next such bug will present the same way.

Divergence happens **only when the delete wins the LWW race**. When the update
wins, the delete is rejected wholesale and both replicas keep the updated row,
which converges. So an observed failure rate measures how often the delete
happened to land last, not how often the defect applies — every delete-wins
interleaving diverges, and does so permanently.

Concretely: the first probe run showed `StreamDelete` diverging while
`ContextDelete` and `RoutineDelete` passed, which read as "only Stream is
affected". Repetition showed all three at 1–2 failures in 12. Reading the low
rate at face value would have shipped two permanent data-loss paths as a rare
edge case.

The corollary for the tests
(`crates/sunrise-e2e/tests/delete_defect_probe.rs`,
`two_core_delete_convergence.rs`): a single green run proves little, since a
run where the update wins passes against broken code too. Confidence comes from
repetition, and from checking that the test still fails when the fix is
reverted.

## Why it is nonetheless the right v1 decision

Every field on v1's shipping entities is a **scalar**. There is no rich text, no
user-visible set, and no ordered list anywhere in the command surface — the
whole materialized schema (`tasks`, `streams`, `routines`) is flat columns. For
those entity shapes LWW is not an approximation of convergence; it *is*
convergence, and it is tested as such:

- **`convergence_under_random_interleaving`** (`crates/sunrise-core/src/engine.rs`)
  — a 32-case proptest that applies a randomly generated op sequence to two
  independent databases under random reordering and duplication, then asserts a
  byte-identical canonical projection of the `tasks` table. This is the flagship
  convergence property, and it passes against the LWW engine.
- **Four chaos scenarios** (`crates/sunrise-e2e/tests/chaos.rs`) over real
  WebSocket transport through the real relay: `drop_heavy_converges` (30% frame
  loss both directions), `corruption_never_applies` (15% bit-flip on inbound
  frames, asserting corrupted data never materializes),
  `delay_preserves_convergence` (50–300 ms hold on every frame), and
  `partition_then_heal` (link cut, divergence, replay backfill). All four end
  with both replicas Live, outboxes drained, and identical canonical state.
- **`two_core_relay_convergence`**
  (`crates/sunrise-e2e/tests/two_core_relay_convergence.rs`) covers live
  edits, offline catch-up, and an explicit LWW conflict end to end; a companion
  case proves double-materialization of a routine collapses via deterministic
  occurrence ids.

A CRDT library buys convergence guarantees for shapes v1 does not have, at the
cost of a large dependency (`loro` dragged in the archived `im` /
`bitmaps` / `sized-chunks` family, all under RUSTSEC unmaintained advisories we
were suppressing in `deny.toml` purely to keep the orphan compiling). Paying
that today would be paying for the roadmap, not the product.

## Alternatives considered

| Option | Why rejected |
|---|---|
| **Realize ADR-0003** — route merge through `sunrise-crdt`/Loro now | Rewrites the entire materialization path in `engine.rs` and the op codec for zero v1-visible behaviour change, since no v1 entity needs a non-LWW type. Reinstates four unmaintained-dependency advisories. |
| **Per-field LWW** (stamp `(ts_ms, device_id)` per column) | Closes the "different fields, one survivor" gap without a CRDT library, but costs a wide schema migration and per-column merge logic in `engine.rs`. Genuinely attractive; deferred because no shipping surface produces concurrent per-field edits — v1 clients submit whole-entity updates. Recorded as the first thing to reach for (see below). |
| **Keep `sunrise-crdt` as a dormant crate** | The status quo, and the reason this ADR exists: a compiling crate named `sunrise-crdt` next to a live merge engine that ignores it is a trap for the next reader. Dormant code with no consumer is a claim the product does not honour. |
| **Entity-level LWW, crate deleted (chosen)** | One merge model, in one place, with the proptest and chaos suite pointed at it. |

## Consequences

- **`docs/05-sync/crdt-design.md`, `docs/05-sync/conflict-resolution.md`, and the
  CRDT-mapping sections of `docs/02-domain/*.md` describe target state, not v1.**
  They stay as the design source of truth for the shapes above; they are not a
  description of what runs today. `docs/implementation/overview.md` is the
  authority on what is live.
- ~~**A skewed device clock wins every conflict**, permanently, because
  `lww_wins` trusts the envelope's raw `ts_ms`.~~ **Fixed** by
  [ADR-0016](./0016-hlc-timestamps.md): the comparison key is now
  `(hlc, device_id, seq)`, ops beyond a five-minute drift window are refused,
  and a peer that has observed a fast device's op sits above it, so the fast
  clock wins a concurrent race but no longer holds a veto.
- **Four RUSTSEC suppressions are gone from `deny.toml`** — RUSTSEC-2026-0248
  (`im`), -2026-0247 (`bitmaps`), -2026-0251 (`sized-chunks`), and
  RUSTSEC-2023-0089 (`atomic-polyfill`). Removing `loro` removes the advisories
  outright rather than suppressing them; `cargo deny check` now reports no
  unmatched ignores. The `BSL-1.0` licence allowance **stays**: it was annotated
  as reaching us via `xxhash-rust` ← `loro`, but `cargo deny list` shows it is
  actually `ryu` (via `serde_json`), which is permanent. One advisory ignore
  remained afterwards — RUSTSEC-2024-0436 (`paste`, via `ratatui`) — and it
  left with `ratatui` under [ADR-0019](./0019-swiftui-macos-client.md). There
  are now **no advisory suppressions in the workspace at all**.
- **No wire or storage format changes.** Envelopes, the op log, and the sync
  protocol are untouched; a vault written before this ADR reads identically
  after it.

## What would force revisiting this

Any one of these reopens the decision, and the reopening should produce a new
ADR rather than an edit to this one:

1. **Collaborative note bodies.** The moment `Note` gets a command path with
   concurrent editing — one user in two windows counts — entity LWW silently
   drops keystrokes. A text CRDT is the only correct answer; nothing cheaper
   works.
2. **Per-field merge becomes user-visible.** If clients start submitting partial
   entity updates (a due-date-only op, a title-only op), concurrent edits to
   distinct fields become routine and the "one survivor" behaviour turns into a
   data-loss bug report. Per-field LWW stamps are the cheap fix and should be
   evaluated before any CRDT library.
3. **Real multi-user shared streams.** `docs/05-sync/shared-documents.md` assumes
   OR-Set grant lists and CRDT-merged concurrent edits across *users*, where
   simultaneous edits are far likelier than across one person's devices.
4. **Ordered, user-arranged lists.** Manual ordering of a Stream's children needs
   fractional indices with a deterministic tiebreak; LWW on a position scalar
   thrashes under concurrent reorder.

Until one of those lands, the merge model is entity-level LWW and the workspace
contains no CRDT library.

## Per-field LWW, revisited

Reason 2 above — "per-field merge becomes user-visible" — was re-examined during
the v1 schema epic, alongside the HLC work in
[ADR-0016](./0016-hlc-timestamps.md), because that work touched every LWW column
and would have been the cheap moment to widen the stamp. **It stays deferred.**
The reasoning has not changed, and it is worth writing down explicitly rather
than leaving as a table row:

**The precondition still does not hold.** Per-field LWW only pays for itself
when clients emit *partial* entity updates. Sunrise's ops are full-state:
`TaskUpdate` carries the whole `Task`, because that is what makes a losing op
recoverable from the log and what lets a replica materialize an entity it has
never seen. With full-state ops, a per-field stamp cannot tell "B did not change
the title" from "B set the title to the value it already had" — both arrive as
the same bytes. Per-field LWW over full-state ops would therefore either resolve
identically to entity LWW, or resolve *wrongly* by treating an unchanged field
as a fresh write. Getting the benefit requires changing the op shape first, and
that is a much larger decision than a wider stamp.

**The cost is not the schema, it is the merge rule.** Four more columns per
entity is cheap. What is not cheap is that every field then has an independent
history, so "which op won" stops having an answer, `updated_at` stops being
meaningful, the op log stops reconstructing the projection by replay, and the
convergence proptest has to compare per-field lineage rather than a canonical
row. That is a merge engine, not a column.

**And the HLC does not change the trade.** ADR-0016 changes *which* write wins;
per-field LWW changes *how much of the entity* the winner replaces. They are
independent, and doing the first does not make the second more or less urgent.

The trigger is unchanged and is restated here so it is unmissable: **the first
partial-update command path.** A due-date-only op, a title-only op, or an
inline-edit surface that submits one field. On the day one of those is proposed,
per-field LWW is a prerequisite for it and not a follow-up, and it needs its own
ADR covering op shape, `updated_at` semantics, and what replay means.
