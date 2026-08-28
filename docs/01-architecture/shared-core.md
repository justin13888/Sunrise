---
status: accepted
---

# Shared Core

The shared core is a Rust crate (`sunrise-core`) that is the only place where the following exist:

- Domain types and validation
- CRDT engine and op log
- Crypto (key derivation, envelope encryption)
- Local persistence (SQLite + SQLCipher)
- Sync state machine and wire codec
- Query engine

Per-platform UI consumes the core; nothing else does.

## Distribution

| Platform | Form | Status |
|---|---|---|
| macOS | `SunriseCore.xcframework` via UniFFI bindings (`just macos-xcframework`) | v1 |
| CLI | Linked directly into the `sunrise` binary | v1 |
| iOS | `xcframework` via UniFFI bindings — same seam, unproven slices | deferred |
| Android | `.aar` via UniFFI bindings (JNI) — same seam | deferred |
| Web | `wasm-bindgen` build, loaded as ES module | deferred ([ADR-0012](../11-adr/0012-web-wasm-deferred.md)) |

UniFFI is used for every native binding because it generates idiomatic
Swift/Kotlin async APIs from one annotated crate
([ADR-0019](../11-adr/0019-swiftui-macos-client.md)); the seam lives in
`crates/sunrise-core-bindings`, and `sunrise-domain` carries no uniffi
dependency. WASM would use `wasm-bindgen` directly because UniFFI's WASM story
is immature.

## Public surface (sketch)

```rust
pub struct Core { /* opaque */ }

#[derive(Clone, Debug)]
pub enum Unlock {
    Passphrase(SecretString),
    DevicePaired,           // device key already on disk
    RecoveryCode(String),   // restore from recovery
}

#[derive(Debug)]
pub enum Command {
    CreateTask(TaskDraft),
    UpdateTask { id: TaskId, patch: TaskPatch },
    CompleteTask(TaskId),
    DeferTask { id: TaskId, to: ScheduledAt },
    DeleteTask(TaskId),
    CreateStream(StreamDraft),
    // … (one variant per user-visible action)
    PromoteToStream { id: TaskId, stream: StreamId },
    AttachNote { target: EntityRef, body: NoteBody },
    PairDevice(PairingChallenge),
    RevokeDevice(DeviceId),
    // …
}

#[derive(Debug)]
pub enum Query {
    Today { now: jiff::Timestamp, contexts: Vec<ContextId> },
    Inbox,
    StreamView { stream: StreamId, filter: FilterSpec },
    Search(SearchSpec),
    EntityById(EntityRef),
    DeviceList,
    SyncStatus,
}

impl Core {
    pub async fn open(cfg: CoreConfig, unlock: Unlock) -> Result<Core>;
    pub async fn submit(&self, cmd: Command) -> Result<CommandResult>;
    pub async fn query(&self, q: Query) -> Result<QueryResult>;
    pub fn changes(&self) -> impl Stream<Item = DomainEvent> + Send;
    pub fn sync_status(&self) -> impl Stream<Item = SyncStatus> + Send;
    pub async fn export(&self, opts: ExportOptions) -> Result<ExportArchive>;
    pub async fn close(self) -> Result<()>;
}
```

Datetime types (`jiff::Timestamp` for absolute instants, `jiff::Zoned` / `jiff::civil::*` for wall-clock semantics) follow [ADR-0011](../11-adr/0011-datetime-jiff.md).

## Determinism rules

1. **No clock access except via `CoreConfig::clock`.** Tests inject a fake clock. Wall-clock time is *never* read directly in the core.
2. **No filesystem access except via `CoreConfig::storage`.** Memory-backed storage for tests; OS-specific paths for production.
3. **No randomness except via `CoreConfig::rng`.** Tests can seed; production uses OS RNG.
4. **No threads spawned except by the core's own runtime.** UI calls into core-owned tokio runtime. No global state.

These rules are enforced by `#![forbid(unsafe_code)]` plus a `clippy.toml` deny list (`std::time::SystemTime::now`, `rand::thread_rng`, `std::fs::*`, …) plus a CI grep.

