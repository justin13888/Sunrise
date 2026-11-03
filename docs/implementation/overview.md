---
status: living
---

# Implementation Overview

This file tracks the state of the implementation against `docs/`. Sunrise stays
at v0.x ([ADR-0042](../11-adr/0042-v0-forever.md)); what is not built is ranked
on the roadmap ([`../roadmap.md`](../roadmap.md)).

The table below reports *reachability from a running client*, not whether a
crate exists and passes its own tests. The companion document is the
per-capability [status audit](../07-clients/parity-matrix.md#status-audit),
which grades the MUSTs the same way; this file grades crates, that one grades
capabilities, and a crate can be reachable while a capability inside it is not.
This page quotes no counts that move with the code: the source that holds each
one is named instead. Fixed values, such as the frame header size or the
frozen baseline storage version, are stated, and so is a number word that only
introduces the items named right after it (the two entities `Note` and
`Person`, **one** consumer — `apps/web`).

## Legend

| Mark | Meaning |
|---|---|
| ✅ **live** | Implemented, reachable from a shipping binary, and covered by tests that assert behaviour |
| 🟨 **partial** | Reachable, but a documented part of its spec is missing |
| 🟧 **orphan** | Crate builds and self-tests pass, but **nothing depends on it** — no product path reaches this code. Test and benchmark harnesses (`sunrise-e2e`, `sunrise-bench`) are exempt: having no dependents is their correct shape, since they exercise other crates rather than being consumed |
| 🟥 **broken** | Does not build, or does not work when run |
| ⬜ **not built** | Not built; ranked on the roadmap, with the issue that tracks it |

## Crate status

| Crate / Component | Status | Notes |
|---|---|---|
| Workspace + CI | ✅ live | Cargo + Bun workspace (`Cargo.toml` `[workspace] members`). CI runs the Rust gates, a `macos-app` job on `macos-26`, the reachability gate (`.github/scripts/orphan-crate-gate.py`), and a nightly bench comparison; `release.yml` publishes a tag-driven GitHub Release and a GHCR image ([#17](https://github.com/justin13888/Sunrise/issues/17)) |
| `sunrise-id` | ✅ live | ULID + `EntityRef`, every entity prefix the registry in `crates/sunrise-id/src/registry/table.rs` declares (`fcs_` for focus sessions and `rvw_` for review snapshots among them), client-side generation |
| `sunrise-error` | ✅ live | Error registry, `Recoverability`. TS mirror (`packages/sunrise-error-ts`) does not exist |
| `sunrise-cbor` | ✅ live | Canonical CBOR, magic prefixes |
| `sunrise-crypto` | ✅ live | Ed25519 / X25519 / XChaCha20-Poly1305 / BLAKE3 / Argon2id; byte-exact `OpEnvelope` |
| `sunrise-crypto-test-vectors` | ✅ live | Dependency-free frozen literals — identity-id, BLAKE3 KDF, stream Merkle roots, and byte-exact `aead_alg=0`/`aead_alg=1` envelope encodings — asserted by `sunrise-crypto/tests/frozen_vectors.rs`, which dev-depends on it |
| `sunrise-test-seed` | ✅ live | The workspace's single reader of `SUNRISE_FUZZ_SEED`. One parser, one precedence rule (`PROPTEST_RNG_SEED` → `SUNRISE_FUZZ_SEED` → a per-process random seed) and one announcement, plumbed into `ProptestConfig::rng_seed` by the `proptest!` blocks that call `proptest_rng_seed()` and into the chaos harness by `Toxic::new`. Exempt from the reachability gate for the same reason `sunrise-crypto-test-vectors` is ([#119](https://github.com/justin13888/Sunrise/issues/119)) |
| `sunrise-domain` | 🟨 partial | Task / Stream / Routine / Context / FocusSession / ReviewSnapshot are complete, as are the capture parser, dependency graph, scheduling constraints, streaks, review/stats folds, export, the `note_body` block-grammar codec, the `notify` reminder planner, `import`'s stable `(source, uid)` → Block id hash, and the `sort_order` base-26 fractional index that gives `Stream.sort_order` its arithmetic. `Block` and `Attachment` have full command paths — their commands, op kinds and queries are the variants of `Command` (`crates/sunrise-core/src/commands.rs`), `InnerOp` (`crates/sunrise-core/src/inner_op.rs`) and `Query` (`crates/sunrise-core/src/queries.rs`) that name them, such as `Command::CreateBlock`, `Command::AttachFile` and `Query::TaskAttachments`. Block is reachable from **both** clients (macOS calendar grid; CLI via `ical import` / `ical export`), Attachment from macOS only. `Note` and `Person` are the two entities that genuinely have no path: a struct and a dead table, with nothing in between |
| `sunrise-storage` | 🟨 partial | Schema, op log and FTS5 are solid. `BlobStore` has external consumers (`sunrise-core::attach`, `sunrise-server::api::blobs`), so it is reachable from the macOS client. The `notes` and `persons` tables are never written, matching the two entities with no command path. The migration list is `0013_baseline.sql` per [ADR-0018](../11-adr/0018-storage-baseline-reset.md) and every append since; this page does not count them or name `STORAGE_V`'s value, because both move with each migration. `crates/sunrise-cbor/src/version.rs` holds the constant and a unit test pins it to the list. `BASELINE_STORAGE_V` stays **13**: `db/schema.rs` refuses any vault stamped below it with a typed `STORAGE_V_PRE_BASELINE`, and `refuses_every_pre_baseline_version` asserts that for every version below it |
| `sunrise-wire-protocol` | ✅ live | 11-byte frame, the message kinds `MsgKind` in `src/messages.rs` enumerates, `Hello`/`HelloAck`, capability negotiation. zstd is implemented but never enabled at any call site |
| `sunrise-sync` | ✅ live | `SyncState`, `Backoff`, the `Transport` trait, and `SseTransport` (`src/sse/`, behind the `sse` feature) — the SSE-plus-typed-`POST` client [ADR-0023](../11-adr/0023-sse-sync-transport.md) put in the `WsTransport`'s place. The driver hands this layer whole encoded wire frames and reads whole encoded frames back. The outbox and the driver live in `sunrise_storage::Outbox` and `sunrise-core::sync_driver` |
| `sunrise-log` | ✅ live | `tracing` + `tracing-subscriber` carry the transport ([ADR-0010](../11-adr/0010-logging-strategy.md), amended) and this crate is the `Plain<T>` wrapper, the `RedactionLayer` field-name veto, the `ev` catalogue check, and subscriber assembly. Both binaries initialise it first thing; `sunrise-server`, `-storage`, `-core`, `-cli` emit against the catalogue |
| `sunrise-pairing` | ✅ live | Full `Noise_XX_25519_ChaChaPoly_SHA256` handshake, SAS confirmation, and the encrypted channel the existing device uses to hand a new one its vault root. `Core::export_vault_root_for_pairing` is the (deliberately conspicuous) counterpart. Proven by `sunrise-e2e/tests/paired_devices_converge.rs`, which contains **no shared key constant** — B learns the root only across the channel |
| `sunrise-auth` | ✅ live | Client-side OIDC relying party: discovery, PKCE, a loopback redirect listener, token exchange and refresh, and credential storage. Consumed by `sunrise-cli` (`login` / `logout` / `whoami`) and by `sunrise-core-bindings`, so it reaches the macOS app |
| `sunrise-core` | 🟨 partial | Open / submit / query / changes / sync_status / close all work. The op kinds are `InnerOp`'s variants, the commands `Command`'s and the queries `Query`'s; this page does not count them. Revoking a device rotates the account identity away from it (`Command::RotateIdentity`, `Query::IdentityStatus`, [ADR-0037](../11-adr/0037-identity-transition.md)), so the device cannot certify itself back in under a fresh id ([#105](https://github.com/justin13888/Sunrise/issues/105)). Every command kind and every query is reachable across the UniFFI seam; `sunrise-cli` reaches a subset, including `devices`, `device revoke`, `identity status` and `identity rotate`. `SystemClock::timezone()` resolves the device's IANA zone by reading `/etc/localtime`, the exception [shared-core.md §Determinism rules](../01-architecture/shared-core.md#determinism-rules) names |
| `sunrise-server` | ✅ live | Every operation is served by `api/`, described by `schemas/generated/openapi.v1.json` and reachable — there is no `routes/` module and no `/sync` WebSocket, and neither `axum` nor `tokio-tungstenite` is a dependency (`tower` and `tower-http` survive in the lock only as `reqwest` transitives, declared nowhere). Relay fanout, cursor-scoped replay backed by a durable SQLite relay log, metrics, OIDC JWKS verification, `X-Sunrise-Device-Sig` binding, and SQLite-backed accounts/devices are real. `/sync` is an SSE stream downstream and typed `POST`s upstream ([ADR-0023](../11-adr/0023-sse-sync-transport.md)): `POST /sync/session` negotiates, `GET /sync/events` fans out with `Last-Event-ID` resumption, and the stream ends when the token expires or the device is revoked, so a revocation reaches a stream already open. Fanout is scoped to the verified subject. `require_device_sig` is derived from the deployment — on wherever an OIDC issuer is configured, off for single-tenant self-host, which `validate` rejects the flag alongside anyway — and `/metrics` is mounted only on a loopback listener. Blob 2PC is **implemented**, not a stub: `init` / `PUT {upload_id}/{chunk_idx}` / `finalize` / `GET {blob_id}` are all mounted, content-addressed and hash-verified on finalize, with a round-trip test, and the fetch streams chunk-by-chunk rather than buffering the whole blob. Its auth is bearer plus account plus device binding, and that is the *same* auth `/sync` runs rather than a stricter one: every route on the surface goes through one of `api/signed.rs`'s extractors ([#22](https://github.com/justin13888/Sunrise/issues/22) is closed by this) |
| `sunrise-integrations` | 🟨 partial | The iCal half is live and dual-consumed: `ical` (RFC 5545 syntax) → `ical_map` (domain mapping) → `ical_vault` (the vault driver), reached by `sunrise-cli`'s `ical import` / `ical export` and, across the seam's `import_ical` / `export_ical`, by the macOS File menu — so both shipping clients reach it. Imports are idempotent because the Block id *is* a hash of `(source, uid)`. The subset is narrow and **reports rather than drops**: `VTODO`, `VALARM`, `VTIMEZONE`, `VJOURNAL`, `VFREEBUSY`, `RDATE`/`EXDATE`/`RECURRENCE-ID`, `ATTACH`, `ATTENDEE` and any `X-` property each raise an `ICalNotice`. `RRULE`, `DESCRIPTION` and `LOCATION` parse and are then reported at the domain boundary, because `Block` has no field for them — so **a recurring event imports as a single occurrence**, and an exported `.ics` carries only `UID`, `SUMMARY`, `DTSTART`, `DTEND`. The GCal half is **implemented, tested and unconsumed**; read-only calendar sync ([ADR-0049](../11-adr/0049-calendar-integrations-per-device-oauth.md)) is not built and is ranked as [#4](https://github.com/justin13888/Sunrise/issues/4): PKCE exchange/refresh with the durable-refresh-token rule and change detection that suppresses phantom deletes, all with injected transport, but nothing has run against the live API (needs a Google OAuth client ID) and there is no `impl EventSyncer` anywhere. `IntegrationProvider` has **no implementor** — not even the live iCal path uses it, so the crate's own claim that integrations "run through" it is not true |
| `sunrise-cli` | ✅ live | The `sunrise` binary: one-shot subcommands, dispatched by `crates/sunrise-cli/src/cmd/mod.rs` against an open vault and by `src/main.rs` for the ones that run before it, plus the env-driven live-sync wiring. Its graded MUSTs are all met; see the [status audit](../07-clients/parity-matrix.md#status-audit). Joining an account is `pair offer` / `request` / `issue` / `accept` — a file per message, because the account's signing key does not travel ([#105](https://github.com/justin13888/Sunrise/issues/105)) — or `pair join` / `add` over the relay. They are dispatched *before* `Core::open`, because the identity a vault belongs to is fixed when the vault is created. Arg parsing is hand-rolled, not clap. This is the reachability story for the core with no UI at all — `crates/sunrise-cli/tests/cli.rs` drives the real binary against a real vault in a separate process |
| `sunrise-client-core` | ✅ live | The client layer every client links ([shared-core.md §The client layer](../01-architecture/shared-core.md#the-client-layer)): undo/redo by inverse command and the session history that replays it against the vault, saved views and their store, the bootstrap preferences file, the edit gate, the zone-read vocabulary, and — behind the default `relay` feature — the account bootstrap, the recovery flow (`recovery.rs`, folded in from the former `sunrise-onboarding`) and background sync. The UniFFI seam and the CLI delegate to it; the web core links its pure half |
| `sunrise-core-bindings` | ✅ live | The UniFFI seam ([ADR-0019](../11-adr/0019-swiftui-macos-client.md)): an opaque async `SunriseCore`, every command, every query and their results, and a `ChangeListener` change stream with the mandatory `on_lagged` resync, fanned out to every subscriber. Of the exported symbols, `parse_saved_view` has no Swift caller at all, and `energy_fit_label` is reached only from the test target. Neither carries a parity MUST — the *Saved searches / views* MUST is met through `SavedViews.load` / `.save` — so they are unconsumed surface rather than an unreachable requirement, which is the distinction the 🟨 mark is for |
| `sunrise-bench` | ✅ live | Criterion suite + linux-x86_64 baselines. `baseline --check` compares against them and annotates regressions; it runs nightly and **does not gate** — on shared runners the same binary reports ±100% against its own baseline from noise alone |
| `sunrise-e2e` | ✅ live | Flagship two-Core relay convergence + the chaos scenarios in `crates/sunrise-e2e/tests/chaos.rs`, plus blocker, context and focus-session convergence |
| `apps/apple` | 🟨 partial | The SwiftUI clients over the UniFFI seam ([ADR-0019](../11-adr/0019-swiftui-macos-client.md)): app source in `Sunrise/` (shared), `macOS/` and `iOS/`, a Swift Testing suite in `SunriseTests/` that compiles into both bundles, and XCTest UI tests on each platform. Built by XcodeGen from `project.yml`, linking the generated xcframework. **Both targets build in CI**, as two jobs on `macos-26` rather than one, downstream of a third — an `apple-xcframework` job that builds the three-slice framework once and publishes it as an artefact, then a `macos-app` job running `mise run macos-app` (xcodegen → `swiftlint --strict` → `xcodebuild test`), and an `ios-app` job running `mise run ios-app`, which builds the `SunriseiOS` product, compiles the same `SunriseTests/` sources against it a second time as `SunriseiOSTests`, which is what makes the shared half of the app answer for itself on both platforms, and — unlike the macOS job — runs its UI tests, `SunriseiOSUITests`, on the iPhone 17 Pro simulator. Every graded macOS MUST but the widget row is met; that row is built but unmet until someone watches a signed Mac offer it ([#376](https://github.com/justin13888/Sunrise/issues/376)). Still partial, and for reasons that are about its *spec* rather than about a MUST: [`desktop.md`](../07-clients/desktop.md) specifies a detached always-on-top focus window, Spotlight indexing of task titles and Continuity Camera, none of which exist; there is no camera QR scanner; and the **macOS** UI test target is `skipped: true` in the scheme — a macOS XCUITest needs `DevToolsSecurity -enable` on the machine, and a simulator runner needs no such change — so on the Mac product CI proves the models behave but never proves a click reaches the core. The iOS job is where that loop closes — `SunriseiOSUITests` runs on the simulator — so the click-to-core path is demonstrated on the platform that carries the **SHOULDs** and not on the one that carries the MUSTs ([ADR-0028](../11-adr/0028-ios-is-a-v1-client.md)). Every graded iOS row is met; [`parity-matrix.md`](../07-clients/parity-matrix.md) carries the row-by-row evidence |
| `apps/web` | 🟨 partial | Runs the real `sunrise-core`, compiled to wasm by `crates/sunrise-core-wasm` (`mise run web-wasm`), in a worker over OPFS, and falls back to a `localStorage` stub where the browser cannot host it ([ADR-0055](../11-adr/0055-web-wasm-core.md)). No sync yet |
| `packages/sunrise-ui-tokens` | ✅ live | The design-token build ([#29](https://github.com/justin13888/Sunrise/issues/29)). TOML sources compiled by `mise run tokens` into `tokens.css`, `tokens.ts`, `tokens.swift` and `tokens.rs`, all committed and guarded by overlapping gates — a vitest drift test, `mise run tokens-check` (which also `rustc`- and `rustfmt`-checks the Rust output) and a `tokens-current` CI job. Its vitest suite also asserts the stream palette against `StreamColor::as_str` in the Rust and the WCAG contrast ratios `../10-cross-cutting/accessibility.md` asks for — the latter over the whole palette, through the exhaustive rule table the loader enforces ([ADR-0030](../11-adr/0030-palette-contrast-gate.md)). `tokens.kt` is not emitted (no Android target); `tokens.rs` has no consumer yet and is `include!`-ready for the first one ([ADR-0029](../11-adr/0029-design-token-pipeline.md)) |
| `packages/sunrise-ui` | 🟨 partial | A naming layer over `@sunrise/ui-tokens`, with `spacing` on the doc's six-step scale. Not a component library, and **one** consumer — `apps/web` — which imports `taskStateGlyph` and, through `main.tsx`, the generated stylesheet. The *values* reach shipping clients: both Apple targets compile `tokens.swift` from the same TOML. They reach them through `sunrise-ui-tokens`, not through this package, which is why this row is 🟨 |

### Entities without a command path

`Note` and `Person` are declared in `sunrise-domain`, have `not_` / `prs_` id
prefixes and `notes` / `persons` tables in the baseline schema, and have no
command, no op kind and no query. Nothing can write them; the two tables are
the only ones in the schema with no writer.

The two sharing rows are MUST in the parity
matrix, not built, and ranked as [#133](https://github.com/justin13888/Sunrise/issues/133) — `Person` is the entity that design
operates on, and there is a complete spec with sound primitives underneath it
and nothing in between — and [ADR-0020](../11-adr/0020-v1-must-demotions.md)
split the **Notes (rich text)** row, which stays a
MUST and is met: it means a Task's `body`, not the free-standing `Note`. Both
tables stay in the frozen baseline schema and stay unwritten, on purpose.

## Reachability gate

`.github/scripts/orphan-crate-gate.py` enforces the criterion this file is
built on, and it is **clean**: roots `sunrise-cli`, `sunrise-core-bindings`,
`sunrise-core-wasm` and `sunrise-server`; every crate is reachable except the
permanently exempt harnesses (`sunrise-bench`, `sunrise-e2e`,
`sunrise-crypto-test-vectors`, `sunrise-test-seed`), whose correct shape is to
have no dependents. `mise run orphan-crates` prints the current count, and its
QUARANTINE list is empty. An entry cannot outlive its fix: a quarantined crate
that becomes reachable fails the run.

A caveat the gate cannot express, and it is the reason this file exists beside
it: the gate proves a crate is reachable from a *binary*, not that each
capability inside it is reachable from a *user*. `parse_saved_view` is that
shape today — a seam method in a crate the gate calls reachable, with no Swift
caller. Crate-level reachability is the floor, not the ceiling.

## Not built

- **Android** — the same UniFFI scaffolding generates Kotlin "when Android
  arrives" (`crates/sunrise-core-bindings/src/lib.rs`), and nothing has asked
  it to: there is no `apps/android`. Not built; the roadmap ranks a Kotlin
  UniFFI smoke test ([#369](https://github.com/justin13888/Sunrise/issues/369)) and no Android client yet.
- **Stream sharing, calendar integrations, and the standalone `Note`** — not
  built. Sharing is a MUST in the parity matrix, ranked as [#133](https://github.com/justin13888/Sunrise/issues/133). Calendar
  integrations are read-only
  ([ADR-0049](../11-adr/0049-calendar-integrations-per-device-oauth.md)) and
  ranked as [#4](https://github.com/justin13888/Sunrise/issues/4); GCal's provider code is implemented and tested, and what is
  missing is the wiring, storage and UI around it. The standalone `Note` has no
  writer.
- **Apple Focus integration** — not wired.
- **Focus Mode's platform effects** — the session record, planner, calibration,
  chunking and unblock cascade are live in the core
  ([ADR-0013](../11-adr/0013-focus-session-op-representation.md)) and are
  surfaced by the macOS `FocusView` (ranked picks, start/end, interruption
  logging, cascade). What is still absent is the *platform* half: nothing
  suppresses notifications, registers a Live Activity, dims other windows, or
  plays a cue. Per-Stream pomodoro overrides remain unimplemented.
  `timeboxed to my next Block` has not been built.
- **A detached always-on-top focus window**, Spotlight indexing of task titles,
  and Continuity Camera — all specified in
  [`../07-clients/desktop.md`](../07-clients/desktop.md) and none implemented.
  Print / PDF export deliberately leaves out Review → Trends and Review →
  History — a chart and a list of links do not paginate into rows, and both
  carry CSV/JSON export beside them.
- **Per-field ops** — the tree still resolves conflicts by entity-level LWW
  ([ADR-0014](../11-adr/0014-entity-level-lww-merge.md)), and that merge model is
  superseded by per-field ops ([ADR-0044](../11-adr/0044-per-field-ops.md)), not
  built and ranked as [#319](https://github.com/justin13888/Sunrise/issues/319). ADR-0044 keeps the workspace free of a CRDT
  library.
- **CI gates** — the bench comparison is wired and runs nightly, but
  **informationally**: on shared runners the same binary reports swings over
  ±100% against its own baseline from scheduling noise alone, so `testing.md`'s
  >5% blocking gate needs dedicated hardware. `cargo-mutants` is wired, but
  nightly rather than per-pull-request: `ci.yml`'s `Mutation coverage` job runs
  the scoped crates as a per-crate shard matrix — the counts live in that
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
  file would be wrong the first time one moves. A scoped crate with no floor
  there fails the gate. The security-review gate has no automated half and cannot have one here:
  `require_code_owner_review` would deadlock the sole maintainer's own pull
  requests, so `CODEOWNERS` only ever requested a reviewer — never gated one.
  `testing.md` §Security-review gate is the scope for a reviewer, not a check.
- **Per-commit fuzzing** — the `cargo-fuzz` targets live under `fuzz/`, its
  own cargo workspace because `cargo-fuzz` needs a nightly rustc for
  `-Zsanitizer=address`. `mise run fuzz-build` compiles them, `mise run
  fuzz-smoke` runs each briefly against a tracked seed corpus, and
  `.github/workflows/ci.yml`'s `fuzz` job runs them nightly. The short
  per-commit budget the specification also asked for is deliberately not
  wired; `testing.md` §Continuous fuzz targets carries the argument.
