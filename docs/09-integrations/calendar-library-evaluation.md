---
status: living
---

# Calendar library evaluation

Do Rust libraries already do the hard calendar work, or must a separate library be written? As of 2 November 2026,
for [ADR-0071](../11-adr/0071-calendar-read-write-through-synced-intents.md).

**Answer.** Crates cover parsing, rule expansion, zones and the DAV protocol. None we found covers a provider-neutral
model, write semantics across Google and Graph, and two-way sync together. So a separate library, `calendar-sync`, is
justified, and its scope is that semantic layer, not parsers.

**Limits.** Registry numbers come from the crates.io and GitHub APIs. README findings are self-descriptions: no crate
was built or run. Items marked **[judgment]** or **[unverified]** have no direct source. Licence notes are engineering
judgment, not legal advice.

## The library

**Name:** `calendar-sync`, the most literal name free on crates.io and under `justin13888` at the time of checking.
`calendar-core`, `calendar-client`, `calsync` and `calendar-rs` are taken on crates.io. Check member crate names again
at first publish. **Licence:** MIT/Apache-2.0, so it stays on the `deny.toml` allow list.

**Rules:**
- No `sunrise-*` dependency.
- `model`, `recurrence` and `engine` perform no I/O, read no clock and no RNG, and build for wasm. The repository carries
  its own lints for this, because Sunrise's gates stop at its workspace. Include `jiff::Timestamp::now`, which Sunrise's
  `clippy.toml` should also be checked for.
- The application supplies storage, HTTP, credentials and scheduling.

**Scope and rough size** (one experienced Rust engineer, including tests, excluding UI) **[judgment]**:

| Component | Weeks |
|---|---|
| Provider-neutral event model and mappers | 3–5 |
| Recurrence semantics and stable instance identity | 2–3 |
| Instance index and expansion windows | 1–2 |
| Two-way sync state machine | 4–8 |
| Field-level three-way merge | 2–3 |
| Unknown-field-preserving write-back | 2–3 |
| "This and following" per provider | 2–4 |
| Offline write queue | 2–3 |
| Adapters (Google 2–3, Graph 3–4, CalDAV 2–3, EventKit 2–4, Android 3–5) | 12–19 |
| Minimal iTIP and free/busy | 3–5 |
| **Total** | **33–55** |

EventKit and Android are included for sizing only and are not first-milestone work. Sync-state recovery and series splits
are the likeliest sources of data-loss bugs.

## Decisions per layer

| Layer | Decision |
|---|---|
| Date and zones | `jiff`, plus a table generated from CLDR `windowsZones.xml` |
| iCalendar | Spike `ical-rs`. Fall back to `calcard` 0.3.14 for parsing only |
| Recurrence | Own expansion in jiff, starting from the civil algorithm in `crates/sunrise-domain/src/routine_gen.rs`, cross-checked against `calcard` and dateutil |
| CalDAV | `libdav`, native only; behaviour reference from `fast-dav-rs` and `vstorage` |
| Google, Graph | Own thin REST clients keeping raw JSON; `oauth2` for auth |
| JMAP | Deferred until the RFCs publish; keep a port for it |
| iTIP | Minimal own logic; `ical-rs` or `mailrs-ical` for wire format |
| iMIP | Not doing |
| Sync, model, queue, index | **Build** |

**Spike exit criteria for `ical-rs` (two days)** **[judgment]**: byte-identical round trip over the libical and ical.js corpora;
override and DST results agree with `calcard`; RSCALE claims reproduced; maintainer and API churn acceptable.

## Crates reviewed

### iCalendar

