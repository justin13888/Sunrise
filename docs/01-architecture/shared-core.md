---
status: accepted
---

# Shared Core

The shared core is a Rust crate (`sunrise-core`) that is the only place where the following exist:

- Domain types and validation
- Merge engine (entity-level LWW, [ADR-0014](../11-adr/0014-entity-level-lww-merge.md)) and op log
- Crypto (key derivation, envelope encryption)
- Local persistence (SQLite + SQLCipher)
- Sync state machine and wire codec
- Query engine

Per-platform UI consumes the core; nothing else does.

## Distribution

| Platform | Form | Status |
|---|---|---|
| macOS | `SunriseCore.xcframework` via UniFFI bindings (`mise run apple-xcframework`) | shipping |
| CLI | Linked directly into the `sunrise` binary | shipping |
| iOS / iPadOS | `SunriseCore.xcframework` via UniFFI bindings — the same seam and the same `mise run apple-xcframework`, which builds the device and simulator slices beside the macOS one | **shipping** ([ADR-0028](../11-adr/0028-ios-is-a-v1-client.md)) |
| Android | `.aar` via UniFFI bindings (JNI) — same seam | not built |
| Web | `crates/sunrise-core-wasm`, a `wasm-bindgen` build loaded as an ES module (`mise run web-wasm`), running the core over plaintext SQLite in OPFS | built, **local only**: no relay and no sync ([ADR-0055](../11-adr/0055-web-wasm-core.md), superseding [ADR-0012](../11-adr/0012-web-wasm-deferred.md)) |

UniFFI is used for every native binding because it generates idiomatic
Swift/Kotlin async APIs from one annotated crate
([ADR-0019](../11-adr/0019-swiftui-macos-client.md)); the seam lives in
`crates/sunrise-core-bindings`, and `sunrise-domain` carries no uniffi
dependency. The web seam uses `wasm-bindgen` directly because UniFFI's WASM
story is immature.

## The client layer

Between the core and a user interface sits `sunrise-client-core`: the client
logic that is not any one client's. Undo and redo by inverse command, saved
views, the bootstrap preferences file and the client settings beside it, the
edit gate, the shared vocabulary read in the caller's zone, the reminder
schedule and its reconcile against the OS, the Focus filter's rules, the
daily briefs' sections and the review's civil windows, the editors'
draft-to-patch functions, the defer and schedule targets, the calendar grid's
arithmetic, the command palette's ranking, a list's keyboard selection, the
note editor's block model, print pagination, the account bootstrap, the
recovery flow
([recovery.md](../03-crypto/recovery.md) §Recovery flow steps 3 to 8), the full
export's writer ([export-format.md](../04-storage/export-format.md)), and
background sync. Every client links it, and a seam's own code lowers its
records onto these functions and does nothing else:

```text
 sunrise-core-bindings (UniFFI)  sunrise-cli   sunrise-core-wasm
            \                       |            /  (no `relay`)
             +------> sunrise-client-core <-----+
                        |              |
                        |   (relay)    v
                        |      sunrise-relay-client --> http-sig
                        v
                   sunrise-core --> sunrise-domain, sunrise-sync, ...
```

What the Apple app still decides in Swift is moving here file by file.
`.github/scripts/swift-logic-gate.py` (`mise run swift-logic`) keeps it from
growing meanwhile: every Swift file under `apps/apple` that holds anything but
views is on its `ALLOWED` list with the reason, the list only shrinks, and a
new file with logic in it fails.

The REST client sits below the client layer and names no engine: it is the
generated client plus the request shapes and encodings it puts on the wire. The
flows that put a vault and the relay together call down into it, never the
reverse. The `relay` feature gates everything in the client layer that links the
REST client or the SSE transport, so the web core links the pure half.

