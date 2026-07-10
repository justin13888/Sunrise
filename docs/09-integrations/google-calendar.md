---
status: accepted
---

# Google Calendar

Bidirectional integration. Per-Stream toggle.

## Auth

- OAuth 2.0 with PKCE; authorization-code flow is mediated server-side, so the client never sees `client_secret`.
- Scopes: `https://www.googleapis.com/auth/calendar.events` (no contacts, no drive).
- Tokens stored in the Stream's `integrations.gcal` config (encrypted at rest in vault).
- Refresh tokens rotate per Google's flow.

### `client_id` provisioning

- **Managed cloud:** a single Google OAuth app is registered to Sunrise; `client_id` and `client_secret` are stored in the managed server's secret store.
- **Self-host:** the operator MUST register their own Google OAuth app and put the credentials under `[integrations.google_calendar]` in `sunrise.toml`. If unconfigured, the integration shows a setup wizard pointing at `https://console.cloud.google.com`.

## Inbound (Google → Sunrise Blocks)

For each user-selected Google calendar:

1. Periodic poll using `events.list` with `pageSize=100`, looping on `pageToken`. The poll interval is a per-Stream CRDT field, range 5–60 minutes (default 5 minutes). `int.run.start` and `int.run.ok` log entries include the configured interval.
2. The final `nextSyncToken` is persisted per `(stream, calendar)`. On `410 Gone` for a sync token (Google's "expired" signal), the integration resets to an empty token and runs a full sync.
3. New / updated events become / update Blocks tagged `source = import:gcal`, `external_id = <google_event_id>`. The Block also carries a `source_calendar_id` (internal) so multi-calendar imports are unambiguous.
4. Imported Blocks are read-only **by default**. If the user edits an imported Block, `source_calendar_id` is cleared and the Block becomes pushable.
5. Deleted Google events become tombstoned imported Blocks.
6. Recurring events are stored as recurrence rules, not pre-expanded (the Block's `rrule` carries it). The supported subset matches [`../08-features/recurrence-engine.md`](../08-features/recurrence-engine.md); lossy imports log `int.import.rrule_lossy { provider: "google", original: "<rule>", emitted: "<rule>" }`.

## Outbound (Sunrise Blocks → Google)

Per-Stream toggle "Push to Google":

1. When a Block is created/edited in this Stream and not imported, push to a designated Google calendar.
2. Sunrise stores the resulting Google event ID in `external_id` for round-trip.
3. Future edits update the Google event.
4. Deletes propagate.

Pushed events have:

- Title: Block title or bound task title.
- Description: bound task description (truncated; full is in Sunrise).
- Color: from Stream color, mapped to Google color set:

  | Sunrise color | Google color id |
  |---|---|
  | accent (default) | 1 (Lavender) |
  | success | 10 (Basil) |
  | warning | 5 (Banana) |
  | danger | 11 (Tomato) |
  | info | 7 (Peacock) |
  | muted | 8 (Graphite) |
  | custom hex | nearest of the above by Lab distance |

- Source: `Sunrise` annotation in the description footer.

Lossy export of a recurrence rule (the Sunrise rule cannot be expressed exactly in Google's RRULE subset) is detected at push time and surfaces a per-Block warning icon.

## Conflict handling

- **Concurrent edit (Sunrise wins).** Push includes the `etag` from the last known Google version. If Google rejects with `412 Precondition Failed`, the integration: (1) fetches the current Google event, (2) ignores Google's edits, (3) re-pushes Sunrise's version with the new etag. A toast says `"Replaced external changes in <event>."` Per-Stream throttle: at most one such toast per session.
- **Google deleted externally.** If `events.get` returns `404`, the Block becomes orphaned: `external_id` is preserved but flagged `external_orphan: true`. The UI offers Delete or Re-push. Re-push clears `external_id` and POSTs as a new event, getting a fresh id.

## Privacy

- We push *only* the Stream the user has opted in. We don't read Google calendars unless the user also opted in to import.
- Tokens never leave the device.
- Aggregate sync metrics are *not* sent to Google or our server beyond the third-party API itself.

## Error model

- 401 / token expired: refresh; if refresh fails (or 401 reappears after a recent successful refresh, indicating external revocation per [`overview.md`](./overview.md)), mark integration as needing reauth, surface banner, and stop scheduling.
- 429: backoff with `Retry-After`.
- 5xx: exponential backoff up to 30 minutes.
- Quota exhausted (rare for personal use): banner + pause until next day.

## Account / calendar selection

In settings, the user picks:

- Which Google account is connected per Stream.
- Which Google calendars to import from (**multi-select**). Each imported Block is tagged with `source_calendar_id` (internal).
- Which Google calendar to push to (**single**). On export the source is ignored; Blocks always push to the configured export calendar.

## Disconnect

Disconnect (best-effort):

- Token revoke is called once; on failure it retries up to 3 times with exponential backoff. The result is logged.
- The integration is marked disabled regardless of revoke success.
- Tokens deleted from vault.
- Imported Blocks remain (read-only, tagged "disconnected") unless the user opts to remove them via the disable modal.
- The UI suggests the user manually revoke at `https://myaccount.google.com/permissions` when revoke retries are exhausted.

## Why not push everything to all calendars by default

A previous prototype showed: silent two-way sync of "everything" creates anxiety about external visibility. Per-Stream opt-in is the right granularity.
