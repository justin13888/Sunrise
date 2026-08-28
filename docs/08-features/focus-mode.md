---
status: accepted
---

# Focus Mode

A dedicated mode that takes one task and removes everything else.

> **v1 scope.** The core half of this spec is implemented: the session record
> ([ADR-0013](../11-adr/0013-focus-session-op-representation.md)), the planner,
> adaptive session length and chunking, the unblock cascade, interruption
> capture, and estimate calibration all live in `sunrise-core` /
> `sunrise-domain` as commands and queries. What is **not** implemented is
> everything that needs a platform surface — notification suppression, the iOS
> Live Activity, the macOS dim, the Android foreground service, and the audible
> cue — plus `timeboxed to my next Block`, which has no target because `Block`
> has no command path in v1, and the **per-Stream** pomodoro override, which
> would need a new field on `Stream`. The 25/5/15-after-4 defaults are
> `sunrise_domain::focus` constants and are configurable per *session* through
> `SessionLength`. `docs/implementation/overview.md` is the authority on what is
> live.

## Entering focus

- From any task list: `f` or "Focus" button on a hovered/selected task.
- From a Block: tap a bound task within it.
- From a notification: a reminder push has a "Focus now" action.
- From the **Focus Planner** (below), which proposes what to work on next.

## Focus Planner — choosing what to focus on

For interdependent, long-horizon work the hardest question is *which task to
touch*, not how long to run the clock. When focus is entered without a specific
task, the planner proposes a ranked queue by walking the task graph:

- **Actionable only.** Filter to tasks whose `blocked_by` are all `done` /
  `cancelled` — the derived-`blocked` rule from
  [`../02-domain/tasks.md`](../02-domain/tasks.md). Blocked tasks never appear;
  the planner is never a dead end.
- **Ranked by leverage.** Each candidate is scored by how much it releases —
  its *downstream unblock weight* / position on the critical path — then by
  `due_at`, `priority`, and `scheduled_at`.
- **Energy-matched.** Sorted against the session's declared energy budget using
  `Task.energy`, so high-energy work lands in high-energy windows.

One keypress accepts the top pick; the queue stays visible so the user keeps
their bearings. The ranking is a pure query (`Query::FocusPlan`) over the graph.

## Composition

Full-screen UI showing:

