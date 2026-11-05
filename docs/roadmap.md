---
status: living
---

# Roadmap

Sunrise has no v1. The product version stays at v0.x
([ADR-0042](./11-adr/0042-v0-forever.md)), and nothing is "deferred past" a
release. Work is **ranked**: this document orders it, and the
[Sunrise App project board](https://github.com/users/justin13888/projects/1)
holds the same order as a **Phase** field (P0–P6) and a **Rank** number on every
open issue. When the two disagree, the board is current and this file is stale.
Fix this file.

## The invariant everything is ranked against

> Merging vaults across client versions MUST NEVER break and MUST NEVER lose data.

A phase that ships features on top of a merge model that loses data only adds
more data to lose. So the order is fixed:
1. Integrity (P0).
2. The commit tree and schema machinery that makes the invariant true (P1).
3. The domain model the UX needs (P2).
4. Completing macOS on that model (P3).
5. Moving the result to the phone (P5).

Server production work (P4) runs alongside P2–P3, because it shares no code with them.

## Triage rules

- **Every open issue has exactly one Phase and one Rank.** An issue without them
  is untriaged and is ranked before anything else is picked up.
- **Rank is one total order across the board.** A lower rank number is done
  first, and phases are contiguous runs of it. The rank is a sort order, not an
  estimate.
- **An issue moves phase only by editing its board fields**, with the reason in
  a comment. It never moves by relabelling it "later".
- **A bug that breaks the invariant goes to P0**, whatever it touches.
- **Dependencies override rank.** Where an issue says "Depends on", the
  dependency is done first, even if it is ranked later.

## P0 — Integrity

Open pull requests first, then security and correctness defects in shipped
behaviour, then the gates that keep them fixed.

1. Revocation soundness: [#411](https://github.com/justin13888/Sunrise/issues/411).
2. Apple session and Keychain: [#258](https://github.com/justin13888/Sunrise/issues/258).
3. Correctness defects in shipped behaviour: [#515](https://github.com/justin13888/Sunrise/issues/515), [#898](https://github.com/justin13888/Sunrise/issues/898), [#838](https://github.com/justin13888/Sunrise/issues/838), [#520](https://github.com/justin13888/Sunrise/issues/520), [#928](https://github.com/justin13888/Sunrise/issues/928), [#807](https://github.com/justin13888/Sunrise/issues/807), [#890](https://github.com/justin13888/Sunrise/issues/890).
4. Gates and test adequacy: [#304](https://github.com/justin13888/Sunrise/issues/304), [#271](https://github.com/justin13888/Sunrise/issues/271), [#497](https://github.com/justin13888/Sunrise/issues/497), [#518](https://github.com/justin13888/Sunrise/issues/518), [#757](https://github.com/justin13888/Sunrise/issues/757), [#770](https://github.com/justin13888/Sunrise/issues/770), [#761](https://github.com/justin13888/Sunrise/issues/761), [#762](https://github.com/justin13888/Sunrise/issues/762),
   [#756](https://github.com/justin13888/Sunrise/issues/756), [#772](https://github.com/justin13888/Sunrise/issues/772).
5. Documentation truth: [#743](https://github.com/justin13888/Sunrise/issues/743), [#744](https://github.com/justin13888/Sunrise/issues/744), [#745](https://github.com/justin13888/Sunrise/issues/745), [#746](https://github.com/justin13888/Sunrise/issues/746), [#747](https://github.com/justin13888/Sunrise/issues/747), [#748](https://github.com/justin13888/Sunrise/issues/748), [#749](https://github.com/justin13888/Sunrise/issues/749), [#764](https://github.com/justin13888/Sunrise/issues/764),
   [#767](https://github.com/justin13888/Sunrise/issues/767), [#768](https://github.com/justin13888/Sunrise/issues/768).
6. Code that lives in the wrong place: [#750](https://github.com/justin13888/Sunrise/issues/750), [#751](https://github.com/justin13888/Sunrise/issues/751), [#758](https://github.com/justin13888/Sunrise/issues/758), [#759](https://github.com/justin13888/Sunrise/issues/759), [#760](https://github.com/justin13888/Sunrise/issues/760).
7. Decisions to take before the code they name changes: [#752](https://github.com/justin13888/Sunrise/issues/752), [#753](https://github.com/justin13888/Sunrise/issues/753), [#754](https://github.com/justin13888/Sunrise/issues/754), [#755](https://github.com/justin13888/Sunrise/issues/755), [#763](https://github.com/justin13888/Sunrise/issues/763), [#765](https://github.com/justin13888/Sunrise/issues/765), [#766](https://github.com/justin13888/Sunrise/issues/766),
   [#769](https://github.com/justin13888/Sunrise/issues/769), [#771](https://github.com/justin13888/Sunrise/issues/771), [#773](https://github.com/justin13888/Sunrise/issues/773), [#774](https://github.com/justin13888/Sunrise/issues/774), [#775](https://github.com/justin13888/Sunrise/issues/775), [#776](https://github.com/justin13888/Sunrise/issues/776).
8. The stabilization cleanup: every open issue labelled
   [`de-slop`](https://github.com/justin13888/Sunrise/issues?q=is%3Aissue+is%3Aopen+label%3Ade-slop).
   They are filed and closed in batches, so this file names the label, not each issue.

## P1 — Commit tree and schema

The design of record is [ADR-0043](./11-adr/0043-commit-tree.md),
[ADR-0044](./11-adr/0044-per-field-ops.md) and
[ADR-0045](./11-adr/0045-schema-identity-and-feature-gating.md), with the rules
in [`02-domain/schema-versioning.md`](./02-domain/schema-versioning.md) and
[`04-storage/migrations.md`](./04-storage/migrations.md).

1. [#441](https://github.com/justin13888/Sunrise/issues/441) Rebuild the projection from the local op log, and give the apps a path to it and to account recovery.
2. [#383](https://github.com/justin13888/Sunrise/issues/383) Revocation withdrawal, now that unknown op kinds are parked.
3. [#454](https://github.com/justin13888/Sunrise/issues/454) Identity-signed authority revocations, behind the feature gate.

## P2 — Domain model

Specified in [`10-cross-cutting/time.md`](./10-cross-cutting/time.md),
[`02-domain/tasks.md`](./02-domain/tasks.md),
[`02-domain/routines-and-recurrence.md`](./02-domain/routines-and-recurrence.md),
[`02-domain/preferences.md`](./02-domain/preferences.md),
[`02-domain/day-schedule.md`](./02-domain/day-schedule.md) and
[`02-domain/places.md`](./02-domain/places.md), and in ADRs
[0046](./11-adr/0046-optional-stream.md),
[0047](./11-adr/0047-deadlines-and-lateness.md),
[0050](./11-adr/0050-preferences-and-day-schedule.md) and
[0051](./11-adr/0051-places.md).

1. [#338](https://github.com/justin13888/Sunrise/issues/338) Day schedule: wake and sleep times.
2. [#333](https://github.com/justin13888/Sunrise/issues/333) Dependency cycles resolved at merge; a requirements model.
3. [#340](https://github.com/justin13888/Sunrise/issues/340) Manual ordering as a fractional index in Rust.
4. [#341](https://github.com/justin13888/Sunrise/issues/341) `SavedView` as a synced entity.
5. [#342](https://github.com/justin13888/Sunrise/issues/342) Fixed, flexible and recurring blocks, with the ordinal `BYDAY` fix ([#928](https://github.com/justin13888/Sunrise/issues/928)).
6. [#339](https://github.com/justin13888/Sunrise/issues/339) Places and on-device geofencing.
7. [#788](https://github.com/justin13888/Sunrise/issues/788) A stream's `paused_until` holds a civil time, behind a `stream.civil_times` feature.

## P3 — macOS complete

1. [#924](https://github.com/justin13888/Sunrise/issues/924) A calendar that beats dedicated calendar apps on read and write ([ADR-0071](./11-adr/0071-calendar-read-write-through-synced-intents.md), [goals](./09-integrations/calendar-goals.md)):
   - Groundwork: provider behaviour tested ([#925](https://github.com/justin13888/Sunrise/issues/925)), a hands-on comparison of calendar apps ([#926](https://github.com/justin13888/Sunrise/issues/926)), registered OAuth clients ([#927](https://github.com/justin13888/Sunrise/issues/927)), and an `ExternalEvent` entity ([#929](https://github.com/justin13888/Sunrise/issues/929)).
   - Reading: Google Calendar ([#930](https://github.com/justin13888/Sunrise/issues/930)) and public calendars by URL ([#931](https://github.com/justin13888/Sunrise/issues/931)).
   - Writing: the edit path to a provider ([#932](https://github.com/justin13888/Sunrise/issues/932)), the conflict UI ([#933](https://github.com/justin13888/Sunrise/issues/933)), Google ([#934](https://github.com/justin13888/Sunrise/issues/934)), recurring-event scopes ([#935](https://github.com/justin13888/Sunrise/issues/935)), Microsoft ([#936](https://github.com/justin13888/Sunrise/issues/936)), and CalDAV ([#937](https://github.com/justin13888/Sunrise/issues/937)).
   - Beyond a calendar app: fast event creation ([#938](https://github.com/justin13888/Sunrise/issues/938)), time blocks shown as busy ([#939](https://github.com/justin13888/Sunrise/issues/939)), time-to-leave warnings ([#940](https://github.com/justin13888/Sunrise/issues/940)), other people's free time ([#942](https://github.com/justin13888/Sunrise/issues/942)), freshness on a closed phone ([#943](https://github.com/justin13888/Sunrise/issues/943)), and encrypted scheduling links ([#945](https://github.com/justin13888/Sunrise/issues/945)).
2. [#489](https://github.com/justin13888/Sunrise/issues/489) The Apple settings move out of `UserDefaults` into the core's `Preferences`.
3. [#458](https://github.com/justin13888/Sunrise/issues/458) A UI test restores an account from its recovery code on macOS and iOS against a seeded relay.
4. [#343](https://github.com/justin13888/Sunrise/issues/343) Planner solver and `plan_preview` ([ADR-0048](./11-adr/0048-interactive-planner.md)).
5. [#344](https://github.com/justin13888/Sunrise/issues/344) Drag with a live preview, on the calendar and in lists.
6. [#345](https://github.com/justin13888/Sunrise/issues/345) Search overhaul ([ADR-0052](./11-adr/0052-search-v2.md)).
7. [#347](https://github.com/justin13888/Sunrise/issues/347) The notification catalogue, including wind-down ([#301](https://github.com/justin13888/Sunrise/issues/301)).
8. [#351](https://github.com/justin13888/Sunrise/issues/351) Upcoming view.
9. [#4](https://github.com/justin13888/Sunrise/issues/4) Calendar reads from Microsoft Graph and CalDAV, authorised per device ([ADR-0071](./11-adr/0071-calendar-read-write-through-synced-intents.md)).
10. [#350](https://github.com/justin13888/Sunrise/issues/350) Full decrypted export.
11. [#352](https://github.com/justin13888/Sunrise/issues/352) Context shift and travel.
12. [#465](https://github.com/justin13888/Sunrise/issues/465) Pairing by a typed six-digit code, for a device with no camera.
13. [#353](https://github.com/justin13888/Sunrise/issues/353) Accessibility audit.
14. [#373](https://github.com/justin13888/Sunrise/issues/373) macOS and iOS screenshots at the top of the README.
15. [#13](https://github.com/justin13888/Sunrise/issues/13) Delight and motivation.

## P4 — Server production

Runs alongside P2 and P3. [`06-server/metrics.md`](./06-server/metrics.md) is
the metric catalogue.

1. [#474](https://github.com/justin13888/Sunrise/issues/474) Cold-start and RSS benches, a same-runner A/B bench gate, and runner-recorded baselines for every key.
2. [#481](https://github.com/justin13888/Sunrise/issues/481) Several relay nodes over Postgres: shared metadata and sessions, `LISTEN`/`NOTIFY` fan-out, a shared limiter and a maintenance lock.
3. [#482](https://github.com/justin13888/Sunrise/issues/482) Relay blobs on an S3-compatible object store, tested against MinIO.

## P5 — Phone parity

1. [#376](https://github.com/justin13888/Sunrise/issues/376) The rest of the widgets: Lock Screen capture, the Stream tile, and macOS reach measured on a signed build. Next Up has shipped.
2. [#377](https://github.com/justin13888/Sunrise/issues/377) The iOS widget extension signed with its own provisioning profile and App Group.
3. [#507](https://github.com/justin13888/Sunrise/issues/507) `mise run apple-app` on the merged iOS system surfaces.
4. [#941](https://github.com/justin13888/Sunrise/issues/941) The calendar on the menu bar, widgets, Lock Screen and watch.

## P6 — More platforms

1. [#944](https://github.com/justin13888/Sunrise/issues/944) A calendar in the web client.
2. [#133](https://github.com/justin13888/Sunrise/issues/133) Sharing streams with other people.
