---
status: accepted
---

# Integrations — Overview

External integrations connect Sunrise to systems we *don't* build. Integrations
run **on-device**: each device holds its own credentials in its platform
keychain, the vault syncs only configuration and the data fetched, and the
server never holds a third-party credential or sees a fetched event in
plaintext. [ADR-0049](../11-adr/0049-calendar-integrations-per-device-oauth.md)
is the decision for calendar credentials and fetching; it amends
[ADR-0025](../11-adr/0025-integration-account-entity.md).
[ADR-0071](../11-adr/0071-calendar-read-write-through-synced-intents.md) adds
writing and series storage, [`calendar-goals.md`](./calendar-goals.md) says what
done means, and [`calendar-architecture.md`](./calendar-architecture.md) is the
design, `proposed` until its conformance spike ends.

| Integration | Direction | Status | Spec |
|---|---|---|---|
| Google Calendar | Read and write | **Not built.** `crates/sunrise-integrations/src/gcal.rs` implements PKCE, refresh and change detection against an injected transport, and has no consumer, no storage and no UI ([#930](https://github.com/justin13888/Sunrise/issues/930)) | [`google-calendar.md`](./google-calendar.md) |
| Microsoft Graph (Outlook.com, Microsoft 365, Exchange Online) | Read and write | **Not built** ([#4](https://github.com/justin13888/Sunrise/issues/4)) | [`microsoft-graph.md`](./microsoft-graph.md) |
| CalDAV (iCloud, Fastmail, Nextcloud, any RFC 4791 server) | Read and write | **Not built** ([#4](https://github.com/justin13888/Sunrise/issues/4)) | [`caldav.md`](./caldav.md) |
| iCalendar (.ics) | Import / export | **Live on both shipping clients**, a narrow subset: `sunrise ical import` / `export`, and the macOS File menu over the seam's `import_ical` / `export_ical` | [`icalendar.md`](./icalendar.md) |

**`IntegrationProvider` has no implementor.** Neither built integration
implements the trait `crates/sunrise-integrations/src/lib.rs` declares — the
live iCal path is driven directly by `ical_vault::{import, export}` from the CLI
and the UniFFI seam. Treat the trait as intended shape, not as a seam anything
runs through yet.

Outbound webhooks and inbound email-to-Sunrise remain non-goals
([`../00-product/non-goals.md`](../00-product/non-goals.md)).

## Design principles

1. **On-device.** Fetching, parsing and diffing run in Rust on the user's
   devices. The server is not a credentialed proxy and never sees a token.
2. **Credentials never leave the device that minted them.** Not in an op, a
   snapshot, an export or a recovery blob.
3. **Share the data, not the secret.** Configuration and fetched events sync
   through the vault like any other entity, so every device sees every event
   whether or not it is connected.
4. **Edits are synced intents.** A device edits offline, and any device of the
   owner's holding a write grant carries the edit to the provider
   ([ADR-0071](../11-adr/0071-calendar-read-write-through-synced-intents.md)).
5. **Failures are visible.** A failing account shows an in-app banner and, where
   the user can fix it, a notification. Never a silent log line.
6. **Core logic in Rust.** Only the browser step of OAuth and the keychain read
   and write are platform code.

## Entities

### `IntegrationAccount` (synced)

```rust
pub struct IntegrationAccount {
    pub id: EntityRef,
    pub provider: IntegrationKind,         // GoogleCalendar | MicrosoftGraph | CalDav
    /// Stable provider identity. Google `sub`; Microsoft `tid` + `oid`;
    /// CalDAV principal URL. A reconnect must present the same subject.
    pub subject: String,
    pub label: String,                     // "Work (a@x.com)"; user-editable
    /// Optional Stream the account's events belong to: they are tinted by it,
    /// filtered with it, and sealed under its key. None: the vault-private domain.
    pub stream: Option<StreamId>,
    /// CalDAV only: server base URL and username. Not secret; prefills
    /// "Connect on this device".
    pub caldav_server: Option<String>,
    pub caldav_username: Option<String>,
    pub calendars: BTreeMap<CalendarId, CalendarSubscription>,
    /// Per-device liveness, merged per entry. Never holds a secret.
    pub fetchers: BTreeMap<DeviceId, FetcherStatus>,
    pub deleted: bool,
}

pub struct CalendarSubscription {
    pub name: String,                      // provider's name, refreshed on fetch
    pub subscribed: bool,
    pub colour: Option<Colour>,            // None: provider colour
    pub busy: BusyTreatment,               // Busy | Free | Hidden | AsEvent (default)
}

pub struct FetcherStatus {
    pub last_ok_at: Option<Timestamp>,
    pub state: FetcherState,               // Ok | NeedsReauth | Error { code }
    pub can_write: bool,                   // this device holds a write grant (ADR-0071)
}
```

Every field merges per field ([ADR-0044](../11-adr/0044-per-field-ops.md));
`calendars` and `fetchers` are maps merged per key.

### `ExternalEvent` (synced)

One entity per provider series or single event, with a map of override registers
keyed by the provider's instance identity. Provider fields, revision and ids
merge as one quiet register, never per field. The fields it normalizes include
title, start and end (`SunriseTime`: zoned, floating for CalDAV, or all-day),
recurrence, location, transparency, status and the provider's web link. The
shape is in [`calendar-architecture.md`](./calendar-architecture.md), and
[ADR-0071](../11-adr/0071-calendar-read-write-through-synced-intents.md) says why
this is not ADR-0049's expanded occurrences.

External events are sealed under their account's Stream key, or in the
vault-private key domain when the account has no Stream
([ADR-0046](../11-adr/0046-optional-stream.md)). They are always **fixed** to
the planner ([`../08-features/planner.md`](../08-features/planner.md)), unless
their calendar's busy treatment is `free` (drawn, never blocking) or `hidden`
(not drawn), or the treatment is `as_event` and the event is transparent.

### Device-local state (never synced)

| State | Where |
|---|---|
| Refresh token, or CalDAV app password | Platform keychain, this-device-only |
| Access token | Memory |
| Sync cursor per calendar (`syncToken`, `deltaLink`, CalDAV `sync-token`) | Local table in the vault DB, outside the op log |
| Next fetch time and back-off | Local table |
| Last revision seen per event, raw payload cache, push attempt journal | Local table |

## Fetching

- **Who fetches:** every device with a live token for the account. There is no
  election ([ADR-0049](../11-adr/0049-calendar-integrations-per-device-oauth.md)
  §Decision 5).
- **Window:** a provider that requires a window is fetched from 30 days before
  today to 365 days after. An event that slides out of the window is left alone,
  not deleted.
- **Cadence:** every 5 minutes while the app is in the foreground; on the
  platform's background refresh schedule otherwise; and on demand from
  Settings → Calendars → Refresh. Each device adds a random jitter of up to one
  minute so connected devices do not fetch in lockstep.
- **Write-if-changed:** a device writes an `ExternalEvent` register only when the
  provider revision differs from the stored one, never because mapped fields
  differ, so builds that map an event differently cannot overwrite each other.
  Two devices fetching the same data write nothing.
- **Deletions:** an event is marked absent only on a positive deletion signal from
  the provider (a delta entry marked removed, a `404` on re-fetch, or a CalDAV
  `sync-collection` removal). An event missing from a truncated or failed listing
  is not a deletion. Graph's `@removed` also covers events outside the delta
  window, so it is confirmed with a read.
- **Liveness:** after each successful fetch a device updates its own
  `fetchers[device]` entry, at most once every 15 minutes, so liveness costs a
  bounded number of ops. The fetching device itself shows its own, exact time.

## Connecting and reconnecting

Settings → Calendars lists every `IntegrationAccount` in the vault. Each row
shows its state *on this device*:

| State on this device | Row shows | Action |
|---|---|---|
| Token held, last fetch OK | "Connected · fetched 3 min ago" | Refresh, Disconnect this device |
| No token | "Not connected on this device · fetched 12 min ago by MacBook" | **Connect on this device** |
| Token rejected | "Needs reconnect" (danger tint) | **Reconnect** |
| Token held, read-only grant | "Connected · read-only on this device" | **Allow editing**, which re-runs consent for the write scope |
| No device fetching for 24 h | "No device is fetching this account" | Connect on this device |

**Connect on this device** runs the provider's authorization (or prompts for
the CalDAV app password), then checks that the authorized subject equals
`IntegrationAccount.subject` and refuses a different account by name. **Add
account** is the same flow with no subject to check, and creates the entity.

A `401` after a successful refresh (external revocation), or a refresh the
provider refuses, sets this device's `fetchers` entry to `needs_reauth`,
deletes the local token, stops this device's fetches, and raises the
*calendar reconnect needed* notification
([`../08-features/notifications.md`](../08-features/notifications.md)).
Other devices keep fetching.

## Disconnecting

- **Disconnect this device:** revoke the token at the provider (best effort,
  three retries with back-off), delete it from the keychain, and remove this
  device's `fetchers` entry. Events stay; other devices keep fetching.
- **Remove account:** a modal with **"Keep imported events"** (unchecked by
  default). Tombstones the `IntegrationAccount`. Every device that observes the
  tombstone disconnects itself as above. Events are tombstoned, or, when the box
  was ticked, kept as read-only orphans with a "source removed" badge. The account's
  open intents are abandoned: an executor that observes the tombstone starts no further
  push, and one already in flight may still reach the provider, and Sunrise no longer tracks its result.
  Disconnecting a device drops its write grant with its `fetchers` entry.
- **Revoking a Sunrise device** does not revoke its provider grants. The device
  list shows which accounts the device was fetching (from `fetchers`), and the
  revoke flow links each provider's own grant-management page.

## Multi-device behaviour, summarized

| Question | Answer |
|---|---|
| Does a new device see events without connecting? | Yes, through the vault. |
| Does connecting one device connect the others? | No. Each device consents once, if it should fetch. |
| Can two devices fetch at once? | Yes. Deterministic ids and write-if-changed make it harmless. |
| Where does a sync cursor live? | On the device that owns it. |
| What reaches the server? | Ciphertext ops, as for every entity. Never a token, never an event in plaintext. |

## Building a new integration

The integration interface is a Rust trait in
`crates/sunrise-integrations/src/lib.rs`:

```rust
#[async_trait]
pub trait IntegrationProvider: Send + Sync + std::fmt::Debug {
    fn kind(&self) -> IntegrationKind;
    async fn run(&self) -> Result<RunSummary, IntegrationError>;
}
```

A calendar provider takes its transport and its credential source by
injection — which is what keeps `gcal.rs` testable with no network — and
returns fetched occurrences. The core owns everything after that: id
derivation, write-if-changed, tombstones and the `fetchers` update, so a new
provider cannot get dedup wrong. The client adds only the authorization step and
a settings row.
