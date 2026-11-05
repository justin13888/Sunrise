---
status: proposed
---

# Calendar architecture

The design for [`calendar-goals.md`](./calendar-goals.md) under
[ADR-0071](../11-adr/0071-calendar-read-write-through-synced-intents.md). This stays
`proposed` until the conformance spike settles the provider behaviour marked **Spike** below.
Numbers here are estimates, not contracts.

## Layers

```text
calendar-sync (separate repository, crates.io, MIT/Apache-2.0)
  calendar-sync-model        series, override, instance id, time variants
  calendar-sync-recurrence   window expansion over the model
  calendar-sync-engine       step(state, input) -> (state, effects); three-way merge,
                             write queue, split planner, instance index, free/busy
  calendar-sync-google       pure mapping + thin REST
  calendar-sync-graph        pure mapping + thin REST
  calendar-sync-caldav       pure mapping + libdav (native only)
  calendar-sync-testkit      fake providers with fault injection, recorded fixtures

Sunrise
  sunrise-domain / sunrise-core     ExternalEvent, intents, conflicts; pure read path
  sunrise-integrations              binds the library to Sunrise entities
  sunrise-client-core (feature)     executor driver, injected HTTP, native provider crates
  sunrise-core-bindings             seam for Apple clients
  apps/apple                        views, OAuth browser step, keychain
```

The model, recurrence and engine crates perform no I/O, read no clock and build for wasm.
The planner reads events through the core, so the read path cannot live only in
`sunrise-integrations`, which depends on `sunrise-core`.

## Entities

```rust
pub struct ExternalEvent {              // one per provider series or single event
    pub id: EntityRef,                  // derived from account subject, calendar, provider id
    pub account: EntityRef,
    pub calendar: CalendarId,
    pub resource: ProviderRegister,     // one quiet register, merged whole
    pub overrides: BTreeMap<InstanceKey, ProviderRegister>,
}

pub struct ProviderRegister {
    pub revision: ProviderRevision,     // etag or changeKey; never ordered across CalDAV
    pub provider_ids: ProviderIds,
    pub mapper_version: u32,
    pub present: bool,
    pub fields: NormalizedEvent,        // title, times, rule, attendees, location, ...
}

pub struct CalendarIntent {             // sealed in the private key domain; append-only
    pub id: IntentId,                   // random; minted once; provider ids derive from it
    pub target: IntentTarget,           // event, instance, or a following split
    pub operation: IntentOperation,     // create, update, delete, rsvp, split
    pub base_revision: Option<ProviderRevision>,
    pub base_fields: BTreeMap<FieldName, FieldValue>,
    pub send_updates: SendUpdates,
    pub expires_at: Timestamp,
}

pub struct IntentOutcome {              // append-only; applied dominates rejected
    pub intent: IntentId,
    pub result: Applied | Rejected { reason },
    pub device: DeviceId,
}

pub struct BlockPublication {           // id = H(block, account, calendar)
    pub block: EntityRef,
    pub calendar: CalendarId,
    pub visibility: Busy | Full,        // default Busy
}
```

`InstanceKey` is the provider's identity for an instance: Google `originalStartTime`, Graph
`originalStart`, CalDAV `RECURRENCE-ID`. ADR-0049's id formula is not reused, because Graph's
`iCalUId` differs per occurrence and Google's is shared.

Entities register through the entity registry under a `vault_requires` feature
([ADR-0045](../11-adr/0045-schema-identity-and-feature-gating.md)). Whether the registry already
allows a quiet nested register, and whether `Merge::AppendOnly`
(`crates/sunrise-id/src/registry/spec.rs`) survives compaction for outcomes, must be checked
first. **Not yet verified.**

## Read

1. A device with a live token fetches through a device-local cursor, as in
   [`overview.md`](./overview.md).
2. It writes a register only when the provider revision differs, and never rewrites a register
   from a newer mapper version.
3. When an incoming op moves a revision away from this device's local shadow, it re-reads that
   event and writes only if the result differs.
4. Occurrences are expanded on read, or into a discardable local cache.

Google forbids `timeMin` and `timeMax` with a sync token, so filter locally. Graph's delta
requires a window, and its `@removed` also covers events outside that window, so confirm a
deletion with a read.

## Write

```text
User edit
   |
   v
CalendarIntent (sealed, synced) ----> displayed: register + open intents
   |
   v  author first; others after a hash-ranked delay, once their relay session has drained
Executor on a device with a write grant, for the owner's non-revoked author
   |
   v
Fresh provider read --> plan from that read --> conditional write --> IntentOutcome
```

**Three-way merge at push, per field:** theirs equals base, apply ours. Theirs equals ours, done.
All three differ, a derived conflict.

**Settled details:**
- **Expiry** is decided by the application's injected clock, which the engine receives as an
  input. The duration is not fixed yet.
- **A rejection is final for a permanent reason,** such as a refused recurrence or a revoked
  author, and other devices stop retrying. A transient failure records no outcome, so another
  device may retry.
- **Undo of a pushed edit is a new intent** restoring the base values, not a provider rollback,
  so it may send attendees a second update. The UI says so before sending.
- **Busy-only visibility** on publish depends on the provider. **Spike:** CalDAV `CLASS` handling.
  A server that cannot honour it gets a generic title and a warning, not a silent failure.
- **Unverified OAuth apps do not ship.** Write support waits for verification, since Sunrise has no
  feature flags.
