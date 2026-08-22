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
| `sunrise-id` | ✅ live | ULID + `EntityRef`, all eleven prefixes (`fcs_` added for focus sessions), client-side generation |
| `sunrise-error` | ✅ live | Error registry, `Recoverability`. TS mirror (`packages/sunrise-error-ts`) does not exist |
| `sunrise-cbor` | ✅ live | Canonical CBOR, magic prefixes |
| `sunrise-crypto` | ✅ live | Ed25519 / X25519 / XChaCha20-Poly1305 / BLAKE3 / Argon2id; byte-exact `OpEnvelope` |
| `sunrise-crypto-test-vectors` | ✅ live | Dependency-free frozen literals — identity-id, BLAKE3 KDF, stream Merkle roots, and byte-exact `aead_alg=0`/`aead_alg=1` envelope encodings — asserted by `sunrise-crypto/tests/frozen_vectors.rs`, which dev-depends on it |
| `sunrise-domain` | 🟨 partial | Task / Stream / Routine / Context / FocusSession / ReviewSnapshot are complete, as are the capture parser, dependency graph, scheduling constraints, streaks, review/stats folds, and export. Every one of those now has a client path. `Block`, `Note`, `Person`, `Attachment` are still structs with no command path |
| `sunrise-storage` | 🟨 partial | Schema, op log, FTS5, and migration upgrade tests (v1→v10) are solid. `BlobStore` has no consumers; 7 tables are never written |
| `sunrise-wire-protocol` | ✅ live | 11-byte frame, 15 msg kinds, `Hello`/`HelloAck`, capability negotiation. zstd is implemented but never enabled at any call site |
| `sunrise-sync` | ✅ live | `SyncState`, `Backoff`, the `Transport` trait, and `WsTransport`. The dead `Outbox` / `Cursor` / `CursorMap` / `SyncStateMachine` exports were deleted — the live implementations are `sunrise_storage::Outbox` and `sunrise-core::sync_driver` |
| `sunrise-log` | ✅ live | No longer a logger: `tracing` + `tracing-subscriber` carry the transport ([ADR-0010](../11-adr/0010-logging-strategy.md), amended) and this crate is the `Plain<T>` wrapper, the `RedactionLayer` field-name veto, the `ev` catalogue check, and subscriber assembly. Both binaries initialise it first thing; `sunrise-server`, `-storage`, `-core`, `-tui` emit against the catalogue. The `ring`/`remote` sinks and the `(ev, lv)` throttle were deleted rather than left as an unimplemented interface |
| `sunrise-pairing` | ✅ live | Full `Noise_XX_25519_ChaChaPoly_SHA256` handshake, SAS confirmation, and the encrypted channel the existing device uses to hand a new one its vault root. `Core::export_vault_root_for_pairing` is the (deliberately conspicuous) counterpart. Proven by `sunrise-e2e/tests/paired_devices_converge.rs`, which contains **no shared key constant** — B learns the root only across the channel |
| `sunrise-onboarding` | 🟨 partial | BIP-39 derivation is absent; `account.rs` has no tests |
| `sunrise-core` | 🟨 partial | Open / submit / query / changes / sync_status / close all work. Implements 5 entities behind 15 op kinds. Every command kind and every query is now reachable from the TUI except `TrustDevice` (env-driven) and `MaterializeRoutines` (timer-driven) |
| `sunrise-server` | 🟨 partial | Relay fanout, retained-ring replay, metrics, OIDC JWKS verification, `X-Sunrise-Device-Sig` binding, and SQLite-backed accounts/devices are real. `/sync` authenticates at the upgrade and scopes fanout to the verified subject. Blob 2PC is still a stub and remains unauthenticated ([#22](https://github.com/justin13888/Sunrise/issues/22)) |
| `sunrise-integrations` | 🟨 partial | GCal read-only import is implemented: PKCE token exchange/refresh with the durable-refresh-token rule, and change detection that suppresses phantom deletes on window slide and page truncation. Transport is injected, so it is fully testable without a network — but nothing has been run against the live API yet (needs a Google OAuth client ID). iCal remains a subset (no VTIMEZONE/VTODO/VALARM) |
| `sunrise-tui` | ✅ live | The v1 client, and now the reachability story for most of the core. Seven views (Today grouped by urgency, Inbox, Browse with a Streams **and Contexts** sidebar, Search, Focus, Routines, Review); capture and annotate through the shared parser with live previews; full CRUD over Tasks, Streams, Contexts and Routines; the dependency graph is writable (`b`); marks and visual-range bulk operations; undo/redo; the activity feed; the weekly/daily review, trends, snapshot history and export; a real line editor with the readline chords and bracketed paste; completion and history on the `:` line; optional mouse; saved views (`~/.config/sunrise/views.toml`); and non-interactive subcommands (`capture`, `today`, `inbox`, `next`, `focus`, `done`, `streams`, `contexts`, `routines`, `search`, `review`, `export`, `sync --once`) |
| `sunrise-core-bindings` | 🟧 orphan | The JSON seam works and is tested, but there is **no UniFFI and no `extern "C"`** anywhere, so no symbol is callable from Swift or Kotlin |
| `sunrise-bench` | ✅ live | Criterion suite + linux-x86_64 baselines. `baseline --check` compares against them and annotates regressions; it runs nightly and **does not gate** — on shared runners the same binary reports ±100% against its own baseline from noise alone |
| `sunrise-e2e` | ✅ live | Flagship two-Core relay convergence + four chaos scenarios, plus blocker, context and focus-session convergence |
| `apps/web` | ⬜ deferred | localStorage stub per [ADR-0012](../11-adr/0012-web-wasm-deferred.md) |
| `packages/sunrise-ui` | 🟨 partial | A 40-line token file, not a component library. Both consumers import only `taskStateGlyph` and hardcode colours |

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
offset), the v1→v10 migration upgrade tests, and the FTS5 hostile-input proptest.

## Known defects

Tracked so they are not rediscovered as surprises. Each now has an issue.

- **A skewed clock wins every conflict, permanently**
  ([#21](https://github.com/justin13888/Sunrise/issues/21)). `lww_wins` trusts
  raw `env.ts_ms` from the device wall clock, with no bound. The naive fix is
  worse than the bug: clamping against *local* time makes two replicas store
  different values for the same row, breaking convergence outright. The real
  answer is an HLC, which is a sealed-envelope change and so a protocol bump.
- **Ring eviction is silent data loss**
  ([#19](https://github.com/justin13888/Sunrise/issues/19)). The client builds
  real sync cursors and the server discards them, replaying the whole retained
  ring. Past the ring bounds, or across a relay restart, a returning device
  loses ops with no error. Offline catch-up works by accident of ring size.
- **No in-session op retry**
  ([#20](https://github.com/justin13888/Sunrise/issues/20)). An unacked op waits
  for the session to end; the chaos tests script the reconnect the driver should
  perform itself, so they prove the *relay* can recover, not that the client does.
- **Blob storage is a stub and unauthenticated**
  ([#22](https://github.com/justin13888/Sunrise/issues/22)). The chunk-upload
  route the `init` response points at is not mounted, so every upload 404s. It
  also blocks attachments: `Command::AttachFile` and `Query::TaskAttachments` do
  not exist, which is why the TUI's attachment pane is a placeholder.
- **Auth is checked once, at the WebSocket upgrade.** A token expiring
  mid-session does not terminate the connection. `auth.md` specifies an
  `AUTH_TOKEN_EXPIRED` close and an out-of-band `0x12 RefreshToken` frame;
  `MsgKind` has no such variant, so it needs a wire-protocol change
  ([#7](https://github.com/justin13888/Sunrise/issues/7)).
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
  needs. Found by wiring the TUI's activity overlay, not by a test.
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
  removed elsewhere. Git history preserves it. The TUI is the v1 client.

## Deferred by decision

- **Web WASM core** — [ADR-0012](../11-adr/0012-web-wasm-deferred.md); MSRV
  blocker. `apps/web/src/wasm.ts` keeps the `loadCore()` seam for a later drop-in.
- **iOS / Android / UniFFI** — platform-engineer owned.
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

`cargo test --workspace --all-targets` passes **1123** tests, 0 failures, 3
ignored (the `#[ignore]`d child-process bodies the vault-lock crash tests spawn).

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
- `cargo run -p sunrise-tui` — the terminal client. Reads the vault directory
  from `SUNRISE_VAULT` (default `~/.sunrise/vault`) and unlocks with a fixed
  single-user dev key. Non-interactive subcommands (`capture`, `today`,
  `inbox`, `streams`, `search`) drive the same vault without a terminal, which
  is also how `crates/sunrise-tui/tests/cli.rs` exercises the whole stack.

> **Sync in the TUI:** setting `SUNRISE_SYNC_URL`
> (e.g. `ws://127.0.0.1:8443/sync`) starts the WebSocket sync driver;
> `SUNRISE_EXPORT_CERT_FILE` / `SUNRISE_TRUST_CERT_FILE` perform the dev
> two-file device-cert exchange (see the README's live sync demo). The
> status line shows `sync: live|catching-up|disconnected|off (N pending)`.
> Unset, the TUI stays fully offline. The wiring is proven headlessly by
> `cargo test -p sunrise-tui --test live_sync` and, end to end, by
> `cargo test -p sunrise-e2e --test two_core_relay_convergence`.
>
> The TUI subscribes to `Core::changes()`, so an inbound synced op repaints on
> arrival rather than on the next keystroke. Bursts coalesce in a 50 ms window,
> so an N-op catch-up batch repaints once.
