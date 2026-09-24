---
status: accepted
---

# Integrations — Overview

External integrations connect Sunrise to systems we *don't* build. Integrations
run **on-device**: each device holds its own credentials in its platform
keychain, the vault syncs only configuration and the data fetched, and the
server never holds a third-party credential or sees a fetched event in
plaintext. [ADR-0049](../11-adr/0049-calendar-integrations-per-device-oauth.md)
is the decision for calendars; it amends
[ADR-0025](../11-adr/0025-integration-account-entity.md).

| Integration | Direction | Status | Spec |
|---|---|---|---|
| Google Calendar | Read-only | **Not built.** `crates/sunrise-integrations/src/gcal.rs` implements PKCE, refresh and change detection against an injected transport, and has no consumer, no storage and no UI ([#4](https://github.com/justin13888/Sunrise/issues/4)) | [`google-calendar.md`](./google-calendar.md) |
| Microsoft Graph (Outlook.com, Microsoft 365, Exchange Online) | Read-only | **Not built** ([#4](https://github.com/justin13888/Sunrise/issues/4)) | [`microsoft-graph.md`](./microsoft-graph.md) |
| CalDAV (iCloud, Fastmail, Nextcloud, any RFC 4791 server) | Read-only | **Not built** ([#4](https://github.com/justin13888/Sunrise/issues/4)) | [`caldav.md`](./caldav.md) |
| iCalendar (.ics) | Import / export | **Live on both shipping clients**, a narrow subset: `sunrise ical import` / `export`, and the macOS File menu over the seam's `import_ical` / `export_ical` | [`icalendar.md`](./icalendar.md) |

**`IntegrationProvider` has no implementor.** Neither built integration
implements the trait `crates/sunrise-integrations/src/lib.rs` declares — the
live iCal path is driven directly by `ical_vault::{import, export}` from the CLI
and the UniFFI seam. Treat the trait as intended shape, not as a seam anything
runs through yet.

Outbound webhooks and inbound email-to-Sunrise remain non-goals
([`../00-product/non-goals.md`](../00-product/non-goals.md)). Writing to an
external calendar is not part of the design.

## Design principles

1. **On-device.** Fetching, parsing and diffing run in Rust on the user's
   devices. The server is not a credentialed proxy and never sees a token.
2. **Credentials never leave the device that minted them.** Not in an op, a
   snapshot, an export or a recovery blob.
3. **Share the data, not the secret.** Configuration and fetched events sync
   through the vault like any other entity, so every device sees every event
   whether or not it is connected.
4. **Read-only.** Sunrise plans around external calendars. It never edits them.
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
}
```

Every field merges per field ([ADR-0044](../11-adr/0044-per-field-ops.md));
`calendars` and `fetchers` are maps merged per key.

### `ExternalEvent` (synced, read-only)

```rust
pub struct ExternalEvent {
    /// BLAKE3-128("sunrise.external-event.v1" ‖ subject ‖ calendar_id
    ///            ‖ uid ‖ recurrence_id_utc_or_empty). Every device mints the same id.
    pub id: EntityRef,
    pub account: EntityRef,
    pub calendar: CalendarId,
    pub uid: String,
    pub recurrence_id: Option<Timestamp>,
    pub title: String,
    pub starts_at: SunriseTime,            // Zoned, Floating (CalDAV only) or AllDay
    pub ends_at: SunriseTime,
    pub location: Option<String>,
    pub transparency: Transparency,        // Opaque | Transparent
    pub status: EventStatus,               // Confirmed | Tentative | Cancelled
    pub url: Option<String>,               // provider's web link
    pub deleted: bool,
}
```

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

## Fetching

- **Who fetches:** every device with a live token for the account. There is no
  election ([ADR-0049](../11-adr/0049-calendar-integrations-per-device-oauth.md)
  §Decision 5).
- **Window:** occurrences from 30 days before today to 365 days after. An
  occurrence that slides out of the window is left alone, not deleted.
- **Cadence:** every 5 minutes while the app is in the foreground; on the
  platform's background refresh schedule otherwise; and on demand from
  Settings → Calendars → Refresh. Each device adds a random jitter of up to one
  minute so connected devices do not fetch in lockstep.
- **Write-if-changed:** a device writes a field of an `ExternalEvent` only when
  the fetched value differs from the stored value. Two devices fetching the same
  data write nothing; two fetching the same change converge on the same value.
- **Deletions:** an occurrence is tombstoned only on a positive deletion signal
  from the provider (a delta entry marked removed, a `404` on re-fetch, or a
  CalDAV `sync-collection` removal). An occurrence missing from a truncated or
  failed listing is not a deletion; `gcal.rs`'s `diff` already enforces this
  (`a_truncated_listing_suppresses_the_deletion_pass`).
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
  was ticked, kept as read-only orphans with a "source removed" badge.
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
