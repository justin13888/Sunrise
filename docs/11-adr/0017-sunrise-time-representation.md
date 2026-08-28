# 0017 — Scheduled times are a tagged `SunriseTime`, not a UTC instant

**Status:** accepted

**Extends:** [ADR-0011 — jiff for date/time](./0011-datetime-jiff.md), whose
civil types this builds on.

**Closes:** issue #6.

## Context

Every scheduled field in v1 was a `jiff::Timestamp` — a UTC instant.
`Task.scheduled_at`, `Task.due_at`, `Task.completed_at`, `Block.starts_at`,
`Block.ends_at`. That is exactly **one** of the four things a person means by
"when", and storing the other three as instants corrupts them:

| The user means | Stored as an instant | What goes wrong |
|---|---|---|
| "the meeting, starting now" | the instant | nothing — this is the case the type fits |
| "09:00 in New York, whatever I'm doing" | the instant 09:00 happened to be, once | correct until the DST rule for that date changes; then it is an hour off, silently, forever |
| "sometime Tuesday morning" | 09:00 UTC on Tuesday | west of UTC a Tuesday-morning task shows up on Monday evening |
| "my birthday, the 4th" | midnight UTC on the 4th | west of UTC the birthday is on the 3rd |

The corruption is invisible at the point of writing and only appears when the
reader is in a different zone, or when tzdata changes, or when the user travels
— which is to say, exactly when a task manager is most load-bearing.

`ScheduleConstraint` had already run into this and solved it locally: it models
time-of-day windows with `jiff`'s civil types, because a "no meetings before
09:00" rule is obviously not an instant. The rest of the domain had not caught up.

## Decision

**A tagged `SunriseTime` with four variants**, applied to those five fields:

```
Instant  { at: Timestamp }                        a fixed point on the timeline
Zoned    { civil: civil::DateTime, tz: String }   a civil time pinned to a named zone
Floating { civil: civil::DateTime }               a civil time with no zone
AllDay   { date: civil::Date }                    a date with no time
```

The kind is part of the **value**, not a convention the reader has to remember:

* `Zoned` carries its zone **name**, not an offset. The instant is derived on
  read, so it stays correct when the zone's rules change — an offset is a fact
  about a zone on a date, not a fact about the value.
* `Floating` and `AllDay` carry no zone at all and resolve against the
  **reading device's** zone. That is the point, not an omission: "09:00" follows
  the user, and "the 4th" is the 4th wherever you are.
* `Instant` is what everything used to be, so an existing value degrades to
  exactly what it always meant.

### Storage: one index column, two sidecars

Each field projects onto three columns — `*_at_ms`, `*_at_kind`, `*_at_tz`:

* `*_at_ms` is the epoch-millisecond **index key**, and it is what every range
  query and `ORDER BY` in the engine reads. This is why the change touched none
  of them.
* For `Instant` and `Zoned` the key is the true instant. For `Floating` and
  `AllDay` — which have no instant until a reader supplies a zone, and a
  database index cannot wait for a reader — it is the **UTC anchoring**. That is
  stable across devices (so an index built on one is valid on another) and
  lossless (anchoring in UTC is a bijection, so `from_parts` reconstructs the
  exact civil value), and it is within a day of any real resolution, which makes
  it a usable coarse filter. Anything needing exactness calls `to_instant` after
  reading.
* The kind cannot be inferred from the key, which is precisely why losing it was
  the bug.

### Compatibility

`DOC_SCHEMA_V` moves to 2. `DOC_SCHEMA_FLOOR` stays at 1, because a v1 payload
still decodes: `SunriseTime`'s hand-written `Deserialize` reads a bare instant
as `Instant`. This is the first real exercise of
[ADR-0015](./0015-envelope-doc-schema-split.md)'s split, and it is what that
split was for.

## What we give up

* **`Option<SunriseTime>` is not `Copy`.** `Zoned` owns a `String`, so ~180
  call sites moved from `t.due_at` to `t.due_at.as_ref()`. Interning zone names
  would restore `Copy` at the cost of a global table; not worth it.
* **Comparison needs a decision at every site.** Two `SunriseTime`s can be
  compared on the index key (cheap, zone-free, matches SQL) or resolved against
  a zone first (correct for display). `Ord` is defined on the index key so a
  sorted list matches `ORDER BY *_at_ms`, and `to_instant` takes a `TimeZone`
  argument so the resolving path cannot be taken by accident.
* **An unresolvable zone name, or a civil time in a DST gap, falls back rather
  than failing.** A scheduled time that cannot be rendered is worse than one
  rendered an hour off, and the stored civil value is unchanged either way.
* **The chrono-era Task fixture no longer re-encodes byte-identically.** It
  still decodes with value identity, which is the contract that test exists for.

## Alternatives considered

| Option | Why rejected |
|---|---|
| **Keep `Timestamp`, add a separate `is_all_day: bool` / `tz: Option<String>` beside each field** | Three fields per time, none of which the type system ties together, and every reader free to consult one and forget the others. It is the current bug with more places to introduce it. |
| **Store everything as `Zoned`, with the device zone for "floating"** | Freezes the authoring device's zone into the value. Fly to Berlin and the 09:00 task moves to 03:00 — the exact failure `Floating` exists to prevent. |
| **RFC 5545 `DATE` / `DATE-TIME` / `DATE-TIME` with `TZID`** | The right *model*, and this is essentially it. Rejected only as a serialization: the iCal string forms need parsing rules that CBOR gives for free, and the domain already speaks jiff. |
| **A second index column per field, resolved in the device zone** | Would make zone-less range queries exact, but the column would have to be rewritten for every row whenever the device zone changed. Coarse-and-stable beats exact-and-invalidated. |
| **Tagged `SunriseTime`, one index key plus sidecars (chosen)** | The kind travels with the value, storage keeps one comparable key, and no query changed. |

## Consequences

* **`Task.completed_at` is always written as `Instant`.** A completion is a
  recorded fact about a moment, not a plan. It is typed like its siblings so a
  reader has one shape to handle, not because the other kinds are meaningful
  there.
* **Routine occurrences remain instants.** The recurrence engine resolves each
  occurrence against the routine's **own** timezone; a floating start would be
  resolved twice against two different zones.
* **Scheduling constraints resolve zone-less times in the device zone**, which
  is what `docs/02-domain/scheduling-constraints.md` §Evaluation timezone
  already specifies for a Task.
* **Clients must not render a `SunriseTime` by resolving it silently.** The
  `Display` impl shows the kind (`2024-06-01T09:00:00[America/New_York]`,
  `2024-06-01T09:00:00`, `2024-07-04`) precisely because a rendering that hides
  it is how "sometime Tuesday" became "Monday 20:00" in the first place.

## What would force revisiting this

1. **Recurring blocks or tasks with a floating anchor.** Expanding a recurrence
   from a zone-less start needs a rule for which zone the expansion happens in;
   the routine's own zone is the obvious answer but it is not written down yet.
2. **Cross-user shared scheduling.** Two people in two zones editing one Block
   raises "whose 09:00?", which no representation answers on its own.
3. **Sub-millisecond precision.** The index key is milliseconds; anything
   needing finer would need a wider column, not a new kind.
