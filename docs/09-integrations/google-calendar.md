---
status: accepted
---

# Google Calendar

Bidirectional integration. Per-Stream toggle.

> **Status: deferred from the v1 MUST set, and wired to nothing.**
> `crates/sunrise-integrations/src/gcal.rs` is implemented and tested — the PKCE
> authorization-code exchange and refresh with the durable-refresh-token rule,
> change detection that suppresses phantom deletes, all against an injected
> transport so the tests need no network — and it has **zero consumers**.
> Nothing has ever run it against the live Google API, there is **no
> `impl EventSyncer`**, and `IntegrationProvider` has no implementor.
> [ADR-0020](../11-adr/0020-v1-must-demotions.md) §(b) deferred Google Calendar
> from the v1 MUST set against
> [issue #4](https://github.com/justin13888/Sunrise/issues/4); this is a
> wiring-and-storage gap, not a protocol gap. Everything below the Auth section
> is the target.

## Auth

**On-device PKCE with a public client. The server is never in the token path.**

- OAuth 2.0 authorization code + PKCE (`code_challenge_method=S256`), run
  entirely on the user's device against
  `https://accounts.google.com/o/oauth2/v2/auth` and
  `https://oauth2.googleapis.com/token`, with a loopback `redirect_uri`. This is
  what `gcal.rs`'s `OAuthFlow::{auth_url, code_exchange_request,
  refresh_request}` already implement, and a test asserts the request body
  contains no `client_secret`.
- **There is no `client_secret`.** An installed app is a *public* client: a
  secret shipped in a binary is not a secret, which is the whole reason PKCE
  exists. `client_id` is a public string and can live in the binary.
- Scopes: `https://www.googleapis.com/auth/calendar.events` (no contacts, no drive).
- Credentials are stored in the `IntegrationAccount` entity, per
  [ADR-0025](../11-adr/0025-integration-account-entity.md) and
  [`overview.md`](./overview.md) §Token storage — **not** in a Stream field,
  which does not exist. The durable refresh token syncs; the short-lived access
  token stays device-local.
- Refresh tokens rotate per Google's flow, and an omitted `refresh_token` on a
  refresh response MUST NOT erase the stored one (`Credentials::apply_refresh`).

An earlier revision of this document specified the exchange as "mediated
server-side, so the client never sees `client_secret`", with the managed server
holding `client_id` and `client_secret` in its secret store. **ADR-0025 deletes
that flow rather than softening it.** It contradicted this directory's own
"The server is not a credentialed proxy" and "Tokens never leave the device",
and it would have put the relay in possession of every user's calendar tokens —
the precise property the architecture exists to avoid. Its premise does not hold
either: a public PKCE client has no secret to protect.

### `client_id` provisioning

A `client_id` is public, so provisioning it is a configuration question, not a
secrets question:

- **Managed cloud:** a single Google OAuth app is registered to Sunrise and its
  `client_id` ships with the client. No `client_secret` is registered, stored or
  used. No such app exists yet — it is one of the reasons ADR-0020 deferred this
  integration.
- **Self-host:** the operator MAY register their own Google OAuth app (as an
  installed/public client) and configure its `client_id` under
  `[integrations.google_calendar]`. If unconfigured, the integration shows a
  setup wizard pointing at `https://console.cloud.google.com`.

## Inbound (Google → Sunrise Blocks)

For each user-selected Google calendar:

1. Periodic poll using `events.list` with `pageSize=100`, looping on `pageToken`. The poll interval is a per-Stream field, range 5–60 minutes (default 5 minutes). `int.run.start` and `int.run.ok` log entries include the configured interval.
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
- **Tokens never reach the server.** The refresh token syncs between the user's
  own devices as ciphertext inside an ordinary op, which the relay cannot open;
  the access token never leaves the device that minted it. See
  [`overview.md`](./overview.md) §Token storage.
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
