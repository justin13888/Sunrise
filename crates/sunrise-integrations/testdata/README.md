# iCalendar parser fixtures

Hand-written `.ics` documents in the shape each vendor emits, used by
`crates/sunrise-integrations/tests/ical_vault.rs` and referenced from
`docs/09-integrations/icalendar.md` §Test surface.

They are *representative*, not captures: each one reproduces the structural
habits that break naive parsers, and nothing here is claimed to be a byte-exact
recording of a real account's export.

| Directory | What it exercises |
|---|---|
| `google/` | `TZID` on a zoned event, `X-` properties, an all-day event with an exclusive `DTEND`, `\n` and `\,` escapes in `DESCRIPTION`. |
| `apple/` | A `VTIMEZONE` block that must not have its `DTSTART` harvested, a `VALARM` inside a `VEVENT`, `DURATION` instead of `DTEND`. |
| `fastmail/` | Folded long lines, a floating (zone-less) event, a `VTODO` this build does not model. |
| `outlook/` | CRLF with folded `DESCRIPTION`, a `DTSTART` with no `VALUE=DATE` parameter on an 8-digit date, `X-MICROSOFT-` properties, an unknown `TZID`. |
