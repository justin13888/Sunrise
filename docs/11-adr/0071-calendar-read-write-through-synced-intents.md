# 0071 — Sunrise reads and writes external calendars: series in the vault, edits as synced intents, and no lease

**Status:** accepted

**Amends** [ADR-0049](./0049-calendar-integrations-per-device-oauth.md). Its
decision 1 (read-only) and decision 4 (stored as expanded occurrences) are
**withdrawn**, and so are its rejections of two-way sync and of series storage.
Its decisions 2, 3, 5, 6, 7 and 8 stand: credentials stay per device, the vault
syncs configuration, any device with a live token fetches, and liveness is
synced. Its rejection of EventKit and of provider push also stands, with the
limits below.

**Amends** [`../00-product/non-goals.md`](../00-product/non-goals.md): "Not a
calendar" and "not a calendar editor" are withdrawn. "Not a calendar protocol
server" stays.

**Amends** every accepted page that restated ADR-0049's read-only or
expanded-occurrence design:
[`overview.md`](../09-integrations/overview.md),
[`google-calendar.md`](../09-integrations/google-calendar.md),
[`microsoft-graph.md`](../09-integrations/microsoft-graph.md),
[`caldav.md`](../09-integrations/caldav.md),
[`icalendar.md`](../09-integrations/icalendar.md),
[`time-blocks.md`](../02-domain/time-blocks.md),
[`identifiers.md`](../02-domain/identifiers.md),
[`time-blocking.md`](../08-features/time-blocking.md),
[`planner.md`](../08-features/planner.md),
[`planning-views.md`](../08-features/planning-views.md),
[`parity-matrix.md`](../07-clients/parity-matrix.md) and
[`desktop.md`](../07-clients/desktop.md).

**Depends on** [ADR-0034](./0034-revocation-bounds-reads-not-writes.md) (no
replica refuses an entity op), [ADR-0044](./0044-per-field-ops.md),
[ADR-0045](./0045-schema-identity-and-feature-gating.md),
[ADR-0046](./0046-optional-stream.md),
[ADR-0048](./0048-interactive-planner.md) and
[ADR-0065](./0065-copy-on-write-entity-versions.md).

**Specified in** [`../09-integrations/calendar-goals.md`](../09-integrations/calendar-goals.md)
(what to build) and
[`../09-integrations/calendar-architecture.md`](../09-integrations/calendar-architecture.md)
(how, `proposed` until the conformance spike confirms the provider behaviour it
rests on).

## Context

Sunrise is opened throughout the day, so its calendar is where people will look
first. If it can only show events, they keep a dedicated calendar app open beside
it and the plan lives in two places. The product goal is now that Sunrise's
calendar read and write beat dedicated calendar apps on features and UI.

ADR-0049 anticipated this: "Demand for write-back. It would be a new ADR with its
own consent and conflict model." This is that ADR. Nothing of ADR-0049 is built,
so changing its entity shape has no migration cost.

Writing to a provider from a multi-device, end-to-end encrypted vault is not
read-only sync with a `PUT` added. Four facts force the design.

1. **The relay cannot arbitrate.** It treats ops as opaque ciphertext and enforces
   no business rule ([`../01-architecture/trust-and-server-role.md`](../01-architecture/trust-and-server-role.md)).
   A lease carried in ops would only be advisory, and it would bring back the
   heartbeat and failover ADR-0049 rejected.
2. **Per-field LWW cannot hold provider truth.** ADR-0044's stamps say when a
   device saw a value, not which provider version it belongs to. A device replaying
   an old response stamps it newer, and a stale title can pair with the newest
   etag. `If-Match` then passes, and a CalDAV `PUT` built from that state overwrites
   real data.
3. **"Write if the mapped value differs" can ping-pong.** Two builds that map one
   event differently overwrite each other on every poll.
4. **Deterministic ids alone do not make writes safe.** Microsoft documents
   `transactionId` for client retries, not across devices, and documents no
   `If-Match` on event update or `accept`. CalDAV has no tombstones, so a stale retry
   after a delete can recreate the event.

## Decision

**1. Sunrise reads and writes.** Google Calendar, Microsoft Graph and CalDAV gain
create, update, delete, RSVP and recurring edits. A write scope is a separate grant
per device, recorded in the synced account configuration, so a device holding a
read-only grant never pushes. ICS subscriptions are added as a read-only source with no
credential: any device fetches the URL, and their events can never be edited.

