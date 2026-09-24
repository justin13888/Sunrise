---
status: accepted
---

# Microsoft Graph (Outlook.com, Microsoft 365, Exchange Online)

Read-only, per-device OAuth, per
[ADR-0049](../11-adr/0049-calendar-integrations-per-device-oauth.md). The
entities, fetch rules and reconnect flow shared by every calendar provider are
in [`overview.md`](./overview.md); this page is what is specific to Microsoft.

> **Status: not built.** `IntegrationKind` has no Microsoft variant
> (`crates/sunrise-integrations/src/lib.rs#IntegrationKind`).
> [#4](https://github.com/justin13888/Sunrise/issues/4) tracks the build.

## Auth

- Microsoft identity platform, OAuth 2.0 authorization code + PKCE, **public
  client**, against the `common` authority
  (`https://login.microsoftonline.com/common/oauth2/v2.0/{authorize,token}`), so
  personal Microsoft accounts and work or school accounts both work.
- **Scopes:** `Calendars.Read`, `offline_access` (for a refresh token), and
  `openid profile` to learn the account. No write scope.
- **One app registration**, with a redirect URI per platform (a loopback URI for
  desktop, a bundle-id or package-signature URI for mobile). A self-hosted build
  MAY substitute its own registration's client id.
- **Tenant consent.** A work or school tenant may require an administrator to
  consent to `Calendars.Read`. The authorization page says so; Sunrise shows
  the provider's message and does not try to work around it.
- **Refresh tokens rotate on every use.** Each token response carries a new
  refresh token, and the device MUST replace the stored token atomically before
  using the new access token. This is the decisive reason tokens are per device:
  two devices sharing one rotating token race, and one of them always loses
  ([ADR-0049](../11-adr/0049-calendar-integrations-per-device-oauth.md)
  §Context 1). On one device, refreshes are serialized behind a single lock.
- **Subject:** `tid` + `oid` from the ID token. A reconnect presenting a
  different pair is refused.
- **Storage:** the refresh token goes in the platform keychain with
  this-device-only accessibility; the access token stays in memory.

## Fetch

For each subscribed calendar:

1. `GET /me/calendars` on connect and once a day, for names, colours and
   `canEdit`/`owner` (displayed only).
2. `GET /me/calendars/{id}/calendarView/delta?startDateTime=…&endDateTime=…`
   over the window in [`overview.md`](./overview.md) §Fetching, following
   `@odata.nextLink` until an `@odata.deltaLink` arrives. `calendarView` returns
   expanded occurrences, so a recurring series arrives as its instances.
3. The `@odata.deltaLink` is stored device-locally per calendar and replayed for
   later fetches. A delta response entry with `@removed` is a deletion.
   `410 Gone` or a `syncStateNotFound` error drops the link and re-fetches the
   window.
4. `Prefer: outlook.timezone="UTC"` is **not** sent: events arrive in their
   own `originalStartTimeZone`, which maps to a `Zoned` time. Windows zone names
   are mapped to IANA names with the CLDR mapping; an unmappable zone falls back
   to UTC and logs `int.graph.zone_unmapped`.
5. Each occurrence maps to an `ExternalEvent`:

   | Graph field | `ExternalEvent` |
   |---|---|
   | `iCalUId` | `uid` |
   | `originalStart` (occurrence and exception types) | `recurrence_id` |
   | `subject` | `title` |
   | `start`/`end` + `isAllDay` | `starts_at`/`ends_at` (`Zoned`, or `AllDay`) |
   | `location.displayName` | `location` |
   | `showAs` (`free` → transparent; otherwise opaque) | `transparency` |
   | `isCancelled` | `status`; cancelled is a deletion |
   | `webLink` | `url` |

   `showAs` values `tentative`, `oof` and `workingElsewhere` are opaque;
   `tentative` also sets `status = Tentative`. Bodies, attendees and online
   meeting data are not stored.

## Error model

| Response | Handling |
|---|---|
| `invalid_grant` on refresh, or `401` after a successful refresh | This device → `needs_reauth`; delete the token; notify |
| `429`, `503` with `Retry-After` | Back off as instructed |
| `410`, `syncStateNotFound` | Drop the delta link; full re-fetch |
| `5xx`, network errors | Exponential back-off up to 30 minutes |

## Disconnect

Microsoft offers no token-revocation endpoint for a single refresh token. The
device deletes its token and shows where to remove the app's access: the
Microsoft account "Apps and services" page for personal accounts, and "My Apps"
for work or school accounts. Everything else is in
[`overview.md`](./overview.md) §Disconnecting.
