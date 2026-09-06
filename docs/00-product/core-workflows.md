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

**Promote / demote semantics.** A Task in Inbox can be promoted to a Stream (move + retag) and demoted back to Inbox (move to Inbox + clear stream-specific state). Both are ordinary `task.update` ops and use the cross-stream-move rules in [`../05-sync/conflict-resolution.md`](../05-sync/conflict-resolution.md); there is no special "promotion" op kind. Bouncing between Inbox and a Stream is allowed unconditionally; *Target state:* the task carries a `move_history` list (append-only, capped at the last 16 entries, oldest pruned on write). No such field exists on `Task` today, and promote/demote is a plain `task.update` with no history behind it. Undo is the inverse op and shares the same merge rules. Promotion order is deterministic across devices, broken by the one comparison key the whole system uses: `(hlc, device_id, seq)` ([`../05-sync/conflict-resolution.md`](../05-sync/conflict-resolution.md) §The comparison key).

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
1. User activates a "context" (e.g. `Travel: Tokyo`).
2. Sunrise filters: streams not relevant become collapsed, items tagged for the trip become primary, recurring routines pause or shift.
3. On return, Sunrise restores prior state and surfaces what accrued during the shift.

**Routine behavior during a context shift.** Each active Routine evaluates against the active context's policy (a per-routine field, synced like any other, default `pause`):

| Policy | Routine behavior during the shift |
|---|---|
| `pause` | Generation horizon stops advancing; existing future occurrences are tombstoned. On resume, generation resumes from `now` (no backfill). |
| `shift_tz` | Future occurrences are re-anchored to the user's *new* TZ. The local time of the rule (e.g. `09:00`) stays; the UTC time changes. DST in either zone follows the standard "wall-clock preserved" rule. |
| `skip` | Occurrences that would fire during the shift are tombstoned with `cause: "context_shift"`. The streak counter applies the routine's `forgiveness_in_window` rule (see [`../02-domain/routines-and-recurrence.md`](../02-domain/routines-and-recurrence.md)). |
| `unchanged` | Routine ignores the shift. |

## 6. Recover / move device

**Trigger.** User got a new phone, reformatted a laptop, or lost a device.

**Path.**
1. Install Sunrise on new device.
2. Either pair from an existing trusted device (QR or numeric code) **or** use recovery code.
3. Sync pulls down ciphertext; local key derivation completes; data appears.
4. Old device, if compromised, is revoked from any trusted device.

See [`03-crypto/pairing-and-onboarding.md`](../03-crypto/pairing-and-onboarding.md) and [`03-crypto/recovery.md`](../03-crypto/recovery.md).
