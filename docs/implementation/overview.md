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
| Workspace + CI | ✅ live | Cargo + Bun workspace, 23 crates; `legacy/` archived and excluded. CI runs the Rust gates, a `macos-app` job on `macos-26`, the reachability gate (`.github/scripts/orphan-crate-gate.py`), and a nightly bench comparison; `release.yml` publishes a tag-driven GitHub Release and a GHCR image ([#17](https://github.com/justin13888/Sunrise/issues/17)) |
| `sunrise-id` | ✅ live | ULID + `EntityRef`, all twelve prefixes (`fcs_` for focus sessions and `rvw_` for review snapshots), client-side generation |
| `sunrise-error` | ✅ live | Error registry, `Recoverability`. TS mirror (`packages/sunrise-error-ts`) does not exist |
| `sunrise-cbor` | ✅ live | Canonical CBOR, magic prefixes |
| `sunrise-crypto` | ✅ live | Ed25519 / X25519 / XChaCha20-Poly1305 / BLAKE3 / Argon2id; byte-exact `OpEnvelope` |
| `sunrise-crypto-test-vectors` | ✅ live | Dependency-free frozen literals — identity-id, BLAKE3 KDF, stream Merkle roots, and byte-exact `aead_alg=0`/`aead_alg=1` envelope encodings — asserted by `sunrise-crypto/tests/frozen_vectors.rs`, which dev-depends on it |
| `sunrise-domain` | 🟨 partial | Task / Stream / Routine / Context / FocusSession / ReviewSnapshot are complete, as are the capture parser, dependency graph, scheduling constraints, streaks, review/stats folds, export, the `note_body` block-grammar codec, the `notify` reminder planner, `import`'s stable `(source, uid)` → Block id hash, and the `sort_order` base-26 fractional index that gives `Stream.sort_order` its arithmetic. `Block` (6 commands, 3 op kinds, 4 queries) and `Attachment` (2 commands, 2 op kinds, `Query::TaskAttachments`) have full command paths: Block is now reachable from **both** clients (macOS calendar grid; CLI via `ical import` / `ical export`), Attachment from macOS only. `Note` and `Person` are the two entities that genuinely have no path: a struct and a dead table, with nothing in between |
| `sunrise-storage` | 🟨 partial | Schema, op log and FTS5 are solid. `BlobStore` has two external consumers (`sunrise-core::attach`, `sunrise-server::api::blobs`), so it is reachable from the macOS client. **2** tables are never written — `notes` and `persons`, matching the two entities with no command path. The migration list is a baseline plus three appends, and `STORAGE_V` is now **16**: `0013_baseline.sql` per [ADR-0018](../11-adr/0018-storage-baseline-reset.md), then `0014_stream_sort_order.sql`, the first migration appended after that reset and the one that gives `streams` a real `sort_order` column instead of a synthesized `"a0"`; then `0015_entity_extra_columns.sql`, which gives `streams`, `contexts`, `routines`, `focus_sessions` and `focus_session_ends` the `extra BLOB` that only `tasks`, `blocks` and `attachments` had, so forward-compat unknowns stop being dropped at the projection on five of the nine column-projected entities; then `0016_stream_description_and_default_context.sql`, which gives `Stream.description` and `Stream.default_context` the columns their CDDL always declared — `description` was accepted by the command surface, carried in the op, and then erased on every replica by the next update, because nothing could materialize it. It backfills in the order the sidebar was already displaying (`name COLLATE NOCASE, stream_id`), so no existing vault rearranges itself on upgrade. `BASELINE_STORAGE_V` stays **13**: `db.rs` refuses any vault stamped below it with a typed `STORAGE_V_PRE_BASELINE`, and `refuses_every_pre_baseline_version` asserts that for every version below it |
| `sunrise-wire-protocol` | ✅ live | 11-byte frame, 15 msg kinds, `Hello`/`HelloAck`, capability negotiation. zstd is implemented but never enabled at any call site |
| `sunrise-sync` | ✅ live | `SyncState`, `Backoff`, the `Transport` trait, and `SseTransport` (`src/sse.rs`, behind the `sse` feature) — the SSE-plus-typed-`POST` client [ADR-0023](../11-adr/0023-sse-sync-transport.md) put in the `WsTransport`'s place. The trait did not change, which is the point of it: the driver still hands this layer whole encoded wire frames and reads whole encoded frames back, so `sync_driver`, the outbox, the cursor bookkeeping and the backoff were untouched by the migration. The dead `Outbox` / `Cursor` / `CursorMap` / `SyncStateMachine` exports were deleted — the live implementations are `sunrise_storage::Outbox` and `sunrise-core::sync_driver` |
| `sunrise-log` | ✅ live | No longer a logger: `tracing` + `tracing-subscriber` carry the transport ([ADR-0010](../11-adr/0010-logging-strategy.md), amended) and this crate is the `Plain<T>` wrapper, the `RedactionLayer` field-name veto, the `ev` catalogue check, and subscriber assembly. Both binaries initialise it first thing; `sunrise-server`, `-storage`, `-core`, `-cli` emit against the catalogue. The `ring`/`remote` sinks and the `(ev, lv)` throttle were deleted rather than left as an unimplemented interface |
| `sunrise-pairing` | ✅ live | Full `Noise_XX_25519_ChaChaPoly_SHA256` handshake, SAS confirmation, and the encrypted channel the existing device uses to hand a new one its vault root. `Core::export_vault_root_for_pairing` is the (deliberately conspicuous) counterpart. Proven by `sunrise-e2e/tests/paired_devices_converge.rs`, which contains **no shared key constant** — B learns the root only across the channel |
| `sunrise-onboarding` | 🟨 partial | BIP-39 derivation is absent; `account.rs` has no tests. Its request shapes are no longer unconsumed: `sunrise-relay-client` carries them over the wire and `sunrise bootstrap` invokes it |
| `sunrise-auth` | ✅ live | Client-side OIDC relying party: discovery, PKCE, a loopback redirect listener, token exchange and refresh, and credential storage. Consumed by `sunrise-cli` (`login` / `logout` / `whoami`) and by `sunrise-core-bindings`, so it reaches the macOS app. 35 tests |
| `sunrise-core` | 🟨 partial | Open / submit / query / changes / sync_status / close all work. Implements **8** entities behind **21** op kinds (`InnerOp`), exposed as **30** commands and **29** queries. Every command kind and every query is reachable across the UniFFI seam; `sunrise-cli` reaches ten commands and thirteen queries — enough that `Query::StreamTasks` and `Query::ContextTasks`, which no binary issued a cycle ago, now have a caller with no UI behind it. `SystemClock::timezone()` now resolves the device's real IANA zone (see [Fixed this cycle](#fixed-this-cycle)) |
| `sunrise-server` | ✅ live | Every operation is served by `api/`, described by `schemas/generated/openapi.v1.json` and reachable — `routes/` and the `/sync` WebSocket are gone, and with them `axum` and `tokio-tungstenite` (`tower` and `tower-http` survive in the lock only as `reqwest` transitives, declared nowhere). Relay fanout, cursor-scoped replay backed by a durable SQLite relay log, metrics, OIDC JWKS verification, `X-Sunrise-Device-Sig` binding, and SQLite-backed accounts/devices are real. `/sync` is an SSE stream downstream and typed `POST`s upstream ([ADR-0023](../11-adr/0023-sse-sync-transport.md)): `POST /sync/session` negotiates, `GET /sync/events` fans out with `Last-Event-ID` resumption, and the stream ends when the token expires or the device is revoked, so a revocation reaches a stream already open. Fanout is scoped to the verified subject. `require_device_sig` is derived from the deployment — on wherever an OIDC issuer is configured, off for single-tenant self-host, which `validate` rejects the flag alongside anyway — and `/metrics` is mounted only on a loopback listener. Blob 2PC is **implemented**, not a stub: `init` / `PUT {upload_id}/{chunk_idx}` / `finalize` / `GET {blob_id}` are all mounted, content-addressed and hash-verified on finalize, with a round-trip test, and the fetch streams chunk-by-chunk rather than buffering the whole blob. Its auth is bearer plus account plus device binding, and since ADR-0023 that is the *same* auth `/sync` runs rather than a stricter one: every route on the surface goes through one of `api/signed.rs`'s four extractors ([#22](https://github.com/justin13888/Sunrise/issues/22) is closed by this) |
| `sunrise-integrations` | 🟨 partial | **No longer an orphan.** The iCal half is live and dual-consumed: `ical` (RFC 5545 syntax) → `ical_map` (domain mapping) → `ical_vault` (the vault driver), reached by `sunrise-cli`'s `ical import` / `ical export` and, across the seam's `import_ical` / `export_ical`, by the macOS File menu — so both shipping clients reach it, which was not true a cycle ago. Imports are idempotent because the Block id *is* a hash of `(source, uid)`. The subset is narrow and **reports rather than drops**: `VTODO`, `VALARM`, `VTIMEZONE`, `VJOURNAL`, `VFREEBUSY`, `RDATE`/`EXDATE`/`RECURRENCE-ID`, `ATTACH`, `ATTENDEE` and any `X-` property each raise an `ICalNotice`. `RRULE`, `DESCRIPTION` and `LOCATION` parse and are then reported at the domain boundary, because `Block` has no field for them — so **a recurring event imports as a single occurrence**, and an exported `.ics` carries only `UID`, `SUMMARY`, `DTSTART`, `DTEND`. The GCal half is **implemented, tested and unconsumed**, deferred to [#4](https://github.com/justin13888/Sunrise/issues/4) by [ADR-0020](../11-adr/0020-v1-must-demotions.md): PKCE exchange/refresh with the durable-refresh-token rule and change detection that suppresses phantom deletes, all with injected transport, but nothing has run against the live API (needs a Google OAuth client ID) and there is no `impl EventSyncer` anywhere. `IntegrationProvider` still has **no implementor** — not even the live iCal path uses it, so the crate's own claim that integrations "run through" it is not true today |
| `sunrise-cli` | ✅ live | The `sunrise` binary: **twenty-three** one-shot subcommands — `capture`, `edit`, `defer`, `done`, `drop`, `today`, `inbox`, `next`, `search`, `streams` (incl. `streams move`), `stream`, `contexts`, `context`, `routines`, `review`, `export`, `ical` (`import` / `export`), `vaults`, `login`, `logout`, `whoami`, `focus`, `sync --once` — plus the env-driven live-sync wiring. Six landed this cycle (`edit`, `defer`, `drop`, `stream`, `context`, `vaults`) and they are what closed the CLI's three partial MUSTs; **all nine are now met**, see the [status audit](../07-clients/parity-matrix.md#v1-status-audit). It submits nine of the core's commands: `CreateTask`, `UpdateTask`, `PromoteToStream`, `DeferTask`, `CompleteTask`, `DeleteTask`, `UpdateStream`, `StartFocus` and `ImportBlock`. Joining an account is not a command any more: `SUNRISE_PAIRING_FILE` is read *before* `Core::open`, because the identity a vault belongs to is fixed when the vault is created. Arg parsing is hand-rolled, not clap. This is the reachability story for the core with no UI at all — `tests/cli.rs` drives the real binary against a real vault in a separate process |
| `sunrise-client-core` | ✅ live | Client-side but UI-free: undo/redo by inverse command over an `EntityLookup`, and saved views with their TOML-subset parser |
| `sunrise-core-bindings` | ✅ live | The UniFFI seam ([ADR-0019](../11-adr/0019-swiftui-macos-client.md)): an opaque async `SunriseCore`, all 30 commands, all 29 queries and their results, and a `ChangeListener` change stream with the mandatory `on_lagged` resync, fanned out to every subscriber. **Re-graded from 🟨 this cycle.** The partial mark was for one stated reason — `import_ical` / `export_ical` had no Swift caller — and `apps/apple` now calls both, so the mark was re-derived rather than inherited. Sweeping every exported symbol for a Swift caller leaves a much smaller residue: `parse_saved_view` has none at all, and `energy_fit_label` is reached only from the test target. Neither carries a parity MUST — the *Saved searches / views* MUST is met through `SavedViews.load` / `.save` — so they are unconsumed surface rather than an unreachable requirement, which is the distinction the 🟨 mark is for |
| `sunrise-bench` | ✅ live | Criterion suite + linux-x86_64 baselines. `baseline --check` compares against them and annotates regressions; it runs nightly and **does not gate** — on shared runners the same binary reports ±100% against its own baseline from noise alone |
| `sunrise-e2e` | ✅ live | Flagship two-Core relay convergence + four chaos scenarios, plus blocker, context and focus-session convergence |
| `apps/apple` | 🟨 partial | The SwiftUI clients over the UniFFI seam ([ADR-0019](../11-adr/0019-swiftui-macos-client.md)): ~19.2k lines of app source across `Sunrise/` (shared, 17.3k), `macOS/` (1.3k) and `iOS/` (0.6k), **477 Swift Testing cases in 75 suites**, plus 4 XCTest UI tests on macOS and 5 on iOS. Built by XcodeGen from `project.yml`, linking the generated xcframework. **Both targets build in CI**, as two jobs on `macos-26` rather than one — a `macos-app` job running `mise run macos-app` (xcframework → xcodegen → `swiftlint --strict` → `xcodebuild test`), and an `ios-app` job running `mise run ios-app`, which builds the `SunriseiOS` product, compiles the same `SunriseTests/` sources against it a second time as `SunriseiOSTests`, which is what makes the shared half of the app answer for itself on both platforms, and — unlike the macOS job — runs its UI tests, `SunriseiOSUITests`, on the iPhone 17 Pro simulator. Landed this cycle: the iCal import/export surface with its grouped notice report, print and PDF export, the drag-and-drop gaps, sidebar stream reorder through the core, and a routine timer that actually starts. **Every one of the 23 macOS MUSTs is now met** — the iCal row was the last unmet one. Still partial, and for reasons that are about its *spec* rather than about a MUST: [`desktop.md`](../07-clients/desktop.md) specifies a detached always-on-top focus window, Spotlight indexing of task titles, Continuity Camera and Sparkle updates, none of which exist; there is no camera QR scanner; and the **macOS** UI test target is `skipped: true` in the scheme — a macOS XCUITest needs `DevToolsSecurity -enable` on the machine, and a simulator runner needs no such change — so on the Mac product CI proves the models behave but never proves a click reaches the core. The iOS job is where that loop closes — `SunriseiOSUITests` runs on the simulator — so the click-to-core path is demonstrated on the platform that carries the **SHOULDs** and not on the one that carries the MUSTs ([ADR-0028](../11-adr/0028-ios-is-a-v1-client.md)). iOS's own 23 SHOULDs grade **21 met, 2 unmet** — saved views and iCal import/export, each a working shared model with no iOS caller, which is the same class of gap the iCal row was on macOS a cycle ago |
| `apps/web` | ⬜ deferred | localStorage stub per [ADR-0012](../11-adr/0012-web-wasm-deferred.md) |
| `packages/sunrise-ui-tokens` | ✅ live | The design-token build [#29](https://github.com/justin13888/Sunrise/issues/29) said had never been built. Six TOML sources compiled by `mise run tokens` into `tokens.css`, `tokens.ts`, `tokens.swift` and `tokens.rs`, all committed and guarded by three overlapping gates — a vitest drift test, `mise run tokens-check` (which also `rustc`- and `rustfmt`-checks the Rust output) and a `tokens-current` CI job. **72 vitest cases**, the repository's first TypeScript tests, which also assert the stream palette against `StreamColor::as_str` in the Rust and the WCAG contrast ratios `../10-cross-cutting/accessibility.md` asks for — the latter over the whole palette since [ADR-0030](../11-adr/0030-palette-contrast-gate.md), which moved contrast from three hand-picked assertions to an exhaustive rule table the loader enforces. `tokens.kt` is not emitted (no Android target); `tokens.rs` has no consumer yet and is `include!`-ready for the first one ([ADR-0029](../11-adr/0029-design-token-pipeline.md)) |
| `packages/sunrise-ui` | 🟨 partial | No longer hand-written: the 40-line token file is now a naming layer over `@sunrise/ui-tokens`, and `spacing` moved to the six-step scale the doc always specified. Still not a component library, and still **one** consumer — `apps/web`, itself deferred — which imports `taskStateGlyph` and, through `main.tsx`, the generated stylesheet. The *values* do now reach shipping clients: both Apple targets compile `tokens.swift` from the same TOML. They reach them through `sunrise-ui-tokens`, not through this package, which is why this row is still 🟨 |

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
`sunrise-server`; **20 of 23 crates reachable**; the three that are not are the
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

- Real `TcpListener` running the production `kynos` router; real HTTP over real
  TCP via the production `SseTransport` — typed `POST`s up, a `text/event-stream`
  fan-out down; real `Hello`/`HelloAck` capability negotiation, unchanged across
  the transport move because `Hello::negotiate` and its frozen fixtures were.
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
proptest. The migration list is four files deep now — `0013_baseline.sql`
and three appends, `0014_stream_sort_order.sql`,
`0015_entity_extra_columns.sql` and
`0016_stream_description_and_default_context.sql` — so there is a three-step
upgrade chain to test as well as a refusal, and `current_storage_v()` is
asserted equal to `STORAGE_V`, now **16**, so the constant and the list cannot
drift apart.

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
  The four cases are pinned by `no_cursor_replays_everything_retained`,
  `a_cursor_narrows_the_replay_to_what_was_missed`,
  `a_cursor_at_the_head_replays_nothing_and_reports_no_gap` and
  `a_cursor_past_retention_gets_a_typed_gap_before_the_replay`, plus
  `a_replay_past_the_ring_bound_is_served_from_the_durable_log`. They were
  `tests/ws_cursors.rs` until ADR-0023; they now live in `api/sync.rs`'s inline
  `mod tests`, under a `-- ws_cursors --` marker that says where they came from.
- **Blob storage is a stub and unauthenticated**
  ([#22](https://github.com/justin13888/Sunrise/issues/22)) — fixed, and the
  entry was wrong on every count by the end. The chunk route is mounted, the
  2PC is content-addressed and hash-verified at `finalize`, and the routes
  require bearer *plus* account *plus* device binding. That was stricter than
  `/sync` until ADR-0023; the two are now the same code — every route on the
  surface, sync included, takes one of `api/signed.rs`'s four extractors, so the
  order of checks is stated once rather than per handler.
  `Command::AttachFile` and `Query::TaskAttachments` both exist.

Similarly, **"auth is checked once, at the WebSocket upgrade"** was stale and
has been rewritten twice. The socket re-checked `exp` on every inbound frame;
the SSE surface re-checks it on every operation, through `resolve`, and on a
timer inside the open event stream — which is the case that matters, since a
subscriber that only reads issues nothing else to check. A mid-session refresh
is `POST /sync/session/refresh`. The coverage moved with the code, into
`api/sync.rs`'s inline `mod tests` under a `-- ws_token_expiry --` marker:
`an_expired_token_ends_the_session`, `a_session_with_no_deadline_is_never_closed`
and the four refresh cases. What remains true is narrower, and is the entry
below.

- **No in-session op retry**
  ([#20](https://github.com/justin13888/Sunrise/issues/20)). An unacked op waits
  for the session to end; the chaos tests script the reconnect the driver should
  perform itself, so they prove the *relay* can recover, not that the client does.
- **Device binding on `/sync` is not signature-based** — no longer true, and it
  was ADR-0023 that closed it rather than a fix aimed at it. The socket's
  objection was structural: an upgrade carries no body to sign. Five typed
  operations do, so all five take `api/signed.rs`'s extractors and are bound
  exactly as the blob routes are, under
  [ADR-0022](../11-adr/0022-device-signature-canonical-json.md)'s
  `header_sig_v2` over the request *value*. What survives from the entry is the
  narrower point it always contained: the binding is `require_device_sig`-gated,
  so a self-host deployment with no OIDC issuer has no device rows to bind to
  and binds nothing ([#7](https://github.com/justin13888/Sunrise/issues/7)
  covers the rest). The open event stream is still re-checked on a
  `device_recheck_ms` timer, because a subscriber that only reads presents no
  further request to check.
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
  for the sync socket. The hand-assembled span that fixed it is itself gone now:
  `api/observe.rs` is handed the matched route rather than the request's URI, so
  there is no query string in reach of the logging path at all.
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

- **Web WASM core** — [ADR-0012](../11-adr/0012-web-wasm-deferred.md). The MSRV
  blocker is **cleared**: [ADR-0026](../11-adr/0026-msrv-bump.md) moved the pin
  to 1.91.1, which is ADR-0012's stated revisit trigger. What is still deferred
  is the work that trigger unblocks — the `rusqlite` 0.31 → 0.40 swap across
  `sunrise-storage` and `sunrise-core`, under ADR-0012's unchanged native
  SQLCipher gate ([#52](https://github.com/justin13888/Sunrise/issues/52)).
  `apps/web/src/wasm.ts` keeps the `loadCore()` seam for a later drop-in.
- **Android** — the same UniFFI scaffolding generates Kotlin "when Android
  arrives" (`crates/sunrise-core-bindings/src/lib.rs`), and nothing has asked
  it to: there is no `apps/android`. Post-v1 by
  [ADR-0027](../11-adr/0027-v1-self-host-first.md). iOS shared this bullet
  until [ADR-0028](../11-adr/0028-ios-is-a-v1-client.md) and no longer does —
  `mise run apple-xcframework` builds its device and simulator slices,
  `ci.yml`'s `Add the iOS slices to the pinned toolchain` step — in both the
  `macos-app` and `ios-app` jobs — adds them on every run, and the
  app that links them is tested on the simulator.
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
  >5% blocking gate needs dedicated hardware. `cargo-mutants` is wired, but
  nightly rather than per-pull-request: `ci.yml`'s `Mutation coverage` job runs
  the four scoped crates as a per-crate shard matrix — the counts live in that
  matrix and are sized by mutant count — on `schedule` and `workflow_dispatch`
  only, and `Mutation coverage gate` feeds every shard's `outcomes.json` to
  `.github/scripts/mutants-gate.py`, which aggregates per crate and compares
  against `mutants/baseline.json` with `--expect-shards` mirroring that matrix,
  so a shard whose runner died reads as a broken run rather than as a coverage
  regression. The mutate step treats cargo-mutants' exit 0, 2 and 3 — clean,
  mutants missed, mutants timed out — as success, because on this workspace 2
  and 3 are the ordinary result and judging them is the gate's job, and it
  propagates every other code, notably 4: the unmutated baseline failed to
  build or test. `mise run mutants <crate> [--shard k/n]` is the same pass
  locally, and `mise run mutants-baseline` — with the shard
  counts it requires — is how a floor is recorded. The floors themselves are
  deliberately not restated here: `mutants/baseline.json` is the only thing the
  gate reads, the numbers ratchet upward as tests improve, and a copy in this
  file would be wrong the first time one moves. It currently carries
  `sunrise-crypto` and `sunrise-sync`; `sunrise-domain` and `sunrise-core` get
  theirs from the first nightly and until then fail the gate for having none.
  `CODEOWNERS` now encodes the security-review gate, though GitHub only enforces
  it once branch protection requires code-owner review.
- **`cargo-fuzz` targets** — `testing.md` specifies six; `fuzz/` does not exist.

## Test suite

All figures below were **measured on `v1-rewrite`** after the final wave, not
carried over from an earlier revision.

| Gate | Result |
|---|---|
| `mise run rust-test` | **1335 passed**, 0 failed, 3 ignored |
| `cargo test -p sunrise-cli` | **77 passed** — 48 in `tests/`, 29 in-crate |
| `mise run rust-doctest` (`--workspace --exclude sunrise-server --doc`, then `-p sunrise-server --doc -- --skip relative_uri`) | 2 passed (sunrise-log), 5 ignored (sunrise-relay-client); the 18 kynos-generated `relative_uri` items are skipped by name in the one crate that has them — #58, #60 |
| `mise run macos-app` | **477 tests in 75 suites passed**; SwiftLint `--strict` clean; exit 0 |
| `mise run rust-fmt-check` | clean |
| `mise run rust-clippy` | clean (pedantic, `-D warnings`) |
| `cargo deny check` | clean |
| `mise run validate` | clean (no TS tests exist yet) |
| `mise run orphan-crates` | clean — 20/23 reachable, QUARANTINE empty |

The 3 ignored are the `#[ignore]`d child-process bodies the vault-lock crash
tests spawn; they are executed, as subprocesses, by the tests that `SIGKILL`
them. The macOS 477 does **not** include the 4 XCUITest cases, which are
`skipped: true` in the scheme and run only under `mise run macos-uitest`.
`mise run ios-app` compiles the same `SunriseTests/` sources against the iOS
product as `SunriseiOSTests` and runs the 5 `SunriseiOSUITests` cases on the
simulator alongside them.

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
mise run rust-test        # cargo test --workspace --all-targets
mise run rust-clippy      # pedantic, -D warnings
mise run rust-fmt-check   # formatting
mise run validate         # Biome CI + typecheck + coverage
mise run macos-app        # xcframework + swiftlint --strict + xcodebuild test
mise run orphan-crates    # every crate reachable from a shipping binary
cargo deny check      # advisories, bans, licences, sources
```

## Boots end-to-end

- `cargo run -p sunrise-server` — the typed REST surface plus the SSE sync
  relay on `127.0.0.1:8443`, both described by the committed
  `schemas/generated/openapi.v1.json`. Self-host mode installs the single-tenant `NullVerifier`,
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
> (e.g. `http://127.0.0.1:8443` — the relay's origin, not a path) starts the
> sync driver over `SseTransport`;
> `SUNRISE_EXPORT_PAIRING_FILE` / `SUNRISE_PAIRING_FILE` perform the dev
> two-file pairing-payload exchange (see the README's live sync demo).
> `sunrise sync --once` drains the outbox and exits, bounded. Unset, the CLI
> stays fully offline. The wiring is proven headlessly by
> `cargo test -p sunrise-cli --test live_sync` and, end to end, by
> `cargo test -p sunrise-e2e --test two_core_relay_convergence`.
