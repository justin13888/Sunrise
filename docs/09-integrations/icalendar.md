---
status: accepted
---

# iCalendar (.ics) Import / Export

**One-shot file import and export. There are no subscription URLs.** `URL` is
explicitly unsupported by the parser — it raises a notice like any other
unmodelled property — and nothing in the workspace fetches an `.ics` over the
network. Google Calendar is deferred and unreachable
([`google-calendar.md`](./google-calendar.md)), and CalDAV is an explicit
non-goal, so this is the only calendar path a v1 user can actually take.

> **Status: partly implemented. This document is the target; the list below is
> what ships.** `crates/sunrise-integrations` implements the syntax layer
> (`ical`), the domain mapping (`ical_map`) and the vault driver
> (`ical_vault`), reached today by `sunrise ical import` / `sunrise ical export`
> and by `import_ical` / `export_ical` on the UniFFI seam. **Both shipping
> clients call it**: the macOS app through File → Import Calendar… (⌘⇧I) and
> Export Calendar ▸ Today | This Week, which closed the last open parity gap
> on that client. The import's notices reach the user there, grouped by code,
> rather than being counted.
>
> Where this document and the build disagree:
>
> - **`VTIMEZONE` is not parsed or emitted.** §Time zones below describes the
>   target. A `TZID` is resolved against the **bundled IANA tzdb** instead; a
>   `TZID` the tzdb does not know is read as UTC and reported. An inline
>   `VTIMEZONE` raises an unsupported-component notice.
> - **`RRULE` does not survive the domain boundary.** It parses, but `Block`
>   has no field to hold it, so it is reported per event and **a recurring
>   event imports as a single occurrence**. `DESCRIPTION` and `LOCATION` are
>   dropped the same way, for the same reason. Nothing is silently lost — every
>   one raises an `ICalNotice` — but the round-trip fidelity §Mapping rules
>   promises does not exist yet.
> - **Export is windowed, not scoped by Stream.** `ExportWindow` is `Day` or
>   `Week`, because `Query::DayBlocks` / `WeekBlocks` are the only Block windows
>   the core has — which is why the macOS menu offers exactly Today and This
>   Week, and the CLI exactly `today` / `day` / `week`. "Export this Stream as
>   .ics" is not implemented on either.
> - **An exported file carries `UID`, `SUMMARY`, `DTSTART`, `DTEND` only** —
>   nothing in the `Block` schema backs the rest, and emitting empty properties
>   would be inventing content. No `DTSTAMP` is written, which some strict
>   readers require.
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
> and LF input, the four RFC 5545 time forms preserved distinctly (UTC instant,
> zoned civil, floating, whole date) all the way into `SunriseTime`, and the
> rule that lossy imports are surfaced rather than dropped.

## Import

- File picker → parse `.ics` → create Blocks tagged `source = import:ics`.
- **Re-importing the same file is idempotent**, because the Block's id *is* the
  hash of `(source, uid)`: the same UID from the same source computes the same
  id and updates the Block already there, and the same UID from two different
  sources computes two ids and so creates two Blocks. No side table, no lookup,
  nothing to keep in step with the vault. `ICS_SOURCE` is the constant the file
  importer passes.

## Export

- Export covers exactly two windows, **Today** and **This Week**:
  `ExportWindow::{Day, Week}` (`crates/sunrise-integrations/src/ical_vault.rs:137-145`),
  surfaced as `sunrise ical export [today|week] [path]`
  (`crates/sunrise-cli/src/main.rs:84`). A whole-Stream export is target state,
  not a shipped option.
- Useful for: sharing a calendar slice with someone outside Sunrise; importing into Outlook etc.

## Mapping rules

Same as CalDAV mapping table. We attempt to preserve fidelity round-trip when possible.

### RRULE subset

Identical to the engine subset documented in [`../08-features/recurrence-engine.md`](../08-features/recurrence-engine.md). Lossy imports/exports are detected and surfaced; everything outside the subset is rejected with a clear diagnostic.

### What the subset REPORTS rather than silently drops

This is the property that holds today, and it is the one worth protecting: a
parser that drops what it does not model without saying so is how a user
discovers, weeks later, that half their calendar is missing. Every one of the
following raises an `ICalNotice` that the caller shows — the CLI prints them,
and the macOS app groups them by code:

`VTODO`, `VJOURNAL`, `VFREEBUSY`, `VALARM`, `VTIMEZONE`,
`RDATE` / `EXDATE` / `RECURRENCE-ID`, `ATTACH`, `ATTENDEE`, `ORGANIZER`,
`CATEGORIES`, `GEO`, `URL`, and **any `X-` property**.

Two deliberate carve-outs:

- **`RRULE` is not on that list at the syntax layer.** It is read and written
  back verbatim, so a file round-tripping through `ical` keeps its rule. The
  notice comes one layer up, from `ical_map`, which has nowhere to put it — see
  the banner at the top of this file.
- **Seven properties are dropped silently**, and they are exactly
  `ical::BOOKKEEPING`: `DTSTAMP`, `SEQUENCE`, `CREATED`, `LAST-MODIFIED`,
  `TRANSP`, `CLASS`, `STATUS`. They are iCalendar's own record-keeping, carry
  nothing a user typed, and Sunrise is not a CalDAV store obliged to preserve
  them byte-for-byte.

## Edge cases

- **Long descriptions:** no client-side truncation. Round-trip preserves DESCRIPTION verbatim. The Block's description field is plain text; iCalendar HTML in `X-ALT-DESC` is dropped on import (logged) and not regenerated on export.
- **Attached files (`ATTACH`):** dropped on import. The UI shows `"This event had attachments which were not imported."` once per event. Documented in user-facing help. Not produced on export (attachments are heavy and require separate handling).
- **Time zones:**
  - *Target state:* `VTIMEZONE` blocks emitted on export. The exporter does not emit them; zoned times go out as TZID references without an accompanying definition.
  - *Target state:* parsing an inline `VTIMEZONE` on import and using it to resolve VEVENT TZID values. The parser skips `VTIMEZONE` bodies entirely (`crates/sunrise-integrations/src/ical.rs:37-39,259-264`); a `TZID` is resolved against the **bundled IANA tzdb** instead, and the skipped component is reported as an `ICalNotice` rather than dropped silently.
  - A TZID that the bundled IANA tzdb does not know falls back to UTC and logs `int.import.tz_unknown`.
  - Floating times (no TZID) are stored as `tz: floating` and treated as user-local on each device.

## Subscribed `.ics` URLs — not in v1

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
rather than silently dropped.
