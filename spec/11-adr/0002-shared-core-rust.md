# 0002 — Shared core in Rust

**Status:** accepted

## Context

Sunrise must run on five clients (desktop, iOS, Android, web, TUI) with identical behavior for: domain logic, CRDT merge, crypto, query engine, sync state machine, local persistence. Duplicating any of these per platform is a maintenance and correctness disaster, especially for the crypto and CRDT layers.

## Decision

Implement the shared core as a Rust crate (`sunrise-core`). Distribute via:

- Tauri-linked native (desktop).
- UniFFI-generated Swift / Kotlin bindings (iOS / Android).
- `wasm-bindgen` build (web).
- Native binary (TUI).

UI per platform consumes the core through this single API.

## Alternatives considered

| Option | Pros | Cons |
|---|---|---|
| TypeScript everywhere (one codebase, ports per platform) | One language for UI and core | Crypto in JS is a pain; mobile bridging is painful; perf insufficient |
| C++ core | Maximum perf | Memory safety concerns; CRDT libs are weaker |
| Kotlin Multiplatform | Single language for shared code + Android-native | Worse iOS / web / TUI story; no good CRDT lib |
| Swift cross-platform | Apple-native | Linux/Windows/Web stories are weak |
| **Rust core** | Memory-safe, mature crypto/CRDT, builds for everything we care about | Higher onboarding cost; less common skill |

## Consequences

- Single source of truth for security-critical and merge-critical code.
- One bug fix benefits every client at once.
- Per-platform UI cannot diverge in domain semantics; they're forced to agree.
- We need a UniFFI-experienced developer for Swift/Kotlin maintenance.
- Build matrix is wider (cross-compile to many targets in CI).
- WASM is a real target, not an afterthought; some Rust idioms (filesystem, threads) require care to port.
