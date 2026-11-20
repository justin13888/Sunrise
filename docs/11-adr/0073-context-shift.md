# 0073 — A context shift is a synced `Shift` entity, and each routine's `shift_policy` is evaluated at generation time, never by rewriting history

**Status:** accepted

**Amends** [`../00-product/core-workflows.md`](../00-product/core-workflows.md)
§5 Travel / context shift: the "context" the user activates is a `Shift`
(decision 2), the four policies mean what decision 4 says, and `pause` no
longer tombstones stored occurrences.

**Depends on** [ADR-0044](./0044-per-field-ops.md) (per-field ops),
[ADR-0045](./0045-schema-identity-and-feature-gating.md) (feature gating and
lossless enums), [ADR-0051](./0051-places.md) (the `plc_` a shift may name),
[ADR-0066](./0066-times-survive-travel.md) (floating and zoned times) and
[`../10-cross-cutting/time.md`](../10-cross-cutting/time.md) §3 and §6 (the one
DST rule, civil-space expansion).

**Answers** [#352](https://github.com/justin13888/Sunrise/issues/352).

## Context

Core workflow §5 specifies a "context shift": the user starts one ("Travel:
Tokyo"), streams that are not relevant collapse, routines pause, shift or skip
by a per-routine policy, and on return the prior state comes back with a
summary of what accrued. None of it was built, and three things in the tree
stood in its way:

- `Routine.paused` / `paused_until` and `Stream.paused` are manual flags. They
  have no notion of a stretch of time that ends by itself, and flipping them
  for every routine at the start of a trip and back at its end is a write per
  routine at each edge, which two devices doing concurrently would race.
- The workflow called the thing a "context", and `Context` is already the tag
  entity (`ctx_`, `crates/sunrise-domain/src/context.rs`).
- The workflow's `pause` "tombstones existing future occurrences", and its
  `skip` relies on a `forgiveness_in_window` streak rule that does not exist.
  A tombstone written at the start of a shift has to be un-written at its end,
  so "restore the prior state" would be a second batch of writes that a
  concurrent edit could interleave with.

## Decision

### 1. `Shift` is a synced entity

```cddl
Shift = {
    id:                tstr .regexp "shf_[0-9A-HJKMNP-TV-Z]{26}",
    created_at:        timestamp,
    updated_at:        timestamp,
    name:              text<128>,           ; "Travel: Tokyo"
    starts_at:         timestamp,           ; an instant: a recorded moment
    ? ends_at:         timestamp,           ; exclusive; absent while it runs
    ? zone:            tstr,                ; destination IANA zone
    ? place:           entity-ref,          ; a plc_ (ADR-0051)
    primary_streams:   [* entity-ref],      ; str_ refs the shift keeps primary
    primary_contexts:  [* entity-ref],      ; ctx_ refs the shift keeps primary
    deleted:           bool,
    unknown-fields,
}
```

- It lives on the vault-meta stream and is written only by `Patch`, one
  register per field (ADR-0044), exactly as a `Place` is. Ending a shift is a
  `Patch` of `ends_at`; renaming it on one device while another ends it keeps
  both.
- It is gated on the feature `shift.entity` (ADR-0045): no `Patch` naming
  `shf_` is written until every device advertises it.
- **At most one shift is in force at an instant**: of the live shifts that
  cover it (`starts_at <= at < ends_at`), the one with the latest
  `starts_at`, a tie going to the greater id. It is a pure function of the
  set of shifts, so two devices that start shifts concurrently pick the same
  one as soon as they hold the same set, with no coordination. Shifts may
  overlap: a conference inside a month-long trip is in force for its days,
  and the trip again after them.
- `Shift::validate` refuses an empty name, an end not after the start, a zone
  the tzdb does not know, a `place` that is not a `plc_`, and focus lists of
  the wrong kinds.

### 2. The name is `Shift`; `Context` stays with tags

The product copy may still say "context shift". No identifier, table, command
or FFI type uses "context" for it.

### 3. Each routine carries a `shift_policy`

`Routine.shift_policy`, one of `pause`, `shift_tz`, `skip`, `unchanged`, is a
string-valued enum made lossless by `lossy_enum!` (ADR-0045 §6). Absent on the
wire, it is `pause`, so every routine written before the field reads as the
default without a migration, and a routine that never changes it encodes as
it did before. An unknown value reads as `pause`, the policy that generates
nothing, and is written back verbatim. Writing a non-default value requires
the feature `routine.shift_policy`.

### 4. The policy is evaluated at generation time

Nothing the user stored is rewritten when a shift starts or ends. The shift
changes what a routine's generation produces while it is in force, and ending
it changes that back, so the "prior state" is never saved and never restored:
it is what generation produces with no shift in force.

For each occurrence the routine itself generates (its rule, end, own pause and
skips first: `Routine::occurrences_in`), at the instant it would fire:

| Policy | While a shift covers the occurrence |
|---|---|
| `pause` | Not generated, and not missed. The horizon resumes at the shift's end with no backfill: a covered key never becomes a catch-up. |
| `shift_tz` | Generated at its key's wall-clock time in the shift's `zone`, by the one DST rule (time.md §3), and planned `zoned` there. The key is unchanged, so the task id, a skip and the streak still agree. |
| `skip` | Generated as skipped, with cause `context_shift`. The streak excuses it. |
| `unchanged` | Generated as if there were no shift. |

- **Which shift covers a `shift_tz` occurrence** is decided on the
  re-anchored instant: the occurrence moves to the zone of the shift that is
  in force at the moment it would fire there. An occurrence whose re-anchored
  time would fall outside every shift fires at its own time, so nothing fires
  before the trip begins at a time the trip chose.
- **A floating routine is not re-anchored.** It already follows the reader
  (ADR-0066 §1): on a device in Tokyo, "09:00 wherever I am" is 09:00 Tokyo.
  A shift with no `zone` re-anchors nothing.
- **The streak** treats an occurrence a `pause` or `skip` policy held back as
  neutral, as it treats an explicit skip: it is neither counted nor missed,
  and spends no forgiveness. This is the `forgiveness_in_window` behaviour the
  workflow named, and it needs no stored state: the derived streak
  (`Routine::derived_streak`) and the read-time miss rule
  (`Routine::streak_as_of`) both take the set of shifts. A miss outside the
  shift still spends the allowance. `shift_tz` and `unchanged` excuse
  nothing: the routine was still asked of the user.
- **Already materialized tasks.** An occurrence task generated before the
  shift started is not deleted or edited. While the shift covers it, views
  read it through the policy: hidden under `pause`, shown as skipped with its
  cause under `skip`, at its re-anchored time under `shift_tz`.

### 5. Focus while a shift is in force

A shift names the streams and contexts it keeps primary. A task is primary
when its stream is one of them or it carries one of the contexts; views demote
the rest behind one collapsed group headed "Paused for *name*". Nothing is
deleted, moved or rescheduled. A shift naming neither demotes nothing.

### 6. Ending a shift, and "While you were away"

Ending is a `Patch` of `ends_at`; per decision 4 there is nothing else to
restore. The client then opens a "While you were away" summary built by one
Rust read over the shift's window (its start to its end, or to the read when
it is still running):

- **Missed**: each occurrence its routine's policy generated during the
  shift that is not in the routine's streak keys and whose task is neither
  done nor cancelled. A `pause` or `skip` routine misses nothing the shift
  covered.
- **Deadlines passed**: each open task whose `hard_due_at`, resolved in the
  reader's zone, fell in the window.
- **Captured**: each live task created in the window that no routine
  generated.

### 7. Surfaces

- CLI: `sunrise shift start <name> [--zone Z] [--place plc_…]`,
  `sunrise shift end` and `sunrise shift status`.
- macOS: a "Start shift…" command-palette action and a toolbar indicator, each
  with its key hint ([`../08-features/keyboard.md`](../08-features/keyboard.md)).

## Alternatives considered

- **Reuse `Routine.paused_until` and `Stream.paused`.** Rejected: a write per
  routine and stream at each edge of the shift, a race between two devices
  doing it, and an end that must find and undo exactly those writes.
- **Tombstone occurrences when the shift starts.** Rejected for the same
  reason: restoring the prior state would be a second batch of writes. A
  derived evaluation is convergent by construction.
- **Re-anchor floating routines too.** Rejected: the device already reports
  where the user is (time.md §7), and a shift's zone that disagrees with it
  would move a "wherever I am" routine away from where the user is.
- **Decide `shift_tz` membership on the occurrence's original instant.**
  Rejected: an occurrence the original zone puts just inside the shift could
  re-anchor to a time before the shift began.
- **Count a skipped occurrence as a forgiven miss** (spending the one
  allowance per 30 days). Rejected: a two-week trip would always end a streak,
  which is what the policy exists to prevent.
- **Call the entity `Context`.** Rejected: it collides with the tag entity.

## Consequences

- Starting or ending a shift is one small write, and every replica derives
  the same generation, streak and summary from the same set of shifts.
- Generation, the streak and every view that shows an occurrence take the set
  of shifts as an input. A path that forgets to pass it behaves as if no shift
  existed: the routine runs `unchanged`.
- An unknown policy from a newer build pauses, which can hide an occurrence
  that build would have generated; it never authors one that build would not.

## Build status

- **Built** (`crates/sunrise-domain/src/shift.rs`): the `Shift` value and its
  validation, the in-force rule (`active_shift`), `ShiftPolicy`, generation
  under a policy (`Routine::occurrences_under_shifts`), the streak excusal
  (`Routine::derived_streak_across`, `Routine::streak_as_of_across`), focus
  (`Shift::keeps_task_primary`) and the summary (`away_summary`), with tests
  for each policy across a zone change and across a DST change inside the
  shift, the streak across a `skip` window, and concurrent shifts converging.
- **Not yet built**: the `shf_` kind and its registry entry, the
  `Routine.shift_policy` field, storage, the engine commands and queries,
  generation wired to the policy, the bindings, `sunrise shift` and the macOS
  surfaces. They are tracked in
  [#1244](https://github.com/justin13888/Sunrise/issues/1244).

## What would force revisiting this

- A shift that must apply to only some devices (one partner travelling in a
  shared vault).
- Routines needing a policy per shift rather than one per routine.
- A destination given only as a place, with the zone to be derived from it.
