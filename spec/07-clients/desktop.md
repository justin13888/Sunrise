---
status: accepted
---

# Desktop Client

Tauri 2 application. Shell is Rust; UI is React (TS). The Sunrise core is statically linked into the Tauri backend.

Targets: macOS 13+, Windows 10+ (1809+), recent mainstream Linux distros (glibc 2.31+).

## Why Tauri (not Electron)

- Native webview (smaller footprint, faster cold start).
- Rust backend integrates the Sunrise core directly with no FFI overhead.
- Memory profile is dramatically smaller than Electron — important when the app may run for weeks.

## Architecture

```
┌─────────────────────┐
│   Tauri WebView     │ React + TS UI
└──────────┬──────────┘
           │ Tauri IPC (typed commands)
┌──────────▼──────────┐
│ Tauri backend (Rust)│
│  ↳ sunrise-core     │ in-process
│  ↳ OS integration   │ tray, hotkeys, notifications
└─────────────────────┘
```

## Platform-specific concerns

### macOS

- **Menu bar item.** Always-on small icon with quick capture, today snapshot, sync status.
- **Touch Bar** (older Intel): MAY surface focus mode timer and quick capture.
- **Spotlight integration.** Use `NSUserActivity` to register tasks for Spotlight search (only their *titles*; Spotlight is local-only on Apple devices). Optional, opt-in.
- **Continuity Camera.** When attaching from a Mac, support iPhone scanning.
- **Sandboxing.** Mac App Store build is sandboxed; direct DMG build is not (preferred for global hotkey reliability).
- **Accessibility permission** required for global hotkey.
- **Notification Center** for reminders, with action buttons.
- **Stage Manager / Mission Control:** the app is a standard window; no special behavior.
- **Apple Silicon and Intel:** universal binary.

### Windows

- **System tray** with quick capture and today snapshot.
- **Jump Lists** for recent Streams.
- **Toast notifications** with action buttons via Windows Notification Service.
- **Global hotkeys** via `RegisterHotKey`.
- **AppX / MSIX packaging** for Microsoft Store.
- **Hibernation / sleep** handling: WS reconnect on resume; flush outbox.

### Linux

- **AppIndicator / status icon** where supported (varies by DE).
- **D-Bus notifications** via `org.freedesktop.Notifications`.
- **Global hotkey** via the desktop-environment portal (`xdg-desktop-portal`); falls back to per-DE methods (Hyprland, Sway, GNOME, KDE) where the portal is unavailable.
- **AppImage / Flatpak / .deb / .rpm** distributions.
- **Wayland and X11** both supported; Wayland-only features (e.g. window restore) gracefully degrade.

## Key responsibilities of the desktop UI shell

- Present quick capture window on hotkey.
- Maintain a long-running WS sync connection while running.
- Schedule reminders via OS APIs based on intents emitted by the core.
- Read/write OS keystore for unlock material:
  - macOS: Keychain.
  - Windows: DPAPI (per-user).
  - Linux: Secret Service API (libsecret); fallback to passphrase if absent.
- Manage clipboard, drag-and-drop, file attachments.

## Update channel

- Auto-update via Tauri's updater, signed releases.
- Channels: `stable`, `beta`, `nightly`.
- Update applies on next launch; a running session is never interrupted by an update.

## Multi-window

- One main window.
- Detached "focus mode" window (always-on-top, compact).
- Optional second window for "stream view side-by-side."
- Capture is its own borderless window.

## Telemetry

Off by default. If user opts in: minimal anonymous metrics (launches, crash reports). Crash reports never include vault content — see [`../10-cross-cutting/telemetry-and-privacy.md`](../10-cross-cutting/telemetry-and-privacy.md).
