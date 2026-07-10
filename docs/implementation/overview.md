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
| 11 | sunrise-server | ✅ shipped | REST endpoints, WS relay, typed OpBatch/Ack routing, retained-ring replay, OIDC verifier trait, metrics, blob 2PC, push fanout |
| 12 | sunrise-tui | ✅ shipped | Today / Inbox / Stream / Search / Focus views, vim keymap, `:` command mode, image preview (`images` feature), insta golden frames, Core integration |
| 13 | apps/desktop | 🟡 frontend-only | Tauri 2 + React renderer; `bun install` + `bun run tauri dev` to bring up. Native shell wiring is platform-owner work |
| 14 | apps/web | 🟡 stub (by decision) | Vite + React + Service Worker; localStorage Core stub per [ADR-0012](../11-adr/0012-web-wasm-deferred.md) (WASM core build deferred — MSRV blocker) |
| 15 | sunrise-core-bindings | 🟡 scaffolded | JSON-FFI surface; UniFFI annotations follow once iOS / Android land |
| 16 | sunrise-integrations | ✅ shipped | iCal RFC 5545 subset; GCal OAuth-URL builder + EventSyncer trait |
| 17 | E2E gating | ✅ shipped | sunrise-e2e flagship convergence + chaos scenarios; sunrise-bench criterion suite + linux-x86_64 baselines in `bench/baseline.json` (CI regression gate deferred — see below) |

## v1 core: complete

The v1 Rust core is feature-complete end to end. Everything from the
persisted vault through the sync relay and back into a second device now
runs for real, exercised by the flagship convergence e2e. In summary,
the following all **landed**:

- **Docs consolidation** — single `docs/` design tree (this file plus
  [`../01-architecture/dependencies.md`](../01-architecture/dependencies.md)),
  contradictions reconciled, ADR-0011 (jiff), ADR-0012 (web WASM deferred),
  and the scheduling-constraints domain spec.
- **jiff migration** — `chrono` and the unused `time` dependency removed;
  `jiff` `0.2.32` is the sole datetime library
  ([ADR-0011](../11-adr/0011-datetime-jiff.md)).
- **Storage** — migrations 0002–0006 with an auto-upgrade path in
  `ensure_schema`: stream name/color, scheduling constraints, routine
  materialization, local identity + persistent outbox + sync cursors, and
  LWW metadata.
- **Read queries** — `Query::StreamList` and `Query::Search` (FTS5,
  hostile-input-safe).
- **Scheduling constraints** on Task / Routine (hard/soft, OR-within-kind,
  AND-across-kinds) per
  [scheduling-constraints.md](../02-domain/scheduling-constraints.md).
- **Routine generation** — `routine_gen` DST-aware RRULE expansion,
  Routine CRUD, and deterministic cross-device materialization
  (blake3 occurrence task ids).
- **Sync wire layer** — op-envelope sealing (XChaCha + Ed25519, Keychain,
  vault-root-derived per-stream keys, per-(stream, device) seq); typed
  `OpBatch` / `Ack` payloads with real stream routing and a relay
  retained-ring replay buffer; `apply_remote` (idempotent, entity-level
  LWW, TrustDevice).
- **WebSocket sync driver** — `Core::start_sync` with live `SyncStatus`,
  outbox drain, reconnect/backoff, and incremental subscribe.
- **Flagship e2e** — two `Core`s converge through the relay (live edits,
  offline catch-up, LWW conflict, routine dedup); the subscribe-own-streams
  and `Core::shutdown` driver bugs are fixed.
- **TUI** — command mode (`:q` / `:view` / `:help` / `:preview`), real
  Stream / Search / Focus views, image preview behind the default-on
  `images` feature, and insta golden-frame snapshots.
- **Benchmarks + chaos** — `sunrise-bench` criterion suite (submit,
  query_today@10k, fts@10k, ws-handshake) with linux-x86_64 baselines
  populated in `bench/baseline.json`; a chaos harness (seeded
  drop/corrupt/delay/partition transport) with four scenarios that
  converge after heal.

## Deferred / platform-owner

Deliberately out of scope for the v1 core; owned by platform engineers or
scheduled for a later phase. None of these block v1.

- **Web WASM core** — the PWA stays on the localStorage stub per
  [ADR-0012](../11-adr/0012-web-wasm-deferred.md) (MSRV blocker on the WASM
  toolchain). `apps/web/src/wasm.ts` mirrors the Core surface behind a
  `loadCore()` seam so the WASM build can drop in later.
- **Desktop Tauri wiring** — `apps/desktop` has the renderer + IPC bridge;
  the native shell (`bun install && bun run tauri dev`) is platform-owner
  work. Tauri 2 is excluded from the cargo workspace so the main
  `cargo build` cycle stays fast.
- **iOS / Android / UniFFI** — `crates/sunrise-core-bindings` exposes the
  JSON-FFI surface; UniFFI annotations and xcframework / .aar pipelines are
  platform-engineer owned.
- **Apple Focus integration** — not yet wired.
- **OIDC JWKS verifier** — server ships the verifier *trait*; the JWKS
  fetch/verify implementation is deferred.
- **Blob fetch** — server exposes the 2PC stub; the fetch path is deferred.
- **Merge journal & per-field CRDT** — v1 conflict resolution is
  entity-level LWW; a merge journal and per-field CRDT are future work.
- **CI gates** — the >5% bench-regression gate and the `cargo-mutants`
  mutation-testing gate (`docs/10-cross-cutting/testing.md`) run in the
  release pipeline, not the v1 core loop. The baseline data and criterion
  suite that feed the regression gate are already in place.

## Workspace test count

**411** tests pass across the workspace (`cargo test --workspace
--all-targets`, summing the `test result:` lines). The number moves as
tests are added, so treat it as "400+" rather than an exact contract.

```
cargo test --workspace --all-targets      # full suite (411 passing)
cargo clippy --workspace --all-targets -- -D warnings   # clean (pedantic)
cargo fmt --check                         # clean
```

## Boots end-to-end

- `cargo run -p sunrise-server` — REST + `/sync` WebSocket relay on
  `127.0.0.1:8443` (plain HTTP, in-memory store by default).
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
