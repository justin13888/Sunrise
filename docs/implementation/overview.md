---
status: living
---

# Implementation Overview

This file tracks the state of the v1 implementation against `docs/`.

**How to read this file.** An earlier revision reported most phases as
"✅ shipped" on the basis that the crate existed and its own tests passed. That
is a crate-existence checklist, not a feature-completeness one, and it
overstated the product substantially — several "shipped" crates are not
reachable from any binary. The table below reports *reachability from a running
client*, which is the only measure that matters to a user.

**Last verified on `v1-rewrite`, after the final wave of client work.** Every
row was re-checked against the source, and every number in
[Test suite](#test-suite) is measured rather than remembered. The companion
document is the per-capability
[status audit](../07-clients/parity-matrix.md#v1-status-audit), which grades the
v1 MUSTs the same way; this file grades crates, that one grades capabilities,
and a crate can be reachable while a capability inside it is not.

## Legend

| Mark | Meaning |
|---|---|
| ✅ **live** | Implemented, reachable from a shipping binary, and covered by tests that assert behaviour |
| 🟨 **partial** | Reachable, but a documented part of its spec is missing |
| 🟧 **orphan** | Crate builds and self-tests pass, but **nothing depends on it** — no product path reaches this code. Test and benchmark harnesses (`sunrise-e2e`, `sunrise-bench`) are exempt: having no dependents is their correct shape, since they exercise other crates rather than being consumed |
| 🟥 **broken** | Does not build, or does not work when run |
| ⬜ **deferred** | Deliberately out of scope for v1, with a recorded decision |

## Crate status

| Crate / Component | Status | Notes |
|---|---|---|
| Workspace + CI | ✅ live | Cargo + Bun workspace, 21 crates; `legacy/` archived and excluded. CI runs the Rust gates, a `macos-app` job on `macos-26`, the reachability gate (`.github/scripts/orphan-crate-gate.py`), and a nightly bench comparison; `release.yml` publishes a tag-driven GitHub Release and a GHCR image ([#17](https://github.com/justin13888/Sunrise/issues/17)) |
| `sunrise-id` | ✅ live | ULID + `EntityRef`, all twelve prefixes (`fcs_` for focus sessions and `rvw_` for review snapshots), client-side generation |
| `sunrise-error` | ✅ live | Error registry, `Recoverability`. TS mirror (`packages/sunrise-error-ts`) does not exist |
| `sunrise-cbor` | ✅ live | Canonical CBOR, magic prefixes |
| `sunrise-crypto` | ✅ live | Ed25519 / X25519 / XChaCha20-Poly1305 / BLAKE3 / Argon2id; byte-exact `OpEnvelope` |
| `sunrise-crypto-test-vectors` | ✅ live | Dependency-free frozen literals — identity-id, BLAKE3 KDF, stream Merkle roots, and byte-exact `aead_alg=0`/`aead_alg=1` envelope encodings — asserted by `sunrise-crypto/tests/frozen_vectors.rs`, which dev-depends on it |
| `sunrise-domain` | 🟨 partial | Task / Stream / Routine / Context / FocusSession / ReviewSnapshot are complete, as are the capture parser, dependency graph, scheduling constraints, streaks, review/stats folds, export, the `note_body` block-grammar codec, the `notify` reminder planner, `import`'s stable `(source, uid)` → Block id hash, and the `sort_order` base-26 fractional index that gives `Stream.sort_order` its arithmetic. `Block` (6 commands, 3 op kinds, 4 queries) and `Attachment` (2 commands, 2 op kinds, `Query::TaskAttachments`) have full command paths: Block is now reachable from **both** clients (macOS calendar grid; CLI via `ical import` / `ical export`), Attachment from macOS only. `Note` and `Person` are the two entities that genuinely have no path: a struct and a dead table, with nothing in between |
| `sunrise-storage` | 🟨 partial | Schema, op log and FTS5 are solid. `BlobStore` has two external consumers (`sunrise-core::attach`, `sunrise-server::routes::blobs`), so it is reachable from the macOS client. **2** tables are never written — `notes` and `persons`, matching the two entities with no command path. The migration list is a baseline plus an append, and `STORAGE_V` is now **14**: `0013_baseline.sql` per [ADR-0018](../11-adr/0018-storage-baseline-reset.md), then `0014_stream_sort_order.sql`, the first migration appended after that reset and the one that gives `streams` a real `sort_order` column instead of a synthesized `"a0"`. It backfills in the order the sidebar was already displaying (`name COLLATE NOCASE, stream_id`), so no existing vault rearranges itself on upgrade. `BASELINE_STORAGE_V` stays **13**: `db.rs` refuses any vault stamped below it with a typed `STORAGE_V_PRE_BASELINE`, and `refuses_every_pre_baseline_version` asserts that for every version below it |
| `sunrise-wire-protocol` | ✅ live | 11-byte frame, 15 msg kinds, `Hello`/`HelloAck`, capability negotiation. zstd is implemented but never enabled at any call site |
| `sunrise-sync` | ✅ live | `SyncState`, `Backoff`, the `Transport` trait, and `WsTransport`. The dead `Outbox` / `Cursor` / `CursorMap` / `SyncStateMachine` exports were deleted — the live implementations are `sunrise_storage::Outbox` and `sunrise-core::sync_driver` |
| `sunrise-log` | ✅ live | No longer a logger: `tracing` + `tracing-subscriber` carry the transport ([ADR-0010](../11-adr/0010-logging-strategy.md), amended) and this crate is the `Plain<T>` wrapper, the `RedactionLayer` field-name veto, the `ev` catalogue check, and subscriber assembly. Both binaries initialise it first thing; `sunrise-server`, `-storage`, `-core`, `-cli` emit against the catalogue. The `ring`/`remote` sinks and the `(ev, lv)` throttle were deleted rather than left as an unimplemented interface |
| `sunrise-pairing` | ✅ live | Full `Noise_XX_25519_ChaChaPoly_SHA256` handshake, SAS confirmation, and the encrypted channel the existing device uses to hand a new one its vault root. `Core::export_vault_root_for_pairing` is the (deliberately conspicuous) counterpart. Proven by `sunrise-e2e/tests/paired_devices_converge.rs`, which contains **no shared key constant** — B learns the root only across the channel |
| `sunrise-onboarding` | 🟨 partial | BIP-39 derivation is absent; `account.rs` has no tests |
| `sunrise-auth` | ✅ live | Client-side OIDC relying party: discovery, PKCE, a loopback redirect listener, token exchange and refresh, and credential storage. Consumed by `sunrise-cli` (`login` / `logout` / `whoami`) and by `sunrise-core-bindings`, so it reaches the macOS app. 35 tests |
| `sunrise-core` | 🟨 partial | Open / submit / query / changes / sync_status / close all work. Implements **8** entities behind **21** op kinds (`InnerOp`), exposed as **30** commands and **29** queries. Every command kind and every query is reachable across the UniFFI seam; `sunrise-cli` reaches ten commands and thirteen queries — enough that `Query::StreamTasks` and `Query::ContextTasks`, which no binary issued a cycle ago, now have a caller with no UI behind it. `SystemClock::timezone()` now resolves the device's real IANA zone (see [Fixed this cycle](#fixed-this-cycle)) |
| `sunrise-server` | ✅ live | Relay fanout, cursor-scoped replay backed by a durable SQLite relay log, metrics, OIDC JWKS verification, `X-Sunrise-Device-Sig` binding, and SQLite-backed accounts/devices are real. `/sync` authenticates at the upgrade and scopes fanout to the verified subject. Blob 2PC is **implemented**, not a stub: `init` / `PUT :upload_id/:chunk_idx` / `finalize` / `GET :blob_id` are all mounted, content-addressed and hash-verified on finalize, with a round-trip test. Its auth is stricter than `/sync`'s — bearer plus account plus device binding ([#22](https://github.com/justin13888/Sunrise/issues/22) is closed by this) |
| `sunrise-integrations` | 🟨 partial | **No longer an orphan.** The iCal half is live and dual-consumed: `ical` (RFC 5545 syntax) → `ical_map` (domain mapping) → `ical_vault` (the vault driver), reached by `sunrise-cli`'s `ical import` / `ical export` and, across the seam's `import_ical` / `export_ical`, by the macOS File menu — so both shipping clients reach it, which was not true a cycle ago. Imports are idempotent because the Block id *is* a hash of `(source, uid)`. The subset is narrow and **reports rather than drops**: `VTODO`, `VALARM`, `VTIMEZONE`, `VJOURNAL`, `VFREEBUSY`, `RDATE`/`EXDATE`/`RECURRENCE-ID`, `ATTACH`, `ATTENDEE` and any `X-` property each raise an `ICalNotice`. `RRULE`, `DESCRIPTION` and `LOCATION` parse and are then reported at the domain boundary, because `Block` has no field for them — so **a recurring event imports as a single occurrence**, and an exported `.ics` carries only `UID`, `SUMMARY`, `DTSTART`, `DTEND`. The GCal half is **implemented, tested and unconsumed**, deferred to [#4](https://github.com/justin13888/Sunrise/issues/4) by [ADR-0020](../11-adr/0020-v1-must-demotions.md): PKCE exchange/refresh with the durable-refresh-token rule and change detection that suppresses phantom deletes, all with injected transport, but nothing has run against the live API (needs a Google OAuth client ID) and there is no `impl EventSyncer` anywhere. `IntegrationProvider` still has **no implementor** — not even the live iCal path uses it, so the crate's own claim that integrations "run through" it is not true today |
| `sunrise-cli` | ✅ live | The `sunrise` binary: **twenty-three** one-shot subcommands — `capture`, `edit`, `defer`, `done`, `drop`, `today`, `inbox`, `next`, `search`, `streams` (incl. `streams move`), `stream`, `contexts`, `context`, `routines`, `review`, `export`, `ical` (`import` / `export`), `vaults`, `login`, `logout`, `whoami`, `focus`, `sync --once` — plus the env-driven live-sync wiring. Six landed this cycle (`edit`, `defer`, `drop`, `stream`, `context`, `vaults`) and they are what closed the CLI's three partial MUSTs; **all nine are now met**, see the [status audit](../07-clients/parity-matrix.md#v1-status-audit). It submits ten of the core's thirty commands: `CreateTask`, `UpdateTask`, `PromoteToStream`, `DeferTask`, `CompleteTask`, `DeleteTask`, `UpdateStream`, `StartFocus`, `ImportBlock`, and `TrustDevice` from the startup path. Arg parsing is hand-rolled, not clap. This is the reachability story for the core with no UI at all — `tests/cli.rs` drives the real binary against a real vault in a separate process |
| `sunrise-client-core` | ✅ live | Client-side but UI-free: undo/redo by inverse command over an `EntityLookup`, and saved views with their TOML-subset parser |
| `sunrise-core-bindings` | ✅ live | The UniFFI seam ([ADR-0019](../11-adr/0019-swiftui-macos-client.md)): an opaque async `SunriseCore`, all 30 commands, all 29 queries and their results, and a `ChangeListener` change stream with the mandatory `on_lagged` resync, fanned out to every subscriber. **Re-graded from 🟨 this cycle.** The partial mark was for one stated reason — `import_ical` / `export_ical` had no Swift caller — and `apps/apple` now calls both, so the mark was re-derived rather than inherited. Sweeping every exported symbol for a Swift caller leaves a much smaller residue: `parse_saved_view` has none at all, and `energy_fit_label` is reached only from the test target. Neither carries a parity MUST — the *Saved searches / views* MUST is met through `SavedViews.load` / `.save` — so they are unconsumed surface rather than an unreachable requirement, which is the distinction the 🟨 mark is for |
| `sunrise-bench` | ✅ live | Criterion suite + linux-x86_64 baselines. `baseline --check` compares against them and annotates regressions; it runs nightly and **does not gate** — on shared runners the same binary reports ±100% against its own baseline from noise alone |
| `sunrise-e2e` | ✅ live | Flagship two-Core relay convergence + four chaos scenarios, plus blocker, context and focus-session convergence |
| `apps/apple` | 🟨 partial | The SwiftUI client over the UniFFI seam ([ADR-0019](../11-adr/0019-swiftui-macos-client.md)): ~17.8k lines of app source, **471 Swift Testing cases in 75 suites**, plus 4 XCTest UI tests. Built by XcodeGen from `project.yml`, linking the generated xcframework. **Built in CI** — a `macos-app` job on `macos-26` runs `just macos-app` (xcframework → xcodegen → `swiftlint --strict` → `xcodebuild test`) on every push and PR. Landed this cycle: the iCal import/export surface with its grouped notice report, print and PDF export, the drag-and-drop gaps, sidebar stream reorder through the core, and a routine timer that actually starts. **Every one of the 23 macOS MUSTs is now met** — the iCal row was the last unmet one. Still partial, and for reasons that are about its *spec* rather than about a MUST: [`desktop.md`](../07-clients/desktop.md) specifies a detached always-on-top focus window, Spotlight indexing of task titles, Continuity Camera and Sparkle updates, none of which exist; there is no camera QR scanner; and the UI test target is `skipped: true` in the scheme, so CI proves the models behave but never proves a click reaches the core |
| `apps/web` | ⬜ deferred | localStorage stub per [ADR-0012](../11-adr/0012-web-wasm-deferred.md) |
| `packages/sunrise-ui` | 🟨 partial | A 40-line token file, not a component library. Its **one** consumer (`apps/web`, itself deferred) imports only `taskStateGlyph` and hardcodes colours. It had two until the Tauri shell was removed; the macOS app is Swift and does not consume it, so no shipping client does |

### Entities without a command path

`Note` and `Person` are declared in `sunrise-domain`, have `not_` / `prs_` id
prefixes and `notes` / `persons` tables in the baseline schema, and have no
command, no op kind and no query. Nothing can write them; the two tables are
the only ones in the schema with no writer.

That collision with `docs/07-clients/parity-matrix.md` is now **resolved**, and
by a decision rather than an edit. [ADR-0020](../11-adr/0020-v1-must-demotions.md)
demoted the two sharing rows to *deferred* — `Person` is the entity that design
operates on, and there is a complete spec with sound primitives underneath it
and nothing in between — and split the **Notes (rich text)** row, which stays a
MUST and is met: it means a Task's `body`, not the free-standing `Note`. Both
tables stay in the frozen baseline schema and stay unwritten, on purpose.

## Reachability gate

`.github/scripts/orphan-crate-gate.py` enforces the criterion this file is
built on, and it is **clean**: roots `sunrise-cli`, `sunrise-core-bindings` and
`sunrise-server`; **18 of 21 crates reachable**; the three that are not are the
permanently exempt harnesses (`sunrise-bench`, `sunrise-e2e`,
`sunrise-crypto-test-vectors`), whose correct shape is to have no dependents.

**The QUARANTINE list is now empty.** Its only entry was `sunrise-integrations`,
parked against issue #4; wiring iCal into both clients lifted it out. The gate's
staleness check is what forced that — an entry cannot outlive its fix, because
a quarantined crate that becomes reachable fails the run.

A caveat the gate cannot express, and it is the reason this file exists beside
it: the gate proves a crate is reachable from a *binary*, not that each
capability inside it is reachable from a *user*. `import_ical` was the worked
example for a full cycle — a correct, tested seam method in a crate the gate
called reachable, with no macOS caller — and closing it did not change a single
thing the gate reports. `parse_saved_view` is the same shape today, at a much
smaller scale. Crate-level reachability is the floor, not the ceiling.

## What genuinely works end to end

The sync path is the strongest thing in the repository, and none of it is faked:

- Real `TcpListener` + `axum::serve` running the production router; real WebSocket
  over real TCP via the production `WsTransport`; real `Hello`/`HelloAck`
  capability negotiation.
- Real Ed25519 signing and XChaCha20-Poly1305 sealing under BLAKE3-derived
  per-stream keys, with signature verification *before* decryption and a
  trusted-device-cert lookup. Untrusted-device, tampered-envelope, and
  wrong-key paths all have negative tests.
- Real SQLCipher vaults, real op log, real persistent outbox.
- `two_core_relay_convergence` covers live edits, offline catch-up, and an LWW
  conflict; `routine_materialization_convergence` proves double-materialisation
  collapses via deterministic occurrence ids. Convergence is asserted under a
  32-case proptest with reordering and duplication, and under four chaos
  scenarios (drop, corrupt, delay, partition).

Also solid: the RRULE DST golden vectors (including Lord Howe's 30-minute
offset), the pre-baseline migration *refusal* tests, and the FTS5 hostile-input
proptest. The migration list is two files deep now — `0013_baseline.sql` and the
appended `0014_stream_sort_order.sql` — so there is a one-step upgrade chain to
test as well as a refusal, and `current_storage_v()` is asserted equal to
`STORAGE_V` so the constant and the list cannot drift apart.

## Known defects

Tracked so they are not rediscovered as surprises. Each now has an issue.

**Fixed since this list was written:** the skewed-clock defect
([#21](https://github.com/justin13888/Sunrise/issues/21)) — `lww_wins` trusted
an unbounded `env.ts_ms`, so a fast device held a permanent veto over every
conflict it entered. The comparison key is now `(hlc, device_id, seq)` and ops
beyond a five-minute drift window are refused; see
[ADR-0016](../11-adr/0016-hlc-timestamps.md). The note here was right that
clamping against local time would have broken convergence — the fix stores the
SENDER's stamp, never the receiver's post-merge reading, for exactly that
reason.

Two more entries were removed from this list because re-checking them against
the code showed they no longer describe it. Both had outlived their fix:

- **Ring eviction is silent data loss**
  ([#19](https://github.com/justin13888/Sunrise/issues/19)) — fixed. Subscriber
  cursors now filter the replay instead of being discarded, a cursor past the
  ring is served from a durable SQLite relay log, and a cursor past *that*
  retention gets a typed `SYNC_CURSOR_GAP` rather than silence.
  `tests/ws_cursors.rs` pins all four cases, including survival across a relay
  restart and the "eviction of already-applied ops is not a gap" boundary.
- **Blob storage is a stub and unauthenticated**
  ([#22](https://github.com/justin13888/Sunrise/issues/22)) — fixed, and the
  entry was wrong on every count by the end. The chunk route is mounted, the
  2PC is content-addressed and hash-verified at `finalize`, and the routes
  require bearer *plus* account *plus* device binding, which is stricter than
  `/sync`. `Command::AttachFile` and `Query::TaskAttachments` both exist.

Similarly, **"auth is checked once, at the WebSocket upgrade"** was stale and
has been rewritten: the server also re-checks `exp` on every inbound frame,
enforces an idle deadline, and handles a mid-session `0x12 RefreshToken`
(`tests/ws_token_expiry.rs`). What remains true is narrower, and is the entry
below.

- **No in-session op retry**
  ([#20](https://github.com/justin13888/Sunrise/issues/20)). An unacked op waits
  for the session to end; the chaos tests script the reconnect the driver should
  perform itself, so they prove the *relay* can recover, not that the client does.
- **`/sync` does not bind the session to a device.** The upgrade verifies the
  bearer token and scopes fanout to the verified subject, but — unlike the blob
  routes — it does not additionally require `X-Sunrise-Device-Sig`, so any
  device holding a valid account token can subscribe as that account
  ([#7](https://github.com/justin13888/Sunrise/issues/7) covers the remaining
  auth work).
**Two more entries left this list this cycle, both closed in code rather than
re-worded:**

- **iCal import/export has no macOS caller** — fixed. It was never a defect in
  the code; both seam methods were correct and tested. It was a parity **MUST**
  no user could reach on the client that requires it, which is precisely the
  class of gap this file exists to surface. `apps/apple` now has the wrapper,
  the model and the two File menu items.
- **`CoreBridge.startRoutineTimer` has no caller** — fixed. It is now started
  from `RootView`'s vault lifecycle, keyed on the bridge's identity so a vault
  switch starts a new timer against the new `Core` rather than inheriting a
  timer bound to one that has been shut down. Until this landed, recurrence only
  advanced when `Core::open` materialized once at unlock or when somebody
  pressed "Generate now": a Mac left open across midnight showed yesterday's
  routines and no more.

**Removed from this list:** *"No delete-convergence coverage"* — fixed, and it
was concealing a real bug rather than merely being a gap. See below.

## Fixed this cycle

Recorded because each presented as something other than what it was:

- **`Stream.sort_order` was a field nothing could write.** It has been on the
  Rust `Stream` and in the domain spec from the beginning, but there was no
  column behind it: `read_stream` synthesized the literal `"a0"` for every row,
  so every stream carried an identical key and `ORDER BY sort_order` was a
  no-op that fell through to name order. It presented as *"the sidebar sorts
  alphabetically"* — a UI choice nobody made — rather than as a missing write
  path, which is why it survived so long. The fix is three pieces that had to
  land together: a base-26 fractional index in `sunrise-domain`, `STORAGE_V`
  **13 → 14** for the column and its backfill, and two callers (a macOS sidebar
  `.onMove` and `sunrise streams move`) so the field has a writer on both
  shipping clients. `DOC_SCHEMA_V` did **not** move: `sort_order` was already a
  required `tstr` on the wire carrying `"a0"`, so nothing was added to the
  payload — only a real value was put into a field that already existed.
  Under entity LWW two concurrent reorders still resolve to one arrangement
  rather than merging; the fractional index keeps a single reorder from
  rewriting every sibling, which is a different guarantee.
- **Every Mac claimed to be in UTC.** `SystemClock::timezone()` returned
  `"UTC"` on **all** of them. `/etc/localtime` on macOS points into a versioned
  tree — `/var/db/timezone/zoneinfo/America/Toronto`, itself a link under
  `/var/db/timezone/tz/<tzdb-version>/zoneinfo/…` — which is not one of the
  directories jiff strips a name from, so `iana_name()` was `None` and the
  fallback took over. The blast radius is the part worth keeping: the device
  zone decides which civil day `Query::DayBlocks` covers and which instants a
  Task's **scheduling constraints** evaluate against, while the client renders
  civil time in the zone the OS gives *it*. West of Greenwich the two disagree
  for the last hours of every evening, so the calendar grid silently returned
  nothing for blocks plainly on it — and every constraint evaluation in the
  product ran in the wrong zone. It presented as an empty calendar late in the
  day, which reads as a UI bug. Now resolved by reading the symlink and
  anchoring on the last `zoneinfo` component, stepping over `posix/` and
  `right/`, and returning a name only when the tzdb can actually load it.
- **The change feed had one seat, and four consumers.** `changes()` handed out a
  single subscription and cancelled the previous one, so the last subscriber to
  ask won and the other three were permanently dead — silently, because a dead
  listener looks exactly like a quiet vault. The task list, menu bar, reminder
  scheduler and sync status were all competing for one slot. Now fanned out, with
  `on_lagged` reaching every consumer rather than one.
- **Deletes diverged on four entity kinds, and the test that would have caught
  it was structurally unable to.** Every convergence test compared through a
  projection built on `StreamTasks`, which filters `deleted = 0` — correctly,
  because that is what a UI wants. The side effect is that "both replicas
  deleted it" and "one replica never heard of it" are indistinguishable, which
  are precisely the two outcomes a delete test exists to separate. Nothing
  proved a delete converged at all. Comparing tombstone-inclusive found
  `TaskDelete`, `StreamDelete`, `ContextDelete` and `RoutineDelete` all
  diverging: both replicas agree on `deleted` and disagree on the contested
  scalar forever. The measured rate was 1–2 in 12, which looks like flakiness
  and is not — divergence only occurs when the delete wins the LWW race, so a
  low rate is a property of the race, not evidence of a mild bug.
- **Undo of a create did nothing.** The inverse of a create is a delete, and
  there wasn't one; undo left the minted entity in place. Redo now re-creates,
  with a **new id** — documented rather than papered over, because no
  id-preserving create exists.
- **The activity feed invented transitions that never happened.** All four
  op-log folds ordered by `(ts_ms, op_id)`, and `op_id` is a ULID whose low
  bits are random — so two ops one device wrote in the same millisecond sorted
  arbitrarily. `fold_activity` is a state machine over successive full-state
  snapshots, so a defer-then-complete pair read backwards is classified against
  the wrong baseline and reports a **reopen** the user never performed, losing
  the third event as a no-change update. The same ordering feeds
  `WeeklyReview`'s counts. Now ordered by `(ts_ms, device_id, seq)` — the
  authoring device's own causal counter, which is what a state-machine fold
  needs. Found by wiring an activity feed against it, not by a test.
- **`x` did not toggle.** Documented as "toggle done", it only ever completed:
  pressing it on a finished task re-sent `CompleteTask`, which the core accepts
  as a no-op. There was no path anywhere in the client to re-open a task.
- **Esc quit the app.** In Normal mode Esc was an alias for quit, so one
  reflexive keypress tore the client down — including mid-session with unsynced
  work in the outbox.
- **The Focus pane's "unblocks N tasks" counted the wrong set.** It read
  `Task.blocks`, which is the time-**Block**s scheduling the task, not the tasks
  it releases. It claimed a payoff unrelated to the dependency graph.
- **The dependency graph had no writer.** `Query::Actionable`, the planner's
  leverage ranking and `Query::UnblockCascade` all read `blocked_by`, and no
  client could set it — so every vault's graph was empty, every planner row
  read `unblocks 0`, and every cascade was empty. The "ranked by leverage"
  queue had no leverage in it.
- **`blocked_by` is the set of dependencies, not of unmet ones.** It keeps its
  members after they finish, so a list row counting it marks a task blocked
  forever once anything ever blocked it — and the planner, which reads the
  *derived* count, then disagrees with the list beside it. Both badges now read
  `Query::Actionable`, which recomputes against the blockers' current states.

- **A crash bricked the vault.** `create_new` plus a `Drop`-only release, with
  `panic = "abort"` in the release profile. Now a real OS advisory lock, proven
  by tests that `SIGKILL` a child process holding it.
- **A device's own ops lost the LWW tie to each other.** The device-id memcmp is
  a *cross-device* rule; applied to one device's successive ops it evaluated
  `dev > dev` and discarded the later one on every remote replica. Reachable by
  creating a task and patching it in the same millisecond, and it surfaced as a
  1-in-6 "flaky" e2e rather than as data loss.
- **Three projections silently dropped data the ops carried**: `blocked_by`
  (blockers existed only in the op log, so every read returned an empty set) and
  `Stream.paused` / `paused_until` / `review_cadence` (hardcoded on read). Each
  presented as a *feature* being unimplemented, and each was only found by
  writing something that needed the field. Every materialised projection is a
  place data can vanish quietly.
- **Bearer tokens could reach the log.** `TraceLayer::new_for_http`'s stock span
  records the full URI, and `?access_token=` is the documented browser fallback
  for the sync socket.
- **Two CI gates were structurally unenforceable.** The determinism and
  log-redaction gates were shaped `if grep ...; then fail; fi`, which takes the
  else branch — printing OK — on *every* failure mode, including a missing
  binary. Both had been green for months while checking nothing.
- **Multi-device was impossible.** Every call site handed the Core a literal
  vault root and the e2e passed the *same* `[0x42; 32]` to both replicas.
  Encryption was real; key distribution was bypassed. `sunrise-pairing` now
  implements the Noise XX handshake, and the paired-device e2e contains no
  shared key constant.
- **`/sync` was unauthenticated and single-tenant**, hashing a fixed constant
  for the account, so any subscriber received every other subscriber's frames.

## Removed

- **Tauri desktop shell (`apps/desktop`).** Cut by decision. It never ran:
  Tauri was not a dependency, there was no `main.rs`, no `tauri.conf.json`, and
  no `#[tauri::command]` attribute, and the renderer called an IPC method that
  did not exist on the Rust side — an error the bridge swallowed, so it rendered
  an empty list forever. It also carried a second `Cargo.lock` that silently
  went stale whenever a workspace crate gained a dependency. Keeping ~160 lines
  of React that claimed to be a client was the same orphan problem ADR-0014
  removed elsewhere. Git history preserves it.
- **Ratatui TUI (`crates/sunrise-tui`).** Replaced by a native SwiftUI macOS
  app over the UniFFI seam ([ADR-0019](../11-adr/0019-swiftui-macos-client.md)).
  Everything in it that was not about drawing a terminal was rescued first,
  into `sunrise-domain` (recurrence phrasing, the annotate grammar, the routine
  projection, the shared vocabulary), `sunrise-client-core` (undo/redo, saved
  views) and `sunrise-cli` (the subcommands). `ratatui`, `crossterm`,
  `ratatui-image` and `image` left the workspace with it, and so did the
  RUSTSEC-2024-0436 advisory suppression they required.

## Deferred by decision

- **Web WASM core** — [ADR-0012](../11-adr/0012-web-wasm-deferred.md); MSRV
  blocker. `apps/web/src/wasm.ts` keeps the `loadCore()` seam for a later drop-in.
- **iOS / Android** — the UniFFI seam is built and generates Kotlin from the
  same scaffolding; only macOS slices have been produced and proven.
- **Stream sharing, Google Calendar, and the standalone `Note`** — the three
  capabilities [ADR-0020](../11-adr/0020-v1-must-demotions.md) removed from the
  v1 MUST set. GCal's provider is implemented and tested; what is deferred is
  the wiring, storage and UI around it ([#4](https://github.com/justin13888/Sunrise/issues/4)).
- **Apple Focus integration** — not wired.
- **Focus Mode's platform effects** — the session record, planner, calibration,
  chunking and unblock cascade are live in the core
  ([ADR-0013](../11-adr/0013-focus-session-op-representation.md)) and are now
  surfaced by the macOS `FocusView` (ranked picks, start/end, interruption
  logging, cascade). What is still absent is the *platform* half: nothing
  suppresses notifications, registers a Live Activity, dims other windows, or
  plays a cue. Per-Stream pomodoro overrides remain unimplemented.
  `timeboxed to my next Block` is now unblocked — `Block` gained its command
  path — but has not been built.
- **A detached always-on-top focus window**, Spotlight indexing of task titles,
  Continuity Camera, and Sparkle updates — all specified in
  [`../07-clients/desktop.md`](../07-clients/desktop.md) and none implemented.
  Print / PDF export used to sit in this list and no longer does: ⌘P and File →
  Export as PDF… now cover task lists, search, the calendar day and week grids,
  and the weekly and daily reviews. Review → Trends and Review → History are
  deliberately outside it — a chart and a list of links do not paginate into
  rows, and both already carry CSV/JSON export beside them.
- **Merge journal & per-field CRDT** — v1 conflict resolution is entity-level
  LWW, now the decided model per [ADR-0014](../11-adr/0014-entity-level-lww-merge.md),
  which supersedes ADR-0003. `crates/sunrise-crdt` and the `loro` dependency are
  deleted; the workspace contains no CRDT library.
- **CI gates** — the bench comparison is wired and runs nightly, but
  **informationally**: on shared runners the same binary reports swings over
  ±100% against its own baseline from scheduling noise alone, so `testing.md`'s
  >5% blocking gate needs dedicated hardware. `cargo-mutants` is not wired.
  `CODEOWNERS` now encodes the security-review gate, though GitHub only enforces
  it once branch protection requires code-owner review.
- **`cargo-fuzz` targets** — `testing.md` specifies six; `fuzz/` does not exist.

## Test suite

All figures below were **measured on `v1-rewrite`** after the final wave, not
carried over from an earlier revision.

| Gate | Result |
|---|---|
| `just rust-test` | **1335 passed**, 0 failed, 3 ignored |
| `cargo test -p sunrise-cli` | **77 passed** — 48 in `tests/`, 29 in-crate |
| `cargo test --workspace --doc` | 0 doc tests |
| `just macos-app` | **471 tests in 75 suites passed**; SwiftLint `--strict` clean; exit 0 |
| `just rust-fmt-check` | clean |
| `just rust-clippy` | clean (pedantic, `-D warnings`) |
| `cargo deny check` | clean |
| `just validate` | clean (no TS tests exist yet) |
| `just orphan-crates` | clean — 18/21 reachable, QUARANTINE empty |

The 3 ignored are the `#[ignore]`d child-process bodies the vault-lock crash
tests spawn; they are executed, as subprocesses, by the tests that `SIGKILL`
them. The macOS 471 does **not** include the 4 XCUITest cases, which are
`skipped: true` in the scheme and run only under `just macos-uitest`.

The `sunrise-cli` line is broken out because it is the one gate that proves the
core without a UI: `main.rs` itself has **no** unit tests, so every subcommand's
coverage is the black-box `tests/cli.rs` suite driving `CARGO_BIN_EXE_sunrise`
against a real vault and a real keystore in a separate process. The day that
stops covering the stack is the day the core's tests stop describing a usable
system.

The number is worth more than it used to be. Earlier revisions of this file
quoted a count that included ~50 tests over orphan crates no product path
reached, plus several that asserted nothing:

- `sunrise-crdt`'s tests went with the crate ([ADR-0014](../11-adr/0014-entity-level-lww-merge.md)).
- The dead `sunrise-sync` exports took 7 tests of unreachable code with them.
- `sunrise-crypto-test-vectors` had one test that called a pure function twice
  and compared the results; it is now 8 frozen vectors, verified by mutation —
  flipping the KDF context to `sunrise.identity_id.v2` fails the suite, which
  the old sentinel could not detect.
- The log-redaction proptest ran 1,000 cases asserting that a string it never
  logged was absent. It now drives a payload through all eight paths a
  `Plain<T>` can take and asserts the sink saw the redaction markers *as well
  as* zero payload bytes, so the negative assertion cannot pass vacuously.

Two flakes were found and fixed rather than retried: an order-dependent
assertion in the context convergence test (ULIDs minted microseconds apart sort
by their random suffix), and the blocker convergence test, which turned out to
be reporting a **real** same-device LWW data-loss bug rather than being flaky.

The same ULID-suffix ordering turned up a third time, and the third time it was
not a test problem at all: the activity fold read same-millisecond ops in
random order and reported transitions that never happened. Its regression test
pins the clock so the tie is *always* taken, over twelve independent tasks —
the old ordering passes with probability (1/6)^12 — and was confirmed failing
before the fix. A flake and a data bug look identical from the outside; the
difference is whether anyone reads the assertion.

Prefer the reachability column above as the signal.

```
just rust-test        # cargo test --workspace --all-targets
just rust-clippy      # pedantic, -D warnings
just rust-fmt-check   # formatting
just validate         # Biome CI + typecheck + coverage
just macos-app        # xcframework + swiftlint --strict + xcodebuild test
just orphan-crates    # every crate reachable from a shipping binary
cargo deny check      # advisories, bans, licences, sources
```

## Boots end-to-end

- `cargo run -p sunrise-server` — REST + `/sync` WebSocket relay on
  `127.0.0.1:8443`. Self-host mode installs the single-tenant `NullVerifier`,
  and the server now **refuses to bind a non-loopback address** while that is
  in use, since it maps every caller to one account. Configure an OIDC issuer
  for multi-user.
- `cargo run -p sunrise-cli -- <subcommand>` — the command-line client. Reads
  the vault directory from `SUNRISE_VAULT` (default `~/.sunrise/vault`) and
  unlocks with **that vault's own root**: 32 random bytes minted on first open
  and kept in the keystore (`SUNRISE_KEYSTORE`, default
  `$XDG_DATA_HOME/sunrise/keys`), one mode-0600 file per vault, deliberately not
  in the vault directory. `SUNRISE_VAULT_ROOT` overrides it with 64 hex
  characters and touches no keystore. `sunrise vaults` lists what this machine
  holds keys for. Every subcommand is one-shot, which is what lets
  `crates/sunrise-cli/tests/cli.rs` exercise the whole stack through the real
  binary.

> **Sync from the CLI:** setting `SUNRISE_SYNC_URL`
> (e.g. `ws://127.0.0.1:8443/sync`) starts the WebSocket sync driver;
> `SUNRISE_EXPORT_CERT_FILE` / `SUNRISE_TRUST_CERT_FILE` perform the dev
> two-file device-cert exchange (see the README's live sync demo).
> `sunrise sync --once` drains the outbox and exits, bounded. Unset, the CLI
> stays fully offline. The wiring is proven headlessly by
> `cargo test -p sunrise-cli --test live_sync` and, end to end, by
> `cargo test -p sunrise-e2e --test two_core_relay_convergence`.
