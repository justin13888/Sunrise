---
status: draft
---

# iOS Client

Native Swift / SwiftUI app. The Sunrise core ships as an `xcframework` via UniFFI bindings, statically linked.

Target: iOS 17+, iPadOS 17+. (Drop older versions in v2 if churn becomes painful.)

## Why native

- We exploit OS surfaces deeply: Lock Screen widgets, Live Activities, Focus filters, Shortcuts, Siri. Each requires native code.
- We need precise BGTask scheduling with low energy budget.
- App Store policy and review feedback are far smoother for native apps than for cross-platform shells.

## Architecture

```
SwiftUI scenes ──▶ ViewModels (ObservableObject) ──▶ CoreClient (Swift wrapper)
                                                          │
                                                          ▼
                                            UniFFI binding
                                                          │
                                                          ▼
                                            sunrise-core (Rust)
```

## iOS-specific surfaces

### Lock Screen widgets

- **Single-tap capture.** Tapping launches the app pre-focused on a quick capture sheet. (iOS doesn't allow direct text entry on the lock screen for a third-party app, but we shorten the path.)
- **Today summary.** Small widget showing tasks-due-today count + first task title.
- **Stream tile.** Large widget showing top items in a chosen Stream.

Widgets read from a shared App Group container synced periodically by the main app's BGTask.

### Home Screen widgets

- Same set as lock-screen widgets, plus larger configurations.

### Focus Filters

When the user enters an iOS Focus mode (Work, Personal, …), Sunrise responds:

- Shows only the configured Streams in Today.
- Mutes notifications from non-matching Streams.
- Communicates via the Intent we register in `Info.plist`.

### Shortcuts and App Intents

Donate intents for: capture, mark-done, defer, start-focus, stream-summary. Users compose Shortcuts that capture into Sunrise from anywhere.

### Siri

"Hey Siri, add 'pick up dry cleaning' to errands" → routes via the capture App Intent. Confirmation handled in voice.

### Live Activities

When a focus session starts, a Live Activity shows the timer in the Dynamic Island and on the Lock Screen.

### Sharing extension

System share sheet → Sunrise → captures the shared text/URL/image as an attached note in the Inbox.

### Apple Watch (MAY)

If shipped: a glance for Today, ability to capture via voice. Syncs to phone via WatchConnectivity. Not v1.

## Background sync

- `BGAppRefreshTask` scheduled by the OS at OS-decided times.
- During the budget window:
  1. Connect WS.
  2. Drain inbox + outbox.
  3. Commit.
  4. Schedule local notifications for any new reminders within the next horizon.
  5. Disconnect cleanly.
- A second `BGProcessingTask` for heavier maintenance (compaction, attachment cleanup).

## Push handling

APNs payload is content-less:

```json
{ "aps": { "content-available": 1 } }
```

The app is woken silently, runs sync, optionally raises a local notification if state warrants one (e.g. a shared peer added a task that mentions the user).

## OS keystore

- Identity and device private keys stored in **Keychain** with `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly` and biometric-protected access for unwrap.
- Where available (most modern iPhones), keys are bound to the **Secure Enclave** so the private key can be used but not extracted.

## File system

- Vault DB in App Container `Library/Sunrise/`.
- Excluded from iCloud document backups by default; included in encrypted iOS device backups.

## Sandboxing constraints

- No global hotkeys (iOS doesn't have them).
- Background time is precious; we spend it carefully.
- Inter-app data passing happens through App Groups, share sheets, and URL schemes.

## App Store considerations

- E2EE app; we comply with App Store's encryption export documentation.
- We commit to no IDFA collection.
- ATT prompt: not required (we don't track).

## Performance budgets

- Cold start to Today: ≤500ms on iPhone 14+.
- Capture sheet ready: ≤100ms.
- Op apply rate ≥10k/sec on iPhone 14.
