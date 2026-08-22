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
| `sunrise-domain` | 🟨 partial | Task / Stream / Routine / Context / FocusSession are complete. `Block`, `Note`, `Person`, `Attachment` are structs with no command path |
| `sunrise-storage` | 🟨 partial | Schema, op log, FTS5, and migration upgrade tests (v1→v10) are solid. `BlobStore` has no consumers; 7 tables are never written |
| `sunrise-wire-protocol` | ✅ live | 11-byte frame, 15 msg kinds, `Hello`/`HelloAck`, capability negotiation. zstd is implemented but never enabled at any call site |
| `sunrise-sync` | ✅ live | `SyncState`, `Backoff`, the `Transport` trait, and `WsTransport`. The dead `Outbox` / `Cursor` / `CursorMap` / `SyncStateMachine` exports were deleted — the live implementations are `sunrise_storage::Outbox` and `sunrise-core::sync_driver` |
| `sunrise-log` | ✅ live | No longer a logger: `tracing` + `tracing-subscriber` carry the transport ([ADR-0010](../11-adr/0010-logging-strategy.md), amended) and this crate is the `Plain<T>` wrapper, the `RedactionLayer` field-name veto, the `ev` catalogue check, and subscriber assembly. Both binaries initialise it first thing; `sunrise-server`, `-storage`, `-core`, `-tui` emit against the catalogue. The `ring`/`remote` sinks and the `(ev, lv)` throttle were deleted rather than left as an unimplemented interface |
| `sunrise-pairing` | 🟧 orphan | `snow` is a declared dependency that appears only in a doc comment. There is no Noise handshake anywhere in the workspace |
| `sunrise-onboarding` | 🟨 partial | BIP-39 derivation is absent; `account.rs` has no tests |
| `sunrise-core` | 🟨 partial | Open / submit / query / changes / sync_status / close all work. Implements 5 entities behind 15 op kinds |
| `sunrise-server` | 🟨 partial | Relay fanout, retained-ring replay, and metrics are real. Auth, accounts, devices, and blob 2PC are stubs — see below |
| `sunrise-integrations` | 🟧 orphan | iCal is a subset; GCal is an OAuth-URL builder plus a trait. Neither is reachable |
| `sunrise-tui` | 🟨 partial | Five views render real Core data and live sync works. **Read-mostly**: uses 3 of 15 Commands — no edit, delete, defer, schedule, move, stream CRUD, or routines |
| `sunrise-core-bindings` | 🟧 orphan | The JSON seam works and is tested, but there is **no UniFFI and no `extern "C"`** anywhere, so no symbol is callable from Swift or Kotlin |
| `sunrise-bench` | ✅ live | Criterion suite + linux-x86_64 baselines in `bench/baseline.json`. Nothing compares against them |
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

Tracked so they are not rediscovered as surprises:

- **A skewed clock wins every conflict, permanently.** `lww_wins` trusts raw
  `env.ts_ms` from the device wall clock, with no HLC and no bound.
- **Ring eviction is silent data loss.** The client builds real sync cursors and
  the server discards them, replaying the whole retained ring instead. Past the
  ring bounds, or across a relay restart, a returning device loses ops with no
  error. Offline catch-up currently works by accident of ring size.
- **No in-session op retry.** An unacked op waits for the session to end; the
  chaos tests script the reconnect the driver should perform itself.
- **No delete-convergence coverage.** The e2e canonical projection filters
  `deleted = 0`, so no test proves a delete converges.
- Routines are materialised only at `Core::open` — a long-running TUI never
  generates new occurrences.

## Not reachable by a user

- **Multi-device is impossible.** Every call site hands the Core a literal vault
  root; the e2e tests pass the *same* `[0x42; 32]` to both replicas. Encryption
  is real, but key distribution is bypassed entirely and `sunrise-pairing` is
  never invoked.
- **`/sync` is unauthenticated.** `ServerState` carries a `TokenVerifier` whose
  only `.verify(` call sites are in its own unit tests — no production path
  calls it. Every session resolves to one synthetic account,
  so a Subscribe from any client is served frames belonging to every other. The
  relay is dev-only until this is fixed.
- **Accounts and devices do not persist.** `GET /accounts/me` returns a
  hardcoded sentinel with `200 OK`; `GET /devices` always returns `[]`.
- **Blob upload always 404s.** The chunk-upload route the `init` response points
  at is not mounted; `finalize` verifies hex string lengths and `fetch` returns
  404 unconditionally.
- **No middleware.** `tower-http` is declared with `trace, cors, limit` and never
  imported — no CORS, no request body size limit, no trace layer.

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
- **CI gates** — the >5% bench-regression gate and `cargo-mutants`
  (`docs/10-cross-cutting/testing.md`) are not wired. Baselines and the criterion
  suite that feed the regression gate are in place.
- **`cargo-fuzz` targets** — `testing.md` specifies six; `fuzz/` does not exist.

## Test suite

`cargo test --workspace --all-targets` passes **537** tests (down from 544: the
[ADR-0014](../11-adr/0014-entity-level-lww-merge.md) cleanup deleted 7 tests with
`sunrise-crdt` and 7 with the dead `sunrise-sync` exports, and added 8 frozen
crypto-vector tests in place of 1 tautological one). Read the number with a
caveat: some still exercise orphan crates that no product path reaches, and a
handful are tautological (see `docs/10-cross-cutting/testing.md`). Prefer the
reachability column above as the signal.

```
cargo test --workspace --all-targets                    # full suite
cargo clippy --workspace --all-targets -- -D warnings   # clean (pedantic)
cargo fmt --check                                       # clean
cargo deny check                                        # clean
bun run validate                                        # no TS tests exist yet
```

## Boots end-to-end

- `cargo run -p sunrise-server` — REST + `/sync` WebSocket relay on
  `127.0.0.1:8443` (plain HTTP, in-memory store by default, **no auth**).
- `cargo run -p sunrise-tui` — Ratatui terminal client. Reads the vault
  directory from `SUNRISE_VAULT` (default `~/.sunrise/vault`) and unlocks
  with a fixed single-user dev key.

> **Sync in the TUI:** setting `SUNRISE_SYNC_URL`
> (e.g. `ws://127.0.0.1:8443/sync`) starts the WebSocket sync driver;
> `SUNRISE_EXPORT_CERT_FILE` / `SUNRISE_TRUST_CERT_FILE` perform the dev
> two-file device-cert exchange (see the README's live sync demo). The
> status line shows `sync: live|catching-up|disconnected|off (N pending)`.
> Unset, the TUI stays fully offline. The wiring is proven headlessly by
> `cargo test -p sunrise-tui --test live_sync` and, end to end, by
> `cargo test -p sunrise-e2e --test two_core_relay_convergence`.
>
> Note the TUI only refreshes on a keypress — it does not subscribe to
> `Core::changes()`, so an inbound synced task appears on your next keystroke
> rather than on arrival.
