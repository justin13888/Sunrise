# 0013 — Focus session op representation

**Status:** proposed

## Context

Focus Mode ([`../08-features/focus-mode.md`](../08-features/focus-mode.md)) records
wall-clock work against a task: when a session started, when it ended, how much
focused time it actually contained, and whether the task was completed in it.
That record must sync across a user's devices through the Loro op-log like every
other entity, and it must respect the core determinism rule
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
- Sessions live in an **OR-Set / op-log** on the owning Stream's doc — never a
  per-task LWW register. Concurrent sessions on different devices are distinct
  ids, so they **coexist and aggregate** instead of overwriting one another.
- A *live* session's elapsed time is **derived on read**:
  `elapsed = clock.now_ms() - started_at`. Nothing ticking is ever persisted.
  `actual_focused_ms` is computed and frozen only at the `end` op.

## Alternatives considered

| Option | Why rejected |
|---|---|
| Live LWW `elapsed_ms` register per task | Ticking value fights under concurrent-device merge; stores wall-clock, violating the determinism rule |
| Single "current session" field on `Task` | Loses session history; two devices in a session concurrently clobber each other; no basis for calibration stats |
| Session as a mutable entity edited to `end` | Start/end as edits to one LWW record reintroduces the merge fight; append-only start+end ops avoid it |

## Consequences

- **Multi-device sessions merge cleanly.** Two devices can each run a focus
  session; both survive and both feed stats. Proven by a `sunrise-e2e`
  convergence test (concurrent sessions retained, `actual_focused_ms` aggregates).
- **Stats and calibration are a fold** over the immutable session log — the
  weekly-review "time spent in focus per Stream" and the estimate-vs-actual
  calibration factor are pure reductions, cheap to recompute and deterministic.
- **New surface area:** a storage migration `0007_focus_sessions`, `FocusSession`
  in `crates/sunrise-domain`, and core `StartFocus` / `EndFocus` / `LogInterruption`
  commands plus `Query::FocusStats` / `Query::FocusPlan`.
- **A dangling live session** (a `start` with no `end`, e.g. the app died) is a
  valid state: it reads as "still running" until an `end` op arrives, and a client
  may synthesize an `end` op on next launch. No special tombstone is needed.
