---
status: accepted
---

# iCalendar (.ics) Import / Export

For one-shot data movement, in addition to the live CalDAV/Google integrations.

## Import

- File picker → parse `.ics` → create Blocks tagged `source = import:ics`.
- Imported entities are read-only.
- Re-importing the same file dedups by UID.

## Export

- "Export this Stream as .ics" → produces a file containing all Blocks (with their RRULE for recurring).
- "Export Today" → minimal one-day .ics.
- Useful for: sharing a calendar slice with someone outside Sunrise; importing into Outlook etc.

## Mapping rules

Same as CalDAV mapping table. We attempt to preserve fidelity round-trip when possible.

## Edge cases

- Long descriptions: truncated only if the destination spec requires it (we don't truncate by default).
- Attached files in iCalendar (`ATTACH` properties): ignored on import; not produced on export (attachments are heavy and require separate handling).
- Time zones: `VTIMEZONE` blocks are emitted on export; on import, we use the embedded definitions, falling back to `TZID` parameter, falling back to UTC.

## Subscribed `.ics` URLs

A remote `.ics` URL can be subscribed to like an inbound calendar:

- Periodic HTTPS GET, parse, treat as imported events.
- Useful for public calendars (sports schedules, conference timetables).
- Read-only.
- Per-Stream toggle.

## Privacy

Subscribed URLs are fetched from the user's device, not the server. The .ics URL itself is stored in the vault config.

## Test surface

Import / export round-trip tests with golden .ics files (Google export, Apple export, Fastmail export, raw VEVENT samples) ensure parsing tolerance.