Beside those, the client layer holds what a screen decides that is not a
screen's own, as codes and data rather than sentences, so each surface words
it: the relay URL rule (below), the sync badge and the sync plan
(`sync_status`), `sunrise://` links (`links`), what a share becomes
(`share`), which copy of a secret survives a keychain migration
(`secret_copies`), and three state machines with their IO injected — a pairing as a
screen walks it (`pairing_flow`, over the relay rendezvous in `pairing`), the
vault session's lifecycle (`vault_session`), and the OIDC session's sign-in,
renewal and sign-out (`account_session`, over `sunrise-auth`'s credential).
Each transition returns the seam calls to make and takes their answers back,
which is what lets every client walk the same script and a test walk it
without one.

The client layer keeps the core's determinism rules below: time is the core's
injected clock or a zone name the caller passes, randomness an injected `Rng`,
and a file a path the caller names.

What a client writes about time goes through it too, so no client re-derives a
kind ([ADR-0066](../11-adr/0066-times-survive-travel.md) §4):

- `editor` turns a task, block or routine editor's form into the patch that
  carries only the fields that changed. A time the user left alone is not
  sent, and one they moved keeps its kind (an all-day deadline stays a date, a
  floating plan stays floating, a zoned block keeps its own zone).
- `schedule` computes defer, snooze, schedule and the Today capture default
  in civil days in the reader's zone, over
  `crates/sunrise-domain/src/time/shift.rs#shift_civil_days`, and emits
  `Command::DeferTaskTo` with the plan's kind rather than an instant.
- `calendar` holds the grid: its civil days and week start (the rule
  `Query::WeekBlocks` reads, `crates/sunrise-domain/src/time/week.rs#first_day_of_week`),
  the kind a drawn, dropped or dragged block is written as, lane layout,
  snapping and drag clamping, and the Resolve menu's merge.
- `civil` is what the three share: a picked moment written as a kind, in a
  zone the caller names. An unknown zone name is an error here, not UTC
  ([ADR-0066](../11-adr/0066-times-survive-travel.md) §3).

### The relay URL rule

A relay URL is where a client sends the account bearer, so there is one rule
for what counts as one (`crates/sunrise-client-core/src/relay_url.rs`):
`https://`, or `http://` only to a loopback host (`localhost`, `127.0.0.0/8`,
`::1`); an origin and nothing after the port, since the relay client appends
its own paths and the pairing QR names an origin; at most 256 bytes, the QR's
bound; and never `ws://`/`wss://`, which is refused with its own code because
sync left the WebSocket (ADR-0023). It replaced three that disagreed: the Apple
sync plan took any `http(s)://` string, path and all, the pairing seam took a
path, and the web client took only an origin. The web client restates it in
TypeScript for its synchronous start-up read, and both are tested against
`crates/sunrise-client-core/vectors/relay-url.json`.

## Public surface (sketch)

```rust
pub struct Core { /* opaque */ }

// crates/sunrise-core/src/unlock.rs. Every arm holds key material, so its
// `Debug` names the mode and nothing else.
pub enum Unlock {
    // The caller derived the vault root and passes it in.
    Passphrase(VaultRootKey),
    // `paired` is `Some` exactly once: the first open of a device being paired.
    DevicePaired { root: VaultRootKey, paired: Option<Box<PairingPayload>> },
    // A fresh root this device mints, plus the identity the recovery blob carried.
    RecoveryCode { root: VaultRootKey, identity: Box<RecoveryPayload> },
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
    pub async fn export(&self, opts: ExportOptions) -> Result<ExportArchive>;  // target state
    pub async fn close(self) -> Result<()>;
}
```

Datetime types (`jiff::Timestamp` for absolute instants, `jiff::Zoned` / `jiff::civil::*` for wall-clock semantics) follow [ADR-0011](../11-adr/0011-datetime-jiff.md).

## Determinism rules

1. **No clock access except via `CoreConfig::clock`.** Tests inject a fake clock. Wall-clock time is *never* read directly in the core.
2. **No filesystem access except through `CoreConfig::vault_dir`.** The core touches only the vault directory the caller names: a temporary directory in tests, an OS-specific path in production.
3. **No randomness except via `CoreConfig::rng`.** Tests can seed; production uses OS RNG.
4. **No threads spawned except by the core's own runtime.** UI calls into core-owned tokio runtime. No global state.

These rules are enforced by `#![forbid(unsafe_code)]` plus a `clippy.toml` deny
list (`std::time::SystemTime::now`, `std::time::Instant::now`,
`std::process::exit`, `rand::thread_rng`, `rand::random`) and two checks in
`ci.yml`'s `Determinism gate` job. The coverage is uneven, and it is worth
knowing where:

| rule | enforced by |
|---|---|
| 1 — clock | `clippy.toml`, for the `std::time` spellings, plus the `MonotonicClock` seam in `sync_driver` for the scheduling deadlines the path-based lint does not match. The lint still does not match `tokio::time::Instant::now`, and that is deliberate: a deadline is not wall-clock time. It must survive an NTP step, and `tokio::time::sleep_until` consumes a monotonic `Instant` and nothing else — so `CoreConfig::clock` is the wrong seam for it, and `sync_driver::MonotonicClock` is the right one. `TokioClock` is the only production implementor; the driver's `run` constructs it, so no caller supplies a clock. **What this bought** ([#207](https://github.com/justin13888/Sunrise/issues/207)): the resync deadline, its `MIN_RESYNC_GAP` floor, the loss-evidence pull-forward and the per-batch retransmit sweep are now driven to exact instants by a hand-moved clock in `sync_driver`'s `scheduling` module, and the reconnect schedule — including `backoff_sleep`'s reset arm, the flat un-jittered 30 s [`../05-sync/offline-queue.md`](../05-sync/offline-queue.md) documents as load-bearing and which previously had no test because reaching it meant sleeping thirty seconds — is asserted through `next_backoff_delay`, a pure function that needs no clock at all. None of those tests sleep. **What remains:** the relay-harness tests are still `#[tokio::test(flavor = "multi_thread")]` and so still cannot use `start_paused`; two 400 ms sleeps and three 10 s polling loops survive in them. Those wait on a *fake server*, not on a deadline, which is why they were not the part worth fixing. |
| 2 — filesystem | `core-filesystem-gate.py`, scoped to `crates/sunrise-core/src`, with a visible allowlist of the two documented exceptions. Its own exit-code and coverage contract is asserted by `test_core_filesystem_gate.py`, because a scanner that stops reading early reports OK either way. |
| 3 — randomness | `clippy.toml` plus a CI grep, because path-based lints miss turbofish and aliasing. The one rule with two independent layers. |
| 4 — threads | Nothing. Honoured in practice and checked by review. |

**Rule 2's two exceptions.** `vault_lock.rs` holds the OS advisory lock on
`<vault>/core.lock` — the cross-process half of the single-writer guarantee,
which opens that file in `CoreConfig::vault_dir` with `std::fs` directly because the point is a lock the OS
releases on process death by any means. `config.rs` reads `/etc/localtime` to
recover the host's IANA zone *name*; `TimeZone::system()` returns a nameless
zero-offset zone on macOS, so the previous fallback made every Mac claim to be in
UTC. Both are named in the gate with their reasons, and that list may shrink but
not grow.

Rule 2 is **not** in `clippy.toml`, and cannot be: that file is workspace-wide
with no per-crate scoping, and many of the other crates here touch the
filesystem on purpose. A `std::fs` entry would fire in crates the rule was
never about, and quieting them takes an `#[allow]` at every legitimate call site
— which is the warning-noise failure `clippy.toml`'s own `rand` comment
describes. The gate is scoped the way the rule is scoped: one crate.

Determinism is **per-device**, and applies to the bytes that leave the device. The op-log encoding (CBOR bytes the device emits over the wire) is bit-for-bit identical for identical input on the same device, time, and RNG seed. SQLite's WAL behavior is allowed to vary across runs; storage internals are not part of the determinism contract. With both `CoreConfig::clock` and `CoreConfig::rng` fixed, op-emit byte sequences are reproducible — this is the basis for sync-protocol round-trip tests.

## Threading

- One `tokio` multi-thread runtime owned by the core.
- All `submit` and `query` calls are `async`; each holds the one database lock while it touches the vault (§Single-writer guarantee).
- Internal sync state machine runs as a long-lived task.
- UI callbacks (`changes`, `sync_status`) are pushed via bounded channels; UI must drain.

## Single-writer guarantee

Exactly **one** `Core` instance per vault path per process. Multiple processes attempting to open the same vault is a supported scenario:

- The vault directory contains `core.lock`, a zero-byte OS-level advisory lock target acquired with `flock(LOCK_EX|LOCK_NB)` on Unix and `LockFileEx(LOCKFILE_EXCLUSIVE_LOCK)` on Windows, via `std::fs::File::try_lock` (stable since Rust 1.89; the `fs4` crate stood in for it until the toolchain pin moved — [ADR-0026](../11-adr/0026-msrv-bump.md)). It is created once and **never unlinked** — unlinking would reintroduce an ABA race in which two holders lock different inodes and both succeed. The kernel releases the lock when the holding process dies by any means, including `SIGKILL` and `abort()`, so a crash cannot strand the vault.
- `flock` is deliberate rather than `fcntl`: `fcntl` locks are owned by the *process* and are dropped when any descriptor for the file is closed, so a contender merely reading holder metadata would destroy the holder's lock. `flock` locks are owned by the open file description.
- Because `flock` degrades to per-process `fcntl` semantics over NFS and is a no-op on some FUSE filesystems, same-process exclusion is enforced separately by a process-local registry of canonicalized vault paths. That registry, not the OS lock, is the authority for the one-Core-per-vault-per-process invariant.
- Acquisition retries for ~250 ms (13 attempts, 20 ms apart). On timeout, `Core::open` returns `CoreError::VAULT_LOCKED { holder_pid, holder_started_at }`, read from `<vault>/core.lock.owner` — a separate advisory payload file. The identity cannot live in `core.lock` itself because Windows `LockFileEx` is mandatory on the locked range, so a contender could never read it. The payload is best-effort and non-authoritative: it exists only for the error message. The caller decides whether to retry or surface the error.
- The lock file contents are the holding process's PID and ISO 8601 start timestamp (≤ 64 bytes), rewritten on each open. They are **not** authoritative — they exist only for the error message — the lock itself is the OS lock.
- There is no core daemon and no second process. The macOS app holds the vault lock for as long as it runs; `sunrise` is one-shot and releases it on exit. Two long-lived writers against one vault would need a daemon, which is a whole subsystem to buy something neither client needs.
- On crash, the OS releases the lock; recovery is a normal unclean-shutdown reopen.

Within a single `Core` there is **one writer lock**: the `Mutex<Db>` in `crates/sunrise-core/src/core/mod.rs`. `submit` holds it across `engine.apply`, and applying a remote batch holds it the same way, so writes are serialised whatever entity they touch and commit in the order they take the lock; reads take the same lock. The LWW comparison key is carried on each op, so convergence does not depend on that order.

## Errors

`thiserror`-defined `enum CoreError` (`crates/sunrise-core/src/core/mod.rs`), wrapping each layer's own error. Over FFI, `sunrise-core-bindings` flattens it into `BindingError`, whose variant a client branches on; no `ErrorCode` crosses the boundary yet. [`error-handling.md`](../10-cross-cutting/error-handling.md) §Errors across the core → UI boundary says what does. Core never produces user-facing strings.

## Versioning

The core's public API is semver-versioned. Breaking changes require a coordinated bump across all client crates. See [`../02-domain/schema-versioning.md`](../02-domain/schema-versioning.md) for the *data* schema versioning, which is independent.

## Build / CI

- `cargo test --workspace --all-targets` — unit and integration tests, multi-device sync simulation (`sunrise-e2e`) and real SQLCipher included; there is no feature flag to turn on.
- The targets CI builds are its jobs in `.github/workflows/ci.yml`: Linux, macOS and Windows test runs (`rust`, `rust-macos`, `rust-windows`), the macOS, iOS device and iOS simulator slices (`apple-xcframework`), and a check-only `wasm32-unknown-unknown` lint of the web core (`wasm-core`). No Android target is built; `kotlin-smoke` compiles the Kotlin bindings on the host.
- Reproducible builds: pinned toolchain (`rust-toolchain.toml`), vendored deps for releases.

## What the core does *not* do

- It does not decide UI text, copy, or icons.
- It does not schedule OS notifications (it produces the *intent*; the UI registers it with the OS).
- It does not access OS-level secret stores directly. The UI fetches the unlock material and passes it in.
- It does not initiate network — except via injected transport handles. UI/platform layer owns sockets.