Determinism is **per-device**, and applies to the bytes that leave the device. The op-log encoding (CBOR bytes the device emits over the wire) is bit-for-bit identical for identical input on the same device, time, and RNG seed. SQLite's WAL behavior is allowed to vary across runs; storage internals are not part of the determinism contract. With both `CoreConfig::clock` and `CoreConfig::rng` fixed, op-emit byte sequences are reproducible — this is the basis for sync-protocol round-trip tests.

## Threading

- One `tokio` multi-thread runtime owned by the core.
- All `submit` and `query` calls are `async` and may execute concurrently.
- Internal sync state machine runs as a long-lived task.
- UI callbacks (`changes`, `sync_status`) are pushed via bounded channels; UI must drain.

## Single-writer guarantee

Exactly **one** `Core` instance per vault path per process. Multiple processes attempting to open the same vault is a supported scenario:

- The vault directory contains `core.lock`, a zero-byte OS-level advisory lock target acquired with `flock(LOCK_EX|LOCK_NB)` on Unix and `LockFileEx(LOCKFILE_EXCLUSIVE_LOCK)` on Windows, via the `fs4` crate. It is created once and **never unlinked** — unlinking would reintroduce an ABA race in which two holders lock different inodes and both succeed. The kernel releases the lock when the holding process dies by any means, including `SIGKILL` and `abort()`, so a crash cannot strand the vault.
- `flock` is deliberate rather than `fcntl`: `fcntl` locks are owned by the *process* and are dropped when any descriptor for the file is closed, so a contender merely reading holder metadata would destroy the holder's lock. `flock` locks are owned by the open file description.
- Because `flock` degrades to per-process `fcntl` semantics over NFS and is a no-op on some FUSE filesystems, same-process exclusion is enforced separately by a process-local registry of canonicalized vault paths. That registry, not the OS lock, is the authority for the one-Core-per-vault-per-process invariant.
- Acquisition retries for ~250 ms (13 attempts, 20 ms apart). On timeout, `Core::open` returns `CoreError::VAULT_LOCKED { holder_pid, holder_started_at }`, read from `<vault>/core.lock.owner` — a separate advisory payload file. The identity cannot live in `core.lock` itself because Windows `LockFileEx` is mandatory on the locked range, so a contender could never read it. The payload is best-effort and non-authoritative: it exists only for the error message. The caller decides whether to retry or surface the error.
- The lock file contents are the holding process's PID and ISO 8601 start timestamp (≤ 64 bytes), rewritten on each open. They are **not** authoritative — they exist only for the error message — the lock itself is the OS lock.
- There is no core daemon and no second process. The macOS app holds the vault lock for as long as it runs; `sunrise` is one-shot and releases it on exit. Two long-lived writers against one vault would need a daemon, which is a whole subsystem to buy something neither client needs.
- On crash, the OS releases the lock; recovery is a normal unclean-shutdown reopen.

`submit` calls within a single `Core` are **per-entity serialized**: the core acquires an in-memory lock keyed by `(stream_id, entity_id)` before applying. Cross-entity calls run concurrently. There is no global submit serialization — concurrent calls on different entities apply in parallel and commit in arrival order. CRDT merge guarantees convergence regardless of arrival order.

## Errors

`thiserror`-defined `enum CoreError`, with stable variant codes carried over FFI. UI translates codes into user-facing copy. Core never produces user-facing strings.

## Versioning

The core's public API is semver-versioned. Breaking changes require a coordinated bump across all client crates. See [`../02-domain/schema-versioning.md`](../02-domain/schema-versioning.md) for the *data* schema versioning, which is independent.

## Build / CI

- `cargo test` — unit tests (no platform deps).
- `cargo test --features integration` — full integration (multi-device sync simulation, real SQLCipher).
- Cross-compile targets in CI: `aarch64-apple-darwin`, `aarch64-apple-ios`, `aarch64-linux-android`, `wasm32-unknown-unknown`, `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`.
- Reproducible builds: pinned toolchain (`rust-toolchain.toml`), vendored deps for releases.

## What the core does *not* do

- It does not decide UI text, copy, or icons.
- It does not schedule OS notifications (it produces the *intent*; the UI registers it with the OS).
- It does not access OS-level secret stores directly. The UI fetches the unlock material and passes it in.
- It does not initiate network — except via injected transport handles. UI/platform layer owns sockets.
