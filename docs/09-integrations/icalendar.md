---
status: accepted
---

# iCalendar (.ics) Import / Export

**One-shot file import and export. There are no subscription URLs yet; they are
ranked as [#931](https://github.com/justin13888/Sunrise/issues/931).** `URL` is
explicitly unsupported by the parser — it raises a notice like any other
unmodelled property — and nothing in the workspace fetches an `.ics` over the
network. The calendar integrations (Google Calendar, Microsoft Graph and CalDAV,
[ADR-0071](../11-adr/0071-calendar-read-write-through-synced-intents.md))
are not built; they are ranked on the roadmap ([`../roadmap.md`](../roadmap.md)),
and they write `ExternalEvent`s, never Blocks. So this is the only calendar path a
user can actually take today, and the only one that creates Blocks.

> **Status: partly implemented. This document is the target; the list below is
> what ships.** `crates/sunrise-integrations` implements the syntax layer
> (`ical`), the domain mapping (`ical_map`) and the vault driver
> (`ical_vault`), reached today by `sunrise ical import` / `sunrise ical export`
> and by `import_ical` / `export_ical` on the UniFFI seam. **Both shipping
> clients call it**: the macOS app through File → Import Calendar… (⌘⇧I) and
> Export Calendar ▸ Today | This Week. The import's notices reach the user
> there, grouped by code, rather than being counted.
>
> Where this document and the build disagree:
>
> - **`VTIMEZONE` is not parsed or emitted.** §Time zones below describes the
>   target. A `TZID` is resolved against the **bundled IANA tzdb** instead; a
>   `TZID` the tzdb does not know is read as UTC and reported. An inline
>   `VTIMEZONE` raises an unsupported-component notice.
> - **A recurring event imports as one recurring Block** when a Block can
>   repeat it: `RRULE`, `EXDATE` and `RECURRENCE-ID` overrides map onto its
>   series (§Recurrence below). A rule outside the engine subset, a
>   whole-day recurring event, and a vault whose paired devices cannot show a
>   recurring block yet each import the **first occurrence** instead, and say
>   so in an `ICalNotice`. `RDATE` is not read.
> - **Export is windowed, not scoped by Stream.** `ExportWindow` is `Day` or
>   `Week`, because `Query::DayBlocks` / `WeekBlocks` are the only Block windows
>   the core has — which is why the macOS menu offers exactly Today and This
>   Week, and the CLI exactly `today` / `day` / `week`. "Export this Stream as
>   .ics" is not implemented on either.
> - **An exported file carries `UID`, `SUMMARY`, `DTSTART`, `DTEND`,
>   `LOCATION`, `DESCRIPTION`, `RRULE`, `EXDATE` and `RECURRENCE-ID`
>   overrides only**, each where the Block holds it — emitting an empty
>   property would be inventing content. No `DTSTAMP` is written, which some
>   strict readers require.
> - **Dedup is by `(source, uid)`, hashed into the Block's id — not by a
>   column.** The pair is hashed into the Block's **id**, exactly as a
>   materialized routine occurrence hashes `(routine, occurrence)` into a
>   Task's, which gives idempotent re-import with no side table to keep in step
>   with the vault. There is no `import_source_id`, and the `external_id` field
>   [ADR-0025](../11-adr/0025-integration-account-entity.md) adds **is not the
>   dedup key** — it exists to round-trip a foreign id outward. See
>   [`../02-domain/time-blocks.md`](../02-domain/time-blocks.md).
> - **Imported entities are not marked read-only.** Nothing enforces it.
>
> What does hold as written: line unfolding and escapes both directions, CRLF
> and LF input (the content-line lexer,
> `crates/sunrise-integrations/src/ical/lex.rs`), the four RFC 5545 time forms
> preserved distinctly (UTC instant, zoned civil, floating, whole date) all the
> way into `SunriseTime` (the date and date-time grammar,
> `crates/sunrise-integrations/src/ical/time.rs`), and the rule that lossy
> imports are surfaced rather than dropped (the component parser and writer,
> `crates/sunrise-integrations/src/ical/mod.rs`).

## Import

- File picker → parse `.ics` → create Blocks tagged `source = import:ics`,
  which read as `kind = fixed`: the planner never moves an imported event.
  One Block per `UID`: the `VEVENT`s carrying a `RECURRENCE-ID` are
  overrides of the event of their `UID`, not Blocks of their own.
- **Re-importing the same file is idempotent**, because the Block's id *is* the
  hash of `(source, uid)`: the same UID from the same source computes the same
  id and updates the Block already there, and the same UID from two different
  sources computes two ids and so creates two Blocks. No side table, no lookup,
  nothing to keep in step with the vault. `ICS_SOURCE` is the constant the file
  importer passes.

## Export

- Export covers exactly two windows, **Today** and **This Week**:
  `ExportWindow::{Day, Week}` (`crates/sunrise-integrations/src/ical_vault.rs:71-77#ExportWindow`),
  surfaced as `sunrise ical export [today|day|week] [path]`
  (`crates/sunrise-cli/src/cmd/reports.rs#ical_export`). A whole-Stream export is target state,
  not a shipped option.
- Useful for: sharing a calendar slice with someone outside Sunrise; importing into Outlook etc.

## Mapping rules

**This is not the CalDAV mapping.** CalDAV is a live integration
([`caldav.md`](./caldav.md)), and it maps events onto `ExternalEvent`, not
onto Blocks, so it shares no table with this file. The
`.ics` file mapping is below, and it is read off
`crates/sunrise-integrations/src/ical_map/` — `group_events`,
`event_to_block`, `block_to_event` and `block_to_events` — which is the only
place it exists.

### `VEVENT` → `Block`

A `VEVENT` becomes a **Block**, never a Task. An event is "this happens between
these two times", which is what a Block is; a `VTODO` really is a Task and is
reported rather than mapped at all (§What the subset REPORTS below).

| iCalendar | Sunrise | What happens on import |
|---|---|---|
| `UID` | the Block's **id**, via `sunrise_domain::imported_block_id(source, uid)`, and `Block::external_id` | The dedup key, and not a column: `(source, uid)` is hashed into the id's 16 bytes. Same source + same UID updates the Block already there; one UID under two sources is two Blocks. A UID Sunrise itself exported (`<block-id>@sunrise.invalid`) is recognised and returns that Block id unhashed, so export → re-import is the identity. The UID is also kept in `external_id`, which is never the dedup key, so export can write it back. |
| `SUMMARY` | `Block::title`, with `title_track_task = false` | Absent, empty or whitespace becomes `(untitled event)` rather than an unlabelled bar on the calendar. The title is the calendar's, so it does not track a Task's title even if one is later bound. |
| `DTSTART` | `Block::starts_at` (`SunriseTime`) | Required. An event without one raises a `Skipped` notice and the import continues with the next event. |
| `DTEND` / `DURATION` | `Block::ends_at` (`SunriseTime`) | `DTEND` wins when a file carries both, and the ignored `DURATION` is reported. With neither, RFC 5545 §3.6.1 gives a whole-date event one day and every other event zero length — and a zero-length or backwards event is `Skipped`, because a Block is a range. |
| `DESCRIPTION` | `Block::notes` | One paragraph per blank-line-separated run, as the CLI writes plain text into a note. Empty is absent. |
| `LOCATION` | `Block::location` | Trimmed; empty is absent. Longer than the 128 characters a Block holds, it is cut there and reported. |
| `RRULE` | `Block::rrule` | The event's bounds become the series anchor. See §Recurrence. |
| `EXDATE` | a cancelled `BlockException` | One per value, at its occurrence key. See §Recurrence. |
| `RECURRENCE-ID` | a `BlockException` | The override of one occurrence. See §Recurrence. |
| everything in §What the subset REPORTS | — | one `ICalNotice` each, shown by the caller. |

### Recurrence

A recurring Block repeats a wall-clock time
([`../02-domain/time-blocks.md`](../02-domain/time-blocks.md) §Recurring
blocks), so its anchor is `zoned` or `floating`:

- **Anchor.** A `TZID` start keeps its zone, and an end in another zone or in
  UTC is read as wall clock in it. A UTC (`…Z`) start is read as wall clock in
  the zone `UTC`, which names the same instants, and exports as `TZID=UTC`. A
  floating start needs a floating end. A whole-day (`VALUE=DATE`) recurring
  event has no wall-clock time to repeat and imports as its first occurrence,
  with a notice.
- **`RRULE`** is the rule, read by the engine's parser except for `UNTIL`,
  which is read in the iCalendar time grammar (`20260331T035959Z`) and held
  in the anchor's civil frame, as
  [`../10-cross-cutting/time.md`](../10-cross-cutting/time.md) §Which kind
  each field uses requires: a UTC or zoned `UNTIL` beside a zoned anchor is
  the wall clock it falls on in the anchor's zone; beside a floating anchor a
  zoned one is its own wall clock and a UTC one its UTC wall clock. A rule
  outside the engine subset
  ([`../08-features/recurrence-engine.md`](../08-features/recurrence-engine.md):
  `RRule::first_unknown`, `BYWEEKNO`, `BYYEARDAY`, …) is reported, not guessed
  at, and the event imports as its first occurrence; its `EXDATE`s and
  overrides are reported with it.
- **`EXDATE`** values, every one of every line, are cancelled exceptions at
  their occurrence keys. A value is read as wall clock in the anchor's frame:
  a UTC or other-zone value is converted into the anchor's zone, and a whole
  date takes the anchor's time of day. A value that names no occurrence of
  the rule (or, beside a floating anchor, a UTC or zoned value, which names a
  different occurrence for every reader) is reported and dropped.
- **`RECURRENCE-ID` overrides**, the `VEVENT`s sharing the event's `UID`, are
  exceptions at the occurrence their `RECURRENCE-ID` names, holding what they
  change from the series: the start and end where they moved, and the title,
  location and notes where they differ. An override that changes nothing
  writes nothing. An override with `STATUS:CANCELLED` is a cancelled
  exception at its occurrence, as an `EXDATE` is, whatever else it states;
  an orphan one is imported as a single event and its notice says it was
  cancelled. One whose `RECURRENCE-ID` names no occurrence is reported
  `Skipped`, and `RANGE=THISANDFUTURE` is reported and applied to its one
  occurrence. A property an override leaves out keeps the series' value,
  since an exception cannot clear a field. An override whose `UID` has no
  event in the file is imported as a single event, and reported.
- **Re-import** writes the series whole, as it writes the times and the
  title: the file states what the calendar holds. A user's edit of one
  occurrence of an imported block is overwritten by the next import, as an
  edit of its title is.
- **Feature gate.** A series is written only once the vault requires
  `block.recurrence`, which an import does by itself when every paired
  device supports it. While one does not, the event imports as its first
  occurrence with a notice, and a later re-import brings the series.

### Time forms

RFC 5545 writes a time four ways and `SunriseTime` models the same four
distinctions, so this row of the mapping is total in both directions and loses
nothing:

| iCalendar | Sunrise |
|---|---|
| `20260301T140000Z` | `SunriseTime::Instant` |
| `TZID=Europe/Berlin:20260301T090000` | `SunriseTime::Zoned` |
| `20260301T090000` (no zone) | `SunriseTime::Floating` |
| `VALUE=DATE:20260301` | `SunriseTime::AllDay` |

A `TZID` the bundled IANA tzdb does not know falls back to UTC and is reported;
see §Edge cases.

### `Block` → `VEVENT`

Export writes these properties and deliberately no others:

- `UID`: the Block's `external_id`, the UID it was imported with, so the
  calendar it came from recognises the event and re-importing under the same
  source lands on the same Block; for a Block with none (made in Sunrise),
  `block_uid`, its own id under the RFC 2606 `.invalid` host.
- `SUMMARY`: the *resolved* title, so a Block that tracks its Task exports
  what a reader would see.
- `DTSTART` and `DTEND`: for a recurring Block, its series anchor.
- `LOCATION` and `DESCRIPTION` (the notes' paragraphs, a blank line between
  them; a note holding other blocks exports its plain text).
- `RRULE`, its `UNTIL` in UTC beside a zoned anchor (RFC 5545 §3.3.10), and
  one `EXDATE` line per cancelled occurrence.
- One `RECURRENCE-ID` override `VEVENT` per occurrence an exception changes,
  stating the occurrence whole: what the exception leaves absent is the
  series'.

A window export (`ExportWindow::Day` / `Week`) writes a recurring Block's
series once, as stored, whatever number of its occurrences the window holds,
so the reader recurs it the same way. `DURATION` is never written: a Block
holds an end. No `DTSTAMP` is written, which some strict readers require.

### The round-trip fidelity rule

The honest rule, replacing "we attempt to preserve fidelity round-trip when
possible":

- **Export → import is the identity**, and is tested as one. Every property the
  exporter writes is one the importer reads back onto the same Block, and the
  `UID` recognition above is what makes the re-import an update rather than a
  second copy.
- **Import → export is not the identity**, and is not claimed to be. It
  preserves what a `Block` can hold — identity, title, start, end, which of
  the four time forms each of those was written in, location, description,
  and a recurring event's rule, cancelled occurrences and overrides — and
  nothing else. Every property outside that set is reported on the way in as
  an `ICalNotice` and is simply absent from anything exported later. A UTC
  recurring start comes back as `TZID=UTC`, the same instants.

So fidelity is promised over the **Block schema**, not over the file. The
guarantee that does hold over the whole file is the reporting one below: what
is not preserved is said out loud rather than dropped in silence.

### RRULE subset

The recurrence vocabulary this codebase understands is the engine subset
documented in
[`../08-features/recurrence-engine.md`](../08-features/recurrence-engine.md),
and the two layers apply it differently: the syntax layer (`ical`) reads an
`RRULE` and writes it back verbatim without validating it against that
subset, so a file round-tripping through `ical` alone keeps its rule whatever
it says; the domain mapping (`ical_map`) keeps a rule only when every part is
in the subset, and reports any other (§Recurrence).

### What the subset REPORTS rather than silently drops

This is the property that holds today, and it is the one worth protecting: a
parser that drops what it does not model without saying so is how a user
discovers, weeks later, that half their calendar is missing. Every one of the
following raises an `ICalNotice` that the caller shows — the CLI prints them,
and the macOS app groups them by code:

`VTODO`, `VJOURNAL`, `VFREEBUSY`, `VALARM`, `VTIMEZONE`, `RDATE`, `ATTACH`,
`ATTENDEE`, `ORGANIZER`, `CATEGORIES`, `GEO`, `URL`, and **any `X-`
property**.

Two deliberate carve-outs:

- **`RRULE`, `EXDATE` and `RECURRENCE-ID` are not on that list at the syntax
  layer.** They are read and written back, so a file round-tripping through
  `ical` keeps them. A notice about one comes one layer up, from `ical_map`,
  when a Block cannot carry it (§Recurrence).
- **Seven properties are dropped silently**, and they are exactly
  `ical::BOOKKEEPING`: `DTSTAMP`, `SEQUENCE`, `CREATED`, `LAST-MODIFIED`,
  `TRANSP`, `CLASS`, `STATUS`. They are iCalendar's own record-keeping, carry
  nothing a user typed, and Sunrise is not a CalDAV store obliged to preserve
  them byte-for-byte. `STATUS:CANCELLED` alone is read: on a
  `RECURRENCE-ID` override it cancels the occurrence (§Recurrence).

## Edge cases

- **Descriptions:** `DESCRIPTION` is stored in `Block::notes`, one paragraph per blank-line-separated run, and exported as those paragraphs with a blank line between them, with no truncation. Whitespace around each paragraph is not kept. `X-ALT-DESC` is reported as an `ICalNotice`, as any `X-` property is.
- **Attached files (`ATTACH`):** dropped on import. The UI shows `"This event had attachments which were not imported."` once per event. Documented in user-facing help. Not produced on export (attachments are heavy and require separate handling).
- **Time zones:**
  - *Target state:* `VTIMEZONE` blocks emitted on export. The exporter does not emit them; zoned times go out as TZID references without an accompanying definition.
  - *Target state:* parsing an inline `VTIMEZONE` on import and using it to resolve VEVENT TZID values. The parser skips `VTIMEZONE` bodies entirely (`crates/sunrise-integrations/src/ical/mod.rs:40-42,280-285`); a `TZID` is resolved against the **bundled IANA tzdb** instead, and the skipped component is reported as an `ICalNotice` rather than dropped silently.
  - A TZID that the bundled IANA tzdb does not know is read as UTC and reported as an `unknown_timezone` notice.
  - Floating times (no TZID) are stored as `tz: floating` and treated as user-local on each device.

## Subscribed `.ics` URLs — not implemented

**There is no subscription mechanism, and `URL` is not even parsed.** The
importer takes a file; there is no HTTP client in `crates/sunrise-integrations`
for iCal, no poll cadence, no subscription config, and no place in the vault to
store a subscription. `URL` is on the parser's explicitly-unmodelled list, so an
`.ics` that carries one raises a notice rather than being followed. The section
below is a target, and adding it means adding network fetching to a path that
today only reads a file the user chose.

- Periodic HTTPS GET, parse, treat as imported events.
- Per-subscription poll cadence: default 1 hour, range 15 minutes – 24 hours, stored in the subscription config in the vault.
- Useful for public calendars (sports schedules, conference timetables).
- Read-only.
- Per-Stream toggle.

## Privacy

Subscription URLs, if they land, are fetched from the user's device, not the
server, and the URL itself lives in the vault. Today the privacy story is
simpler and stronger: **a file import touches no network at all.**

## Test surface

Round-trip fixtures live at
`crates/sunrise-integrations/testdata/{google,apple,fastmail,outlook}/basic.ics`,
driven by `crates/sunrise-integrations/tests/ical_vault.rs` against a real
vault. They are **hand-written in the shape each vendor emits**, not captures —
the README beside them says which structural habit each one reproduces — so a
passing suite is evidence about the parser, not about a particular account.

The named behaviours the suite pins, and each is one this document asserts
above: re-import updates rather than duplicates
(`re_importing_the_same_file_updates_rather_than_duplicates`), and does so
across a vault close/reopen; the same UID under two sources is two Blocks; the
four RFC 5545 time kinds survive into `SunriseTime` and back; export followed by
re-import is the identity; a `VTODO` and an unknown `TZID` are **reported**
rather than silently dropped. `google/recurring.ics` pins the recurring
mapping: a recurring event imports as one series with its `EXDATE` and its
moved occurrence (`a_recurring_event_imports_as_one_series_with_its_exceptions`),
re-importing it rewrites the same series
(`re_importing_a_recurring_event_is_idempotent`), and its week export reads
back onto the same series
(`a_recurring_block_exports_and_re_imports_as_the_same_series`).
