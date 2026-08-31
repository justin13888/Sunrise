# 0013 — Focus session op representation

**Status:** accepted

**Amended:** the chosen representation originally specified an **OR-Set on a
Loro Stream doc**. That mechanism no longer exists —
[ADR-0014](./0014-entity-level-lww-merge.md) superseded
[ADR-0003](./0003-crdt-loro-vs-automerge.md), deleted `crates/sunrise-crdt`,
and made **entity-level last-writer-wins in SQLite** the whole of the v1 merge
model. See [Amendment](#amendment-2026-08--or-set--append-only-row) below for
what changed, what survived, and why the decision did not need to be reopened.

## Context

Focus Mode ([`../08-features/focus-mode.md`](../08-features/focus-mode.md)) records
wall-clock work against a task: when a session started, when it ended, how much
focused time it actually contained, and whether the task was completed in it.
That record must sync across a user's devices like every other entity, and it
must respect the core determinism rule
([`../01-architecture/shared-core.md`](../01-architecture/shared-core.md) §determinism):
no domain state stores wall-clock time directly — time is read through
`Clock::now_ms` (`crates/sunrise-core/src/config.rs`), so it can be faked in tests
and never drifts between the write path and the read path.

A running timer is the awkward case. The naive model is a single mutable
"session" register per task holding an `elapsed_ms` that ticks. Under LWW-register
merge semantics that value becomes a point of contention: two devices that each
believe a session is live will fight over one register keyed by `(ts_ms,
device_id)`, and a ticking counter is exactly the "stored wall-clock" the
determinism rule forbids. We need a representation where concurrent sessions on
multiple devices merge without loss and where "how long has this run?" is always
*derived*, never stored.

## Decision

Model a **`FocusSession` as an append-only record keyed by its own `EntityRef`**
(`EntityKind` prefix `fcs_`), not as a mutable field on the task.

- A session is written as a **`start` op** (`task_id`, `stream_id`, `started_at`,
  `planned_ms`, `energy`, `kind: Work | Break`) and, later, a separate **`end` op**
  (`ended_at`, `actual_focused_ms`, `interruptions`, `completed_task`). The two ops
  address the same session id; the session is otherwise immutable.
- **Concurrency is resolved by the key, not by a merge type.** Two devices
  starting a session mint two different `fcs_` ids, so they are two different
  records. There is no shared register for them to contend over: both survive a
  merge and both aggregate into the stats, with no add-wins set, no counter, and
  no reconciliation step. Nothing about this depends on a CRDT library.
- A *live* session's elapsed time is **derived on read**:
  `elapsed = clock.now_ms() - started_at`. Nothing ticking is ever persisted.
  `actual_focused_ms` is computed and frozen only at the `end` op.

### How that lands in storage

`crates/sunrise-storage/migrations/0010_focus_sessions.sql` gives each op its own
table, and that separation is load-bearing rather than cosmetic:

| Table | Written by | Key |
|---|---|---|
| `focus_sessions` | the `start` op | the session's own `fcs_` id |
| `focus_session_ends` | the `end` op | the same session id |
| `focus_interruptions` | `LogInterruption`, and the `end` op's list | `(session, at_ms, reason)` |

Every write is an insert that ignores a conflict on its own primary key. Three
consequences follow directly:

1. **Neither op can lose to the other.** If the `end` lived as an `UPDATE` of
   the `start` row, an `end` that overtook its own `start` on the wire would
   update zero rows and vanish from the projection. As its own insert it simply
   lands, and the session view is the `LEFT JOIN` of the two tables.
2. **A dangling `start` is the *absence* of an end row**, which is why "still
   running" needs no flag, no tombstone, and no repair pass.
3. **Re-delivery is a no-op**, so the ops ride the same at-least-once transport
   as everything else without an idempotence layer of their own.

Focus ops are therefore the one op family that **bypasses the LWW comparison** in
`materialize_remote` (`crates/sunrise-core/src/engine.rs`). Running LWW on them
would be actively wrong: a `start` stamped later than its own `end` — routine
under clock skew across two devices — would suppress the `end`.

## Amendment (2026-08): OR-Set → append-only row

**What the original text said.** The Decision section previously read:
"Sessions live in an **OR-Set / op-log** on the owning Stream's doc — never a
per-task LWW register." That was written against
[ADR-0003](./0003-crdt-loro-vs-automerge.md), when the plan of record was that
Sunrise data merged through Loro, and an OR-Set was a type the merge layer would
supply.

**What changed.** [ADR-0014](./0014-entity-level-lww-merge.md) established that
ADR-0003 was **never realized**: `crates/sunrise-crdt` existed but nothing in the
workspace depended on it, and what actually merges Sunrise data is entity-level
LWW over `(ts_ms, device_id)` in SQLite. ADR-0014 deleted the crate and the
`loro` dependency. There is no CRDT library in the dependency graph, so there is
no OR-Set to put a session in — the mechanism this ADR named is gone, and it was
never running to begin with.

**Why the decision nonetheless stands.** The OR-Set was never the point; it was
the means. What this ADR actually requires is that *concurrent sessions on
different devices coexist and aggregate rather than clobber each other* — and
that requirement is satisfied by the part of the decision that was always the
substantive one: **a session is keyed by its own `EntityRef`.** Two devices
starting sessions concurrently mint different ids, so they were never going to
collide in the first place; the set they live in only ever needed to preserve
distinct members, which an append-only table of distinct primary keys does. The
OR-Set was buying add-wins semantics against concurrent *removal*, and sessions
are never removed.

So the amendment is a genuine narrowing of mechanism with no loss of the
property, and it is worth being precise about what we gave up rather than
pretending the OR-Set was decorative:

- **We lose add-wins removal semantics.** If focus sessions ever became
  deletable, a concurrent delete/re-add would resolve by LWW rather than
  add-wins. v1 has no delete path for a session, so this is unexercised — but it
  is a real narrowing, and it belongs in [What would force
  revisiting](#what-would-force-revisiting-this) below.
- **We lose a per-op causal ordering.** An op-log-backed set carries causality;
  three independent inserts do not. Nothing in the session model needs it: the
  `start`/`end` pair is ordered by the session id they share, not by delivery.

The `end` op's `interruptions` list needed one small addition to fit: because
`LogInterruption` is its own command, interruptions are a **grow-only set** keyed
by `(session, at_ms, reason)`, unioned from both the interruption ops and the
`end` op's list. A grow-only set is convergent under plain insert-or-ignore, so
this too costs no merge machinery.

**Status change.** `proposed` → `accepted`, on the strength of the implementation
landing behind it and the convergence test below passing against the real relay.

## Alternatives considered

| Option | Why rejected |
|---|---|
| Live LWW `elapsed_ms` register per task | Ticking value fights under concurrent-device merge; stores wall-clock, violating the determinism rule |
| Single "current session" field on `Task` | Loses session history; two devices in a session concurrently clobber each other; no basis for calibration stats |
| Session as a mutable entity edited to `end` | Start/end as edits to one LWW record reintroduces the merge fight; append-only start+end ops avoid it |
| **OR-Set on a Loro Stream doc** *(the original decision)* | The mechanism no longer exists — ADR-0014 removed the CRDT layer entirely. It was also more than the problem needed: distinct ids already prevent the collision, and sessions are never removed, so add-wins bought nothing |
| One `focus_sessions` row updated in place by the `end` op | An `end` that overtakes its `start` updates zero rows and is silently lost. Two tables cost one `LEFT JOIN` and remove the ordering hazard outright |

## Consequences

- **Multi-device sessions merge cleanly.** Two devices can each run a focus
  session; both survive and both feed stats. Proven end to end by
  `crates/sunrise-e2e/tests/two_core_focus_convergence.rs` — two real `Core`s over
  the real relay start concurrent sessions on one task, log an interruption on
  one of them, close both, and assert that both sessions are retained on both
  replicas *and* that `total_focused_ms` is the sum rather than either device's
  figure alone.
- **Stats and calibration are a fold** over the immutable session log —
  `sunrise_domain::focus::fold_focus_stats` is a pure function over a `Vec` of
  records, so the weekly-review "time spent in focus per Stream" and the
  estimate-vs-actual calibration factor unit-test with no database and no clock.
  Only *ended* sessions calibrate: a running session's focused time is derived
  from the clock, and a factor that drifts with wall time is not a calibration.
- **A dangling live session** (a `start` with no `end`, e.g. the app died) is a
  valid state: it reads as "still running" until an `end` op arrives, and
  `Query::RunningFocusSessions` is how a client finds it on next launch. No
  special tombstone is needed, and no client is obliged to synthesize an `end`.
- **New surface area, as built:** storage migration `0010_focus_sessions`
  (`STORAGE_V` 9 → 10), `EntityKind::FocusSession` (`fcs_`),
  `sunrise_domain::focus`, the `FocusStart` / `FocusEnd` / `FocusInterrupt` inner
  ops, the `StartFocus` / `EndFocus` / `LogInterruption` commands, and the
  `FocusPlan` / `FocusStats` / `TaskFocusSessions` / `RunningFocusSessions` /
  `UnblockCascade` queries.
- **The session log is a second, non-authoritative record of task completion.**
  `EndFocus { completed_task }` records what happened in the session; it does
  **not** complete the task. Task state has exactly one writer
  (`Command::CompleteTask`), so the two can never disagree about what is true —
  only about what a given session observed.

## What would force revisiting this

1. **Deletable sessions.** A user-facing "delete this session" (privacy, or a
   mistaken start) reintroduces exactly the concurrent add/remove case the
   OR-Set was for, and LWW on a tombstone column is the wrong answer for a set.
2. **Server-side aggregation.** The fold assumes every replica holds the whole
   session log. If sessions are ever compacted or trimmed server-side, the
   calibration factor has to become an incrementally-maintained value rather
   than a pure fold, which is a different design.
3. **Cross-user shared sessions** (pairing/mob work). Two *users* focused on one
   task is not the multi-device case and would need a real merge type again.
