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

| Platform | Form |
|---|---|
| Desktop (Tauri) | Linked Rust crate in the Tauri backend |
| iOS | `xcframework` via UniFFI bindings |
| Android | `.aar` via UniFFI bindings (JNI) |
| Web | `wasm-bindgen` build, loaded as ES module |
| TUI | Linked into the TUI binary |

UniFFI is used for mobile bindings because it generates idiomatic Swift/Kotlin async APIs. WASM uses `wasm-bindgen` directly because UniFFI's WASM story is immature.

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
    Today { now: DateTime<Utc>, contexts: Vec<ContextId> },
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

## Determinism rules

1. **No clock access except via `CoreConfig::clock`.** Tests inject a fake clock. Wall-clock time is *never* read directly in the core.
2. **No filesystem access except via `CoreConfig::storage`.** Memory-backed storage for tests; OS-specific paths for production.
3. **No randomness except via `CoreConfig::rng`.** Tests can seed; production uses OS RNG.
4. **No threads spawned except by the core's own runtime.** UI calls into core-owned tokio runtime. No global state.

These rules are enforced by `#![forbid(unsafe_code)]` plus a `clippy.toml` deny list (`std::time::SystemTime::now`, `rand::thread_rng`, `std::fs::*`, …) plus a CI grep.

## Threading

- One `tokio` multi-thread runtime owned by the core.
- All `submit` and `query` calls are `async` and may execute concurrently.
- Internal sync state machine runs as a long-lived task.
- UI callbacks (`changes`, `sync_status`) are pushed via bounded channels; UI must drain.

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
