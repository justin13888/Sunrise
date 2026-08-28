---
status: accepted
---

# iCalendar (.ics) Import / Export

For one-shot data movement, in addition to the live CalDAV/Google integrations.

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
