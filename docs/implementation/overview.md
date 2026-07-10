---
status: living
---

# Implementation Overview

This file tracks the state of the v1 implementation against `docs/`.
Each phase has a per-section file (added as the surface stabilizes); this
overview is the entry point.

## Phase status

| Phase | Crate / Component | Status | Notes |
|---|---|---|---|
| 0 | Workspace + CI | ✅ shipped | Cargo + Bun workspace; legacy/ archived |
| 1 | sunrise-log | ✅ shipped | NDJSON, `Plain<T>`, sinks, throttle, redaction property test (1k cases) |
| 2 | sunrise-id / error / cbor | ✅ shipped | ULID + EntityRef, error registry, magic prefixes |
| 3 | sunrise-crypto | ✅ shipped | Ed25519 / X25519 / XChaCha20-Poly1305 / BLAKE3 / Argon2id; byte-exact OpEnvelope |
| 4 | sunrise-domain | ✅ shipped | Task / Stream / Routine entities, RRULE subset parser, validation |
| 5 | sunrise-crdt | ✅ shipped | Loro StreamDoc, 3-replica convergence proptest |
| 6 | sunrise-storage | ✅ shipped | SQLCipher schema, op log, blob store, FTS5 |
| 7 | sunrise-wire-protocol | ✅ shipped | 11-byte frame, 15 msg kinds, Hello/HelloAck |
| 8 | sunrise-sync | ✅ shipped | State machine, cursors, outbox, backoff, transport trait |
| 9 | sunrise-pairing / onboarding | ✅ shipped | SAS, QR, account flow, recovery wrapper |
| 10 | sunrise-core | ✅ shipped | Open / submit / query / changes / sync_status / close + engine pipeline |
| 11 | sunrise-server | ✅ shipped | REST endpoints, WS relay, OIDC verifier trait, metrics, blob 2PC, push fanout |
| 12 | sunrise-tui | ✅ shipped | Today / Inbox / Stream / Search / Focus views, vim keymap, Core integration |
| 13 | apps/desktop | 🟡 scaffolded | Tauri 2 + React shell; `bun install` + `bun run tauri dev` to bring up |
| 14 | apps/web | 🟡 scaffolded | Vite + React + Service Worker; localStorage stub until WASM core build |
| 15 | sunrise-core-bindings | 🟡 scaffolded | JSON-FFI surface; UniFFI annotations follow once iOS / Android land |
| 16 | sunrise-integrations | ✅ shipped | iCal RFC 5545 subset; GCal OAuth-URL builder + EventSyncer trait |
| 17 | E2E gating | 🟡 partial | sunrise-e2e harness, bench/baseline.json, chaos test layout |

## What still needs work for "v1 done"

Docs consolidation is **done** (this tree, plus
[`../01-architecture/dependencies.md`](../01-architecture/dependencies.md)).
The remaining v1 build work, in roughly dependency order:

1. **jiff migration**: replace `chrono` with `jiff` across all crates per
   [ADR-0011](../11-adr/0011-datetime-jiff.md); drop the declared-but-unused
   `time` dependency.
2. **Storage migration-apply fix + stream metadata**: fix migration
   application, and persist stream name / color.
3. **Stream/Search queries**: implement the `StreamList` and `Search`
   read queries.
4. **Scheduling constraints**: implement
   [scheduling-constraints.md](../02-domain/scheduling-constraints.md)
   (migration 0003).
5. **Routine generation**: `routine_gen` recurrence engine driving Task
   materialization (migration 0004).
6. **Op-envelope sealing + key management + persistent outbox**
   (migration 0005).
7. **Typed `OpBatch` / `Ack` + relay replay buffer**.
8. **`apply_remote` + LWW metadata** (migration 0006).
9. **WebSocket sync driver + live sync status**: the `Core` → `Transport`
   glue that flows local ops through `sunrise-sync` to the relay WS hub
   (the relay itself works — see
   `crates/sunrise-server/tests/ws_handshake.rs`).
10. **Two-Core relay-convergence e2e**.
11. **TUI completion**: command mode, real Stream / Search / Focus, images.
12. **Criterion benches + baseline populate**: `bench/baseline.json`
    schema is committed; populate baselines and wire the >5% guard.
13. **Chaos harness + scenarios**: layout is in place at `tests/chaos/`;
    populate toxic-proxy scenarios once the sync layer is live.
14. **Web WASM core** (gated spike): `cargo build -p sunrise-core --target
    wasm32-unknown-unknown` + `wasm-bindgen`; `apps/web/src/wasm.ts`
    currently returns a localStorage stub.

### Deferred / platform-owner

Out of scope for the v1 core; owned by platform engineers or later phases:

- **Real Tauri bundling**: `apps/desktop` has the renderer + IPC bridge;
  `bun install && bun run tauri dev` brings up the live shell. Tauri 2 is
  excluded from the cargo workspace so the main `cargo build` cycle stays
  fast.
- **iOS / Android build pipelines**: `crates/sunrise-core-bindings`
  exposes the JSON-FFI surface; UniFFI scaffolding and xcframework / .aar
  pipelines are platform-engineer owned.
- **Mutation-testing gate**: the `cargo-mutants` 90% gate and per-crate
  target list (`docs/10-cross-cutting/testing.md`) run in the release
  pipeline, not the v1 core loop.

## Workspace test count

Counts move as the slices above land, so no fixed number is pinned here.

```
cargo test --workspace                    # full suite
cargo clippy --workspace -- -D warnings   # clean
cargo fmt --check                         # clean
```

## Boots end-to-end

- `cargo run -p sunrise-server` — REST + `/sync` WebSocket relay
- `cargo run -p sunrise-tui` — Ratatui terminal client against `~/.sunrise/vault`
