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
release ships. Twenty-four rows, graded **24 met**; the newest is the widget
row ([Widgets](#widgets)). The two that were
unmet — saved views and iCal import/export, each a working shared model with no
iOS caller — now have one: a **Views** menu on the toolbar of the screens a
saved view can name, and **Import calendar…** / **Export calendar** in Browse's
overflow, where the Mac has a File menu.

The platform surfaces below are built, except the Apple Watch app, the
Secure Enclave binding and two widgets; [Platform surfaces](#platform-surfaces)
says which is which.

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
  job that builds the framework it links. It runs on every trigger except
  `pull_request`, where it is skipped. A skipped job reports a check GitHub
  counts as successful, so the required context is still satisfied. Before
  merge, the same `mise run ios-app` runs locally on a Mac instead (ADR-0028,
  clause 2 of "What would force revisiting this"). Four triggers
  (`.github/workflows/ci.yml:3-17`), one of them branch-filtered: `push` on
  `master`. `pull_request` carries no `branches:` key, deliberately — that key
  filters on the *base* branch, so constraining it meant a pull request stacked
  on another pull request's branch ran nothing at all. Every pull request is
  gated by the rest of the workflow, whatever it targets. The nightly schedule
  is constrained too, but by GitHub rather than by this file: a `schedule`
  fires on the repository's
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

## Platform surfaces

> **Built:** the Next Up widget, the focus Live Activity, Focus Filters, the
> share extension, printing, and the Shortcuts and App Intents surface, each in
> its own section below, as are [Background sync](#background-sync) and
> [Push handling](#push-handling)
> ([#368](https://github.com/justin13888/Sunrise/issues/368)).
>
> **Not built:** the Apple Watch app (`apps/apple` has no
> `WatchConnectivity` and `project.yml` no watch target), the Secure Enclave
> binding ([OS keystore](#os-keystore)), and the capture widget and the Stream
> tile ([#376](https://github.com/justin13888/Sunrise/issues/376)).
>
> Every extension Sunrise ships — the widgets, which also draw the Live
> Activity, and the share extension — runs **without the vault**. None links the
> core, none holds a key, and each one meets the app only through a file in the
> App Group container whose contents are bounded in its section.


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
task revealed. The Home Screen sizes also print the snapshot's age as a
relative time ("5 min"), which VoiceOver reads as "Updated 5 min ago". The
Lock Screen sizes have no room for it and print none.

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
  the task's id, its title, its section and its link. It also holds two
  counts (open and overdue) and a timestamp. It holds no notes,
  streams, contexts or dates. This is a deliberate plaintext copy outside
  SQLCipher, the same trade a reminder notification's title makes. On iOS the
  file is written with `completeUntilFirstUserAuthentication`, because a Lock
  Screen widget has to draw while the phone is locked.
- **The snapshot exists only while a vault is open, or until the next launch
  where the app ended without notice.** Every time the session leaves
  `.unlocked`, the app erases the file and reloads the widgets: on a lock, a
  sign-out, a failure, or the start of a vault switch. A vault switch erases
  the old snapshot before the new vault's is written. The app also erases it
  when it is told it is terminating (a quit on macOS), and again at launch,
  before any vault is open. iOS kills a suspended app, and a crash ends one,
  without telling it; that snapshot stays on disk, and on the widget, until
  Sunrise next launches. A suspended iOS app still holds its vault open, so
  the Lock Screen widget keeps drawing while the app is in the background.
  With no snapshot,
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

While the app is suspended, the snapshot is refreshed only by a background
run ([Background sync](#background-sync)): every refresh task and silent push
ends by republishing it from the vault it just synced. Between runs, the age
stamp on the Home Screen sizes shows how old it is.

#### Not built

These are tracked in [#376](https://github.com/justin13888/Sunrise/issues/376):

- **Single-tap capture** (Lock Screen). A tap opens the app to the capture
  sheet with the field focused. It is blocked on a route: `sunrise://capture`
  with no text is refused by the link parser today, and the capture intent
  runs in the background.
- **Stream tile.** A large widget showing the top items in a chosen Stream.
  It needs a configuration intent, and the app has to publish per-stream rows.

### Focus Filters

**Built** ([#368](https://github.com/justin13888/Sunrise/issues/368)). Settings
▸ Focus ▸ a Focus ▸ Focus Filters ▸ Sunrise offers **Set Sunrise Streams**
(`SunriseFocusFilter`, `apps/apple/Sunrise/Intents/SetFocusFilterIntent.swift`),
a `SetFocusFilterIntent` with a multi-select of the vault's streams. While that
Focus is on:

- **Today shows only the picked streams.** No other list is narrowed: the
  Inbox, a stream or a context the user opens by name is a list they asked for.
- **Reminders from the other streams are muted.** `ReminderScheduler` withdraws
  them from the OS schedule rather than delivering them silently, so the
  64-alert cap is spent on reminders that can fire, and the reconcile that runs
  when the Focus ends puts them back. The filter's intent runs that reconcile
  itself, so it happens with no window open: it uses the open vault's
  scheduler, or opens the vault the way any intent does, re-plans once and
  closes it again. A reminder is let through when its stream
  cannot be read, and a time block's always is, because a block belongs to no
  stream; dropping an alert over a failed read is the worse mistake.

These rules are normative for any client that adds a Focus filter:

- **The scope is device-local and never synced.** A Focus is a fact about one
  device at one moment. `FocusFilterStore` keeps it in `UserDefaults`, persisted
  because iOS runs the filter's intent when the Focus changes, which may launch
  Sunrise in the background and end it again long before anyone opens it.
- **No streams picked means no filter.** A Focus whose filter names nothing
  filters nothing, rather than emptying Today and silencing the phone.
- **The rules live in one place** (`FocusFilter`, `apps/apple/Sunrise/Focus/`),
  so Today and the scheduler cannot disagree about what a Focus lets through.

iOS resolves the picked streams when the Focus changes. If the vault cannot be
opened then — before the first unlock after a restart — the streams the filter
was configured with are answered from the store's record of their names, so the
filter still applies.

The filter is iOS-only for now. macOS has the API, but
[desktop.md](./desktop.md) specifies no Focus filter and the Mac window does
not re-read Today when the scope changes.

### Shortcuts and App Intents

**Built.** `apps/apple/Sunrise/Intents/` is shared with macOS, and
`SunriseShortcuts` offers eight App Shortcuts, each runnable without bringing
Sunrise forward:

| Intent | Does |
|---|---|
| Capture Task | captures a line through the same parser as quick capture |
| Complete Task | marks a task done and says what it unblocked |
| Defer Task | pushes a task out by an hour, to tomorrow or to next week |
| Get Today's Tasks / Get Inbox Tasks | reads one of the two fixed lists |
| Get Stream Summary | says how many tasks are open in a stream and names the first three |
| Start Focus Session / End Focus Session | opens and closes a focus session |

**Defer** sends the same `DeferTask` command as the row's Defer menu and a
reminder's snooze buttons, so the task's deferral count goes up. The target
instant is the seam's `snooze_target_ms`, which makes "tomorrow" a date rather
than 24 hours. The plan-time semantics of
[ADR-0047](../11-adr/0047-deadlines-and-lateness.md) change this command,
not the intent ([#334](https://github.com/justin13888/Sunrise/issues/334)). A
finished task is refused rather than given a date.

**Stream Summary** takes a `StreamEntity`. Unlike the task picker, the stream
picker opens the vault to list its streams, because the Focus filter's settings
page has no typed search to fall back on.

### Siri

"Hey Siri, add 'pick up dry cleaning' to errands" → routes via the capture App Intent. Confirmation handled in voice.

### Live Activities

**Built** ([#368](https://github.com/justin13888/Sunrise/issues/368)). A
running focus session shows its task's title and its timer on the Lock Screen
and in the Dynamic Island. A session with a planned length counts down to its
end; one that runs until the task is done counts up. The system draws the
timer, so it keeps time while Sunrise is suspended and the activity is never
updated once a second.

- **The content state is the title and two instants, and nothing else**
  (`FocusActivityState`, `apps/apple/Widgets/Shared/FocusActivity.swift`).
  The activity is drawn by another process on the Lock Screen, so the title is
  plaintext outside SQLCipher for as long as the session runs, the same trade
  a reminder and the widget snapshot make. The session id is a static attribute.
- **It follows the vault, not the buttons.** `FocusLiveActivity`
  (`apps/apple/iOS/`) reconciles against `Query::RunningFocusSessions` on every
  change batch while the vault is open, and the two focus intents reconcile
  before they return. So a session started from the Focus screen, a
  `sunrise://focus` link, Siri, or another device all get the same activity,
  and a session ended anywhere loses it. Activities for any other session are
  ended. The decisions are `FocusActivityPlan`, which is shared and tested
  without ActivityKit.
- **The intents may start it from the background.** `StartFocusIntent` and
  `EndFocusIntent` are `LiveActivityIntent`s on iOS, which is what lets a
  Shortcut or a spoken phrase put the timer on the Lock Screen with Sunrise
  never opened.
- **It goes when the vault closes.** A lock, a sign-out or a vault switch ends
  every focus activity, as the widget snapshot is erased. The next vault to
  open shows whatever its own running session calls for.

The activity is drawn by `SunriseWidgetsiOS`, the widget extension, and the
app's `Info.plist` carries `NSSupportsLiveActivities`.

### Sharing extension

**Built** ([#368](https://github.com/justin13888/Sunrise/issues/368)). The
system share sheet offers **Sunrise** (`SunriseShare`, `apps/apple/Share/`) for
text, a web link, and up to ten images. What is shared becomes one task in the
Inbox:

- the **title** is the first line of the text, else the link's host and path,
  else "Shared image";
- the **note** holds the rest of the text, and the link last;
- each **image** is an attachment, under the name the sharing app gave it.

Nothing is parsed for tags: a shared paragraph is somebody else's prose, and a
`#` in it is not a stream.

**The extension never opens the vault, and that bounds its key access to
none.** It links no Rust, holds no vault root and reads no Keychain item. It
copies what it was handed into the App Group container as a `PendingCapture`
(`apps/apple/Share/Shared/`), written with complete file protection, and says
"Added to your Inbox". The app files it through the ordinary seam the next time
it opens a vault or comes to the foreground (`SharedCapture`,
`apps/apple/Sunrise/Tasks/`). So until then, the shared item is outside the
vault, readable only while the phone is unlocked.

Filing is resumable. The extension publishes a capture with one atomic rename,
so the app never sees half of one. The app writes the id of the task it created
back into the record before it attaches anything, and removes each image as it
lands, so an interrupted filing resumes on the same task. An image the record
lists and the folder no longer holds is dropped rather than retried forever.

**An image the core would refuse on every attempt is refused once.** The
extension turns away an empty image and one over 100 MB (decimal, the core's
`MAX_ATTACHMENT_BYTES`) while the user is still on the share sheet. Should one
reach the app anyway, it is dropped from the capture before the task is
created, and the task's note gets a "Not attached" line naming it. A name
longer than the core's 256-character limit is shortened, extension kept,
rather than refused. Any other failure leaves the capture for the next pass.

**A pending share belongs to no vault until it is filed, and it is filed into
the next vault the app opens.** The extension cannot say which vault the share
was meant for: it never opens one, and the App Group holds one queue for the
device. So a share made while one vault was open, and filed after the user
switched to another, lands in the second vault's Inbox. The window is narrow:
the open vault files a share as soon as the app comes forward. Tagging each capture with the vault open at share time was rejected:
the extension would have to read which vault is open, which is the app's
state, and a capture tagged with a vault that never reopens would wait
forever.

### Printing

**Built on iOS** ([#368](https://github.com/justin13888/Sunrise/issues/368)).
A **Print…** button on the toolbar of Today and of every pushed task list, and
⌘P on an attached keyboard, hand the shared `PrintDocument` to
`UIPrintInteractionController` (`apps/apple/iOS/PrintController.swift`). The
pages are the same pages the Mac prints: `PrintPageView` and the PDF it renders
to are shared (`apps/apple/Sunrise/Print/PrintPage.swift`). So are which
screens print and where a page breaks. An empty list is refused with the
refusal haptic rather than printed as a blank sheet, as the Mac beeps. There is
no separate Export as PDF, because the print sheet saves the same pages to
Files.

### Apple Watch (MAY)

If shipped: a glance for Today, ability to capture via voice. Syncs to phone via WatchConnectivity. Not built.

## Background sync

**Built** ([#367](https://github.com/justin13888/Sunrise/issues/367)). Three
OS entry points run one sync, and never two at once:

| Entry point | Identifier | When the OS runs it |
|---|---|---|
| `BGAppRefreshTask` | `dev.sunrise.SunriseiOS.refresh` | no sooner than 15 minutes after the request; the OS decides when |
| `BGProcessingTask` | `dev.sunrise.SunriseiOS.maintenance` | on external power and a network, no sooner than a day after the request |
| silent push | — | when the relay's dispatcher sends one ([Push handling](#push-handling)) |

The app asks for the next refresh and maintenance window whenever it goes to
the background, and re-arms the refresh at the start of every refresh it runs,
so a run the OS cuts short still leaves one pending. `project.yml` declares
the `fetch`, `processing` and `remote-notification` background modes and both
task identifiers in `BGTaskSchedulerPermittedIdentifiers`.

Each run (`SessionModel.backgroundSync`, `apps/apple/Sunrise/Sync/`):

1. **Opens the vault if the OS launched the app with no window.** It opens
   only from the two states nobody chose: the launch that has opened nothing
   yet, and a Keychain that would not answer, which before the first unlock
   after a restart is expected. A vault the user locked stays locked, a failed
   open is not retried, and a device with no vault has nothing to sync; each
   answers "no data". The open is single-flight, so a window appearing during
   it joins it rather than racing it into the vault lock.
2. **Renews the account token if it is due**, binds the relay device as a
   foreground start does, and builds the same `SyncPlan`. A plan that is off
   answers "no data".
3. **Calls `SunriseCore::sync_once(url, bearer, relay_device_id, budget_ms, cancel)`**
   with what is left of a 25-second budget the whole run shares: the vault
   open, the renewal and the device binding come out of it first. The core
   starts the sync driver if nothing has, ends any session that was up before
   the app was suspended — a session that looks live over a socket that died
   while suspended is the failure this prevents — cuts short a reconnect delay
   the driver was suspended in, and waits for a session dialled after the call to report `Live`
   with an empty outbox: every stream caught up, every local op acked. It
   returns whether that happened, how many changes the vault published
   meanwhile, and the driver's state. The driver keeps running afterwards; a
   suspended process simply stops scheduling it.
4. **Re-plans local notifications and republishes the widget snapshot** from
   the vault it just synced, binding the app's surfaces to it first on a cold
   launch.
5. **Files the push token** if the relay does not hold it
   ([Push handling](#push-handling)).

**Expiry.** When the OS ends the budget — the task's expiration handler, or
28 seconds after the run began, which is the only bound a silent push has —
`BackgroundSync` answers the OS `.failed` at once, whatever step the run is
in, and cancels the run behind that answer. The cancel reaches Rust through
the `SyncCancel` handle `CoreBridge.syncOnce` passes in, because UniFFI's
async glue does not forward a Swift task's cancellation; `sync_once` returns
as soon as it sees it, and nothing after the sync (the re-plan, the token
upload) runs. Nothing is half applied: the
wait holds no transaction, and the driver commits each inbound op whole while
the database lock is held, never across an await. The next run resumes from
the cursors the committed ops advanced.

**What the OS is told.** A run that changed the vault reports new data — even
one cut short, since what landed landed whole. A run that finished with
nothing new, or had nothing it was allowed to do, reports no data. A run that
could not finish reports failure, and a refresh task completes unsuccessfully.

**Maintenance** runs the same sync today. Compaction and the attachment cache
trim join it once the core has them.

## Push handling

**Built** ([#367](https://github.com/justin13888/Sunrise/issues/367)). The
APNs payload is content-less:

```json
{ "aps": { "content-available": 1 } }
```

`application(_:didReceiveRemoteNotification:)` runs the background sync
above — joining it if a refresh is already running — and answers the fetch
handler with `.newData`, `.noData` or `.failed` as that section describes.
Nothing is shown: a peer's change reaches the user as the reminders the
re-plan schedules.

**The token.** The app calls `registerForRemoteNotifications()` at launch;
a silent push needs no permission prompt. `project.yml` gives the target the
`aps-environment` entitlement, `development` in Debug and `production` in
Release, which must match the relay's `[push.apns]` gateway. The token goes
to the relay as lowercase hex through `SunriseCore::register_push_token`,
which calls `sunrise_relay_client::register_push_token`: a
`POST /api/v1/devices/push-tokens` signed with the ADR-0022 device binding
over the canonical body, filed under the relay device id the request is
signed as. The app uploads when the (relay, relay device id, token) triple
differs from the last one the relay accepted: on a new token, on a rotated
one, and after a re-pairing mints a new relay device id. It checks on token
delivery, at every foreground sync start, on sign-in, and after every
background run. A refused upload is not recorded, so the next check retries
it. A device with no relay device id yet, or no account bearer, uploads
nothing until it has both.

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
  `X-Sunrise-Device` headers are absent and `require_device_sig` is off. It is
  *required* to be off in the single-tenant self-host mode
  [ADR-0027](../11-adr/0027-v1-self-host-first.md) makes the only shape, and it
  is off on a multi-tenant relay whose operator set `require_device_sig =
  false`. A relay with an OIDC issuer and the flag left unset requires the
  binding, and there an unsigned refresh-minted token is refused.

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
`UserDefaults` or the vault, and the two routes by which the app registers
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
