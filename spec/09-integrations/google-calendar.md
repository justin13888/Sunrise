---
status: draft
---

# Google Calendar

Bidirectional integration. Per-Stream toggle.

## Auth

- OAuth 2.0 with PKCE.
- Scopes: `https://www.googleapis.com/auth/calendar.events` (no contacts, no drive).
- Tokens stored in the Stream's `integrations.gcal` config (encrypted at rest in vault).
- Refresh tokens rotate per Google's flow.

## Inbound (Google → Sunrise Blocks)

For each user-selected Google calendar:

1. Periodic poll (configurable, default 5 min) using `events.list` with `syncToken`.
2. New / updated events become / update Blocks tagged `source = import:gcal`, `external_id = <google_event_id>`.
3. Imported Blocks are read-only.
4. Deleted Google events become tombstoned imported Blocks.
5. Recurring events are stored as recurrence rules, not pre-expanded (the Block's `rrule` carries it).

## Outbound (Sunrise Blocks → Google)

Per-Stream toggle "Push to Google":

1. When a Block is created/edited in this Stream and not imported, push to a designated Google calendar.
2. Sunrise stores the resulting Google event ID in `external_id` for round-trip.
3. Future edits update the Google event.
4. Deletes propagate.

Pushed events have:

- Title: Block title or bound task title.
- Description: bound task description (truncated; full is in Sunrise).
- Color: from Stream color (mapped to Google color set).
- Source: `Sunrise` annotation in the description footer.

## Conflict handling

- Sunrise edited a pushed Block while Google version drifted: Sunrise wins (we re-push current state). The user is notified if the Google version had been mutated externally.
- Google deleted a pushed Block externally: Sunrise's Block becomes "orphaned"; the user chooses to delete in Sunrise or re-push.

## Privacy

- We push *only* the Stream the user has opted in. We don't read Google calendars unless the user also opted in to import.
- Tokens never leave the device.
- Aggregate sync metrics are *not* sent to Google or our server beyond the third-party API itself.

## Error model

- 401 / token expired: refresh; if refresh fails, mark integration as needing reauth, surface banner.
- 429: backoff with `Retry-After`.
- 5xx: exponential backoff up to 30 minutes.
- Quota exhausted (rare for personal use): banner + pause until next day.

## Account / calendar selection

In settings, the user picks:

- Which Google account is connected per Stream.
- Which Google calendar to import from (multi-select).
- Which Google calendar to push to (single).

## Disconnect

Disconnect:

- Revoke tokens with Google.
- Tokens deleted from vault.
- Imported Blocks remain (read-only, tagged "disconnected") unless user opts to remove.

## Why not push everything to all calendars by default

A previous prototype showed: silent two-way sync of "everything" creates anxiety about external visibility. Per-Stream opt-in is the right granularity.
