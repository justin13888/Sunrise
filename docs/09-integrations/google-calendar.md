---
status: accepted
---

# Google Calendar

Each device authorizes on its own and keeps its token in its keychain; the vault
syncs the account configuration and the fetched events.
[ADR-0049](../11-adr/0049-calendar-integrations-per-device-oauth.md) is the
decision, and [`overview.md`](./overview.md) specifies the entities, fetching
and reconnect flow shared by every calendar provider. This page is what is
specific to reading from Google. Writing is added by
[ADR-0071](../11-adr/0071-calendar-read-write-through-synced-intents.md) and
designed in [`calendar-architecture.md`](./calendar-architecture.md).

> **Status: not built.** The `crates/sunrise-integrations/src/gcal/` module implements
> the PKCE authorization-code exchange
> (`crates/sunrise-integrations/src/gcal/oauth.rs#OAuthFlow`), refresh with the
> durable-refresh-token rule
> (`crates/sunrise-integrations/src/gcal/oauth.rs#apply_refresh`) and change detection
> (`crates/sunrise-integrations/src/gcal/poll.rs#diff`)
> that suppresses phantom deletes, all against an injected transport. It has
> **zero consumers**: nothing has run it against the live API, there is no
> `impl EventSyncer`, and there is no storage or UI.
> [#930](https://github.com/justin13888/Sunrise/issues/930) tracks the build.

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
  `client_id` is public and ships in the binary. A build MAY substitute its own
  `client_id`s, as §Client registration says.
- **Scopes:** `https://www.googleapis.com/auth/calendar.readonly` to read, plus
  `openid` to learn the account's `sub`. Writing adds
  `https://www.googleapis.com/auth/calendar.events`, a separate grant a device
  asks for when it first needs to push. No contacts, no drive. Both calendar
  scopes are "sensitive", so the production OAuth app needs Google's
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

There is no server-mediated exchange and no `client_secret` in the managed
server's secret store. ADR-0025 and ADR-0049 rule it out: it would put the
relay in possession of every user's calendar tokens.

### Client registration

> **Status: not registered.** No Google Cloud project, client or verification
> exists yet, and no build carries a Google `client_id`.
> [#1082](https://github.com/justin13888/Sunrise/issues/1082) tracks the
> registration and verification. The build input the ids go in is added with
> the OAuth browser step in
> [#930](https://github.com/justin13888/Sunrise/issues/930).

Sunrise's official builds use one Google Cloud project, owned by the project
owner:

- **OAuth clients**, one per platform from the table in §Auth: a Desktop app
  client for macOS and an iOS client bound to the iOS app's bundle id. Windows,
  Linux and Android clients are registered when those apps ship.
- **Data Access** lists exactly `openid`, `calendar.readonly` and
  `calendar.events`. Verification is requested once for both calendar scopes,
  so turning on writes needs no second review. Reading calendar events is
  [Google's own example of a sensitive scope](https://developers.google.com/identity/protocols/oauth2/production-readiness/sensitive-scope-verification),
  which needs verification but not the security assessment restricted scopes
  need. The Data Access page shows each scope's class; if it shows either one
  as restricted, verification includes that assessment and this page changes.
- **Branding** names Sunrise and links its homepage and privacy policy on a
  domain the project verifies, which verification requires.
- **Publishing status** is "In production" once verification is approved.
  Before then the project stays in "Testing", where only listed test users
  can connect and their refresh tokens expire after 7 days. No build ships
  Google to users before verification is approved
  ([`calendar-architecture.md`](./calendar-architecture.md)).

**The `client_id`s are build inputs.** They are compiled into the client
build, never fetched from the relay, never synced through the vault and never
entered in Settings: the relay is not in the token path, and a token is bound
to the client that minted it, so a device whose `client_id` changed must
reconnect. A build with no Google `client_id` for its platform does not offer
Google; there is no placeholder id.

**Substituting your own.** Anyone building the apps from source can use their
own registrations instead of the official ones, and must for an iOS build
whose bundle id differs from the official app's, because an iOS client is
bound to a bundle id. Create a Google Cloud project, register the clients and
scopes above, add yourself as a test user (or get your own verification), and
supply your `client_id`s as the build's inputs. A self-hosted relay needs none
of this: official app builds connect Google the same way whichever relay they
sync through ([`self-hosting.md`](../06-server/self-hosting.md) §Calendar
OAuth clients).

## Fetch

> **Superseded in part by
> [ADR-0071](../11-adr/0071-calendar-read-write-through-synced-intents.md).** The
> endpoints, sync tokens and error handling below stand. Asking for expanded
> instances, and mapping each occurrence to its own flat `ExternalEvent` with no
> attendees, do not: events are stored as a series with override registers, keeping
> attendees. This section is rewritten when the conformance spike ends.

For each subscribed calendar:

1. `calendarList.list` on connect and once a day, to refresh names, colours
   and the list offered in Settings. A calendar the user can see only as
   free/busy is listed and marked "busy only": its events carry no title, and
   they import as events titled "Busy". `AccessRole::can_read_details`
   in `gcal/mod.rs` already tells the two apart.
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
