---
status: accepted
---

# Clients — Overview

Sunrise has five first-class clients. Each consumes the same shared core ([`../01-architecture/shared-core.md`](../01-architecture/shared-core.md)) and speaks the same sync protocol. Each owns its platform's idioms, native APIs, and visual design.

| Client | Tech | Spec |
|---|---|---|
| Desktop (macOS / Windows / Linux) | Tauri 2 + React + Rust core | [`desktop.md`](./desktop.md) |
| iOS / iPadOS | Swift + SwiftUI + UniFFI core | [`mobile-ios.md`](./mobile-ios.md) |
| Android | Kotlin + Jetpack Compose + UniFFI core | [`mobile-android.md`](./mobile-android.md) |
| Web (PWA) | React + WASM core | [`web.md`](./web.md) |
| TUI | Rust + Ratatui + Rust core | [`tui.md`](./tui.md) |

## Sub-specs in this directory

- [`parity-matrix.md`](./parity-matrix.md) — what each client must / should / may support.
- [`shared-ui-system.md`](./shared-ui-system.md) — design tokens and components shared cross-platform.
- [`interaction-patterns.md`](./interaction-patterns.md) — common gestures and keyboard idioms.

## What clients share

- The core (one library, many bindings).
- The wire protocol.
- The design tokens (colors, type scale, spacing).
- The mental model (Today, Streams, Inbox, Focus).

## What clients don't share

- UI implementation. Each is native to its platform.
- Specific keyboard shortcuts (HIG-conformant per OS).
- Background and lifecycle behaviors (vastly different per OS).
- Notification scheduling APIs.

## Why not React Native / Flutter / Compose Multiplatform?

We considered each. The combination of (a) deep native integration we want (lock screen widget, focus filters, Tasker, tray, etc.), (b) Tauri's mature desktop story, and (c) wanting native performance and feel won out. The shared core gets the de-duplication; the UI being native preserves quality.

Compose Multiplatform was the strongest contender. We may revisit it for a v2 if the shared-core approach proves too costly to maintain.

## Distribution

| Client | Channel |
|---|---|
| Desktop macOS | Mac App Store + direct DMG |
| Desktop Windows | Microsoft Store + direct MSI |
| Desktop Linux | AppImage + Flatpak + .deb / .rpm |
| iOS | App Store |
| Android | Play Store + F-Droid (FOSS variant) |
| Web | `https://app.sunrise.example/` (managed cloud); self-hosted at user-provided URL |
| TUI | Cargo, Homebrew, prebuilt binaries on GitHub releases |

## Versioning

Each client has its own version, independent of server version. Within a major version, all clients support the previous protocol version (N-1) and the current one. The CI runs end-to-end tests across version pairs.
