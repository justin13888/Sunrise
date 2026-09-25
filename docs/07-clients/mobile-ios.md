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

**The app ships and is in CI, and it is held to SHOULD level.**
[`parity-matrix.md`](./parity-matrix.md) now carries a filled iOS column and
[ADR-0028](../11-adr/0028-ios-is-a-v1-client.md) is the record of why: every
row the shell reaches is a **SHOULD**, and none of them is a MUST until an iOS
release ships. Twenty-three SHOULDs, graded **23 met**. The two that were
unmet — saved views and iCal import/export, each a working shared model with no
iOS caller — now have one: a **Views** menu on the toolbar of the screens a
saved view can name, and **Import calendar…** / **Export calendar** in Browse's
overflow, where the Mac has a File menu.

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
- CI: an `ios-app` job on `macos-26`, downstream of the `apple-xcframework`
  job that builds the framework it links. It runs whenever CI runs, except on
  a pull request that touches nothing the Apple apps are built from — no Rust,
  no manifest, no `apps/apple/**`, no `mise.toml` — which the `changes` job
  decides. A skipped job reports a check GitHub counts as successful, so the
  required context is still satisfied. Four triggers
  (`.github/workflows/ci.yml:3-17`), one of them branch-filtered: `push` on
  `master`. `pull_request` carries no `branches:` key, deliberately — that key
  filters on the *base* branch, so constraining it meant a pull request stacked
  on another pull request's branch ran nothing at all. Every pull request is
  gated now, whatever it targets. The nightly schedule is constrained too, but
  by GitHub rather than by this file: a `schedule` fires on the repository's
  **default branch** alone, and that is `master`, so the 04:00 nightly builds
  `master` and nothing else, and no `--ref` can change it.
  `workflow_dispatch` is the one that will build any ref on request:
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

