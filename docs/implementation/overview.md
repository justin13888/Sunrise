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
| `sunrise-id` | ✅ live | ULID + `EntityRef`, all ten prefixes, client-side generation |
| `sunrise-error` | ✅ live | Error registry, `Recoverability`. TS mirror (`packages/sunrise-error-ts`) does not exist |
| `sunrise-cbor` | ✅ live | Canonical CBOR, magic prefixes |
| `sunrise-crypto` | ✅ live | Ed25519 / X25519 / XChaCha20-Poly1305 / BLAKE3 / Argon2id; byte-exact `OpEnvelope` |
| `sunrise-crypto-test-vectors` | 🟧 orphan | No consumer, despite its own doc comment claiming it is "consumed only by the `sunrise-crypto` test suite". Its one vector pairs a real public key with a zero sentinel instead of a frozen expected value, so it cannot detect a BLAKE3 derivation change — which is the entire purpose of a test-vector crate |
| `sunrise-domain` | 🟨 partial | Task / Stream / Routine are complete. `Block`, `Note`, `Context`, `Person`, `Attachment` are structs with no command path |
| `sunrise-storage` | 🟨 partial | Schema, op log, FTS5, and migration upgrade tests (v1→v6) are solid. `BlobStore` has no consumers; 7 tables are never written |
| `sunrise-wire-protocol` | ✅ live | 11-byte frame, 15 msg kinds, `Hello`/`HelloAck`, capability negotiation. zstd is implemented but never enabled at any call site |
| `sunrise-sync` | 🟨 partial | `WsTransport` and backoff are live. `Outbox`, `Cursor`, `CursorMap`, `SyncStateMachine` are exported dead code with live-looking names — the real implementations are elsewhere |
| `sunrise-crdt` | 🟧 orphan | **Loro is not in the data path.** Merge is entity-level LWW in SQLite. ADR-0003 is unrealized |
| `sunrise-log` | 🟧 orphan | No crate calls `sunrise_log::init`, so ADR-0010 and `log-events.md` describe nothing that runs. Two of four documented sinks (`file`, `remote`) do not exist |
| `sunrise-pairing` | 🟧 orphan | `snow` is a declared dependency that appears only in a doc comment. There is no Noise handshake anywhere in the workspace |
| `sunrise-onboarding` | 🟨 partial | BIP-39 derivation is absent; `account.rs` has no tests |
| `sunrise-core` | 🟨 partial | Open / submit / query / changes / sync_status / close all work. Implements 3 entities behind 9 op kinds |
| `sunrise-server` | 🟨 partial | Relay fanout, retained-ring replay, and metrics are real. Auth, accounts, devices, and blob 2PC are stubs — see below |
| `sunrise-integrations` | 🟧 orphan | iCal is a subset; GCal is an OAuth-URL builder plus a trait. Neither is reachable |
| `sunrise-tui` | 🟨 partial | Five views render real Core data and live sync works. **Read-mostly**: uses 3 of 15 Commands — no edit, delete, defer, schedule, move, stream CRUD, or routines |
| `sunrise-core-bindings` | 🟧 orphan | The JSON seam works and is tested, but there is **no UniFFI and no `extern "C"`** anywhere, so no symbol is callable from Swift or Kotlin |
| `sunrise-bench` | ✅ live | Criterion suite + linux-x86_64 baselines in `bench/baseline.json`. Nothing compares against them |
| `sunrise-e2e` | ✅ live | Flagship two-Core relay convergence + four chaos scenarios |
| `apps/desktop` | 🟥 broken | See below |
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
offset), the v1→v6 migration upgrade tests, and the FTS5 hostile-input proptest.

## Known defects

Tracked so they are not rediscovered as surprises:

- **A crash bricks the vault.** `vault_lock.rs` uses `create_new` with no PID
  liveness check and releases only on `Drop`, while the release profile sets
  `panic = "abort"`. The module doc claims `fcntl`/`LockFileEx`; it does not use
  them.
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
- **`apps/desktop` does not run.** It now compiles (the manifest inherited from a
  workspace root that did not apply), but Tauri is not a dependency, there is no
  `main.rs`, no `tauri.conf.json`, and no `#[tauri::command]` attribute. The
  renderer calls `query_today`, which does not exist on the Rust side, and the
  IPC bridge swallows the error — so it renders an empty list forever.

## Deferred by decision

- **Web WASM core** — [ADR-0012](../11-adr/0012-web-wasm-deferred.md); MSRV
  blocker. `apps/web/src/wasm.ts` keeps the `loadCore()` seam for a later drop-in.
- **iOS / Android / UniFFI** — platform-engineer owned.
- **Apple Focus integration** — not wired.
- **Merge journal & per-field CRDT** — v1 conflict resolution is entity-level LWW.
- **CI gates** — the >5% bench-regression gate and `cargo-mutants`
  (`docs/10-cross-cutting/testing.md`) are not wired. Baselines and the criterion
  suite that feed the regression gate are in place.
- **`cargo-fuzz` targets** — `testing.md` specifies six; `fuzz/` does not exist.

## Test suite

`cargo test --workspace --all-targets` passes **411** tests. Read that number
with two caveats: roughly 50 of them exercise orphan crates that no product path
reaches, and a handful are tautological (see `docs/10-cross-cutting/testing.md`).
Treat it as "400+", and prefer the reachability column above as the signal.

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
