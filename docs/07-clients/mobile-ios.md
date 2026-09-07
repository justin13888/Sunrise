---
status: accepted
---

# iOS Client

Native Swift / SwiftUI app in `apps/apple`, sharing its entire view layer with
the [macOS client](./desktop.md). The Sunrise core ships as an `xcframework` via
UniFFI bindings, statically linked.

Target: **iOS 26, iPadOS 26**, Apple Silicon simulator or device.
`apps/apple/project.yml` pins `deploymentTarget.iOS: "26.0"`, matching the Mac's
26.0 — a shared source file cannot use an API only one of its two targets can
reach, and matching the majors is what lets the shared tree carry no
`@available` forks at all. An earlier revision of this page specified iOS 17+;
that is no longer what is built.

## Status

**The app ships and is in CI, and it is a v1 client at SHOULD level.**
[`parity-matrix.md`](./parity-matrix.md) now carries a filled iOS column and
[ADR-0028](../11-adr/0028-ios-is-a-v1-client.md) is the record of why: every
row the shell reaches is a **SHOULD**, and none of them is a MUST until an iOS
release ships. Twenty-three SHOULDs, graded **21 met and 2 unmet** — saved
views and iCal import/export, both of them a working shared model with no iOS
caller.

The platform surfaces below are still specification rather than description.
Read [Platform surfaces](#platform-surfaces-not-built) with that in mind.

What exists today:

- `apps/apple/iOS/` — `SunriseiOSApp.swift`, `VaultTabs.swift`, `TabRoute.swift`:
  a five-tab shell (Today, Calendar, Browse, Focus, Search) over
  `.tabViewStyle(.sidebarAdaptable)`, so iPhone gets a tab bar and iPad gets a
  sidebar from one declaration. Routines, Review, Morning and Evening are pushed
  onto Browse's stack from its "More" toolbar menu rather than owning tabs.
- Every screen below the shell is the *same* `apps/apple/Sunrise/` source the Mac
  compiles. Capture is a sheet where the Mac has a borderless panel; there is no
  menu bar, no global hotkey and no `NSPanel`, and those live in `macOS/` as
  whole files rather than as conditionals so this target simply does not see
  them.
- `SunriseiOSTests` — the shared `SunriseTests/` suite, compiled a second time
  against the iOS product. That is the claim: not that the iOS app compiles, but
  that it behaves the way the Mac does everywhere the two share a model.
- `SunriseiOSUITests` — and unlike the macOS UI tests, these are **not** skipped.
  A simulator runner needs no change to the machine's security posture, so iOS is
  the platform where a tap is proved to reach the core on every build.
- CI: an `ios-app` job on `macos-26`. It carries no `if:` and no path filter,
  so it runs whenever CI runs. Four triggers
  (`.github/workflows/ci.yml:3-11`), two of them branch-filtered: `push` on
  `master` or `v1-rewrite` and `pull_request` targeting either — `branches:` is
  nested under those two events and constrains only them — plus the nightly
  schedule and `workflow_dispatch`, which carry no `branches:` key. Only one of
  those two is actually unconstrained. GitHub fires a `schedule` on the
  repository's **default branch** alone, and that is `master`, so the 04:00
  nightly builds `master` and never `v1-rewrite` — a branch constraint that
  comes from GitHub's rule rather than from this file, and one no `--ref` can
  change. `workflow_dispatch` is the one that will build any ref on request:
  `gh workflow run ci.yml --ref <branch>`.

## Run it

```
mise run ios-run     # build, boot the simulator, install, launch
```

`mise run ios-app` builds and *tests*; `mise run ios-open` hands the project to
Xcode. Neither launches anything — `ios-run` is the one that does.

The simulator is `iPhone 17 Pro` by default, set as `ios_sim` in `mise.toml` so
CI and a laptop stay on one device. To start over from first run:

```
xcrun simctl uninstall 'iPhone 17 Pro' dev.sunrise.SunriseiOS
```

The build is signed ad-hoc (`CODE_SIGN_IDENTITY=-`), and it has to be: iOS gates
the Keychain on an application-identifier entitlement that only a signed binary
carries, so with signing off every `SecItemAdd` returns
`errSecMissingEntitlement` and the vault cannot store its root. Ad-hoc is enough
for the simulator and needs no developer account.

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

## Platform surfaces (not built)

> **None of the surfaces in this section, in [Background sync](#background-sync), or in [Push handling](#push-handling) exists yet.** Grepping `apps/apple` finds no
> `WidgetKit`, `BGAppRefreshTask`, `BGProcessingTask`, `ActivityKit`,
> `WatchConnectivity`, `INFocusStatus` or `SecureEnclave`, and `project.yml`
> declares no widget, share or watch extension target. This section is the
> specification these surfaces will be built to, not a description of the app.
> Tracked in [#31](https://github.com/justin13888/Sunrise/issues/31); widgets
> specifically in [#14](https://github.com/justin13888/Sunrise/issues/14).
>
> The one exception is **Shortcuts and App Intents**, which is real — six intents
> live in `apps/apple/Sunrise/Intents/` and are shared with macOS. The set
> differs from the list below: there is no "defer" or "stream-summary" intent,
> and there are "today" and "inbox" intents that this page does not mention.


### Lock Screen widgets

- **Single-tap capture.** Tap opens the app cold to the dedicated `CapturePresenter` view (text field focused, software keyboard up). No per-tap deep-link payload is needed because Lock Screen widgets cannot embed input. (iOS doesn't allow direct text entry on the lock screen for a third-party app, but we shorten the path.)
- **Today summary.** Small widget showing tasks-due-today count + first task title.
- **Stream tile.** Large widget showing top items in a chosen Stream.

Widgets read from a shared App Group container synced periodically by the main app's BGTask.

#### Widget refresh cadence

`BGAppRefreshTask` is scheduled every 15 minutes (iOS may delay). The widget shows a "last updated <relative>" stamp. On-app-open also refreshes synchronously.

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
  1. Open a sync session and its event stream (`POST /api/v1/sync/session`,
     then `GET /api/v1/sync/events` — [ADR-0023](../11-adr/0023-sse-sync-transport.md)
     replaced the WebSocket this step used to name).
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

**Partly built.** `apps/apple/Sunrise/Identity/Keychain.swift` stores the vault
root in the Keychain today, and `SunriseiOSTests` exercises it on the simulator —
which is why the iOS build is signed even for tests.

Two gaps against the design:

- The accessibility class is `kSecAttrAccessibleAfterFirstUnlock`
  (`Keychain.swift:62`), not the `…ThisDeviceOnly` variant this page specifies.
  The difference matters: the current item is included in an encrypted device
  backup and can restore onto another device. Tracked in
  [#42](https://github.com/justin13888/Sunrise/issues/42); the constant is in the shared
  tree, so macOS is affected identically.
- There is no biometric-protected access for unwrap and no **Secure Enclave**
  binding; `kSecAttrTokenIDSecureEnclave` appears nowhere in `apps/apple`.

## File system

- Vault DB under the app's `Application Support` directory —
  `VaultLocation.swift:34-39` resolves `.applicationSupportDirectory` and appends
  `Sunrise`, which on iOS lands inside the App Container. The first vault keeps
  `Sunrise/vault`; additional ones get `Sunrise/vaults/<id>`.
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

- Cold start to Today: ≤ 500 ms on iPhone 14+ Wi-Fi; ≤ 800 ms on iPhone 11 Wi-Fi. Measured from app-launch event to first-painted Today list (not full sync).
- Network condition: Wi-Fi for the gate; cellular adds up to +200 ms allowance.
- Capture sheet ready: ≤ 100 ms.
- Op apply rate ≥ 10 k/sec on iPhone 14.
