# 0012 — Web WASM core deferred; localStorage stub is the v1 web story

**Status:** accepted

## Context

The target web architecture ([`../07-clients/web.md`](../07-clients/web.md)) runs the Rust
`sunrise-core` compiled to `wasm32-unknown-unknown` inside a Web Worker, with the
SQLite vault persisted to OPFS. `apps/web/src/wasm.ts` ships a `localStorage`
stub behind a `loadCore()` seam so the PWA renders during development, with the
intent that a real WASM build replaces it.

This ADR records a **gated spike (slice C8)** that evaluated compiling the real
core to WASM via [`sqlite-wasm-rs`](https://crates.io/crates/sqlite-wasm-rs)
(SQLite built for `wasm32-unknown-unknown` with an OPFS SAHPool VFS). The spike
carried a **hard gate**: the native workspace must continue to build with
SQLCipher intact (`cargo test -p sunrise-storage` + `-p sunrise-core` pass
**unchanged**). If enabling the WASM path breaks native SQLCipher, we stop and
document the fallback rather than force it.

The gate failed. This ADR is that fallback.

## What was tried

The storage layer (`sunrise-storage::db`, `oplog`, `sync_local`, `keychain`) and
`sunrise-core` (`engine`, `core`, `keychain`) are built entirely on **rusqlite's
`Connection` API**. The workspace pins `rusqlite 0.31` with the
`bundled-sqlcipher` feature (→ `libsqlite3-sys 0.28`, SQLite/SQLCipher compiled
from the vendored amalgamation via `cc`). Getting a rusqlite-shaped SQLite into
WASM therefore requires one of three integration paths — all evaluated:

1. **rusqlite's first-class `ffi-sqlite-wasm-rs` feature.** The integration
   mechanism the slice brief assumed — a `[patch.crates-io]` on `libsqlite3-sys`
   pointing at a wasm fork — is **obsolete**. As of 2026, `sqlite-wasm-rs`
   (0.5.5) ships raw `wasm32-unknown-unknown` FFI bindings plus VFS crates
   (`memory` / `sahpool`-OPFS / `relaxed-idb`-IndexedDB), and rusqlite integrates
   it directly through a feature named `ffi-sqlite-wasm-rs`
   (`= [dep:sqlite-wasm-rs]`). That feature exists **only in `rusqlite 0.40.x`**
   (verified: absent in every release 0.32–0.39). Adopting it means bumping the
   workspace from `rusqlite 0.31` to `0.40` — a nine-minor-version jump across
   `sunrise-storage` + `sunrise-core` (100+ call sites; `engine.rs` alone has
   64 rusqlite references) that swaps the native SQLite/SQLCipher stack wholesale
   (`libsqlite3-sys 0.28 → 0.38`, SQLite 3.45 → 3.53.x).

   **Empirical gate result:** bumping `rusqlite` to `0.40.1` (native features
   `bundled-sqlcipher`, `blob`, `trace`; `default-features = false` so
   `ffi-sqlite-wasm-rs` stays off on native) resolves `libsqlite3-sys 0.38.1`,
   whose `build.rs` uses the `cfg_select!` macro. `cfg_select!` was stabilized in
   **Rust 1.91**; the workspace pins **`rust-version = "1.88.0"`** and the build
   fails to compile with `error: cannot find macro cfg_select in this scope` —
   **before the wasm target is even attempted, on the native `bundled-sqlcipher`
   build itself.** `rusqlite 0.40.1` requires `libsqlite3-sys` `> 0.38.0`, so
   pinning back to `0.38.0` to dodge `cfg_select!` is not permitted by the
   resolver. This is a direct gate #1 failure: the only rusqlite line with
   sqlite-wasm-rs support cannot build the native workspace at the pinned MSRV.

2. **Raw `sqlite-wasm-rs` FFI on wasm, keeping rusqlite on native.** This means
   a parallel wasm-only reimplementation of the entire storage layer against the
   raw `sqlite_wasm_rs` C FFI (`sqlite3_open_v2`, `sqlite3_prepare_v2`, …). That
   FFI is inherently `unsafe`, and the workspace **forbids unsafe code**
   (`unsafe_code = "forbid"`, workspace-wide). It would also duplicate every SQL
   path in `db`/`oplog`/`sync_local`/`keychain`/`engine`. Rejected on the
   no-unsafe rule alone, before scope.

3. **`[patch.crates-io]` on `libsqlite3-sys` (the brief's assumed mechanism).**
   A `[patch]` is **global across all targets** — it would replace the native
   `bundled-sqlcipher` shim too. The only fork found (`libsqlite3-sys-le 0.21.0`)
   exposes no wasm/`ffi-sqlite-wasm-rs` feature (its features are the ordinary
   `min_sqlite_version_*` / `pkg-config` / `vcpkg` set) and its version does not
   satisfy rusqlite 0.31's `^0.28` requirement. Non-viable and, being global,
   directly hazardous to native SQLCipher — exactly what the gate forbids.

## Decision

**Defer the web WASM core.** Do **not** adopt `sqlite-wasm-rs` for v1. Keep the
existing `apps/web/src/wasm.ts` **`localStorage` stub behind `loadCore()`** as the
v1 web story. No workspace dependency, MSRV, or `cfg`-gating changes are made; the
native SQLCipher build and its tests remain exactly as they were.

No `crates/sunrise-core-wasm` crate is created; the pipeline (`just web-wasm`) and
CI `wasm-check` job (slice phases 2–3) are **not** added, since they would only
guard a path that cannot yet compile.

## Alternatives considered

| Option | Why rejected (now) |
|---|---|
| Bump `rusqlite` → `0.40` for `ffi-sqlite-wasm-rs` | Requires Rust ≥ 1.91 (`libsqlite3-sys 0.38.1` `cfg_select!`) vs pinned MSRV 1.88; large native SQLite/SQLCipher stack swap; breaks the gate |
| Raw `sqlite-wasm-rs` FFI, wasm-only storage reimpl | Inherently `unsafe` (workspace forbids it); duplicates the entire storage/query surface |
| `[patch.crates-io]` on `libsqlite3-sys` | Global (hits native SQLCipher); no fork exposes a wasm feature compatible with rusqlite 0.31 |
| Ship `localStorage` stub as v1 web (**chosen**) | Not encrypted, not the OPFS target — but keeps native intact and unblocks UI work; revisit when the toolchain moves |

## Consequences

- **Web persistence in v1 is the `localStorage` stub only** — in-tab, unencrypted,
  no OPFS, no real `sunrise-core`. `apps/web/src/wasm.ts` and its `CoreApi` seam
  are unchanged; UI engineers keep iterating against it. `docs/07-clients/web.md`
  carries a status note pointing here.
- **The native build is untouched.** `rusqlite 0.31` / `libsqlite3-sys 0.28` /
  SQLCipher stay pinned; `cargo test -p sunrise-storage -p sunrise-core` pass
  unchanged (verified before and after the spike).
- **Revisit trigger.** Re-open the WASM core when *either* the workspace MSRV
  moves to Rust ≥ 1.91 (making `libsqlite3-sys 0.38.1` buildable) *or* a
  `rusqlite` release carrying `ffi-sqlite-wasm-rs` compiles on the then-current
  MSRV. At that point the intended shape still holds: a `sunrise-core-wasm`
  wasm-bindgen crate mirroring the `sunrise-core-bindings` JSON-FFI surface
  (`open_vault` / `submit_json` / `query_json` / `close_vault`), a dedicated
  Web Worker registering the OPFS SAHPool VFS, and `loadCore()` feature-detecting
  the worker path with the stub as fallback.
- **Known v1 web gap carried forward:** even once the WASM path lands,
  `sqlite-wasm-rs` gives plaintext SQLite in OPFS by default (no SQLCipher key
  pragmas on wasm; multi-tab exclusivity becomes the JS layer's `navigator.locks`
  job). Both are spec-accepted v1 web gaps to document loudly when the work
  resumes; they are **not** in play today because no WASM ships in v1.