**2. Series, not expanded occurrences.** `ExternalEvent` is one series entity with
a map of override registers, keyed by provider instance identity (Google
`originalStartTime`, Graph `originalStart`, CalDAV `RECURRENCE-ID`). A non-recurring
event is a series with no rule. It stays separate from `Block`. Occurrences are
expanded on read or into a discardable local cache, because expansion depends on a
window and the projection must rebuild byte-identically (ADR-0065).

**3. One quiet register per provider resource.** The normalized fields, the provider
revision and the provider ids merge as one value, never per field. It does not raise
a user-visible version conflict. Presence lives inside the register, not in
`deleted`, so a restored event is not lost to a concurrent delete. Removing an account
still tombstones its entities as ADR-0049 says, and abandons its open intents.

**4. Write only when the provider revision differs.** Each register carries a mapper
version, and an older build does not rewrite a newer mapping. Each device keeps a
local shadow of the last revision it saw. When an incoming op moves a revision away
from that shadow, the device re-reads that event from the provider and writes only if
the result differs. Devices settle once the provider is quiet. The register is still
last-writer-wins, and CalDAV etags have no order, so a stale response can win for a moment.
This repair is what ends it, on any device that holds a token. A push is never harmed,
because it starts from a fresh read.

**5. An edit is a synced intent.** A device with no token can edit offline, and any
device with a write grant can carry the edit out. An intent has a random id, minted
once through the injected RNG, from which every provider id derives. It carries its
base revision and per-field base values, so every pusher performs the same merge.
A fetch never touches an intent and an edit never touches a register; the display is
the register with open intents overlaid.

**6. Synced versus device-local.**

| State | Where | Why |
|---|---|---|
| Tokens, cursors, back-off, attempt journal, raw payload cache, shadow | Device-local | They describe one device's session, and the payload can be fetched again |
| Account configuration, `fetchers`, write grant | Synced | ADR-0049, plus the capability from decision 1 |
| Provider resource register | Synced, quiet | Decision 3 |
| Intent, with base revision and base values | Synced, append-only | Any device must reach the same merge |
| Outcomes | Synced, append-only | A monotonic join: applied dominates rejected |

