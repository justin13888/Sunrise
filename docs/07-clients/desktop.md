---
status: accepted
---

# macOS Client

A native **SwiftUI** application in `apps/macos/`. The Sunrise core is a Rust
static library, reached through a UniFFI seam
([ADR-0019](../11-adr/0019-swiftui-macos-client.md)).

Target: **macOS 26**, Apple Silicon. Swift 6 with
`-strict-concurrency=complete`. Universal (Intel) is one `rustup target add`
and one entry in the justfile's `ffi_slices`; it is not built today.

> An earlier revision of this file specified a Tauri 2 + React app across
> macOS, Windows and Linux. That app never existed — there was no `main.rs`, no
> `tauri.conf.json`, and Tauri was not a dependency anywhere. ADR-0019 records
> the replacement and why.

## Architecture

```
┌──────────────────────────────┐
│ SwiftUI views                │  @Observable view models
└──────────────┬───────────────┘
               │ Swift actor over the handle
┌──────────────▼───────────────┐
│ SunriseCore  (UniFFI object) │  async open / submit / query / subscribe
└──────────────┬───────────────┘
               │ generated Swift + SunriseCore.xcframework
┌──────────────▼───────────────┐
│ sunrise-core-bindings (Rust) │
│  ↳ sunrise-core              │  in-process; no daemon, no IPC
└──────────────────────────────┘
```

There is **no separate core process**. The app holds the vault lock for as long
as it runs, which is also why the CLI is one-shot: two long-lived writers
against one vault would need a daemon, and a daemon is a whole subsystem to buy
something neither client needs.

### Build

```
just macos-xcframework    # cargo build → uniffi-bindgen → lipo → xcframework
just macos-app            # + xcodegen generate && xcodebuild
```

`project.yml` (XcodeGen) is committed; the generated `.xcodeproj` is not.
`out/` and `build/` are gitignored — the Swift bindings are generated from the
Rust source on every build, so committing them would let the two drift.

### Concurrency

* `SunriseCore.open` is an **async constructor**; `submit` and `query` are
  `async throws`. UniFFI supplies the tokio runtime.
* The change stream is a `ChangeListener` callback wrapped Swift-side in an
  `AsyncStream`. Callbacks arrive on a **tokio worker thread**, never the main
  actor; hop deliberately.
* **`onLagged` is not optional.** The broadcast channel behind the stream holds
  256 events and is lossy past that — a sync catch-up burst will overrun a slow
  consumer. Treat `onLagged` as "re-run every query this screen is showing".
  A bridge that implements only `onChange` shows stale data after every burst,
  silently.
* Coalesce repaints on a 50 ms window. The TUI proved that number out; below it
  a burst repaints per op for no visible benefit.
* Do **not** enable `SWIFT_UPCOMING_FEATURE_EXISTENTIAL_ANY`: UniFFI 0.32 does
  not emit `any`, and it produces 20 warnings in generated code. Strict
  concurrency itself is clean.

## Platform integration

- **Menu bar item** (`MenuBarExtra`). Quick capture, the daily snapshot, sync
  status. Refresh on launch, every 60 s while visible, and immediately on a
  relevant change event (debounced 500 ms).
- **Quick capture** — a borderless window on a global hotkey (⌘⇧N).
  It does **not** need the Accessibility permission. The hotkey is registered
  with Carbon's `RegisterEventHotKey`, which *reserves* one combination with
  the window server; the permission is only required by
  `NSEvent.addGlobalMonitorForEvents`, which observes every keystroke in every
  app. Reserving one chord is narrower and asks less of the user, so that is
  what `HotkeyCenter` does. An earlier revision of this file specified the
  permission, and the app's settings screen still displays its status with the
  caption "Not required for the shortcut above" precisely because this document
  said otherwise. Registration fails when another app already holds the chord;
  that is an ordinary state (`HotkeyStatus`), not an error — the menu bar item
  still opens capture.
- **Notification Center** for reminders, with action buttons.
  `docs/08-features/notifications.md` owns quiet hours and primary-device
  dedup; the app schedules from the intents the core emits.
- **Keychain** holds the unlock material. Nothing else does.
- **Spotlight** (`NSUserActivity`) for task *titles only*, decrypted on-device
  and indexed locally — Spotlight never sees plaintext via iCloud. Opt-in:
  a first-run prompt, and a Settings → Integrations toggle to change later.
- **Continuity Camera** for attaching a scan from an iPhone.
- **Drag and drop** — tasks between streams, files onto a task.
- **`sunrise://` URL scheme**, registered for notification deep links
  ([`interaction-patterns.md`](./interaction-patterns.md)).
- **Stage Manager / Mission Control**: a standard window, no special behaviour.

### Sandboxing

The direct `.dmg` build is **not** sandboxed. A sandboxed build cannot register
a reliable system-wide hotkey, and quick capture is the feature the persona
uses most. A Mac App Store build would have to trade that away; it is under
evaluation, not committed.

## Multi-window

- One main window.
- A detached, compact, always-on-top **focus** window.
- Quick capture is its own borderless window.
- Two-column stream comparison is **deferred**, not cut: it was specified for
  the Tauri app, nothing was built, and it is not a v1 MUST.

## Update channel

Sparkle-style signed updates over the direct channel, applied on next launch —
a running session is never interrupted by an update. Channels: `stable`,
`beta`.

## Telemetry

Off by default. If the user opts in: minimal anonymous metrics (launches, crash
reports). Crash reports never include vault content — see
[`../10-cross-cutting/telemetry-and-privacy.md`](../10-cross-cutting/telemetry-and-privacy.md).
