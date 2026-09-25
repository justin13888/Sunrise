---
status: accepted
---

# Google Calendar

Read-only. Each device authorizes on its own and keeps its token in its
keychain; the vault syncs the account configuration and the fetched events.
[ADR-0049](../11-adr/0049-calendar-integrations-per-device-oauth.md) is the
decision, and [`overview.md`](./overview.md) specifies the entities, fetching
and reconnect flow shared by every calendar provider. This page is what is
specific to Google.

> **Status: not built.** `crates/sunrise-integrations/src/gcal.rs` implements
> the PKCE authorization-code exchange
> (`crates/sunrise-integrations/src/gcal.rs#OAuthFlow`), refresh with the
> durable-refresh-token rule
> (`crates/sunrise-integrations/src/gcal.rs#apply_refresh`) and change detection
> that suppresses phantom deletes, all against an injected transport. It has
> **zero consumers**: nothing has run it against the live API, there is no
> `impl EventSyncer`, and there is no storage or UI.
> [#4](https://github.com/justin13888/Sunrise/issues/4) tracks the build.

## Auth

**On-device PKCE with a public client, per device. The server is never in the
token path.**

- OAuth 2.0 authorization code + PKCE (`code_challenge_method=S256`) against
  `https://accounts.google.com/o/oauth2/v2/auth` and
  `https://oauth2.googleapis.com/token`, with `access_type=offline` so a refresh
  token is issued.
- **One OAuth client per platform**, because Google binds a token to the client
  that minted it:

  | Platform | Google client type | Redirect |
  |---|---|---|
  | macOS | Desktop app | Loopback `http://127.0.0.1:<port>/callback` |
  | iOS / iPadOS | iOS | Custom scheme from the bundle id |
  | Windows, Linux | Desktop app | Loopback |
  | Android | Android (package + signing cert) | App link |

  A token minted by one platform's client is unusable by another's, which is one
  of the reasons tokens are never synced.
- **There is no `client_secret`.** An installed app is a public client; a
  `client_id` is public and ships in the binary. A self-hosted build MAY
  substitute its own `client_id`s.
- **Scope:** `https://www.googleapis.com/auth/calendar.readonly`, plus `openid`
  to learn the account's `sub`. No write scope, no contacts, no drive. The
  scope is "sensitive", so the production OAuth app needs Google's
  verification; until it has it, test users' refresh tokens expire after 7
  days, and the app shows "Needs reconnect" when they do.
- **Storage:** the refresh token goes in the platform keychain with
  this-device-only accessibility. The access token stays in memory.
- **Refresh:** an omitted `refresh_token` on a refresh response MUST NOT erase
  the stored one (`Credentials::apply_refresh`, already implemented). Google
  caps live refresh tokens per account per client; past the cap it invalidates
  the oldest, which surfaces on that device as "Needs reconnect".
- **Subject:** the ID token's `sub` is the account's `subject`. A reconnect that
  returns a different `sub` is refused.

An earlier revision specified a server-mediated exchange with a `client_secret`
in the managed server's secret store. ADR-0025 deleted it, and ADR-0049 keeps
it deleted: it would put the relay in possession of every user's calendar
tokens.

## Fetch

For each subscribed calendar:

1. `calendarList.list` on connect and once a day, to refresh names, colours
   and the list offered in Settings. A calendar the user can see only as
   free/busy is listed and marked "busy only": its events carry no title, and
   they import as events titled "Busy". `AccessRole::can_read_details`
   in `gcal.rs` already tells the two apart.
2. `events.list` with `singleEvents=true`, `timeMin`/`timeMax` bounding the
   window in [`overview.md`](./overview.md) §Fetching, `maxResults=250`, looping
   on `pageToken`. `singleEvents=true` makes Google expand recurring series,
   so each occurrence arrives with its own `recurringEventId` and
   `originalStartTime`.
3. The final `nextSyncToken` is stored device-locally per calendar. Later
   fetches pass it and receive only changes. On `410 Gone`, the device drops the
   token and does a full fetch of the window.
4. Each occurrence maps to an `ExternalEvent`:

   | Google field | `ExternalEvent` |
   |---|---|
   | `iCalUID` | `uid` |
   | `originalStartTime` (recurring) | `recurrence_id` |
   | `summary` | `title` |
   | `start`/`end` (`dateTime` + `timeZone`, or `date`) | `starts_at`/`ends_at` (`Zoned`, or `AllDay`) |
   | `location` | `location` |
   | `transparency` | `transparency` |
   | `status` | `status`; `cancelled` is a deletion |
   | `htmlLink` | `url` |

   Attendees, descriptions, attachments and conference data are not stored.

## Error model

| Response | Handling |
|---|---|
| `401` after a successful refresh, or `invalid_grant` on refresh | This device → `needs_reauth`; delete the token; notify ([`overview.md`](./overview.md) §Connecting and reconnecting) |
| `403` `rateLimitExceeded` / `429` | Back off, honouring `Retry-After` |
| `403` `quotaExceeded` (daily) | Pause this device's fetches until the next UTC day; banner |
| `410` on a sync token | Full re-fetch of the window |
| `5xx`, network errors | Exponential back-off up to 30 minutes |

## Disconnect

`https://oauth2.googleapis.com/revoke` with this device's refresh token, best
effort, three retries. On final failure, the UI links
`https://myaccount.google.com/permissions` so the user can revoke by hand.
Everything else is in [`overview.md`](./overview.md) §Disconnecting.

## Privacy

- Sunrise reads only calendars the user subscribes to.
- Tokens never reach the server or the vault. Events reach the server only as
  ciphertext ops.
- Nothing about the user's Sunrise data is sent to Google.
