---
status: accepted
---

# iCalendar (.ics) Import / Export

For one-shot data movement, in addition to the live CalDAV/Google integrations.

> **Status: partly implemented. This document is the target; the list below is
> what ships.** `crates/sunrise-integrations` implements the syntax layer
> (`ical`), the domain mapping (`ical_map`) and the vault driver
> (`ical_vault`), reached today by `sunrise ical import` / `sunrise ical export`
> and by `import_ical` / `export_ical` on the UniFFI seam. The macOS app does
> not call either yet, which is an open parity gap.
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
>   the core has. "Export this Stream as .ics" is not implemented.
> - **An exported file carries `UID`, `SUMMARY`, `DTSTART`, `DTEND` only** —
>   nothing in the `Block` schema backs the rest, and emitting empty properties
>   would be inventing content. No `DTSTAMP` is written, which some strict
>   readers require.
> - **Dedup is by `(source, uid)` but not via the columns below.** The schema
>   has no `external_id` or `import_source_id`; the pair is hashed into the
>   Block's **id**, exactly as a materialized routine occurrence is. That gives
>   the same idempotence with no side table to keep in step with the vault.
> - **Imported entities are not marked read-only.** Nothing enforces it.
>
> What does hold as written: line unfolding and escapes both directions, CRLF
> and LF input, the four RFC 5545 time forms preserved distinctly (UTC instant,
> zoned civil, floating, whole date) all the way into `SunriseTime`, and the
> rule that lossy imports are surfaced rather than dropped.

## Import

- File picker → parse `.ics` → create Blocks tagged `source = import:ics`.
- Imported entities are read-only.
- Re-importing the same file dedups by **`(import_source_id, uid)`**: the same UID from two different sources creates two Blocks (same `external_id`, different `import_source_id`); the same UID from the same source on a subsequent import is treated as an update.

## Export

- "Export this Stream as .ics" → produces a file containing all Blocks (with their RRULE for recurring).
- "Export Today" → minimal one-day .ics.
- Useful for: sharing a calendar slice with someone outside Sunrise; importing into Outlook etc.

## Mapping rules

Same as CalDAV mapping table. We attempt to preserve fidelity round-trip when possible.

### RRULE subset

Identical to the engine subset documented in [`../08-features/recurrence-engine.md`](../08-features/recurrence-engine.md). Lossy imports/exports are detected and surfaced; everything outside the subset is rejected with a clear diagnostic.

## Edge cases

- **Long descriptions:** no client-side truncation. Round-trip preserves DESCRIPTION verbatim. The Block's description field is plain text; iCalendar HTML in `X-ALT-DESC` is dropped on import (logged) and not regenerated on export.
- **Attached files (`ATTACH`):** dropped on import. The UI shows `"This event had attachments which were not imported."` once per event. Documented in user-facing help. Not produced on export (attachments are heavy and require separate handling).
- **Time zones:**
  - `VTIMEZONE` blocks are emitted on export.
  - On import, `VTIMEZONE` is parsed if present and used to resolve VEVENT TZID values.
  - A TZID that is not in `VTIMEZONE` and not in the IANA TZDB falls back to UTC and logs `int.import.tz_unknown`.
  - Floating times (no TZID) are stored as `tz: floating` and treated as user-local on each device.

## Subscribed `.ics` URLs

A remote `.ics` URL can be subscribed to like an inbound calendar:

- Periodic HTTPS GET, parse, treat as imported events.
- Per-subscription poll cadence: default 1 hour, range 15 minutes – 24 hours, stored in the subscription config in the vault.
- Useful for public calendars (sports schedules, conference timetables).
- Read-only.
- Per-Stream toggle.

## Privacy

Subscribed URLs are fetched from the user's device, not the server. The .ics URL itself is stored in the vault config.

## Test surface

Round-trip fixtures live at `crates/sunrise-integrations/testdata/{google,apple,fastmail,outlook}/*.ics`. CI imports each fixture, exports it, diffs the result, and asserts that any differences fall within the documented "lossy" set. Raw VEVENT samples are included for parser tolerance.
