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
| Workspace + CI | ✅ live | Cargo + Bun workspace; `legacy/` archived and excluded |
| `sunrise-id` | ✅ live | ULID + `EntityRef`, all twelve prefixes (`fcs_` for focus sessions and `rvw_` for review snapshots), client-side generation |
| `sunrise-error` | ✅ live | Error registry, `Recoverability`. TS mirror (`packages/sunrise-error-ts`) does not exist |
| `sunrise-cbor` | ✅ live | Canonical CBOR, magic prefixes |
| `sunrise-crypto` | ✅ live | Ed25519 / X25519 / XChaCha20-Poly1305 / BLAKE3 / Argon2id; byte-exact `OpEnvelope` |
| `sunrise-crypto-test-vectors` | ✅ live | Dependency-free frozen literals — identity-id, BLAKE3 KDF, stream Merkle roots, and byte-exact `aead_alg=0`/`aead_alg=1` envelope encodings — asserted by `sunrise-crypto/tests/frozen_vectors.rs`, which dev-depends on it |
| `sunrise-domain` | 🟨 partial | Task / Stream / Routine / Context / FocusSession / ReviewSnapshot are complete, as are the capture parser, dependency graph, scheduling constraints, streaks, review/stats folds, and export. `Block` (5 commands, 3 op kinds, 2 queries) and `Attachment` (2 commands, 2 op kinds, `Query::TaskAttachments`) also have full command paths, reachable from macOS but not from the CLI. `Note` and `Person` are the two that genuinely have none: a struct and a dead table, with nothing in between |
| `sunrise-storage` | 🟨 partial | Schema, op log and FTS5 are solid. `BlobStore` has two external consumers (`sunrise-core::attach`, `sunrise-server::routes::blobs`), so it is reachable from the macOS client. **2** tables are never written — `notes` and `persons`, matching the two entities with no command path. The migration story is one baseline (`0013_baseline.sql`) per [ADR-0018](../11-adr/0018-storage-baseline-reset.md), not an upgrade chain: `db.rs` refuses any vault stamped below the baseline with a typed `STORAGE_V_PRE_BASELINE`, and `refuses_every_pre_baseline_version` asserts that for every version below it |
| `sunrise-wire-protocol` | ✅ live | 11-byte frame, 15 msg kinds, `Hello`/`HelloAck`, capability negotiation. zstd is implemented but never enabled at any call site |
| `sunrise-sync` | ✅ live | `SyncState`, `Backoff`, the `Transport` trait, and `WsTransport`. The dead `Outbox` / `Cursor` / `CursorMap` / `SyncStateMachine` exports were deleted — the live implementations are `sunrise_storage::Outbox` and `sunrise-core::sync_driver` |
| `sunrise-log` | ✅ live | No longer a logger: `tracing` + `tracing-subscriber` carry the transport ([ADR-0010](../11-adr/0010-logging-strategy.md), amended) and this crate is the `Plain<T>` wrapper, the `RedactionLayer` field-name veto, the `ev` catalogue check, and subscriber assembly. Both binaries initialise it first thing; `sunrise-server`, `-storage`, `-core`, `-cli` emit against the catalogue. The `ring`/`remote` sinks and the `(ev, lv)` throttle were deleted rather than left as an unimplemented interface |
| `sunrise-pairing` | ✅ live | Full `Noise_XX_25519_ChaChaPoly_SHA256` handshake, SAS confirmation, and the encrypted channel the existing device uses to hand a new one its vault root. `Core::export_vault_root_for_pairing` is the (deliberately conspicuous) counterpart. Proven by `sunrise-e2e/tests/paired_devices_converge.rs`, which contains **no shared key constant** — B learns the root only across the channel |
| `sunrise-onboarding` | 🟨 partial | BIP-39 derivation is absent; `account.rs` has no tests |
| `sunrise-auth` | ✅ live | Client-side OIDC relying party: discovery, PKCE, a loopback redirect listener, token exchange and refresh, and credential storage. Consumed by `sunrise-cli` (`login` / `logout` / `whoami`) and by `sunrise-core-bindings`, so it reaches the macOS app. 35 tests |
| `sunrise-core` | 🟨 partial | Open / submit / query / changes / sync_status / close all work. Implements **8** entities behind **21** op kinds (`InnerOp`), exposed as **29** commands and **29** queries. Every command kind and every query is reachable across the UniFFI seam; `sunrise-cli` covers the one-shot subset |
| `sunrise-server` | ✅ live | Relay fanout, cursor-scoped replay backed by a durable SQLite relay log, metrics, OIDC JWKS verification, `X-Sunrise-Device-Sig` binding, and SQLite-backed accounts/devices are real. `/sync` authenticates at the upgrade and scopes fanout to the verified subject. Blob 2PC is **implemented**, not a stub: `init` / `PUT :upload_id/:chunk_idx` / `finalize` / `GET :blob_id` are all mounted, content-addressed and hash-verified on finalize, with a round-trip test. Its auth is stricter than `/sync`'s — bearer plus account plus device binding ([#22](https://github.com/justin13888/Sunrise/issues/22) is closed by this) |
| `sunrise-integrations` | ⬜ deferred | Implemented and tested, with **no v1 consumer by decision**. GCal read-only import is real: PKCE token exchange/refresh with the durable-refresh-token rule, and change detection that suppresses phantom deletes on window slide and page truncation. Transport is injected, so it is fully testable without a network — but nothing has been run against the live API yet (needs a Google OAuth client ID), `IntegrationProvider` has no implementor, and `EventSyncer` is `#[cfg(test)]`-only. No crate depends on it because its only consumer would be read-only external calendar sync ([#4](https://github.com/justin13888/Sunrise/issues/4)), which is deliberately out of v1. The crate is waiting on that issue, not orphaned by accident. iCal remains a subset (no VTIMEZONE/VTODO/VALARM) |
| `sunrise-cli` | ✅ live | The `sunrise` binary: thirteen one-shot subcommands (`capture`, `today`, `inbox`, `next`, `focus`, `done`, `streams`, `contexts`, `routines`, `search`, `review`, `export`, `sync --once`) plus the env-driven live-sync wiring. This is the reachability story for the core with no UI at all — `tests/cli.rs` drives the real binary against a real vault in a separate process |
| `sunrise-client-core` | ✅ live | Client-side but UI-free: undo/redo by inverse command over an `EntityLookup`, and saved views with their TOML-subset parser |
| `sunrise-core-bindings` | ✅ live | The UniFFI seam ([ADR-0019](../11-adr/0019-swiftui-macos-client.md)): an opaque async `SunriseCore`, all 29 commands, all 29 queries and their results, and a `ChangeListener` change stream with the mandatory `on_lagged` resync. `just macos-xcframework` generates the Swift and packages the framework |
| `sunrise-bench` | ✅ live | Criterion suite + linux-x86_64 baselines. `baseline --check` compares against them and annotates regressions; it runs nightly and **does not gate** — on shared runners the same binary reports ±100% against its own baseline from noise alone |
| `sunrise-e2e` | ✅ live | Flagship two-Core relay convergence + four chaos scenarios, plus blocker, context and focus-session convergence |
| `apps/macos` | 🟨 partial | The SwiftUI client over the UniFFI seam ([ADR-0019](../11-adr/0019-swiftui-macos-client.md)): ~8.1k lines of app source, 162 Swift Testing cases and 4 XCTest UI tests (skipped by default). Built by XcodeGen from `project.yml`, linking the generated xcframework. **Not built in CI**, so nothing catches a Swift-side break. Several parity-matrix MUSTs are unmet — notifications, QR pairing, Spotlight and calendar; see the note below the table |
| `apps/web` | ⬜ deferred | localStorage stub per [ADR-0012](../11-adr/0012-web-wasm-deferred.md) |
| `packages/sunrise-ui` | 🟨 partial | A 40-line token file, not a component library. Both consumers import only `taskStateGlyph` and hardcode colours |

### Entities without a command path

`Note` and `Person` are declared in `sunrise-domain`, have `not_` / `prs_` id
prefixes and `notes` / `persons` tables in the baseline schema, and have no
command, no op kind and no query. Nothing can write them; the two tables are
the only ones in the schema with no writer.

This collides with `docs/07-clients/parity-matrix.md`, which marks
**Notes (rich text)** as a MUST for macOS and marks the sharing rows MUST as
well — sharing being what `Person` exists to model. Either the matrix is
aspirational on those rows or the implementation is missing; that is a
scope decision, not a documentation one, so the matrix is **left unchanged
here** pending the MUST-by-MUST audit now in progress. Recorded so the
discrepancy is not mistaken for an oversight.

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
offset), the pre-baseline migration *refusal* tests (there is one migration,
`0013_baseline.sql`; there is no upgrade chain to test), and the FTS5
hostile-input proptest.

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
- **No delete-convergence coverage.** The e2e canonical projection filters
  `deleted = 0`, so no test proves a delete converges.

## Fixed this cycle

Recorded because each presented as something other than what it was:

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
- **Apple Focus integration** — not wired.
- **Focus Mode's platform effects** — the session record, planner, calibration,
  chunking and unblock cascade are live in the core
  ([ADR-0013](../11-adr/0013-focus-session-op-representation.md)), but nothing
  suppresses notifications, registers a Live Activity, dims other windows, or
  plays a cue, and no client surfaces any of it yet. Per-Stream pomodoro
  overrides and `timeboxed to my next Block` are unimplemented (the latter needs
  `Block` to gain a command path).
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

`cargo test --workspace --all-targets` passes **1182** tests, 0 failures, 3
ignored (the `#[ignore]`d child-process bodies the vault-lock crash tests
spawn). That figure is from the last full run, not from this revision — the
crate-status corrections above were made by reading the source, and the suite
was not re-run to produce them. A static count of `#[test]` / `#[tokio::test]`
attributes currently gives 1104, which is consistent with it once the six
`proptest!` blocks and the parameterised cases are accounted for.

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
cargo test --workspace --all-targets                    # full suite
cargo clippy --workspace --all-targets -- -D warnings   # clean (pedantic)
cargo fmt --check                                       # clean
cargo deny check                                        # clean
bun run validate                                        # no TS tests exist yet
```

## Boots end-to-end

- `cargo run -p sunrise-server` — REST + `/sync` WebSocket relay on
  `127.0.0.1:8443`. Self-host mode installs the single-tenant `NullVerifier`,
  and the server now **refuses to bind a non-loopback address** while that is
  in use, since it maps every caller to one account. Configure an OIDC issuer
  for multi-user.
- `cargo run -p sunrise-cli -- <subcommand>` — the command-line client. Reads
  the vault directory from `SUNRISE_VAULT` (default `~/.sunrise/vault`) and
  unlocks with a fixed single-user dev key. Every subcommand is one-shot, which
  is what lets `crates/sunrise-cli/tests/cli.rs` exercise the whole stack
  through the real binary.

> **Sync from the CLI:** setting `SUNRISE_SYNC_URL`
> (e.g. `ws://127.0.0.1:8443/sync`) starts the WebSocket sync driver;
> `SUNRISE_EXPORT_CERT_FILE` / `SUNRISE_TRUST_CERT_FILE` perform the dev
> two-file device-cert exchange (see the README's live sync demo).
> `sunrise sync --once` drains the outbox and exits, bounded. Unset, the CLI
> stays fully offline. The wiring is proven headlessly by
> `cargo test -p sunrise-cli --test live_sync` and, end to end, by
> `cargo test -p sunrise-e2e --test two_core_relay_convergence`.