> **Apart from two exceptions, none of the surfaces in this section, in [Background sync](#background-sync), or in [Push handling](#push-handling) exists yet.** Grepping `apps/apple` finds no
> `BGAppRefreshTask`, `BGProcessingTask`, `ActivityKit`,
> `WatchConnectivity`, `INFocusStatus` or `SecureEnclave`, and `project.yml`
> declares no share or watch extension target. This section is the
> specification these surfaces will be built to, not a description of the app.
> Tracked in [#368](https://github.com/justin13888/Sunrise/issues/368) and
> [#367](https://github.com/justin13888/Sunrise/issues/367).
>
> The two exceptions:
>
> - **Shortcuts and App Intents.** Six intents live in
>   `apps/apple/Sunrise/Intents/` and are shared with macOS. The set differs
>   from the list below: there is no "defer" or "stream-summary" intent, and
>   there are "today" and "inbox" intents that this page does not mention.
> - **The Next Up widget** ([Widgets](#widgets)), on the Home Screen and the
>   Lock Screen. The capture widget and the Stream tile are not built
>   ([#376](https://github.com/justin13888/Sunrise/issues/376)).


### Widgets

**Built: Next Up** (#14). It shows how much of Today is left and what comes
first. It is one widget in every size, because every size answers the same
question and only the number of rows changes:

| Family | Draws | Tap opens |
|---|---|---|
| Home Screen small | the open count, the overdue count, the first task and its section | the first task |
| Home Screen medium / large | the counts, then 3 / 8 rows, each with its own link, and "+N more" | the row tapped; elsewhere, the first task |
| Lock Screen rectangular | "N left" (with the overdue count), then the first two titles | the first task |
| Lock Screen circular | the open count | the first task |
| Lock Screen inline | "N left · first title" | the first task |

A row opens `sunrise://task/<id>?action=open`, which lands on Today with the
task revealed. Every size also prints "updated … ago".

The same sources build the macOS widget, which offers the three Home Screen
sizes; see [`desktop.md`](./desktop.md#platform-integration).

#### The snapshot is the whole contract

These rules are normative for any client that adds a widget.

- **The widget extension never opens the vault.** It links no Rust. There are
  three reasons. The app holds the vault lock while it runs. The vault root
  is in the app's Keychain items. And a widget's memory ceiling is far below
  what `Core::open` needs. The extension reads one JSON file,
  `widget-snapshot.json`, from the App Group container. The app writes it
  (`WidgetSnapshot`, in `apps/apple/Widgets/Shared/`).
- **What is on the widget is the core's decision.** The rows are the open tasks
  of `Query::Today`, in the order it returns them. Each row's section
  (overdue / due / scheduled) comes from `today_section` and is carried as
  data, so the widget never re-derives the start-of-day boundary. The app only
  chooses which fields to project and how many rows.
- **What leaves the vault is bounded.** The file holds, for at most 8 tasks,
  the task's id, its title, its section and its link. It also holds three
  counts (open, overdue and Inbox) and a timestamp. It holds no notes,
  streams, contexts or dates. This is a deliberate plaintext copy outside
  SQLCipher, the same trade a reminder notification's title makes. On iOS the
  file is written with `completeUntilFirstUserAuthentication`, because a Lock
  Screen widget has to draw while the phone is locked.
- **The snapshot exists only while a vault is open.** Every time the session
  leaves `.unlocked`, the app erases the file and reloads the widgets: on a
  lock, a sign-out, a failure, or the start of a vault switch. A vault switch
  erases the old snapshot before the new vault's is written. With no snapshot,
  every size says **Open Sunrise** and never shows an empty list. An empty list
  would claim that nothing is due, when the truth is that the widget cannot
  see.
- **A snapshot from another format version counts as absent.**

#### Refresh cadence

The app rewrites the snapshot at these moments:

- when a vault is attached;
- after each change batch from the core (debounced 500 ms);
- every 15 minutes while it runs, which catches a day rolling over;
- whenever the app returns to the foreground.

It writes, and asks WidgetKit to reload, **only when the widget would draw
something different**, because iOS budgets reloads. The widget's timeline is
one entry with a `.never` policy: nothing in the extension can compute a new
state, so only a write from the app can produce one.

While the app is suspended, nothing refreshes the snapshot. The "updated …
ago" stamp shows how old it is. A `BGAppRefreshTask` that opens the vault in
the background and republishes belongs to background sync
([#367](https://github.com/justin13888/Sunrise/issues/367)).

#### Not built

These are tracked in [#376](https://github.com/justin13888/Sunrise/issues/376):

- **Single-tap capture** (Lock Screen). A tap opens the app to the capture
  sheet with the field focused. It is blocked on a route: `sunrise://capture`
  with no text is refused by the link parser today, and the capture intent
  runs in the background.
- **Stream tile.** A large widget showing the top items in a chosen Stream.
  It needs a configuration intent, and the app has to publish per-stream rows.

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

If shipped: a glance for Today, ability to capture via voice. Syncs to phone via WatchConnectivity. Not built.

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

The accessibility class is `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`
(`KeychainVaultRootStore.accessibility`): available before the user unlocks the
screen after a reboot, which background sync needs, and never carried to other
hardware by a backup, which is what makes the vault root's guarantee in
[`../03-crypto/recovery.md`](../03-crypto/recovery.md#device-backups-do-not-carry-the-vault-root)
true. A root written by a build that used the weaker
`…AfterFirstUnlock` is raised to this class on the next load rather than left as
it was. What a restore onto new hardware then costs the user is written down in
that section; the same file is shared with macOS, where the login keychain
implements no protection classes and the guarantee therefore does not yet hold.

### The OIDC credential is in the same class, and why

`dev.sunrise.Sunrise.oidc-credentials` — the access and refresh tokens from a
completed login — asks for `…AfterFirstUnlockThisDeviceOnly` too
(`KeychainCredentialStore.accessibility`). It was deliberately left in
`…AfterFirstUnlock` when the vault root moved, on the argument that a session
is not a vault; that argument does not survive contact with the two things that
would have had to catch a travelling token, neither of which does:

- **The refresh grant carries no device id.** `OidcClient::refresh` exchanges
  the refresh token with no `sunrise_device_id` parameter, so the access token
  it mints carries the device claim of the *original* authorization.
- **The relay checks that claim only when a device signature is presented.**
  `api::signed::verify_bytes` returns before the comparison when the
  `X-Sunrise-Device` headers are absent and `require_device_sig` is off — which
  is the default, and is *required* to be off in the single-tenant self-host
  mode [ADR-0027](../11-adr/0027-v1-self-host-first.md) makes the only shape.

So a refresh token lifted out of an encrypted backup opens a live session
against the account from hardware the account never authorized. What it reaches
is the relay surface — device list, blob store, op metadata, the ability to
push — and not the plaintext, which is sealed to Stream keys that hang off a
vault root that did not travel. The cost of closing it is one tap: a device
restored onto new hardware already has no vault root and must pair with a
surviving device before it is useful, and signing in again happens on a screen
the user is already standing in front of.

### The relay device id is here too

`dev.sunrise.Sunrise.relay-device-id` holds the ULID the relay mints at
registration, per vault, in the same class — so on iOS a restore onto new
hardware leaves neither the vault root nor the device id behind, and the two
halves of the binding stay consistent. The argument for the Keychain over
`UserDefaults` or the vault, and the fact that the app cannot yet register
itself, are in
[`desktop.md`](./desktop.md#device-binding); the store is shared code and the
reasoning does not differ by platform.

One gap against the design remains:

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