Opaque provider payloads never sync. A device that can push can read the original
itself, a device with no token never writes to a provider, and they are large against
the relay's per-channel log. Sealing follows ADR-0049: under the account's target Stream
key, or the private domain when it has none. So an account targeted at a Stream that is
later shared (#133) shows its normalized fields, attendees included, to the recipients,
and the sharing flow must warn about that.

**7. Who pushes: no lease, no election.**

1. The author's device pushes first. Other devices with a write grant wait a delay
   ranked by a hash of intent and device. This only narrows the race.
2. A device takes over only when its relay session has recently drained.
3. Every push starts with a fresh provider read, and plans from that read, so any
   device can resume a half-finished operation.
4. Intents run in causal order per entity and expire, so an old intent never pushes
   itself automatically.
5. Each operation uses the strongest precondition its provider offers.

**8. Who may cause a push.** Intents are sealed in the private key domain, never under
a shareable Stream key. The executor acts only on an intent authored by one of the
owner's non-revoked devices. Under ADR-0034 the merge still accepts the op; the
executor is what declines to act, and the decline is recorded as an outcome.

**9. Provider conflicts are derived.** An open intent, its base and the current
register determine a conflict, so every replica agrees without writing anything, in
the way ADR-0065 D18 derives `dependency_cycle`. It appears in `Query::Conflicts`.
Choosing *theirs* abandons the intent. Choosing *ours* writes a new intent based on
the current revision. A provider rejection is an outcome, not a conflict.

**10. "This and following" creates first, then trims.** Neither Google nor Graph
offers a split operation that we have found. Create the new series with a derived id,
then trim the old one. A crash leaves a visible duplicate that any device can finish,
never lost occurrences.

**11. Publishing a Block is opt-in.** A new `BlockPublication` entity maps a Block to
an account and calendar, and each publish is an intent. The default is busy-only:
generic title, no notes, private visibility, with a plain warning that a published
event leaves end-to-end encryption. The Block stays the source of truth. The mirror is
suppressed from planner inputs, writes are debounced and never notify attendees, and an
external edit to a mirror shows as a conflict, not a silent adoption.

**12. The recurrence engine sits beside `routine_gen`.** Occurrence keys derive task
ids (`crates/sunrise-domain/src/routine_gen.rs`), so swapping that expander would
duplicate tasks and break streaks. A differential test covers the shared subset.

**13. The generic logic lives in a separate library, `calendar-sync`.** It lives in its
own MIT/Apache-2.0 repository, depends on no `sunrise-*` crate, and reaches this
workspace through crates.io, since `deny.toml` denies git sources. Its model,
recurrence and sync-engine crates do no I/O and read no clock. Sunrise supplies storage,
HTTP, credentials and scheduling. Selection of its dependencies is recorded in
[`../09-integrations/calendar-library-evaluation.md`](../09-integrations/calendar-library-evaluation.md).

**14. What the trust model limits, stated plainly.**

| Feature | Position |
|---|---|
| Public scheduling links | No server-rendered page. A sealed slots blob with the key in the URL fragment is possible, needs its own ADR, and is owner-confirmed rather than instant |
| Provider push | Polling stays the default. An opt-in doorbell, which tells the relay when a calendar changed and names it, needs its own ADR |
| Other people's free/busy | Direct from the device through the provider. Results are ephemeral and never sync |
| Invitations | Sent by the provider, and disclosed as such. A CalDAV server without auto-scheduling gets no RSVP. Inbound email stays a non-goal |
| Web client | A token-less editor first. No keychain equivalent exists there yet |
| EventKit | Stays out unless the iCloud spike meets ADR-0049's revisit condition |

## Alternatives considered

| Option | Why not |
|---|---|
| **Stay read-only** | Keeps a dedicated calendar app open next to Sunrise, which is the outcome this ADR exists to end. |
| **A lease or elected pusher** | The relay cannot compare-and-swap ciphertext, so a lease would be advisory, and a quiet holder would strand every edit until failover noticed. |
| **Only the authoring device pushes** | An edit made on a phone that then goes flat would never reach the provider while a connected Mac sits idle. |
| **Per-field LWW for provider fields** | Allows a stale value that no later poll repairs, and a torn value and etag pair (Context 2). |
| **Sync raw provider payloads** | Large against the relay's per-channel log, and carries everything the provider holds to Stream share recipients. A pushing device can fetch the original. |
| **Device-local base snapshots** | Two pushers with different bases reach different merges. |
| **Merge `ExternalEvent` into `Block`** | The Block is Sunrise's own encrypted plan. Merging makes private planning data one bug away from a provider. |
| **Replace `routine_gen` with the new engine** | Changes occurrence keys, so duplicates tasks and breaks streaks. |
| **Build the library in-tree, extract later** | Would avoid cross-repository releases while the model settles, and was weighed. The owner chose a separate repository from the start, so the library's boundary holds from its first commit. |
| **`rrule` crate as the recurrence layer** | Stalled, with open DST bugs and no `RECURRENCE-ID` handling. |
| **`calcard` for expansion** | Pulls `chrono-tz`, a second time zone database beside the bundled jiff one, so resolved instants could differ. Its active development moved under AGPL or SEL. |

## Consequences

- **Residual risks remain, and the docs must say so.** A duplicated Graph RSVP mail. A
  Graph lost update limited to the patched properties. A CalDAV stale retry recreating
  an event when a device passes the freshness check but has not yet received the
  outcome.
- **Writes depend on the Apple conflict UI,** which is unbuilt (ADR-0065 §Delivered).
- **`gcal.rs`'s poll half is replaced.** Its `diff` treats an absent id as deleted,
  contradicting the positive-deletion rule in
  [`../09-integrations/overview.md`](../09-integrations/overview.md). Its PKCE flow and
  decode cases are kept as fixtures. The #660 split lands first, unchanged.
- **Determinism gates do not see dependency code.** `clippy.toml` and
  `.github/scripts/core-filesystem-gate.py` stop at this workspace, so `calendar-sync`
  carries its own rules, and an end-to-end test through a shipped binary proves the
  crate is really used.
- **Wasm stays clean by feature gate.** `sunrise-client-core` links
  `sunrise-integrations` unconditionally, so `libdav` and the executor go behind a
  feature, as `relay` does.
- **Re-consent.** Adding a write scope asks existing read-only users to authorize again,
  and the OAuth apps need verification for the broader scope.
- **Parity with dedicated apps is not reached before background refresh and the web
  client arrive.** Dedicated apps with server-side push will be fresher on mobile until
  then, and this ADR does not pretend otherwise.

## What would force revisiting this

1. **The conformance spike contradicts a provider assumption,** for example Graph
   enforcing `If-Match`, which would remove a residual risk, or Google reusing a
   deleted client id, which would add one.
2. **iCloud CalDAV proves too hostile to write through.** That is ADR-0049's own trigger
   for EventKit.
3. **Duplicate pushes are observed** in spite of decision 7. The answer is a stronger
   precondition per operation, not an election.
4. **JMAP Calendars publishes and a major provider offers it.** It would become another
   provider crate.
