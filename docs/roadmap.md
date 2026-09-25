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

1. Open pull requests: [#318](https://github.com/justin13888/Sunrise/pull/318), [#317](https://github.com/justin13888/Sunrise/pull/317), [#306](https://github.com/justin13888/Sunrise/pull/306).
2. Revocation soundness: [#252](https://github.com/justin13888/Sunrise/issues/252), [#248](https://github.com/justin13888/Sunrise/issues/248), [#280](https://github.com/justin13888/Sunrise/issues/280), [#282](https://github.com/justin13888/Sunrise/issues/282), [#257](https://github.com/justin13888/Sunrise/issues/257), [#241](https://github.com/justin13888/Sunrise/issues/241), [#315](https://github.com/justin13888/Sunrise/issues/315).
3. Apple session and Keychain: [#313](https://github.com/justin13888/Sunrise/issues/313), [#307](https://github.com/justin13888/Sunrise/issues/307), [#276](https://github.com/justin13888/Sunrise/issues/276), [#284](https://github.com/justin13888/Sunrise/issues/284), [#258](https://github.com/justin13888/Sunrise/issues/258), [#254](https://github.com/justin13888/Sunrise/issues/254), [#247](https://github.com/justin13888/Sunrise/issues/247), [#183](https://github.com/justin13888/Sunrise/issues/183).
4. Sync transport: [#269](https://github.com/justin13888/Sunrise/issues/269), [#273](https://github.com/justin13888/Sunrise/issues/273), [#296](https://github.com/justin13888/Sunrise/issues/296).
5. Gates and documentation truth: [#304](https://github.com/justin13888/Sunrise/issues/304), [#271](https://github.com/justin13888/Sunrise/issues/271), [#272](https://github.com/justin13888/Sunrise/issues/272), [#270](https://github.com/justin13888/Sunrise/issues/270), [#294](https://github.com/justin13888/Sunrise/issues/294), [#298](https://github.com/justin13888/Sunrise/issues/298), [#295](https://github.com/justin13888/Sunrise/issues/295),
   [#292](https://github.com/justin13888/Sunrise/issues/292), [#290](https://github.com/justin13888/Sunrise/issues/290), [#256](https://github.com/justin13888/Sunrise/issues/256), [#285](https://github.com/justin13888/Sunrise/issues/285), [#275](https://github.com/justin13888/Sunrise/issues/275), [#251](https://github.com/justin13888/Sunrise/issues/251), [#245](https://github.com/justin13888/Sunrise/issues/245), [#287](https://github.com/justin13888/Sunrise/issues/287), [#300](https://github.com/justin13888/Sunrise/issues/300), [#299](https://github.com/justin13888/Sunrise/issues/299), [#297](https://github.com/justin13888/Sunrise/issues/297), [#288](https://github.com/justin13888/Sunrise/issues/288),
   [#265](https://github.com/justin13888/Sunrise/issues/265), [#60](https://github.com/justin13888/Sunrise/issues/60).

## P1 — Commit tree and schema

The design of record is [ADR-0043](./11-adr/0043-commit-tree.md) (proposed),
[ADR-0044](./11-adr/0044-per-field-ops.md) and
[ADR-0045](./11-adr/0045-schema-identity-and-feature-gating.md), with the rules
in [`02-domain/schema-versioning.md`](./02-domain/schema-versioning.md) and
[`04-storage/migrations.md`](./04-storage/migrations.md).

1. [#328](https://github.com/justin13888/Sunrise/issues/328) Entity registry and generated DTOs, so the rest of this phase lands in one place.
2. [#320](https://github.com/justin13888/Sunrise/issues/320) Park unknown op kinds and replay them after an upgrade.
3. [#321](https://github.com/justin13888/Sunrise/issues/321) Lossless enums.
4. [#322](https://github.com/justin13888/Sunrise/issues/322) Unknown fields at every nesting level; forward-compatible `SunriseTime`.
5. [#329](https://github.com/justin13888/Sunrise/issues/329) Envelope container floor tolerance.
6. [#370](https://github.com/justin13888/Sunrise/issues/370) Sync request bodies accept unknown fields, so negotiation can grow.
7. [#327](https://github.com/justin13888/Sunrise/issues/327) Migration rigor: fixtures × migrations in CI, a backup before migrating, `quick_check`, rebuild from the op log.
8. [#323](https://github.com/justin13888/Sunrise/issues/323) Schema fingerprint and registry.
9. [#324](https://github.com/justin13888/Sunrise/issues/324) `vault_requires` and the read-only experience for an older client.
10. [#319](https://github.com/justin13888/Sunrise/issues/319) Per-field ops and CRDT field types.
11. [#326](https://github.com/justin13888/Sunrise/issues/326) A cross-version merge harness that asserts nothing is lost.
12. [#325](https://github.com/justin13888/Sunrise/issues/325) Hash-chained ops, causal heads and a state digest.
13. [#330](https://github.com/justin13888/Sunrise/issues/330) Compaction and snapshots.

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

1. [#336](https://github.com/justin13888/Sunrise/issues/336) Time correctness: every stored time is a `SunriseTime`, and no comparison goes through UTC-as-civil.
2. [#337](https://github.com/justin13888/Sunrise/issues/337) The `Preferences` entity; the week starts on Sunday by default.
3. [#332](https://github.com/justin13888/Sunrise/issues/332) Optional stream with a private key domain; Inbox becomes a view.
4. [#338](https://github.com/justin13888/Sunrise/issues/338) Day schedule: wake and sleep times.
5. [#334](https://github.com/justin13888/Sunrise/issues/334) Planned, target and hard deadlines, plus the lateness engine.
6. [#335](https://github.com/justin13888/Sunrise/issues/335) Triage queue and bulk actions.
7. [#331](https://github.com/justin13888/Sunrise/issues/331) Recurrence overhaul.
8. [#333](https://github.com/justin13888/Sunrise/issues/333) Dependency cycles resolved at merge; a requirements model.
9. [#340](https://github.com/justin13888/Sunrise/issues/340) Manual ordering as a fractional index in Rust.
10. [#341](https://github.com/justin13888/Sunrise/issues/341) `SavedView` as a synced entity.
11. [#342](https://github.com/justin13888/Sunrise/issues/342) Fixed, flexible and recurring blocks.
12. [#339](https://github.com/justin13888/Sunrise/issues/339) Places and on-device geofencing.

## P3 — macOS complete

1. [#349](https://github.com/justin13888/Sunrise/issues/349) Restore from a recovery code. A lost device must not mean lost data.
2. [#343](https://github.com/justin13888/Sunrise/issues/343) Planner solver and `plan_preview` ([ADR-0048](./11-adr/0048-interactive-planner.md)).
3. [#344](https://github.com/justin13888/Sunrise/issues/344) Drag with a live preview, on the calendar and in lists.
4. [#345](https://github.com/justin13888/Sunrise/issues/345) Search overhaul ([ADR-0052](./11-adr/0052-search-v2.md)).
5. [#347](https://github.com/justin13888/Sunrise/issues/347) The notification catalogue, including wind-down ([#301](https://github.com/justin13888/Sunrise/issues/301)).
6. [#348](https://github.com/justin13888/Sunrise/issues/348) Shortcut hints everywhere.
7. [#351](https://github.com/justin13888/Sunrise/issues/351) Upcoming view.
8. [#4](https://github.com/justin13888/Sunrise/issues/4) Read-only calendar sync from Google, Microsoft and CalDAV, authorised per device ([ADR-0049](./11-adr/0049-calendar-integrations-per-device-oauth.md)).
9. [#346](https://github.com/justin13888/Sunrise/issues/346) Attachments: thumbnails, native previews, cache ([ADR-0053](./11-adr/0053-attachment-thumbnails-and-native-rendering.md)).
10. [#350](https://github.com/justin13888/Sunrise/issues/350) Full decrypted export.
11. [#352](https://github.com/justin13888/Sunrise/issues/352) Context shift and travel.
12. [#354](https://github.com/justin13888/Sunrise/issues/354) Camera QR pairing and the relay rendezvous.
13. [#353](https://github.com/justin13888/Sunrise/issues/353) Accessibility audit.
14. [#13](https://github.com/justin13888/Sunrise/issues/13) Delight and motivation.

## P4 — Server production

Runs alongside P2 and P3. [`06-server/metrics.md`](./06-server/metrics.md) is
the metric catalogue.

1. [#355](https://github.com/justin13888/Sunrise/issues/355) Audit the full API and set a per-endpoint rate-limit policy, including the reverse-proxy checks.
2. [#357](https://github.com/justin13888/Sunrise/issues/357) Graceful shutdown, readiness, and a container `HEALTHCHECK`.
3. [#358](https://github.com/justin13888/Sunrise/issues/358) Server migrations and SQLite pragmas.
4. [#356](https://github.com/justin13888/Sunrise/issues/356) The metric catalogue.
5. [#366](https://github.com/justin13888/Sunrise/issues/366) End-to-end sync p99 < 500 ms, measured; bench baseline fixes.
6. [#363](https://github.com/justin13888/Sunrise/issues/363) Device signatures required by default.
7. [#359](https://github.com/justin13888/Sunrise/issues/359) Account deletion, blob GC, an admin CLI.
8. [#360](https://github.com/justin13888/Sunrise/issues/360) Encryption at rest and online backup.
9. [#361](https://github.com/justin13888/Sunrise/issues/361) OpenTelemetry tracing.
10. [#362](https://github.com/justin13888/Sunrise/issues/362) APNs delivery.
11. [#365](https://github.com/justin13888/Sunrise/issues/365) Crypto assurance: an external audit, a constant-time lint, fuzzing on PRs.
12. [#364](https://github.com/justin13888/Sunrise/issues/364) Scale-out: a storage trait, Postgres, S3, cross-node fan-out.

## P5 — Phone parity

1. [#367](https://github.com/justin13888/Sunrise/issues/367) Background sync on iOS: `BGAppRefreshTask` and silent push.
2. [#376](https://github.com/justin13888/Sunrise/issues/376) The rest of the widgets: Lock Screen capture, the Stream tile, and macOS reach measured on a signed build. ([#14](https://github.com/justin13888/Sunrise/issues/14) shipped Next Up.)
3. [#368](https://github.com/justin13888/Sunrise/issues/368) iOS system surfaces.
4. [#12](https://github.com/justin13888/Sunrise/issues/12) Internationalisation.

## P6 — More platforms

1. [#369](https://github.com/justin13888/Sunrise/issues/369) Portability CI: Windows and Linux builds, and a Kotlin UniFFI smoke test.
2. [#52](https://github.com/justin13888/Sunrise/issues/52) The WASM web core.
3. [#11](https://github.com/justin13888/Sunrise/issues/11) Deploy the web client.
4. [#133](https://github.com/justin13888/Sunrise/issues/133) Sharing streams with other people.