- **Pinning:** Sunrise takes `calendar-sync` at an exact version from crates.io. The owner publishes
  it, and no Sunrise integration work starts until the crates it needs are released.

**Goals that need follow-up specs:** full event search needs `docs/08-features/search.md` and
[ADR-0052](../11-adr/0052-search-v2.md) to index more than title and location. Leave alerts and write
re-consent need rows in `docs/08-features/notifications.md`.

### Per-provider operation table

| Operation | Google | Graph | CalDAV |
|---|---|---|---|
| Create | Client-chosen id. **Spike:** re-insert after delete, simultaneous insert | `transactionId` plus a filterable extended property holding the intent id, read first. **Spike:** scope and lifetime | `If-None-Match: *` on a deterministic href and UID. A stale retry can resurrect after delete |
| Update | `If-Match` | Read, compare `changeKey`, minimal `PATCH`. **Spike:** `If-Match`. A race remains, limited to patched properties | `If-Match`; rebuild from a fresh read, since `PUT` replaces the object |
| Delete | `If-Match`; 404 or 410 means done | 404 means done. **Spike:** header support. Organizer `cancel` sends mail | `If-Match`; 404 means done |
| RSVP | Conditional patch of the user's attendee entry. **Spike:** whether the full array is required | `accept` is not documented as conditional or idempotent, so a duplicate can send a second reply | Conditional `PUT` of `PARTSTAT` with `If-Schedule-Tag-Match` (RFC 6638). Unavailable without auto-scheduling. **Spike:** iCloud |
| Split | No API found. Insert, then conditional trim | No endpoint found. Master `PATCH` with no precondition; can send mail per edited instance | Conditional `PUT` of the old series, `If-None-Match` `PUT` of the new. Not atomic |

Moving later overrides onto the new series adds more conditional writes. An Outlook split has been
observed producing two masters with no shared id ([one incident report](https://pnp.github.io/blog/post/one-outlook-edit-became-two-microsoft-graph-events/)),
so correlation cannot rely on the provider.

## Conformance spike

On real accounts, before the entity model is frozen:

- Graph: `If-Match` on `PATCH`, `DELETE` and `accept`; `transactionId` across devices and over time;
  delta on non-primary calendars; whether delta returns masters; sparse delta rows.
- Google: two simultaneous inserts with one id; re-insert after delete; etag stability between list
  and read, and across clients.
- CalDAV: iCloud and Fastmail RFC 6638 support, href handling and `sync-collection`; CORS for a browser
  client.
- Recurrence: what each provider does to exceptions when a series changes.

## Quotas and limits to respect

- Relay: a per-channel log limit (`crates/sunrise-server/src/relay_log/append.rs`) and a device op rate
  (`docs/05-sync/backpressure-and-quotas.md`). A large first import must pace itself, and a whole-series
  register has to stay small.
- Google: per-user and per-project quotas, with jitter advised.
- Graph: per-mailbox throttling. Published numbers are dated, so react to `429` instead of relying on them.
- iCloud: limited app-specific passwords, all revoked on a password change. The UX must handle a credential
  dying silently.

## Migration from the `gcal` module

#660 has landed, splitting the module without changing behaviour. Do not build on `gcal/poll.rs`: `diff`
reports an id absent from a complete listing as deleted, it has no `etag` or recurrence, and `EventSyncer` is read-only. Keep the PKCE flow, the
refresh rule and the decode cases as fixtures. Check for overlap with the `oauth2` use in `sunrise-auth`.

## Milestones

| M | Delivers | Depends on | First benefit |
|---|---|---|---|
| M0 | Conformance spike, hands-on benchmark, OAuth registration and verification, #660 | None | None |
| M1 | Library model and recurrence; `ExternalEvent` series entity; read-time expansion; differential test against `routine_gen` | M0 | None |
| M2 | Google read end to end on CLI and Apple; Settings → Calendars; events on the grid | M1 | Real calendar on every device |
| M3 | Graph and CalDAV read; ICS subscriptions | M2 | All accounts |
| M4 | Intents, executor, outcomes; derived conflicts and Apple conflict UI; Google single-event writes and RSVP | M2 | First write |
| M5 | Recurring writes with split; Graph writes; CalDAV writes | M3, M4 | Full write |
| M6 | Quick entry, command bar, multi-zone grid; `BlockPublication`; planner takes external events as fixed inputs | #342, M4 | Plan and publish |
| M7 | Reminders and leave alerts, system surfaces, others' free/busy, background refresh, doorbell ADR, web calendar, sealed scheduling links ADR | M4 | Parity |

The library's semantic layer is roughly 33–55 engineer-weeks excluding UI. That is a judgment, not a
measurement, and sync-state recovery and series splits carry the most risk.

## Testing

- **Library:** fuzzed parsers; property tests for sync convergence over random operations with injected
  faults (410, replay, partial page, 412); recorded fixtures; parser corpora and a recurrence oracle listed
  in [`calendar-library-evaluation.md`](./calendar-library-evaluation.md).
- **Sunrise:** two devices pushing one intent apply it once where the provider allows, and the known residual
  cases are asserted as such. No op payload contains a credential or raw provider payload. A revoked author's
  intent produces no provider request. A device with no token displays events and queues edits.
- **Live:** a scheduled suite against probe accounts per provider, and containerized CalDAV servers
  (RustiCal, Stalwart) as out-of-process test targets.