- **The task title** (large).
- **The body** (rich text), if any.
- **A timer** (default Pomodoro: 25 minutes work, 5 minutes break, configurable per Stream). The session may instead be **sized to the task** (see [Adaptive session length](#adaptive-session-length)); either way the session records the **actual focused time** it contained, not just its planned length.
- **Three actions only**: complete, defer, capture-aside.
- **Sub-task list**, if the task has notes containing a checklist.
- **What this unblocks**, if the task has dependents (see [Unblock cascade](#unblock-cascade)).

## What focus does to the system

- iOS: registers a Live Activity; locks the user into the app via Focus filters if set.
- Android: foreground service with a persistent notification; suppresses other Sunrise notifications.
- Desktop: on macOS, dims other windows (where supported) and hides the tray badge. On Linux/Windows, the dim is a no-op (focus mode still works; just no dim).
- Web: requests page visibility lock where possible; exits gracefully on tab close.
- CLI: `sunrise focus` starts and ends a session one-shot; there is no
  full-screen takeover, since there is no interactive terminal client.

### Notification suppression scope

Focus mode suppresses **reminder** notifications only. Background sync wakeups continue (silent). Suppression starts on focus-mode start and ends 30 s after focus-mode end.

## Capture-aside

While focused, ideas appear. Press `a` to open a tiny capture overlay; it lands in **Inbox** — always, regardless of any current-stream context. The core half is `Core::capture_aside`, which runs the normal capture parser and then drops any resolved `#stream`, so the destination cannot depend on what the user was focused on; the overlay itself is the client's. Rationale: focus mode is for *not switching context*. Returns to focus immediately. Critical for users who can't context-switch without losing their thread.

## Pomodoro and timer policy

- Default 25/5; user-configurable per Stream and per session.
- Long-break after 4 cycles (configurable).
- Audible cue (off by default; opt-in).
- Visual progress bar.

### Adaptive session length

The 25/5 default holds, but the length is a choice made per session start:
`one pomodoro (25m)` · `sized to estimate` (from `Task.estimated_duration_s`) ·
`timeboxed to my next Block` · `until done`. When a task's estimate exceeds one
session, the timer shows **chunk N of M** with checkpoints, so a long task shows
visible progress *within* a sitting instead of ending "barely dented."

The first, second and fourth are `sunrise_domain::focus::SessionLength`;
`timeboxed to my next Block` waits on `Block` gaining a command path.

## Unblock cascade

When a task is completed mid-session, the engine recomputes the graph frontier
and shows the concrete payoff — e.g. *"Done. This unblocked "Deploy" and "QA".
Short break, then keep the chain?"* — turning the dependency graph into a felt
reward: you watch your work release downstream work. This is **informational and
opt-out**: it offers the next actionable task, it does not keep score (see
[What we don't do](#what-we-dont-do)).

## Estimate calibration

Because every session records **actual focused time per task**, over time we
derive a per-user, per-Stream, per-energy **calibration factor** (e.g. "your
30-minute estimates run ~1.7× long"). Surfaced in the weekly review and fed back
into scheduling so future plans are honest. This is the compounding payoff of
recording actuals — estimates get better the more you focus. Session records are
represented per [ADR-0013](../11-adr/0013-focus-session-op-representation.md).

## Interruption capture

Beyond capture-aside (above), bailing out early or switching task offers a
one-tap reason (self / meeting / blocked / other) → a distraction journal
summarized in the weekly review ("top focus-breakers"). No shame UI; it is data
the user opted into. Breaks scale with session length and cumulative load, and
respect quiet hours ([`notifications.md`](./notifications.md)).

## What completing a focus session records

- The task gets a focus-session entry in its activity timeline:
  - Started at, ended at, duration.
  - Whether the task was completed in this session.
- Routines: a focus session that completes a routine occurrence increments the routine streak.
- Reviews: weekly review surfaces "time spent in focus" per Stream, plus the estimate-vs-actual calibration factor (see [`reviews-and-stats.md`](./reviews-and-stats.md), which already lists `focus-session` as a user-visible op).

### Auto-completion

Closing a session with "complete" **completes the task**, if it is still open.

This is the only automatic completion in Sunrise, and it is not an inference:
the focus screen offers three actions and complete is one of them, so the flag
is the user saying they finished the work. Before this the fact was recorded on
the session and the task register never moved, which is what made
`completed_at` a user-action-only field.

It stays narrow on purpose — see [`../00-product/non-goals.md`](../00-product/non-goals.md)
"Not an automation platform". Three nearby signals are deliberately **not**
triggers, because each is an inference rather than a statement:

| Signal | Why not |
|---|---|
| A routine occurrence whose window elapsed | Missed is not done. Auto-completing it would inflate every count in the review; the catch-up policy is where a missed occurrence belongs. |
| All of a task's blockers completing | Blockers describe order, not content. "No blockers left" means *ready*, which `EffectiveTaskState` already derives. |
| A Block ending with the task still bound | [`../02-domain/time-blocks.md`](../02-domain/time-blocks.md) says outright there is no "ran the block" state and that we trust the user. |

Mechanics, because "automatic" and "convergent" have to hold together:

- The completion is derived **once, on the device that closed the session**,
  and emitted as an ordinary full-state `task.update` op. A replica applying
  the `focus.end` op derives nothing, so the completion cannot be re-derived,
  re-timed, or derived differently anywhere.
- It merges through the same entity-level LWW path as a hand-edit, so a
  concurrent edit of the same task on another device is resolved by the same
  `(hlc, device_id, seq)` rule as every other conflict. The session log is
  still not a writer of task state.
- An already-`done` task is a no-op — no second op. A `cancelled` one is left
  alone: `cancelled -> done` is not a legal transition and a session must not
  overrule the user. A deleted one is not resurrected.

## What we don't do

- We don't gamify focus (no streaks of focus per se, no daily quota that yells at you).
- We don't complete tasks on any signal other than the one above. No rules, no
  triggers, no conditions.
- We don't auto-block notifications system-wide. iOS Focus filters and OS-level DND remain the user's choice.
- We don't track keystrokes or productivity scores.

## Focus from a watch

Apple Watch / Wear OS (when shipped): start/stop focus, see timer. No editing.

## States

Empty / loading / error / conflict states follow the four-state contract in [`../07-clients/shared-ui-system.md`](../07-clients/shared-ui-system.md#four-state-view-contract). (Focus mode itself is never "empty" — it shows an idle screen when no task is selected.)
