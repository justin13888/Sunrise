---
status: accepted
---

# Recurrence Engine

Materializes Routines into Tasks on a schedule. Lives in the core; runs on every device with idempotent generation.

## Inputs

- Routine entity (template + RRULE + tz + horizon + skip dates).
- Current time, in the routine's tz.
- Existing materialized occurrences (to dedup).

## RRULE subset (v1)

v1 supports the following RFC 5545 RRULE features. The same subset is reused by Google Calendar and iCalendar import/export — see [`../09-integrations/`](../09-integrations/) and reference this section.

| Property | v1 |
|---|---|
| `FREQ` | DAILY, WEEKLY, MONTHLY, YEARLY |
| `INTERVAL` | yes |
| `COUNT` | yes |
| `UNTIL` | yes (UTC) |
| `BYDAY` | yes (e.g. `MO,WE,FR`, `1MO`, `-1FR`) |
| `BYMONTHDAY` | yes |
| `BYMONTH` | yes |
| `BYSETPOS` | yes |
| `BYHOUR`, `BYMINUTE`, `BYSECOND` | no |
| `BYWEEKNO`, `BYYEARDAY` | no |
| `WKST` | yes (default `MO`) |
| `EXDATE` | yes — maps to `Routine.skip_dates` at import |
| `RDATE` | no — dropped at import with an `int.import.rrule_lossy` warning |
| `RSCALE` | no |

`EXDATE` and `RDATE` are separate iCal properties, not RRULE parts. EXDATE maps to `Routine.skip_dates` at import; RDATE is not supported in v1 (import drops it with an `int.import.rrule_lossy` warning).

Unsupported properties on import are silently dropped with a `int.import.rrule_lossy` `warn` log. Parsing uses a hand-written parser in `sunrise-domain` (`crates/sunrise-domain/src/rrule.rs`), not a third-party crate: the supported subset is deliberately narrow, it adds zero unvetted transitive dependencies to a security-frozen workspace, and it lets us emit an exact error taxonomy rather than remap a library's errors. See [`../01-architecture/dependencies.md`](../01-architecture/dependencies.md).

## Output

- Zero or more new Task entities created within `[now, now + horizon]` with `routine_id` and `routine_occurrence` set.

## Algorithm

```
for each non-paused, non-archived Routine R:
    for each occurrence O in expand(R.rrule, R.starts_at, R.ends_at, [now, now+horizon]):
        if O in R.skip_dates: continue
        occurrence_key := stable_id(R.id, O)
        if exists Task with (routine_id=R.id, routine_occurrence=O):
            continue
        emit CreateTask op with:
            id = derive_task_id(R.id, O)              # deterministic
            routine_id = R.id
            routine_occurrence = O
            stream_id = R.template.stream_id
            title = R.template.title
            … (rest of template)
            scheduled_at = O
```

`stable_id` and `derive_task_id` are deterministic. This guarantees idempotence: any device running the engine for a given (R, O) will emit the *same* op_id, and the merge layer dedups on it.

## Horizon

Default: 14 days for `FREQ=DAILY` or shorter; 60 days for weekly+. Configurable per Routine. The horizon is a generation hint, not a constraint — past-horizon occurrences will be generated next time the engine runs.

## Catchup policy

When a device wakes after being offline for many days:

- `skip` policy: drop missed occurrences.
- `merge` policy: emit one Task summarizing them.
- `queue` policy: emit one Task per missed occurrence.

Selection UX: per-Routine setting in the Routine config view — `Catch-up policy` segmented control with options Skip / Merge / Queue. Default = Skip.

The op-log idempotence still applies: a device that *previously* emitted an op for a missed occurrence (because it was online then) won't re-emit; the offline device sees that op via sync and merges.

## Edge cases

- **DST transitions.** Occurrence is computed in the routine's tz. If a 9am routine falls in a "spring forward" gap, we use the same wall-clock 9am after the gap (skip the missing hour); for "fall back," we keep the first occurrence.
- **Tz changes.** If the routine's tz is changed, future occurrences shift. Past-generated occurrences are not retroactively moved.
- **Routine deletion.** Stops generation. Existing occurrences remain unless explicitly deleted by the user.
- **Adaptive cadence (`every X days since last completion`).** Stores `(last_completed_at, X)` as an LWW Register. Concurrent completions: the most recent `last_completed_at` wins, ties broken by lex `device_id`. The "next due" is `last_completed_at + X days`, recomputed on every read. See [`../05-sync/conflict-resolution.md`](../05-sync/conflict-resolution.md).

## Streak counter

- Maintained on the Routine entity as a PN-counter.
- `complete(occurrence)` on or before the occurrence's scheduled day + grace = +1.
- `skip(occurrence)` or missed = -1 or reset to 0 (configurable).
- Re-completing a previously-skipped occurrence does not retroactively repair the streak.

## Generation timing

- On every app launch.
- On a periodic core timer (every 6 hours when running).
- On a Routine edit.
- After bulk sync application (since new ops may have come from another device that already generated some occurrences).

## Tests

- Property tests: random RRULEs across DST transitions; assert generation is deterministic and idempotent.
- Multi-device convergence test: two devices with different clocks generate the same Routine's occurrences; assert no duplicates after merge.
