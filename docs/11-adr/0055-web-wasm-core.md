# 0055 — The web client runs `sunrise-core` compiled to wasm, over plaintext SQLite in OPFS, one tab at a time

**Status:** accepted

**Supersedes** the decision of [ADR-0012](./0012-web-wasm-deferred.md): the
`localStorage` stub stops being the web story and becomes the fallback. Its
hard gate and its account of the rejected integration paths still stand.

**Tracked by** [#52](https://github.com/justin13888/Sunrise/issues/52).

## Context

ADR-0012 deferred the web core on one blocker: the only `rusqlite` line with a
`wasm32-unknown-unknown` backend (0.40, feature `ffi-sqlite-wasm-rs`) pulls a
`libsqlite3-sys` whose build script needs Rust 1.91, and the workspace pinned
1.88. [ADR-0026](./0026-msrv-bump.md) moved the pin to 1.91.1, which fired
ADR-0012's revisit trigger. What was left was the work ADR-0012 sized, under
its hard gate: `cargo test -p sunrise-storage -p sunrise-core` must pass
unchanged on native SQLCipher.

That work surfaced three things ADR-0012 did not name, and it has to say what
it gives up. This ADR records both.

## Decision

### 1. `rusqlite` 0.40.2 everywhere, `ffi-sqlite-wasm-rs` only in the wasm crate

The workspace moves from `rusqlite` 0.31 to 0.40.2 (`libsqlite3-sys` 0.28 to
0.38, SQLite 3.45 to 3.53) with the same `bundled-sqlcipher`, `blob` and
`trace` features, plus `fallible_uint`: 0.40 put the checked `u64`
`ToSql`/`FromSql` impls that 0.31 carried unconditionally behind that feature,
and turning it on keeps every `u64` column binding exactly as it did. No call
site changed, and the hard gate passed unchanged.

`ffi-sqlite-wasm-rs` is enabled by `crates/sunrise-core-wasm/Cargo.toml` alone,
as a dependency of the wasm target only. On that target `rusqlite` binds
`sqlite-wasm-rs` 0.5 instead of `libsqlite3-sys`, and `bundled-sqlcipher` —
spelled `libsqlite3-sys?/bundled-sqlcipher` upstream — does nothing. The
storage layer's `PRAGMA key` and `PRAGMA kdf_iter` are unknown pragmas to
plain SQLite, which ignores them, so `sunrise-storage` opens the same way on
both targets. Native builds are untouched by the feature.

### 2. `sunrise-core` builds for `wasm32-unknown-unknown`, through three seams

- **tokio features.** The root `[workspace.dependencies] tokio` enabled
  `rt-multi-thread`, `fs`, `net` and `signal`. A member inherits a workspace
  dependency's features and cannot remove them, and `mio`, behind all four,
  refuses the wasm target. The root now carries only `macros`, `sync`, `time`
  and `io-util`; `sunrise-core` takes `rt`, and each native crate names the
  rest itself.
- **The process id.** `Core::open` recorded `std::process::id()` in the vault
  lock's owner file. It panics on this target; a browser worker has no process
  id, so the web build records 0.
- **The vault lock.** `VaultLock` is an OS file lock plus a process-local
  registry. There is no filesystem for the first half on this target, so the
  web build keeps only the registry, which still refuses a second `Core` in the
  same worker. Exclusivity across tabs is decision 4's.

The wall clock needed no seam: `CoreConfig::with_clock` already injects one,
and the web crate passes a clock that reads `Date.now()`.

### 3. A JSON seam, `crates/sunrise-core-wasm`

Four exports, the shape ADR-0012 named: `openVault`, `submitJson`,
`queryJson`, `closeVault`. A command is `sunrise_core::Command` and a query
`sunrise_core::Query` in `serde_json`'s externally tagged form, so the web adds
no vocabulary of its own for the core to drift from. The dispatch layer is
target-independent and tested natively against real SQLCipher; only the OPFS
install, the clock and the `wasm-bindgen` exports are wasm-only.

The database lives in OPFS through `sqlite-wasm-vfs` 0.2's `SyncAccessHandle`
pool VFS (0.3 moved to `sqlite-wasm-rs` 0.6, which `rusqlite` 0.40 does not
bind). That VFS works only in a dedicated worker, so the core runs in one:
`apps/web/src/core.worker.ts`. `apps/web/src/wasm.ts`'s `loadCore()` starts it
when the browser has workers, OPFS and `navigator.locks`, and falls back to the
`localStorage` stub when any is missing or the worker fails to open the vault.

The bundle is built by `mise run web-wasm` into `apps/web/public/wasm/`, which
is not tracked. A web build without it is the stub, as before.

### 4. The two web gaps ADR-0012 named are accepted, and stated

**The vault is not encrypted at rest.** `sqlite-wasm-rs` is SQLite without
SQLCipher, so `vault.db` sits in OPFS in plaintext, and the 32-byte vault root
the worker mints on first run is stored beside it in OPFS. Everything the
native vault protects at rest — task titles, notes, the device's private keys
— is readable by anything that can read this origin's OPFS: the browser
itself, its extensions with storage access, and anyone with the device's
unlocked user account. The gap is local only: the web client does not sync
yet, and when it does its ops are sealed by the same core that seals native
clients' ops, so nothing it sends is weaker for it.

What the product says to a user, wherever the web client can hold data:

> Sunrise on the web stores your tasks on this device **without encryption**.
> Anyone who can use this browser profile can read them. For encryption on
> this device, use the Sunrise app.

Once the web client syncs, the notice gains one sentence: *What leaves this
device is still end-to-end encrypted.* It must not say so before then, when
nothing leaves.

That notice is not yet in the web client: it needs a string-catalog entry
(ADR-0054), and is tracked with the rest of the web client's unencrypted-vault
UI.

**One tab holds the vault.** Two tabs writing one SQLite file through the
SAHPool VFS would corrupt it, and the file lock native clients take has no
filesystem here. The worker takes an exclusive `navigator.locks` lock named
`sunrise-vault` before it opens the vault and holds it for its lifetime. A
second tab's worker waits on that lock and opens the vault when the first tab
closes; it does not fall back to the stub, which would show it different data.
The read-only-view tabs `docs/07-clients/web.md` describes are not built.

## Alternatives considered

| Option | Why rejected |
|---|---|
| Encrypt at rest now with SQLite3MultipleCiphers (`sqlite-wasm-rs`'s `sqlite3mc` feature) | Encryption with a key stored in the same OPFS as the database protects nothing. It is worth doing once the web client has the passphrase unlock `docs/07-clients/web.md` specifies, which keeps the key out of storage; until then it would only make the gap harder to see. |
| A second tab falls back to the `localStorage` stub | It would show the user a different, empty task list under the same product, and write to it. Waiting is slower and never wrong. |
| A second tab is refused outright with an error | Needs a user-visible string, and loses the hand-over when the first tab closes, which the lock gives for free. |
| `sqlite-wasm-vfs` 0.3 / `sqlite-wasm-rs` 0.6 | `rusqlite` 0.40 binds `sqlite-wasm-rs` 0.5; two SQLite builds in one wasm module would not link. |
| Leave tokio's root features alone and give `sunrise-core` its own tokio | A workspace dependency's features are additive; a crate cannot opt out of what the root enables, so every path to `sunrise-core` would still carry `mio`. |

## Consequences

- The web client can run the real core. It does so only where the wasm bundle
  was built and served; otherwise it is the stub, as it was.
- Attachments do not work on the web yet: the blob store is plain `std::fs`,
  which this target does not have. Adding an attachment fails with an error; it
  does not panic.
- Sync does not run on the web yet. The SSE transport is `hyper` over `tokio`
  sockets, which this target does not have, and `Core::start_sync` needs a
  tokio runtime the worker does not run. The seam opens a local vault only.
- CI builds `sunrise-core-wasm` for `wasm32-unknown-unknown` and lints it, so a
  change that makes the core unbuildable for the web goes red.
- The vault root is not recoverable on the web: clearing the origin's storage
  loses the vault, and there is no pairing or recovery flow to restore it from.
