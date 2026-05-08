---
status: draft
---

# Android Client

Native Kotlin / Jetpack Compose app. The Sunrise core ships as an `.aar` via UniFFI bindings (JNI under the hood).

Target: Android 10 (API 29) and above. Wear OS support is MAY for v1.

## Why native

- Same reasoning as iOS: deep OS surface use (Quick Settings tile, Glance widgets, Tasker integration, sharing intents).
- WorkManager scheduling for background sync is platform-specific.
- Jetpack Compose gives us a fast UI iteration loop.

## Architecture

```
Compose UI ──▶ ViewModels ──▶ CoreClient (Kotlin wrapper)
                                  │
                                  ▼
                       UniFFI binding (JNI)
                                  │
                                  ▼
                          sunrise-core (Rust .so)
```

## Android-specific surfaces

### Glance widgets (Home screen)

- Today summary, Stream tile, capture button.
- Implemented with Jetpack Glance for App Widgets.
- Bound to a Repository that reads a periodically-refreshed snapshot from the core.

### Quick Settings tile

- Tile labeled "Sunrise: Capture."
- Tap → launches capture sheet directly.
- Present alongside Wi-Fi, Bluetooth, etc.

### Tasker / automation

- Expose Sunrise actions via:
  - **App Actions** (Google's manifest-driven assistant integration).
  - **Tasker plugin** (intent-based; Tasker users compose advanced flows).

### Sharing intents

- Receive `ACTION_SEND` and `ACTION_SEND_MULTIPLE` for text, images, files.
- Captured into Inbox as a task with attached note/file.

### Foreground service for focus

- A persistent notification while in focus mode.
- Avoids being killed by the OS during a session.

### Notifications

- Channels per Stream and per type (reminders, sharing).
- Action buttons: Done, Defer 1h, Snooze.
- Bundled by Stream when many reminders fire close in time.

### Wear OS (MAY)

- Tile + complication for Today count.
- Voice capture via Wear's voice intent.

## Background sync

- **WorkManager** PeriodicWorkRequest (~15 min cadence) for routine sync.
- **OneTimeWorkRequest** scheduled when an op leaves the outbox while the app is backgrounded, for prompt delivery on next opportunity.
- **Doze / app standby** are respected; we don't try to escape them.

## Push (FCM)

- Content-less data messages.
- High-priority for direct-relevance ops (a reminder fired, a sharing action); normal for bulk.
- On receipt: schedule a high-priority WorkManager task to drain.

## OS keystore

- **Android Keystore** for identity / device private keys.
- On devices with **StrongBox** (hardware-backed, available on most flagships from Pixel 3+): keys are non-extractable.
- Without StrongBox (older or budget devices): fall back to TEE-backed Keystore.
- Without TEE: fall back to a passphrase-derived wrapping key.
- User authentication required to unwrap (biometric or device credential).

## File system

- Vault under `Context.filesDir/sunrise/<account>/`.
- Excluded from cloud backup (`android:allowBackup="false"` for the app data flag, with a `BackupAgent` that filters out vault content).

## Distribution

- Play Store (primary): Android App Bundle.
- F-Droid (secondary, FOSS-only build): no Google services. Push falls back to UnifiedPush. Distribution lag of a few weeks behind Play Store is expected.

## OEM quirks

- **Battery optimization whitelisting**: we ask once on first launch (with explanation), document in help. Some OEMs (Xiaomi, Huawei, OnePlus historically) aggressively kill background work; we document workarounds and degrade gracefully.
- **Custom skins** that override widget rendering: tested on Samsung One UI, Pixel, Xiaomi MIUI as a representative set.

## Performance budgets

- Cold start to Today: ≤700ms on a 2022 mid-range device.
- Op apply rate ≥5k/sec on the same.
- Doze-friendly: average background CPU ≤20s/day.
