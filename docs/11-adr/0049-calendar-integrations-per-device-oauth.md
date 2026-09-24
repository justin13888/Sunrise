# 0049 — Calendar integrations are read-only, each device holds its own OAuth token in its keychain, and the vault syncs configuration and fetched events

**Status:** accepted

**Amends** [ADR-0025](./0025-integration-account-entity.md). The
`IntegrationAccount` entity stays, and so does its rationale for being an entity
rather than a Stream field. Its decision that "secrets ride as ordinary entity
fields" and "the refresh token … go[es] in the entity" is **withdrawn**: no
credential is ever written to the vault. Its deletion of the server-mediated
OAuth exchange stands.

**Amends** [`../00-product/non-goals.md`](../00-product/non-goals.md): CalDAV,
Microsoft 365 / Exchange and Apple iCloud calendars leave the non-goals.

**Depends on** [ADR-0048](./0048-interactive-planner.md) (external events are
fixed planner inputs), [ADR-0044](./0044-per-field-ops.md) (per-field LWW for
fetched events) and [ADR-0046](./0046-optional-stream.md) (the private key
domain).

**Specified in** [`../09-integrations/overview.md`](../09-integrations/overview.md),
[`google-calendar.md`](../09-integrations/google-calendar.md),
[`microsoft-graph.md`](../09-integrations/microsoft-graph.md) and
[`caldav.md`](../09-integrations/caldav.md). **Tracked by**
[#4](https://github.com/justin13888/Sunrise/issues/4).

## Context

ADR-0025 made the account an entity and put the refresh token in it, so that a
user would "log in once" and every paired device would receive the credential
through sync. Nothing of it is built: there is no `IntegrationAccount` in
`crates/`, `IntegrationKind` names only `GoogleCalendar` and `ICalendar`
(`crates/sunrise-integrations/src/lib.rs#IntegrationKind`), and
`crates/sunrise-integrations/src/gcal.rs` has no consumer. So the decision can
be revisited at no migration cost, and four facts about real providers say it
has to be.

1. **Microsoft rotates the refresh token on every use.** A Microsoft identity
   platform token response returns a new refresh token, and the one just spent
   should be treated as consumed. Two devices sharing one synced token race:
   whichever refreshes second presents a token the first already spent, and
   under LWW the vault may converge on the stale one. The failure is
   intermittent, device-dependent and invisible until an account silently stops
   fetching, which is exactly the "flaky business logic" the product mandate
   rules out.
2. **Google binds a token to the OAuth client that minted it, and client IDs
   are per platform.** An installed-app client is registered as an iOS client
   (bound to a bundle id and a custom-scheme redirect) or a desktop client (a
   loopback redirect). A refresh token minted by the iOS client cannot be
   refreshed by the desktop client. Google also caps the number of live refresh
   tokens per account per client ID and silently invalidates the oldest past
   the cap. A single synced token cannot serve both a Mac and an iPhone.
3. **A synced token is the highest-value secret in the vault.** ADR-0025 says so
   itself: it is "the first thing in the vault whose exposure is worth more than
   the user's own data", because it reaches the user's entire mailbox-adjacent
   calendar, and every device, backup and recovery path would carry it.
4. **Revocation cannot reach a token a device minted.** Revoking a Sunrise
   device ([`../03-crypto/key-rotation.md`](../03-crypto/key-rotation.md)
   §Revocation) bounds what that device can read from *later* epochs. A
   refresh token it already decrypted keeps working at the provider until the
   provider revokes it. A per-device token at least limits the loss to the
   token that device minted and names which grant to revoke.

Meanwhile what users actually need to share is not the credential: it is the
*events* (so every device, including one never connected, plans around the
meeting) and the *configuration* (which calendars, what colour, whether they
block time).

## Decision

**1. Read-only.** Sunrise reads calendars from three providers and writes to
none: **Google Calendar**, **Microsoft Graph** (Outlook.com, Microsoft 365,
Exchange Online) and **CalDAV** (Apple iCloud, Fastmail, Nextcloud and any
RFC 4791 server). Scopes are the narrowest read scopes each provider offers.
Outbound push to an external calendar is not part of the design.

**2. Credentials are per device, in the platform keychain, never in the
vault.** Each device that fetches runs its own OAuth authorization (or, for
CalDAV, holds its own app password), and stores the refresh token or password
in the platform secret store with this-device-only accessibility (on Apple,
`kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`, never iCloud Keychain). No
credential is written to an op, a snapshot, an export or a recovery blob. The
short-lived access token lives in memory only.

**3. The vault syncs configuration.** `IntegrationAccount` holds provider,
provider subject (the stable account id: Google `sub`, Microsoft `oid` plus
`tid`, CalDAV principal URL), a display label, the subscribed calendars, and per
calendar a colour and a busy treatment (`busy`, `free`, `hidden`, or
`as_event`, which honours the event's own transparency or `showAs`), and an
optional target Stream for the account's events. It is a
per-field entity ([ADR-0044](./0044-per-field-ops.md)), so editing a colour on
one device and subscribing a calendar on another both survive.

**4. The vault syncs fetched events, as fixed external events.** Each fetched
occurrence becomes an `ExternalEvent` entity with a deterministic id:

```
id = EntityRef(ExternalEvent,
       BLAKE3-128("sunrise.external-event.v1" ‖ account_subject ‖ calendar_id
                  ‖ uid ‖ recurrence_id_utc_or_empty))
```

so two devices fetching the same occurrence mint the same id. Recurring events
are stored as the occurrences inside the fetch window, expanded by the provider
where it can (Graph `calendarView`, Google `singleEvents=true`, CalDAV
`expand`), not as a series. External events are always `fixed` to the planner,
read-only in every client, and sealed under the account's target Stream key,
or in the vault-private key domain when it has none
([ADR-0046](./0046-optional-stream.md)).

**5. Any device with a live token fetches. There is no election.** Every
connected device fetches on its own schedule. Duplicate work is bounded (a
handful of devices, polling every few minutes) and its writes dedup by id. A
device writes a field only when its fetched value differs from the stored one,
so two devices fetching the same unchanged data write nothing, and two fetching
the same change converge under per-field LWW on the same value. Sync cursors
(`syncToken`, `deltaLink`, CalDAV `sync-token`) are device-local, because they
describe this device's fetch session, not the account.

**6. Liveness is synced; secrets are not.** `IntegrationAccount.fetchers` is a
per-device map of `{ last_ok_at, state: ok | needs_reauth | error }`. It is what
lets every device say "Fetched 3 min ago by MacBook" or "No device is fetching
this account", and it is the one piece of per-device integration state that
syncs.

**7. "Reconnect on this device."** An account whose config reached a device
that holds no token for it shows **Connect on this device** in Settings →
Calendars, and its events still show, because they came through the vault.
Connecting verifies that the authorized provider subject equals the stored one
and refuses a different account ("You signed in as a@x.com; this calendar
belongs to b@y.com"). A 401 after a refresh marks this device `needs_reauth`,
raises the *calendar reconnect needed* notification
([`../08-features/notifications.md`](../08-features/notifications.md)), and
stops this device's fetches until it reconnects.

**8. CalDAV leaves the non-goals.** It is how iCloud is reached at all (Apple
offers no OAuth calendar API to third parties), and self-hosters asked for it.

## Alternatives considered

| Option | Why not |
|---|---|
| **ADR-0025 as written: one synced refresh token** | Breaks on Microsoft rotation and Google per-platform client binding (§Context 1–2), and makes the vault carry its highest-value secret on every device, backup and recovery path. |
| **Sync the token, but only between devices of one platform** | Halves the Google problem and none of the Microsoft one, and adds a platform-scoped secret class the key hierarchy has no place for. |
| **Server fetches on the user's behalf** | The server would hold plaintext credentials and see every event. Disqualified by the trust model ([`../01-architecture/trust-and-server-role.md`](../01-architecture/trust-and-server-role.md)). |
| **Primary-fetcher election** (the old `overview.md` target) | Needs a heartbeat control op and a failover rule, and a fetcher that goes quiet leaves every device stale until the election notices. Deterministic ids plus write-if-changed make duplicate fetches harmless, so there is nothing to elect. |
| **Store recurring events as a series (RRULE + exceptions)** | Needs full RFC 5545 expansion, including rules outside Sunrise's subset ([`../08-features/recurrence-engine.md`](../08-features/recurrence-engine.md)), and each provider's own exception model. Storing the occurrences the provider already expanded is exact and simple; the cost is re-writing occurrences as the window slides, which is bounded by the window. |
| **EventKit on Apple devices** | Reads whatever accounts macOS or iOS already has, with no OAuth. But its identifiers are per device, it exists on no other platform, and an event it surfaces could not be deduplicated against the same event fetched by an Android or Windows device. Revisit if CalDAV to iCloud proves too hostile. |
| **Provider push (Google `watch`, Graph subscriptions)** | Both deliver change notices to a public HTTPS endpoint, which means the Sunrise server in the path and learning when each user's calendar changes. Polling from the device keeps the server out. |
| **Two-way sync** | The previous Google design. Write access to someone's calendar is a much larger trust and scope request, and conflict handling across providers is a product of its own. Sunrise plans around calendars; it does not replace them ([`../00-product/non-goals.md`](../00-product/non-goals.md)). |

## Consequences

- **Every device consents once.** A new Mac shows the account's events at once
  (they came through the vault) and asks to connect only if the user wants
  that Mac to fetch too. One connected device is enough for every device to see
  events; connecting more buys freshness and redundancy, not access.
- **ADR-0025's revocation paragraph no longer applies to calendars.** Revoking a
  Sunrise device does not revoke its provider grant. The device list shows
  which devices had a live calendar token (from `fetchers`), and the revoke
  flow tells the user where to revoke that grant at the provider.
- **Disconnecting has two scopes.** *Disconnect this device* deletes the local
  token and revokes it at the provider. *Remove account* tombstones the
  `IntegrationAccount`; every device that observes the tombstone deletes its
  own token and revokes it, and the account's `ExternalEvent`s are tombstoned
  (or, if the user ticks "Keep fetched events", kept as orphaned read-only
  `ExternalEvent`s that no device refreshes; they never become Blocks).
- **A new entity kind, `ExternalEvent`**, and new fields on
  `IntegrationAccount`, both added through the entity registry and gated by the
  `vault_requires` feature `external_event.entity`
  ([ADR-0045](./0045-schema-identity-and-feature-gating.md) §7).
- **`IntegrationKind` gains `MicrosoftGraph` and `CalDav`.** The provider code
  (HTTP, parsing, diffing) lives in `sunrise-integrations` with injected
  transport, as `gcal.rs` already does. Only the browser step of OAuth and the
  keychain read and write are platform code.
- **OAuth client registrations are per platform**, and Google requires app
  verification for the `calendar.readonly` scope. Both are release
  prerequisites tracked on #4.

## What would force revisiting this

1. **A provider that issues device-bound tokens natively** (for example
   DPoP-bound refresh tokens) *and* supports multi-device grants. Syncing still
   would not be needed, but the reconnect flow could shrink.
2. **Fetch load**: a user with many devices and many calendars hitting provider
   quotas. The answer is a jittered back-off keyed on `fetchers`, not an
   election.
3. **Demand for write-back.** It would be a new ADR with its own consent and
   conflict model.
