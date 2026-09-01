# 0007 — Native-per-platform UI vs shared UI framework

**Status:** accepted

**Amended:** the *principle* — native UI per platform, over one shared Rust core
— stands and is what the tree does. The **platform list in the Decision below
does not**: every entry on it has since been withdrawn or deferred by a later
ADR, and only one client the list never mentions is shipping. See the
**Amendment** at the end of this file for what changed and why the decision
itself did not need reopening.

## Context

We could implement mobile and desktop UI once with a cross-platform framework (React Native, Flutter, Compose Multiplatform, Tauri-everywhere) or build natively per platform.

## Decision

**Native UI per platform**, with the shared core (Rust) carrying domain, CRDT, crypto, and sync logic.

- Desktop: Tauri 2 + React + TS.
- iOS: SwiftUI.
- Android: Jetpack Compose.
- Web: React PWA.
- TUI: Ratatui.

## Alternatives considered

| Option | Why rejected |
|---|---|
| React Native | We want deep iOS/Android surface integration (lock screen widgets, Live Activities, Quick Settings, Tasker, Glance); RN's bridge complicates each. Performance ceiling is also lower for our list-heavy UI. |
| Flutter | Same surface-integration cost. Engine is heavyweight. App "feel" pulls toward Flutter-Material rather than native. |
| Compose Multiplatform | Strongest contender. Improves rapidly. Considered. Decided to revisit for v2 if maintaining native iOS becomes painful; for v1, native iOS via SwiftUI buys us tighter platform integration and a faster iteration loop on iOS-specific features. |
| Tauri everywhere (incl. mobile) | Tauri mobile is young; webview UX on mobile is consistently below native standards for the sort of tactile UI we want. |

## Consequences

- Three UI codebases (plus web, plus TUI).
- The shared core is the de-duplication mechanism — domain logic and crypto and CRDT are *not* duplicated.
- We get the best UX per platform.
- We invest in a UniFFI binding pipeline for iOS/Android.
- Per-platform engineers can move fast within their platform.
- New features touch multiple UIs; we keep features tightly scoped per release.

## Reversal path

If maintenance burden of three UIs becomes painful, the next likely move is **Compose Multiplatform** for iOS+Android (the two most expensive parallel codebases). Desktop, web, and TUI are unlikely to consolidate.

## Amendment (2026-09): the platform list is obsolete, the principle is not

**What the original text said.** The Decision named five surfaces: Tauri 2 +
React on desktop, SwiftUI on iOS, Jetpack Compose on Android, a React PWA on
web, and a Ratatui TUI. The Consequences section counted "three UI codebases
(plus web, plus TUI)".

**What changed.** Four of the five are gone or deferred, none of them by this
ADR:

* **Tauri desktop and the Ratatui TUI were both deleted.**
  [ADR-0019](./0019-swiftui-macos-client.md) supersedes
  [ADR-0006](./0006-tui-framework.md), replaces the Tauri specification in
  `docs/07-clients/desktop.md`, and makes a native SwiftUI app the v1 client.
  `apps/desktop` and `crates/sunrise-tui` are both removed from the tree. The
  Tauri shell had never run in the first place: it had no `main.rs`, no
  `tauri.conf.json`, and Tauri was not a dependency anywhere.
* **The React PWA is deferred.**
  [ADR-0012](./0012-web-wasm-deferred.md) leaves `apps/web` as a `localStorage`
  stub behind a `loadCore()` seam rather than a client.
* **Android has no client and no date.** ADR-0019 defers every platform but
  macOS; nothing in the tree targets Compose.

What *is* shipping is the surface this list never named: a SwiftUI app for macOS
and, since, one for iOS, both compiled from `apps/apple` against the same
`sunrise-core-bindings` seam.

**Why the decision nonetheless stands.** The list was the illustration; the
decision was the *rule* — build the UI natively per platform and let a shared
Rust core carry domain, merge, crypto and sync, so that platform count costs UI
work rather than duplicated logic. That rule is what the tree does, and ADR-0019
is an application of it rather than a departure from it: it consolidated the
client budget onto one platform's native toolkit precisely because the core was
already carrying everything that was not rendering. The two amendments the
Consequences section needs are arithmetic, not principle — there are two UI
codebases sharing most of their source, not "three plus web plus TUI", and the
UniFFI binding pipeline the list justified for iOS and Android is what macOS and
iOS actually run on.

The **Reversal path** below is unchanged and remains the live option: Compose
Multiplatform is still the first move if iOS and Android ever both need
maintaining.
