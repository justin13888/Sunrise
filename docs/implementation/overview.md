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

These are the items that the spec calls out but that v1 leaves to
follow-up sessions or platform-engineer ownership:

1. **Sync wire layer**: `Core::submit` writes ops locally, but those ops
   don't yet flow through `sunrise-sync` to the relay's WS hub. The relay
   itself works (see `crates/sunrise-server/tests/ws_handshake.rs`) — the
   missing piece is the `Core` → `Transport` glue.
2. **Real Tauri bundling**: `apps/desktop` has the renderer + IPC bridge;
   `bun install && bun run tauri dev` brings up the live shell. Tauri 2
   isn't in `Cargo.lock` because the cargo workspace excludes the
   src-tauri crate so the main `cargo build` cycle stays fast.
3. **WASM core build**: `apps/web/src/wasm.ts` returns a localStorage
   stub. The real build is `cargo build -p sunrise-core --target wasm32-unknown-unknown`
   plus `wasm-bindgen` post-processing.
4. **iOS / Android build pipelines**: `crates/sunrise-core-bindings`
   exposes the JSON-FFI surface; UniFFI scaffolding and xcframework /
   .aar pipelines are owned by the platform engineers.
5. **Toxic-proxy chaos suite**: harness layout is in place at
   `tests/chaos/`; scenarios are populated as the sync layer comes up.
6. **Performance benchmarks**: `bench/baseline.json` schema is committed;
   nightly auto-baselines + the >5% regression guard run after the
   benches themselves are written.
7. **Mutation testing**: `cargo-mutants` 90% gate runs as part of the
   release pipeline; the per-crate target list is documented in
   `docs/10-cross-cutting/testing.md`.

## Workspace test count

```
cargo test --workspace            # ~232 tests passing
cargo clippy --workspace -- -D warnings   # clean
cargo fmt --check                 # clean
```

## Boots end-to-end

- `cargo run -p sunrise-server` — REST + `/sync` WebSocket relay
- `cargo run -p sunrise-tui` — Ratatui terminal client against `~/.sunrise/vault`
