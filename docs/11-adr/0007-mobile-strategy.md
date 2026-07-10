# 0007 — Native-per-platform UI vs shared UI framework

**Status:** accepted

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
