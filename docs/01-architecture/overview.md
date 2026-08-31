---
status: accepted
---

# Architecture Overview

Sunrise is split into four cleanly bounded layers. Layers communicate only through the interfaces immediately above/below them.

```
┌──────────────────────────────────────────────────────────────────┐
│  Presentation                                                    │
│  Per-platform UI: macOS (SwiftUI) and the `sunrise` CLI (Rust);  │
│  iOS / Android / Web deferred. Owns: layout, input, native APIs. │
└──────────────────────────────────────────────────────────────────┘
                                │  uses
                                ▼
┌──────────────────────────────────────────────────────────────────┐
│  Shared Core (Rust, statically linked or WASM)                   │
│  Domain logic, op log + merge, crypto, query engine, sync state  │
│  machine, local storage. Pure, deterministic, headless.          │
└──────────────────────────────────────────────────────────────────┘
                                │  speaks
                                ▼
┌──────────────────────────────────────────────────────────────────┐
│  Sync Protocol                                                   │
│  Encrypted op transport over WebSocket (HTTP/2 long-poll fallback).│
│  Server is a relay + blob store. Cannot read content.            │
└──────────────────────────────────────────────────────────────────┘
                                │
                                ▼
┌──────────────────────────────────────────────────────────────────┐
│  Server                                                          │
│  Authn for sync, encrypted relay, blob storage, push fanout,     │
│  rate limiting. Stateless except for blob store. Self-hostable.  │
└──────────────────────────────────────────────────────────────────┘
```

## What lives where

| Concern | Layer | Why |
|---|---|---|
| Domain entities, validation | Core | Single source of truth; testable without UI |
| Ops, merge | Core | Determinism is critical |
| Crypto, key handling | Core | Audit surface stays small |
| Local DB schema | Core | UI doesn't touch SQLite directly |
| Sync state machine | Core | Reconnect logic is too subtle to duplicate per-platform |
| Notifications scheduling | Per-platform UI | Each OS has its own scheduler |
| Filesystem / keychain | Per-platform UI | Each OS has its own APIs; core gets injected handles |
| Background sync | Per-platform UI | iOS BGTasks vs Android WorkManager vs systemd vs browser SW |
| Rendering | Per-platform UI | Native look-and-feel per platform |

## Why a shared core in Rust

See [`../11-adr/0002-shared-core-rust.md`](../11-adr/0002-shared-core-rust.md). Summary:

- We need *one* implementation of merge, crypto, and sync — not four.
- It must run inside a macOS app (Swift via UniFFI), a CLI (Rust native), and — when they are scheduled — iOS (Swift via UniFFI), Android (Kotlin via UniFFI) and the browser (WASM).
- Rust gives us memory safety, deterministic builds, and one ecosystem (RustCrypto, rusqlite, sqlcipher).

## What the core exposes

A narrow, async, message-passing API. No callbacks across the FFI boundary except for **two** push channels: domain change events, and sync status events.

```rust
// Pseudocode — see 01-architecture/shared-core.md for the full surface
pub struct Core { /* … */ }

impl Core {
    pub async fn open(vault_path: &Path, unlock: Unlock) -> Result<Core>;
    pub async fn submit(&self, cmd: Command) -> Result<CommandResult>;
    pub async fn query(&self, q: Query) -> Result<QueryResult>;
    pub fn changes(&self) -> Stream<DomainEvent>;
    pub fn sync_status(&self) -> Stream<SyncStatus>;
    pub async fn close(self) -> Result<()>;
}
```

UI layers do not call SQLite, do not invoke crypto, and do not parse the wire protocol. They send commands and render queries.

## Trust boundaries

1. **User device ↔ user device (same identity).** Mutually trusted via paired device keys.
2. **User device ↔ server.** Server is *untrusted for content* (E2EE) and *trusted for delivery* (best-effort relay; degrades gracefully).
3. **User identity ↔ another user's identity (sharing).** Each side authenticates the other; shared documents have their own access keys.
4. **UI ↔ Core.** Same process. UI is trusted with plaintext (it has to render it).

See [`threat-model.md`](./threat-model.md) for what each boundary defends against.
