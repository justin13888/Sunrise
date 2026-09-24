---
status: accepted
---

# CalDAV (iCloud, Fastmail, Nextcloud, any RFC 4791 server)

Read-only, per-device credentials, per
[ADR-0049](../11-adr/0049-calendar-integrations-per-device-oauth.md). The
entities, fetch rules and reconnect flow shared by every calendar provider are
in [`overview.md`](./overview.md); this page is what is specific to CalDAV.

CalDAV is how Sunrise reaches iCloud calendars at all: Apple offers no OAuth
calendar API to third parties. It also covers every self-hosted calendar server,
which is why it left the non-goals.

> **Status: not built.** `IntegrationKind` has no CalDAV variant
> (`crates/sunrise-integrations/src/lib.rs#IntegrationKind`). The iCalendar
> parser the fetch reuses is live (`crates/sunrise-integrations/src/ical.rs`).
> [#4](https://github.com/justin13888/Sunrise/issues/4) tracks the build.

## Credentials

- **Username + app-specific password over HTTPS Basic auth.** iCloud requires an
  app-specific password (created at `account.apple.com`); Fastmail and
  Nextcloud offer the same. Sunrise never asks for an account's primary
  password and the connect screen says so, with a link to the provider's
  app-password page for the three named providers.
- **One app password per device** is the recommendation the connect screen
  makes, because it gives per-device revocation at the provider for free.
- **Storage:** the password goes in the platform keychain with this-device-only
  accessibility. The server URL and username are not secret: they live in
  `IntegrationAccount.caldav_server` / `caldav_username`, so "Connect on this
  device" asks only for the password.
- **Plain HTTP is refused.** Only `https://` URLs are accepted.
- **Subject:** the resolved principal URL. A reconnect that resolves a different
  principal is refused.

## Discovery

1. From the user's input (an email address or a URL), try
   `https://<host>/.well-known/caldav` (RFC 6764), following redirects on the
   same registrable domain only. Presets fill the host for iCloud
   (`caldav.icloud.com`), Fastmail and Nextcloud (the user's server).
2. `PROPFIND` `current-user-principal`, then `calendar-home-set`.
3. `PROPFIND` depth 1 on the home set for calendars (`resourcetype` includes
   `calendar`, `supported-calendar-component-set` includes `VEVENT`), with
   `displayname`, `calendar-color` (Apple extension) and `sync-token`.

## Fetch

1. **Changes:** `REPORT sync-collection` (RFC 6578) with the stored
   `sync-token`, device-local per calendar. A `403`/`409` with
   `valid-sync-token` precondition failure drops the token and re-fetches the
   window. A removed href is a deletion.
2. **Servers without sync-collection:** compare the collection's `getctag`,
   then each resource's `getetag`, and fetch what changed.
3. **Occurrences:** `REPORT calendar-query` with a `time-range` over the
   window and `<C:expand start end>`, so the server returns expanded
   occurrences. iCloud, Fastmail and Nextcloud support `expand`.
4. **Servers without `expand`:** the device expands locally with the Sunrise
   RRULE subset ([`../08-features/recurrence-engine.md`](../08-features/recurrence-engine.md)).
   A rule outside the subset imports its first occurrence and every explicit
   `RECURRENCE-ID` exception, and logs `int.import.rrule_lossy`, the same
   notice the `.ics` importer uses ([`icalendar.md`](./icalendar.md)).
5. Each occurrence maps to an `ExternalEvent`: `UID` → `uid`, `RECURRENCE-ID`
   → `recurrence_id`, `SUMMARY` → `title`, `DTSTART`/`DTEND` (or `DURATION`) →
   `starts_at`/`ends_at` (a `TZID` becomes `Zoned`, a `VALUE=DATE` becomes
   `AllDay`, a floating time becomes `Floating`), `LOCATION` → `location`,
   `TRANSP` → `transparency`, `STATUS` → `status` (`CANCELLED` is a deletion),
   and `URL` → `url`.

## Error model

| Response | Handling |
|---|---|
| `401` | This device → `needs_reauth`; delete the password; notify |
| `403`/`409` on a sync token | Drop the token; full re-fetch |
| `429`, `503` with `Retry-After` | Back off as instructed |
| `5xx`, network errors | Exponential back-off up to 30 minutes |
| A calendar that disappears from the home set | Its subscription is marked missing; events are kept until the user removes it |

## Disconnect

CalDAV has no revocation endpoint. The device deletes its password and shows
the provider's app-password page so the user can revoke that password there.
Everything else is in [`overview.md`](./overview.md) §Disconnecting.