| Crate | Latest | Downloads (all / recent) | Licence | Notes |
|---|---|---|---|---|
| [ical-rs](https://crates.io/crates/ical-rs) ([repo](https://github.com/pimalaya/ical)) | 0.6.1, 2026-10 | 1.8k / 1.8k | MIT/Apache | New in 2026-07. Claims byte-faithful editing, three-way merge, recurrence, JSCalendar. Untested |
| [calcard](https://crates.io/crates/calcard) | 0.3.14, 2026-09 | 83k / 41k | MIT/Apache | **Archived.** [Development moved to Stalwart](https://github.com/stalwartlabs/calcard) under [AGPL or SEL](https://raw.githubusercontent.com/stalwartlabs/stalwart/main/crates/calcard/Cargo.toml). Requires `chrono` and `chrono-tz` |
| [icalendar](https://crates.io/crates/icalendar) | 0.17.17, 2026-10 | 1.06M / 410k | MIT/Apache | Active. [`BTreeMap` properties](https://raw.githubusercontent.com/hoodie/icalendar/main/src/components/other.rs) cannot keep order, so no byte fidelity. Rejected for write-back |
| [ical](https://crates.io/crates/ical) | 0.11.0, 2024-03 | 1.7M / 356k | Non-SPDX on crates.io | [Repository archived](https://github.com/Peltoche/ical-rs) |
| [caldata](https://crates.io/crates/caldata) | 0.18.0, 2026-10 | 3.4k / 2k | Apache declared | [Lists a GPL-3.0 dependency](https://docs.rs/caldata/latest/caldata/). Check the graph |
| [mailrs-ical](https://docs.rs/mailrs-ical/latest/mailrs_ical/) | 2.2.0, 2026-10 | 3.1k | Apache/MIT | No I/O; typed METHOD and REPLY building |

**Name collision:** the GitHub repo `Peltoche/ical-rs` publishes crate `ical`, while crate `ical-rs` is Pimalaya's unrelated library.

**Post-archive `calcard` fixes are copyleft.** Never copy them into a permissive fork **[judgment]**.

### Recurrence

- [`rrule`](https://crates.io/crates/rrule) 0.14.0, 2025-04: expands rules with RDATE and EXDATE. The README says nothing about
  RECURRENCE-ID. [Open issues](https://github.com/fmeringdal/rust-rrule/issues) include a BYDAY union bug, lost occurrences and DST
  handling. **Rejected.**
- [`calcard` `expand_dates`](https://raw.githubusercontent.com/stalwartlabs/calcard/main/src/icalendar/dates.rs): handles RDATE, EXDATE,
  RECURRENCE-ID and `RANGE`. Useful as a differential oracle.
- [`ical-rs`](https://raw.githubusercontent.com/pimalaya/ical/master/CHANGELOG.md): series plus overrides, highest-SEQUENCE selection,
  `THISANDFUTURE`. Its README and changelog disagree on non-Gregorian expansion.

### Date and time

[`jiff`](https://docs.rs/jiff/latest/jiff/_documentation/comparison/index.html) defaults to the RFC 5545 "compatible" strategy for gaps and
folds, and Sunrise already uses only it ([ADR-0011](../11-adr/0011-datetime-jiff.md)). Floating time is a separate variant holding a civil
datetime, never a coerced zone, which `SunriseTime` already models ([ADR-0017](../11-adr/0017-sunrise-time-representation.md)).

### CalDAV

| Crate | Latest | Licence | Assessment |
|---|---|---|---|
| [libdav](https://docs.rs/libdav/latest/libdav/) | 0.11.2, 2026-10 | ISC | Requests prepare and parse without I/O. **[unverified]** `sync-collection` support: confirm in source. Pulls hyper and DNS, so native only |
| [vstorage](https://docs.rs/vstorage/latest/vstorage/) | 0.12.0, 2026-10 | **EUPL-1.2** | Design reference. Compares whole items, so no field merge |
| [fast-dav-rs](https://docs.rs/fast-dav-rs/latest/fast_dav_rs/) | 0.19.0, 2026-10 | **LGPL-3.0** | Behaviour reference for stale-token recovery |
| [minicaldav](https://crates.io/crates/minicaldav) | 0.8.0, 2023 | **GPL-3.0** | Stale, avoid |
| [kitchen-fridge](https://crates.io/crates/kitchen-fridge) | 0.4.0, 2022 | MIT | Stale, avoid |

### Standards in progress

[JMAP Calendars](https://datatracker.ietf.org/doc/draft-ietf-jmap-calendars/) waits in the RFC Editor queue on
[JSCalendarbis](https://datatracker.ietf.org/doc/draft-ietf-calext-jscalendarbis/). We found no evidence that Google, Microsoft or iCloud offer it
**[unverified]**. Keep the model close to JSCalendar so a provider crate can be added.

### Provider clients and native bridges

- [`google-calendar3`](https://crates.io/crates/google-calendar3): generated typed structs likely drop unknown fields **[inference]**. Not for write-back.
- [`graph-rs-sdk`](https://crates.io/crates/graph-rs-sdk): [last commit 2025-08](https://github.com/sreeise/graph-rs-sdk/commits). Avoid as a core dependency.
- [`oauth2`](https://crates.io/crates/oauth2): already in this workspace.
- [`objc2-event-kit`](https://crates.io/crates/objc2-event-kit) if EventKit is ever revisited. Android needs `jni` plus own Kotlin.

### Projects to read

| Project | Licence | Lesson |
|---|---|---|
| [io-pimdir](https://github.com/pimalaya/io-pimdir) | MIT/Apache | Closest architecture: I/O-free sync verbs. Opaque bodies suit DAV better than JSON APIs. New, so judge by running it |
| [cosmic-pim](https://github.com/Magnetar-OS/cosmic-pim) | MPL-2.0 | CalDAV reconciliation and durable writeback. New |
| [caldir](https://github.com/t4t5/caldir) | MIT | Plugin providers, `.ics` on disk |
| [Stalwart](https://github.com/stalwartlabs/stalwart) | **AGPL or SEL** | Fullest scheduling logic. Behaviour reference only |
| [RustiCal](https://github.com/lennart-k/rustical) | **AGPL-3.0** | Live test target |

## Provider facts the design absorbs

- **Google:** [410 means wipe and resync; `nextSyncToken` only on the last page](https://developers.google.com/workspace/calendar/api/guides/sync).
  [`timeMin` and `timeMax` cannot accompany a sync token](https://developers.google.com/workspace/calendar/api/v3/reference/events/list).
  [Instances carry `recurringEventId` and `originalStartTime`](https://developers.google.com/workspace/calendar/api/guides/recurringevents).
  [Quotas and jitter](https://developers.google.com/workspace/calendar/api/guides/quota).
  [Push needs public HTTPS and is "not 100% reliable"](https://developers.google.com/workspace/calendar/api/guides/push).
- **Google verification:** [reading Calendar events is Google's own example of a sensitive scope, which needs verification but not the restricted-scope security assessment](https://developers.google.com/identity/protocols/oauth2/production-readiness/sensitive-scope-verification).
  [One secondary source](https://developer.nylas.com/docs/provider-guides/google/google-verification-security-assessment-guide.md) lists all calendar scopes as sensitive. Confirm on the project's Data Access page.
- **Graph:** [v1.0 delta is `calendarView/delta`, requires a date range and has no `$select`](https://learn.microsoft.com/en-us/graph/delta-query-events).
  [`iCalUId` differs per occurrence, `id` changes without `ImmutableId`, and `transactionId` gives create idempotency](https://learn.microsoft.com/en-us/graph/api/resources/event?view=graph-rest-1.0).
  [Tokens have no fixed lifetime, and replays must be tolerated](https://learn.microsoft.com/en-us/graph/delta-query-overview).
  [Six pattern and three range types](https://learn.microsoft.com/en-us/graph/outlook-schedule-recurring-events), so many rules cannot be represented and the mapper must report loss.
  [No this-and-following endpoint, per a Q&A responder](https://learn.microsoft.com/en-sg/answers/questions/2137307/how-to-edit-selected-and-following-recurrent-outlo), which is not official documentation.
- **iCloud:** [app-specific passwords need 2FA, are limited to 25, and all are revoked on a password change](https://support.apple.com/en-us/102654).
  [Maintainers describe no official documentation for basic operations](https://github.com/python-caldav/caldav/issues/393).
- **EventKit:** [predicates are capped at four years](https://developer.apple.com/documentation/eventkit/ekeventstore/predicateforevents(withstart:end:calendars:).md).
- **Android:** [sync adapters, `SYNC_DATA` columns and read-only Instances](https://developer.android.com/identity/providers/calendar-provider).

## Conformance assets

| Asset | Use | Caveat |
|---|---|---|
| [libical test data](https://github.com/libical/libical/tree/master/test-data) | Parser corpus | Check licence before vendoring |
| [dateutil `test_rrule.py`](https://github.com/dateutil/dateutil/blob/master/tests/test_rrule.py) | Rule expectations | Check licence; port as data |
| [ical.js](https://github.com/kewisch/ical.js) | Corpus, oracle | MPL-2.0 |
| [`rrule` tests](https://github.com/fmeringdal/rust-rrule/tree/main/rrule/src/tests) | RFC and DST cases | Some known bugs |
| [CalDAVTester](https://github.com/apple/ccs-caldavtester) | Scenario reference | Archived; a server suite |
| [caldav-server-tester](https://github.com/python-caldav/caldav-server-tester) | Server quirks | GPL-3.0; test tool only |
| RustiCal, Stalwart containers | Live CalDAV | AGPL; out of process only **[judgment]** |
| Google, Graph, iCloud | Record-replay and live probes | No official suite found |
