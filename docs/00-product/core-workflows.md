---
status: accepted
---

# Core Workflows

Six workflows define Sunrise. Every UI surface is justified by serving at least one. Anything not serving one of these is a candidate for deletion.

## 1. Capture

**Trigger.** Idea strikes anywhere — in bed, on a call, mid-walk, mid-SSH session.

**Path.**
1. Global hotkey (macOS), pull-down (iOS), notification action, lock-screen widget, `sunrise capture` from a shell, or browser keyboard shortcut.
2. Single-line input. Optional: `#stream`, `@context`, `!high`, `^tomorrow 9am` inline annotations.
3. Enter commits to local DB. UI dismisses. Sync happens after.

**Latency targets** (p95 on the platform baselines in [`../10-cross-cutting/performance-budgets.md`](../10-cross-cutting/performance-budgets.md)):

- Hotkey → focused input: ≤ 100 ms.
- Enter → visible confirmation: ≤ 50 ms.

Capture works fully offline; sync is best-effort after commit.

## 2. Plan today

**Trigger.** Morning ritual or after a long break.

**Path.**
1. "Today" view shows: scheduled blocks, due-today tasks, manually-pulled tasks, and a "promote from inbox" affordance.
2. User drags or keyboard-promotes inbox items into Today, optionally onto time blocks.
3. The system surfaces overdue items grouped by stream so they can be deferred or dropped, not blindly bumped.

**Promote / demote semantics.** A Task in Inbox is promoted to a Stream by setting its `stream_id`, and demoted by clearing it ([ADR-0046](../11-adr/0046-optional-stream.md)). The Inbox is a view, so a demoted task appears there only if no time or triage mark keeps it out ([`../02-domain/tasks.md`](../02-domain/tasks.md#inbox)). Both are ordinary writes to the `stream_id` field and use the cross-stream-move rules in [`../05-sync/conflict-resolution.md`](../05-sync/conflict-resolution.md); there is no special "promotion" op kind. Bouncing between Inbox and a Stream is allowed unconditionally. Undo is the inverse op and shares the same merge rules. Promotion order is deterministic across devices, broken by the one comparison key the whole system uses: `(hlc, device_id, seq)` ([`../05-sync/conflict-resolution.md`](../05-sync/conflict-resolution.md) §The comparison key).

## 3. Do work in focus

**Trigger.** User is ready to execute.

**Path.**
1. Pick a task → enter Focus mode.
2. Focus mode hides everything else, optionally starts a Pomodoro / time-block timer, and exposes one keypress to: complete, defer, split, capture an aside.
3. "Capture an aside" lands in inbox without leaving focus.

## 4. Review weekly

**Trigger.** End of week or user-defined cadence.

**Path.**
1. Review walks the user through, **per stream**: what got done, what slipped, recurring items needing tuning, captured asides needing triage.
2. Output is a clean inbox, an updated routine cadence, and an explicit decision per slipped item: do, defer, drop, delegate.

## 5. Travel / context shift

**Trigger.** User is changing context — boarding a flight, starting a 2-week trip, taking a sabbatical from one job.

**Path.**
1. User starts a shift (e.g. `Travel: Tokyo`), optionally with a destination zone and place. The entity is a `Shift`, not a "context": `Context` is the tag entity ([ADR-0073](../11-adr/0073-context-shift.md) §1–2).
2. Sunrise filters: streams and contexts the shift does not keep primary collapse behind one "Paused for Travel: Tokyo" group, items in the streams and contexts it keeps primary come first, and recurring routines follow their policy. Nothing is deleted or rescheduled.
3. On return the user ends the shift. Every effect of the shift is derived while it is in force, so ending it is the whole restore. Sunrise then opens "While you were away": routine occurrences missed during the shift, deadlines that passed, and tasks captured during it (ADR-0073 §6).

At most one shift is in force at a time: of those covering the moment, the one started latest, a tie going to the greater id, so two devices that start shifts concurrently agree once synced.

**Routine behavior during a context shift.** Each routine's `shift_policy` (a per-routine field, synced like any other, default `pause`) is evaluated at generation time for each occurrence the shift in force covers. No stored occurrence is tombstoned or rewritten (ADR-0073 §4):

| Policy | Routine behavior during the shift |
|---|---|
| `pause` | Covered occurrences are not generated and not missed; the generation horizon resumes at the shift's end with no backfill. |
| `shift_tz` | Covered occurrences keep their wall-clock time (e.g. `09:00`) in the shift's destination zone; the UTC time changes and the occurrence key does not. DST in the destination follows the one DST rule ([`../10-cross-cutting/time.md`](../10-cross-cutting/time.md) §3). A floating routine already follows the user and is unaffected. |
| `skip` | Covered occurrences are generated as skipped with cause `context_shift`. The streak excuses them: neither counted nor missed, spending no forgiveness (see [`../02-domain/routines-and-recurrence.md`](../02-domain/routines-and-recurrence.md) §Streak). |
| `unchanged` | Routine ignores the shift. |

The rules are built as pure functions in `crates/sunrise-domain/src/shift.rs`. Storing shifts, the per-routine field, the CLI's `sunrise shift start|end|status` and the macOS "Start shift…" action are not built yet (ADR-0073 §Build status, [#1244](https://github.com/justin13888/Sunrise/issues/1244)).

## 6. Recover / move device

**Trigger.** User got a new phone, reformatted a laptop, or lost a device.

**Path.**
1. Install Sunrise on new device.
2. Either pair from an existing trusted device (QR or numeric code) **or** use recovery code.
3. Sync pulls down ciphertext; local key derivation completes; data appears.
4. Old device, if compromised, is revoked from any trusted device.

See [`03-crypto/pairing-and-onboarding.md`](../03-crypto/pairing-and-onboarding.md) and [`03-crypto/recovery.md`](../03-crypto/recovery.md).
