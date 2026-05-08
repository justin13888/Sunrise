---
status: draft
---

# CalDAV

Open standard for calendar interop (Apple iCloud, Fastmail, Nextcloud, hosted servers).

## Auth

- Per CalDAV spec: HTTP Basic or Digest. Use server-issued app-specific passwords where available.
- Credentials stored in vault, encrypted, like Google.

## Capabilities

- **Discovery.** Use `PROPFIND` to discover the user's calendar home and calendars.
- **Inbound.** Polling for changes via `sync-collection` reports + ETag comparison.
- **Outbound.** PUT new/updated VEVENTs.

## Mapping

Same Block model as Google. The shape of VEVENT maps cleanly:

| iCalendar field | Sunrise Block field |
|---|---|
| SUMMARY | title |
| DTSTART | starts_at |
| DTEND | ends_at |
| DESCRIPTION | notes (text only) |
| LOCATION | location |
| RRULE | rrule |
| EXDATE | (custom; recurrent skip) |
| STATUS | (mapped; CANCELLED becomes deleted) |

## Why CalDAV beyond Google

- Apple users on iCloud who don't use Google Calendar.
- Self-hosted users (Nextcloud, Radicale).
- Privacy-conscious users on Fastmail, ProtonCalendar (when they add CalDAV).

## Limitations / known issues

- Some CalDAV servers don't support `sync-collection`. Fallback: `getetag` per resource (slower; longer poll).
- Server quirks (Apple iCloud's idiosyncratic auth, Fastmail's URL structure) are encoded as adapter shims.
- Free/busy queries are MAY for v1.

## Configuration

Per-Stream:

- Server URL (auto-discoverable from email domain via `.well-known/caldav`).
- Username + app-specific password.
- Calendar(s) to import from / push to.

## Reliability

- Polling cadence configurable (default 10 min).
- ETag mismatch on PUT → fetch latest, merge intent, retry.
- Permanent failure (deleted calendar) → integration paused, user notified.

## TUI

- `:caldav` subcommand for status, manual sync trigger.
- No graphical config; the user edits the integration spec in the vault settings file or via TUI prompts.
