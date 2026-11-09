---
status: accepted
---

# macOS Client

A native **SwiftUI** application in `apps/apple/`. The Sunrise core is a Rust
static library, reached through a UniFFI seam
([ADR-0019](../11-adr/0019-swiftui-macos-client.md)).

Target: **macOS 26**, Apple Silicon. Swift 6 with
`-strict-concurrency=complete`. Universal (Intel) is one `rustup target add`
and one entry in `mise.toml`'s `macos_slices`; it is not built today.

## What the app is today

Roughly 18k lines of Swift under `apps/apple/Sunrise/`, covered by 477 Swift
Testing cases in 75 suites, built and linted `--strict` in CI on `macos-26`.
This section is the *shipped* inventory; everything under
[Platform integration](#platform-integration) is marked for whether it exists.

One main window, a sidebar, and a detail pane. The sidebar's fixed destinations
are Today, Inbox, Search, Calendar, Focus, Routines, Review, Morning and
Evening, plus a row per Stream and per Context.

| Surface | What it does |
|---|---|
| Task list | Today / Inbox / per-Stream / per-Context / search results, with a capture bar |
| Task editor | Details, a rich-text **Notes** pane over the Task `body`, Attachments, and an Activity timeline |
| Calendar | Day and week Block grids; drag on empty grid to create, drop a task onto it to schedule, drag a block to move it or its bottom edge to resize |
| Focus | The planner's ranked picks, session start/end, interruption logging, unblock cascade |
| Routines | Recurrence edited in plain English |
| Review | Weekly, daily, trends and history, with CSV/JSON export; the weekly and daily halves also print |
| Morning / Evening | The two daily briefs, also saveable as views |
| Settings | Vault switcher, relay URL, notification prefs, hotkey status, the vim toggle |
| Pairing | A six-leg copy/paste handshake with SAS confirmation |
| Menu bar | Quick capture, today's counts, sync status |
| Widget | **Next Up** in Notification Centre and on the desktop: what is left on Today, first things first |
| File menu | Import Calendar… (⌘⇧I) and Export Calendar ▸ Today \| This Week, over the seam's iCal pair; Print… (⌘P) and Export as PDF… |
| Import report | A sheet over the window listing what an `.ics` created and updated, and every notice **grouped by code** — an importer whose losses nobody sees is the failure the report exists to prevent |

**Notes fidelity.** The editor works on structured blocks decoded at the seam,
never on raw CBOR. The domain codec reports a `Fidelity` computed by re-encoding
what it decoded and comparing bytes; a body this build cannot reproduce exactly
is rendered **read-only** rather than rewritten. Under entity-level LWW
([ADR-0014](../11-adr/0014-entity-level-lww-merge.md)) that is what stops an
older client from silently flattening a body it does not fully understand.

**Not built here:** the free-standing `Note` entity
([ADR-0020](../11-adr/0020-v1-must-demotions.md)), stream sharing (a MUST in
the parity matrix, ranked as [#133](https://github.com/justin13888/Sunrise/issues/133)), and calendar integrations
([ADR-0071](../11-adr/0071-calendar-read-write-through-synced-intents.md), ranked on
the [roadmap](../roadmap.md)).

**iCal import/export is built:** File → Import Calendar… (⌘⇧I) and Export
Calendar ▸ Today | This Week call the seam's `import_ical` / `export_ical`
through `IcalModel`. The status audit in
[`parity-matrix.md`](./parity-matrix.md#status-audit) grades every macOS MUST
and names the ones not met. One of them is the widget row: the widget is
built, but no build this repository makes has the team the Mac's grant is keyed
to, so nobody has watched a Mac offer it
([#1116](https://github.com/justin13888/Sunrise/issues/1116)).

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

### Run

```
mise run macos-run                      # build and open the app
mise run macos-run /tmp/sunrise-demo    # …against a throwaway vault and key store
```

This is the only task that puts the app on screen. Everything under
[Build](#build) either tests or hands the project to Xcode.

Launch the product with `open`, never by executing
`Sunrise.app/Contents/MacOS/Sunrise`. Executing the binary starts the process
without registering it as a foreground app: it runs, opens no window, and is
indistinguishable from a hang. `macos-run` uses `open` for exactly this reason.

The optional argument is a scratch vault directory. It forwards
`-sunrise-ui-test-vault` — the `#if DEBUG` hook in
`Sunrise/Identity/UITestHarness.swift` that the UI tests already use — which
redirects both the vault and the key store, so a demo or a walk through first-run
touches neither the developer's data nor their Keychain. The key store is
in-memory and dies with the process, so every scratch run is a first run.

### Build

```
mise run apple-xcframework    # cargo build → uniffi-bindgen → lipo → xcframework
mise run macos-app            # + xcodegen generate, swiftlint --strict, xcodebuild test
mise run macos-uitest         # the XCUITest target, which macos-app does not run
mise run macos-open           # open the generated project in Xcode
```

None of these four launches the app; `macos-app` builds and *tests*, and
`macos-open` stops at Xcode. See [Run](#run) above.

`project.yml` (XcodeGen) is committed; the generated `.xcodeproj` is not.
`out/` and `build/` are gitignored — the Swift bindings are generated from the
Rust source on every build, so committing them would let the two drift.

**CI builds this.** `.github/workflows/ci.yml` has a `macos-app` job on the
`macos-26` runner — pinned because `project.yml` sets a macOS 26.0 deployment
target that no earlier image can build — which runs `mise run macos-app` as a
single step on every push to `master`. It does **not** run on a pull request,
because a macOS job queues for up to two hours behind the account's
five-runner cap; there it is skipped, and a skipped job still reports a
passing check. The evidence before merge is the same command run locally —
`mise run apple-app` on a Mac, for any change that touches something the app
is built from (see the README's Apple section for that list). The framework it links is built
upstream, once per run, by the `apple-xcframework` job. Plus a 04:00 UTC
nightly on `master` alone, since GitHub fires a `schedule` only on the
repository's default branch, which is `master`. Any other ref builds on demand
through `workflow_dispatch` — `gh workflow run ci.yml --ref <branch>` — and
the results appear on that branch's pull request. So a Swift-side break is
caught, at the latest by the merge that brings it in.

**The UI tests are not run by that job.** `SunriseUITests` is `skipped: true` in
the `Sunrise` scheme, because a macOS XCUITest takes control of another process
and the machine has to be told that is allowed; it is compiled on every build
but only executed by `mise run macos-uitest`, which has a scheme of its own so that
the skip can be bypassed, on a developer machine. Two separate grants are
needed: `sudo DevToolsSecurity -enable`, and accepting the automation prompt the
runner raises the first time it launches. That target is the one that proves a
click reaches the core through the real window, so the automated coverage is
view-model-level. Worth knowing when reading a
green CI run: it proves the app builds, lints and its models behave — not that
every screen is still reachable.

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
* Coalesce repaints on a 50 ms window. The number was proven out on the removed
  terminal client; below it a burst repaints per op for no visible benefit.
* Do **not** enable `SWIFT_UPCOMING_FEATURE_EXISTENTIAL_ANY`: UniFFI 0.32 does
  not emit `any`, and it produces 20 warnings in generated code. Strict
  concurrency itself is clean.

## Platform integration

Each entry is marked **built** (in the app today) or **specified** (this
document's intent, not yet implemented).

- **built — Menu bar item** (`MenuBarExtra`). Quick capture, the daily snapshot,
  sync status. Refresh on launch, every 60 s while visible, and immediately on a
  relevant change event (debounced 500 ms) — all three are implemented, and the
  60 s poll runs only while the menu is open.
- **built — Notification Centre / desktop widget** (`SunriseWidgets`, a
  WidgetKit extension embedded in the app). This is **Next Up** in its small,
  medium and large sizes. It is the same widget, built from the same
  `apps/apple/Widgets/` sources, as the iOS one, so
  [`mobile-ios.md`](./mobile-ios.md#widgets) is its specification: the
  snapshot contract, what leaves the vault, the refresh cadence, and the
  erasure whenever the vault is not open. The Mac's refresh is the change
  feed plus the 15-minute re-read. A Mac app is not suspended, so no
  foreground trigger is needed.

  Two things differ from iOS. The extension is **sandboxed**, because macOS
  loads no widget that is not (see [Sandboxing](#sandboxing)). And its App
  Group is **`$(TeamIdentifierPrefix)dev.sunrise`**, not iOS's
  `group.dev.sunrise`: a Developer ID build is granted a group prefixed with
  its own team without a provisioning profile, and `release.yml` exports
  with none. On a Mac, `containerURL` returns a path whether or not the
  group was granted. So the app first reads the grant off its own signature
  (`WidgetSnapshotStore.isGranted`), and a build with no entitlements, such
  as the one `mise run macos-app` makes, writes nothing. No build this
  repository makes today has a team, so whether
  the Mac offers the widget and grants its container has **not been
  observed** ([#1116](https://github.com/justin13888/Sunrise/issues/1116)).
- **built — Quick capture** — a borderless window on a global hotkey (⌘⇧N).
  It does **not** need the Accessibility permission. The hotkey is registered
  with Carbon's `RegisterEventHotKey`, which *reserves* one combination with
  the window server; the permission is only required by
  `NSEvent.addGlobalMonitorForEvents`, which observes every keystroke in every
  app. Reserving one chord is narrower and asks less of the user, so that is
  what `HotkeyCenter` does. The app's settings screen displays the
  permission's status with the caption "Not required for the shortcut above".
  Registration fails when another app already holds the chord;
  that is an ordinary state (`HotkeyStatus`), not an error — the menu bar item
  still opens capture.
- **built — Notification Center** for reminders, with action buttons.
  `docs/08-features/notifications.md` owns quiet hours and primary-device
  dedup; the app schedules from the intents the core emits. Scheduling
  **follows the change feed** (500 ms debounce) rather than polling, and
  reconciles the desired set against `UNUserNotificationCenter`'s pending
  requests, so a re-run adds and cancels rather than duplicating. Snooze
  targets come from the domain across the seam, never from local date
  arithmetic. Only Task and Block reminders are scheduled; the two daily briefs
  are **views**, and nothing schedules a notification for them.
- **built — Keychain** holds the unlock material. Nothing else does. The
  account is **per vault**, so a second vault gets its own item rather than
  overwriting the first. Three items hold something of the user's, under three
  services, and **all three** ask for
  `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`:
  `dev.sunrise.Sunrise.vault-root`, `dev.sunrise.Sunrise.oidc-credentials` and
  `dev.sunrise.Sunrise.relay-device-id`.

  A **fourth** service is written, and it is not one of those three.
  `KeychainDomain.probe()` adds one fixed non-secret byte under
  `dev.sunrise.Sunrise.keychain-domain-probe` on every cold launch to find out
  which keychain this binary can reach, and deletes it again — sweeping the
  whole service under both domains, so a probe killed before its cleanup is
  reclaimed by the next one rather than leaving residue for the life of the
  installation. It holds nothing of the user's and is never read back; that is
  what makes writing to a user's keychain to answer a capability question
  acceptable, and it is the reason the count above says "of the user's" rather
  than "in total".
  An item an older build left in the weaker `…AfterFirstUnlock` is raised on
  the next load rather than left where it was, and a Keychain that refuses the
  raise fails the load rather than handing back a secret whose guarantee is not
  the one the app claims.

  Each store also runs a **keychain migration** on `load`, immediately before
  that raise — `KeychainMigration` in
  `apps/apple/Sunrise/Identity/KeychainMigration.swift`. The order is
  load-bearing: move the item to the keychain this build addresses, then raise
  the class, because the class only starts meaning anything once the item is
  somewhere that implements one. Which keychain that is comes from
  `KeychainDomain.probe()`, which asks the platform rather than assuming from
  `#if os(…)`. On a **Mac** the probe answers `.login` for every Debug build
  and for any Release build no team's provisioning profile authorises, and
  `.dataProtection` only for a Release signed by the team with a profile that
  grants `keychain-access-groups`. No such build has been made yet, so both
  halves are inert on this platform today; see below.
  On iOS it answers `.dataProtection` — the only keychain that platform has —
  which `KeychainDomainTests` pins, and there the migration is a no-op for
  the other reason: one keychain means the source and destination name one
  stored item.

  **The Mac does not honour the class yet**: without the App Sandbox or an
  authorised keychain-access-group entitlement the app uses the file-based
  login keychain, which stores no protection class at all, so a Mac moved by
  Migration Assistant or restored from Time Machine carries all three items
  with it. iOS enforces the class; see
  [`../03-crypto/recovery.md`](../03-crypto/recovery.md#device-backups-do-not-carry-the-vault-root).
- **built — App Intents / Shortcuts.** Eight intents — capture, complete,
  defer, today, inbox, stream summary, start focus, end focus — plus a
  `TaskEntity` and a `StreamEntity`, each with an `EntityStringQuery`, and an
  `AppShortcutsProvider`, which is what puts them in
  Shortcuts, Spotlight and Siri without the user assembling anything. Capture
  calls the same `previewCapture` as ⌘⇧N, so capture semantics are identical
  across every surface. An intent **adopts the app's open vault** when there is
  one and otherwise opens and closes its own under a **counted lease**, sharing
  one in-flight open so two concurrent intents cannot race the vault lock.
- **built — Drag and drop**, at every site
  [`interaction-patterns.md`](./interaction-patterns.md#drag-and-drop-matrix)
  names but one. A task row is `.draggable` and reaches four destinations: the
  calendar grid (creating a Block bound to it), a sidebar Stream (the same
  command the `M` sheet sends), a sidebar Context (adding rather than replacing,
  since a task has one Stream and any number of Contexts), and another task row
  (reordering). An existing block drags to move and its bottom edge to resize,
  both snapping to the grid's chosen step; the Adjust sheet is still there for
  exact times. The sidebar's Streams reorder by `.onMove`, which writes
  `Stream.sort_order` through the core and therefore **syncs**, and so does task
  order: a row drop is `MoveTasks`, which writes `Task.sort_order`. Files drop onto a task's
  Attachments pane. Today and Search decline a reorder drop rather than
  accepting one that would snap back, because the core ranks those two lists.
  The one gesture not built is **Calendar block → Task**, and what is missing is
  two modifiers rather than a layout: `BlockChip` is not `.draggable`, and the
  task row's drop destination only reorders. `TaskListModel.bind(_:to:)` exists
  and is tested, so the write is ready and building the gesture is UI work.
- **built — `sunrise://` URL scheme**, registered in `Info.plist` and handled by
  `onOpenURL`, for notification deep links
  ([`interaction-patterns.md`](./interaction-patterns.md)). When no main window
  exists the URL is re-handed to LaunchServices so a window opens to receive it.
- **built — Stage Manager / Mission Control**: a standard window, no special
  behaviour.
- **specified — Spotlight indexing of task titles** (`NSUserActivity` /
  `CSSearchableItem`), decrypted on-device and indexed locally. **Not
  implemented**: there is no `NSUserActivity` or Core Spotlight code in the app,
  and no Settings → Integrations toggle. Note that Sunrise *does* appear in
  Spotlight today, but through App Intents (above), which surfaces **actions**,
  not task titles. The two are different features and only the second exists.
- **specified — Continuity Camera** for attaching a scan from an iPhone. **Not
  implemented** — attachments come from the file importer or a drop.
- **built — Print / PDF export.** ⌘P and File → Export as PDF…, a
  parity-matrix **SHOULD**. The screen is
  turned into a `PrintDocument` — a title, a stamp and a list of sections of
  rows — which is a plain value a test can assert against, and only then handed
  to `ImageRenderer` and paginated into a `PDFDocument`. Printing goes through
  `PDFDocument.printOperation`; the PDF export writes the same pages to a save
  panel. Splitting it there is deliberate: `ImageRenderer` and
  `NSPrintOperation` cannot be exercised in a unit test, and everything that
  decides *what appears on the page* lives on the testable side of that line.
  Covered: task lists, search results, the calendar day and week grids, and the
  weekly and daily reviews. **Not** covered, by decision: Review → Trends and
  Review → History, which are a chart and a list of links rather than rows; both
  render a title-and-date page and both already carry CSV/JSON export beside
  them.

### Sandboxing

The direct `.dmg` build is **not** sandboxed. A sandboxed build cannot register
a reliable system-wide hotkey, and quick capture is the feature the persona
uses most. A Mac App Store build would have to trade that away, and
[ADR-0031](../11-adr/0031-macos-distribution.md) declines the trade: the App
Store is **not** a distribution channel, and the direct `.dmg` ships alone. The
sandbox stays off.

The one sandboxed binary inside the bundle is the widget extension,
`Contents/PlugIns/SunriseWidgets.appex`. macOS loads no widget that is not
sandboxed. The extension needs nothing but read access to its App Group
container, so the sandbox costs it nothing. It has no network entitlement,
and it opens no Keychain item and no vault.

### The data-protection keychain is not a one-line entitlement

The Mac's missing protection class (§Platform integration, and
[`../03-crypto/recovery.md`](../03-crypto/recovery.md#device-backups-do-not-carry-the-vault-root))
is fixed by moving the app onto the **data-protection keychain**, which needs
the `keychain-access-groups` entitlement — *not* the App Sandbox, which stays
off. Three things were measured on an Apple-silicon Mac, and together they say
what that costs:

1. **Unsigned, as `mise run macos-app` builds today:** the file-based login
   keychain accepts `SecItemAdd` and reads back **no** `kSecAttrAccessible` at
   all, and `SecItemAdd` with `kSecUseDataProtectionKeychain` returns
   `errSecMissingEntitlement` (-34018).
2. **Ad-hoc signed with `keychain-access-groups`:** the process is
   **`Killed: 9` before `main`**. `codesign -v -vvv` reports the binary "valid
   on disk" and "satisfies its Designated Requirement"; the kill is AMFI
   refusing a *restricted* entitlement that no provisioning profile grants. A
   team-prefixed group (`$(AppIdentifierPrefix)…`) dies the same way — the
   entitlement is restricted, not the name.
3. **Ad-hoc signed with `com.apple.security.application-groups`** — the other
   entitlement that reaches the data-protection keychain — the process *runs*,
   and `SecItemAdd` with `kSecUseDataProtectionKeychain` still returns -34018,
   because an ad-hoc signature carries no team id for the group to be validated
   against.

So the entitlement cannot be on every build: a build carrying it will not
launch without a real signing identity and a profile that grants it, and every
Debug consumer is team-less by design. `mise run macos-app` and
`mise run macos-run` build `CODE_SIGNING_ALLOWED=NO`, which is what keeps the
Mac app buildable by a contributor with no Apple account, the same thing
`DEVELOPMENT_TEAM: ""` exists for; `mise run macos-uitest` and a Run from
`mise run macos-open` sign ad-hoc, and would die at AMFI.

It is therefore on **Release only**: `apps/apple/macOS/Sunrise.entitlements`
carries `keychain-access-groups` with the one group
`$(AppIdentifierPrefix)dev.sunrise.Sunrise`, and `apps/apple/project.yml`
points the `Sunrise` target's Release configuration at it. Debug signs with
`apps/apple/macOS/SunriseDebug.entitlements` instead, which carries only the
widgets' App Group, and the Release file carries that group too. Release is the configuration the team signs: `release.yml`
archives Release with the Developer ID identity. Its one team-less caller,
the workflow's `unsigned_macos_dmg` dry run, archives
`CODE_SIGNING_ALLOWED=NO`, which embeds no entitlements, so it is unaffected. A
local Release build without a team produces no bundle at all: Xcode
refuses it before signing, with `"Sunrise" has entitlements that require
signing with a development certificate`, rather than producing the
configuration-2 bundle AMFI would kill. A team-less Release build that is
wanted anyway adds `CODE_SIGNING_ALLOWED=NO`, as the dry run does.

What is still missing is outside this repository's files: an Apple Developer
Program team for `DEVELOPMENT_TEAM`, and a Developer ID provisioning profile
that grants the group. The release pipeline installs that profile from the
`MACOS_PROVISIONING_PROFILE` secret, refuses an app tag without it, and checks the
exported app carries both the group and the embedded profile before
notarizing ([#389](https://github.com/justin13888/Sunrise/issues/389);
[`releasing.md`](./releasing.md#macos_provisioning_profile) says how to create
it).

The **hardened runtime**, which is a different setting, is on and has to be:
Apple's notary service rejects a submission without it.
`apps/apple/project.yml` sets `ENABLE_HARDENED_RUNTIME: YES` in
`settings.base`, and that is the only place it is set — a local
`xcodebuild archive` therefore produces the same bundle the release pipeline
signs. The app needs no `com.apple.security.cs.*` exception for it: it loads a
statically linked xcframework and no plug-ins, and neither `RegisterEventHotKey`
nor `AXIsProcessTrusted` is restricted by the runtime.

#### Debug and Release keep separate keychains

The Release-only entitlement has a price, and it falls on developers rather
than users. On one Mac, against one vault, a signed Release build probes
`.dataProtection` and moves the three items there on their first `load`; a
Debug build of the same bundle identifier probes `.login`, cannot read the
data-protection keychain at all, and so finds none of them. What it shows is
the lost-vault screen for a vault that is intact, which is the failure this
page warns about everywhere else, reproduced on purpose.

Nothing is lost, and nothing should be done from that screen. In particular,
**do not recover or re-pair from it**: the root is still in the data-protection
keychain, and the next signed Release build reads it. The remedies are to point
the Debug build at a scratch vault (`mise run macos-run <dir>`, which also uses
an in-memory key store) or to keep one Mac's real vault on one configuration.
The reverse direction is harmless: a Release build that finds an item only in
the login keychain migrates it.

The alternative — the entitlement on Debug as well, with every contributor
signing with a team — was rejected because it makes an Apple Developer Program
membership the price of building the app, which `DEVELOPMENT_TEAM: ""` exists
to avoid.

#### How the items move

**The migration is built, and so is the entitlement; the signature is not.**
Everything that does not need a signing identity is in the tree:

- `KeychainDomain` — `.login` and `.dataProtection`, and a memoised `probe()`
  that adds one fixed non-secret byte under a probe-only service in
  `.dataProtection`, keeps the status, sweeps whatever sits under that service,
  and answers `.dataProtection` only on `errSecSuccess`. The byte goes in under
  `…AfterFirstUnlockThisDeviceOnly`, the class all three stores write under,
  because the question is whether this app can store a secret there the way it
  stores secrets, not whether it can reach the keychain at all. It **never
  throws**: it fails open to the weaker-but-reachable keychain, because a
  refused domain would leave a secret the app cannot see at all.
- Failing open is right *before* a migration and wrong *after* one, so a `load`
  that finds nothing at the resolved domain **reads the other domain before
  reporting nothing** (`KeychainItem.readAcrossDomains`). **`nil` out of that
  method means both keychains answered *not-found*, and nothing else.** `nil`
  is the one answer `SessionModel` reads as absence, so a refusal from the
  other domain — locked, or a prompt denied — is raised as
  `KeychainError.otherDomainUnreadable` rather than shown as the lost-vault
  screen for a condition an unlock fixes. It is its own case rather than the
  bare status so the message names the keychain that refused, not the one that
  is working. `SessionModel` maps any throw out of a `load` to
  `.locked(.keychainUnavailable)`, the screen that offers Keychain Access.
  `AccountModel.restore` reports the refusal rather than presenting a
  signed-out session, and its Try again acts on the refusal's *shape*: for
  every shape that may have left a copy of the token unread it re-reads the
  store, and for `.migrationUnverified`, where both copies have been read, it
  goes to the login, whose cross-domain write collapses the pair.
- On an unentitled Mac a `.dataProtection` **query** answers
  `errSecItemNotFound` (-25300); the -34018 refusal is on the *mutating* calls,
  `SecItemAdd`, `SecItemUpdate` and `SecItemDelete`.
- All three stores `clear` across both domains: whatever a read can reach, a
  clear removes, or signing out would leave a live refresh token where the next
  launch looks. Both deletes are *attempted* before either status is raised,
  because the delete in the domain this build cannot address is itself refused.
- The two cross-domain **mutations** swallow only the statuses that mean the
  other store was *unreachable* — the missing-entitlement refusal and
  not-found — and raise anything else. A store reached well enough to refuse on
  its own terms may still hold the copy the mutation was meant to remove, and
  reporting that as success would leave a refresh token behind a Sign out, or
  two relay device ids that refuse every later launch.
- The **write** raises that as `KeychainError.writtenButOtherDomainRefused`.
  `writeAcrossDomains` writes before it cleans up, so it can throw with the
  secret already stored, while every caller reads a throwing write as "nothing
  was stored". `KeychainCredentialStore.save`, whose `Void` return answers "was
  the token stored", treats that one case as success and rethrows everything
  else; widening `CredentialStore` to disclose a partial success is the shape
  #255 proposes for the sign-out path.
- Which stores write across both domains. The **credential** store does: its
  token is rewritten with no user action (`refreshIfNeeded` renews at 75% of
  its life), so a launch whose probe failed open would otherwise leave a fresh
  token in one keychain and a stale one in the other. The **relay device id**
  store does too (#254), because `RelayDeviceRegistration.bind` rewrites it at
  every sync start (#183), and two differing ids would refuse every later load
  and register a fresh relay row per sync start; it lets
  `writtenButOtherDomainRefused` through, since both of its callers record with
  `try?` and act on nothing. The **vault root** deliberately does not: a root
  in the other domain may be the key the vault on disk is sealed under, and no
  second copy exists anywhere, so a pair costs a refused `.migrationUnverified`
  load with both roots readable, where a delete would cost the vault.
  `KeychainMigration`'s own write is exempt too: it deletes its source only
  after the verify step, and a cross-domain write would take the source out
  from under it.
- `KeychainMigration` — five resumable steps holding one invariant: **a
  readable copy exists at every instant.** Read the destination, read the
  source, write the destination, read it back and compare byte-for-byte, and
  only then delete the source. A kill between any two steps leaves a state the
  next launch finishes from. What follows the two reads is decided by
  `planSecretCopies` and `verifySecretWrite` in
  `crates/sunrise-client-core/src/secret_copies.rs`, from presence, byte
  equality and (for a credential) each copy's expiry — never the bytes. It
  refuses two vault roots or two relay device ids that differ, and a read-back
  that finds bytes other than the ones written; two differing credentials are
  ordered by expiry instead (#258). It falls back to the source item for every
  other Security status, because locking a user out over a *destination* problem
  while the secret is perfectly readable where it has always been is a worse
  trade than the raise takes. That fallback's value is **what `load` answers
  with**: `KeychainMigration.loadMigratingIfNeeded` is the single step all three
  stores call, and a caller that discarded it and read the destination instead
  would see nothing and report the lost vault the fallback exists to prevent.
- Each of the three stores migrates **its own** item inside its own `load`.
  There is no launch-time pass over all three: the OIDC credential is keyed per
  account and the other two per vault, so "all three" is not one set.

None of it changes behaviour on any build this repository can produce yet. The
probe answers `.login` on an unsigned or ad-hoc-signed Mac, and iOS has only
one keychain, so in both cases the migration's source and destination are two
names for one stored item and it does nothing at all — a case the code checks
for explicitly and the tests pin, because a migration that missed it would
verify that item against itself and then delete it.

#### What no test reaches

Every scheme's `test` action is Debug, and Debug stays unentitled, so the
cross-domain move a signed Release performs is exercised by no test. **Seven
lines are declared untestable**, because each needs a keychain state no build
here can produce — a copy in the domain this build cannot write, or a lock or
denied prompt landing mid-case:

1. `writeAcrossDomains`'s raise of `writtenButOtherDomainRefused`.
2. The same method's cross-domain **delete effect**, the other domain's copy
   actually being removed.
3. `deleteInOtherDomain`'s re-raise — `meansTheOtherStoreWasUnreachable`
   answering *false* inside a running cross-domain delete. The predicate's
   answer itself is pinned by `KeychainUnreachableStatusTests`.
4. The cross-domain clear's **tie-break**, that this domain's status wins when
   both deletes refuse.
5. `KeychainCredentialStore.save`'s `catch`, reached only through item 1.
6. `KeychainMigration.loadMigratingIfNeeded`'s destination-step throw — one
   item with three branches: the `accessibilityNotRaised` rethrow, the
   `guard let migrated else` re-raise, and the `return migrated` rescue.
7. `readAcrossDomains`'s **raise** of `KeychainError.otherDomainUnreadable`.

The suite cannot stage the mid-case refusal. It performs no keychain-state call
and inherits whatever the host session unlocked. `SecKeychainLock`,
`SecKeychainUnlock` and `SecKeychainSetUserInteractionAllowed` are declared
`API_DEPRECATED("SecKeychain is deprecated", macos(10.2, 10.10))` and
`API_UNAVAILABLE(ios, watchos, tvos, macCatalyst)`, while `SunriseTests/`
compiles into the iOS app too; and the lock has no scope smaller than the
machine's default login keychain, which `.serialized` cannot keep away from the
cases running beside it. A fault-injection seam inside the type that holds the
vault root is rejected as a second implementation of `Security.framework`. Which
items an entitled test host would unblock, why, and a hand-run mutation of
item 4 that survives are in
[#1180](https://github.com/justin13888/Sunrise/issues/1180).

`theProbeDeletesWhateverItWrote` in `KeychainDomainTests` is not one of the
seven, but it is vacuous on macOS: the probe's `SecItemAdd` there is refused,
so removing `probe()`'s cleanup leaves it green; it has teeth on iOS only.
`aProbeReclaimsAnOrphanAnEarlierProbeLeftBehind` beside it plants an orphan in
`.login` and pins the sweep on both platforms.

An entitled test host is where five assertions that encode "this build reaches
exactly one domain" are **extended to assert the entitled behaviour**, keeping
the Debug answer on Debug:

- `theProbeAnswersWhatThisBuildCanActuallyReach` — `KeychainDomainTests`
- `aDestinationThisBuildCannotReachFallsBackToTheSource`
- `theUnreachableDomainRefusesMutationsAndAnswersReadsAsEmpty`
- `aRefusalOnThisDomainDoesNotSpareTheCopyInTheOther`
- `aWriteRefusedInItsOwnDomainIsNotReportedAsAPartialSuccess`

The last four are in the `macOS`-only `KeychainMigrationFallbackTests` suite.
`theMigrationsDestinationWriteDoesNotTakeTheSourceWithIt` there is guarded with
`.enabled(if: KeychainDomain.current == .dataProtection)` and starts running on
that host: it pins that `KeychainMigration`'s plain `write(_:)` does not take a
same-service, same-account source in the other domain with it.

## Multi-vault

More than one vault, switched from Settings. A `VaultRegistry` in
`UserDefaults` holds the known vaults and the selected one; each gets its own
directory and its own Keychain account, and the first is pinned to the id
`default` so an existing install keeps its vault.

**The switch order is load-bearing**, because `crates/sunrise-core/src/vault_lock.rs`
allows exactly one open vault per process — a process-local registry of
canonicalized paths, backed by an OS advisory lock. `SessionModel.switchTo`
therefore resolves the target first (it can fail cheaply), then **tears down the
old bridge before re-pointing anything**, then re-runs the launch decision. Get
that order wrong and the app deadlocks against its own lock, naming this very
process as the holder. Every surface is rebuilt on the new bridge; the capture
panel closes and the menu bar and reminder models are dropped and remade.

The seam supports many vaults **sequentially**, not concurrently: `shutdown()`
then `open()`. There is no way to hold two vaults open at once, and there cannot
be without changing the lock rule.

## Device binding

Every request the sync driver makes can carry the
[ADR-0022](../11-adr/0022-device-signature-canonical-json.md) binding: an
`X-Sunrise-Device` naming a relay device row, an `X-Sunrise-Device-Sig` over the
canonical request, and the `Date` the signature covers. A relay configured with
`require_device_sig` refuses anything else.

The signing half has always been available — it is the vault's own `D_S_priv`,
and `Core::device_signer` reaches it across the seam. What was missing was a
home for the other half: the **relay device id**, a ULID the relay mints at
`POST /api/v1/devices` and returns **only** to the registering device. It never
travels back through the op stream, so a client that loses it cannot get it
again.

It lives in the Keychain, under `dev.sunrise.Sunrise.relay-device-id`, per
vault, in the vault root's own protection class
(`KeychainRelayDeviceIDStore`). The class is not about secrecy — the id is sent
in the clear on every request that uses it, which is why the CLI keeps its copy
in a plain file. It is about the id and the key it names being present or absent
*together*: a device holding one without the other signs with a key the named
row does not hold, and the relay answers that as a bad **bearer** — deliberately
indistinguishable from a token problem, so that a caller cannot enumerate an
account's devices, and therefore undiagnosable from the client.

The two rejected homes, for the record:

- **`UserDefaults`**, where the relay URL and the vault registry correctly live,
  because neither is a secret and both must be repairable without a vault. A
  preference domain that gets reset costs a setting the user can retype, and
  costs this one a binding nobody can retype.
- **The vault**, which would carry the id with the *account* rather than the
  installation — and would put per-device data in a synced, converging store,
  where every device replicates every other device's id and a merge has to
  decide which one is "this" one.

**The app registers itself, by one of two routes** (#183), and each ends in
that store (`RelayDeviceRegistration` in `RelayDeviceStore.swift`):

- **The device that created the vault** registers inside the recovery ceremony.
  `SunriseCore::bootstrap_account` runs `POST /api/v1/accounts` then
  `POST /api/v1/devices`, and the app records the returned `device_id` only
  after the relay accepted it. A Keychain refusal at that point still shows the
  recovery code, because the relay already holds the blob that code opens.
- **Every other device** — one admitted by pairing, or a founding device whose
  id was never recorded — registers at sync start. `SessionModel.bindRelayDevice`
  calls `SunriseCore::register_relay_device`, which is `POST /api/v1/devices`
  alone, as `sunrise recover` does. It publishes nothing about the account and
  asserts no terms acceptance ([`../06-server/api.md`](../06-server/api.md)
  §Terms acceptance). It does nothing when an id already resolves, or while a
  recovery ceremony is outstanding.

**A stored id is valid only where it was minted.** The relay looks it up
under the account on that relay (`Store::active_device`), so the same id sent
to another relay, or under another account's bearer, names no row and is
answered as a bad bearer. The store therefore records each id with its scope:
the relay URL (trimmed, trailing slashes dropped) and the bearer's OIDC `iss`
and `sub`, read from the JWT without verifying it. A stored id is presented
only while that scope is the one sync runs in; after the relay URL changes or
the user signs in to another account it resolves to nothing, and the next sync
start registers again and replaces it. A value stored with no scope reads as
nothing for the same reason. An opaque (non-JWT) bearer has no readable
account, so its scope rests on the relay URL alone.

A driver started *before* the ceremony recorded its id keeps running unbound
until the next start, because a repeated start is a no-op rather than a
re-point. For the same reason a running driver keeps the id it started with
when the relay URL or the account changes mid-process; the next launch binds
the new scope.

The environment override the CLI has for the same case still wins: launch with
`SUNRISE_SYNC_DEVICE_ID` set to an id registered elsewhere, and the driver
presents and signs for it and the app registers nothing. That is the same
variable name, the same precedence (override before stored) and the same
meaning as `sunrise_cli::livesync::ENV_SYNC_DEVICE_ID`.

An unbound driver is not a failure state and is not refused: it is what every
self-host relay runs, and a client that would not connect without a binding
could never reach the relay that mints one.

**A Keychain that refuses the read is not an unregistered device** (#284). A
locked keychain, a dismissed access prompt, or an unreadable other keychain may
be hiding an id this device is bound by, so `RelayDeviceID.resolve` throws
rather than answering no id: `bind` registers nothing on top of it, and
`SyncPlan` stays off with a reason naming the Keychain instead of starting an
unbound driver. The next sync start reads again. Two disagreeing copies
(`.migrationUnverified`) are the one refusal read as no id, because both were
read and the re-registration's cross-domain write is what collapses them.

## Pairing

A second device is paired over the relay or, as a fallback, a **six-leg copy/paste handshake**: the QR/text
code, three Noise XX messages, a SAS comparison, and the sealed vault root.

**The relay is the default transport; the user is the fallback.** When this
device has a relay and a signed-in account, `PairingModel` drives the
`RelayPairing` seam over the relay's rendezvous
([`../06-server/api.md`](../06-server/api.md) §Pairing rendezvous) in three
phases: show or take the code, compare the SAS, done. With no relay, no
account, or a relay that does not answer, it falls back to the copy/paste legs
below and says why. Of those, five move bytes the user copies between the two
Macs; the sixth moves none, because it is the SAS. There is still no camera
scanner ([#464](https://github.com/justin13888/Sunrise/issues/464)).

- **Show QR** is a real CoreImage render, with the same payload as copyable text
  beside it — which is what actually gets used, since there is **no camera
  scanner**. Accepting a code is a paste.
- **SAS is confirmed on both sides**, six digits, and the confirm button is
  deliberately not the default action.
- Two entry points: the first-run "pair with an existing device" branch, and a
  route out of `LockedView` when the vault exists but its key does not.
- Three messages cross, not one: the sponsor's `PairingOffer` (the account's
  public identity, no secret at all), the joiner's `PairingRequest` (the
  `D_S_pub`/`D_D_pub` it just minted), and the sponsor's `PairingGrant` (the
  issued cert, the vault root and every Stream key). The joiner assembles a
  `PairingPayload` from the three, so it is a member of the account rather than
  a second account holding the same root. Its cert is signed by the sponsor
  under `ID_S_priv` — which never leaves that device (`#105`) — and published as
  a `device_cert` op; there is no separate trust step to forget.
- The spec's 90-second SAS timeout is not implemented; there is an abort button
  instead.

## Multi-window

- One main window (`WindowGroup`), plus the `MenuBarExtra`.
- Quick capture is its own borderless, non-activating `NSPanel`.
- A detached, compact, always-on-top **focus** window is **specified, not
  built** — Focus is a sidebar destination in the main window today.
- Two-column stream comparison is **not built**, and not cut: it was specified
  for the Tauri app, nothing was built, it is not a MUST, and it is not ranked
  on the roadmap.

## Update channel

**Built, with one step left to the owner.** Sparkle, over the direct channel,
applied on next launch — a running
session is never interrupted by an update. Channels: `stable` and `beta`, with
the beta channel behind a menu item the user turns on
(*Sunrise ▸ Include Beta Updates*). The decision, and the trust argument that
is the substance of it, is
[ADR-0038](../11-adr/0038-macos-update-feed.md); the operator's half is
[`releasing.md`](./releasing.md) §The update feed.

How it fits together:

- The app links Sparkle (`apps/apple/project.yml`, macOS target only) and reads
  a signed `appcast.xml` published as an asset of the GitHub Release.
- `release.yml` generates that feed from every published release and signs it,
  and signs each `.dmg`, with an EdDSA key held as the
  `SPARKLE_ED_PRIVATE_KEY` repository secret. A release is a prerelease — and
  so lands on `beta` — exactly when `verify` says it is; there is no second
  place that reads a version string.
- **The EdDSA key is subordinate to the Developer ID certificate, not a second
  co-equal trust root** — in *lifecycle*, which is the part that decides how it
  is held. Sparkle authorises a change of EdDSA key with the app's Apple code
  signature, so a Developer-ID-signed release rotates the key in-band with no
  user action, while the certificate has no such in-band recovery. It is **not**
  a claim that a stolen feed key is harmless: Sparkle accepts an update on
  *either* credential, so whoever holds the key can ship code until a rotation
  reaches a user. ADR-0038 has the mechanism, the source it is read from, and
  the rotation procedure.

**One thing is not done and only the repository owner can do it.** The key pair
does not exist yet, so `SUPublicEDKey` is empty in the committed project file.
Until it is filled in, the app starts no updater at all and both menu items are
disabled with the reason attached — fail-closed, because an updater that cannot
verify what it downloads is worse than none.

## Windows

There is no Windows client. The portable core a Windows client would link is
built and tested on Windows by CI's `rust-windows` job
([dependencies.md](../01-architecture/dependencies.md) §Windows), and the
`sunrise` CLI is part of it. Two things about how secrets are written there
are decided here.

**Writes are staged and renamed.** Every secret the CLI writes (the vault key,
the parked pairing secret, the exported pairing grant) and the credential file
`sunrise_auth::FileStore` keeps go through a sibling file that is renamed over
the target. A crash mid-write leaves the previous file whole on every platform.
Before issue #369 the CLI's Windows branch truncated the target in place. A
hard-link test in each crate tells a rename from an in-place rewrite and runs
on Windows too.

**Files are owner-only.** On Unix those files are created mode `0600`. On
Windows they are created with a protected DACL holding one entry, which allows
the current user full control. Nothing is inherited from the directory, so a
vault directory under `C:\`, which hands `Authenticated Users` modify to new
files, hands them nothing. `SYSTEM` and `Administrators` get no entry. An
administrator can still take ownership, and backup software reads past a DACL
by design. The DACL is set at creation, before any secret byte is written, by
`sunrise-private-file`, the one crate allowed `unsafe` for it
([ADR-0067](../11-adr/0067-windows-owner-only-files.md)). Tests in that crate,
`sunrise-auth` and `sunrise-cli` read the DACL back on Windows CI. A file
written before that change keeps its inherited ACL until it is next written.
For a vault key, that means until the vault is created again.

## Telemetry

Off by default. If the user opts in: minimal anonymous metrics (launches, crash
reports). Crash reports never include vault content — see
[`../10-cross-cutting/telemetry-and-privacy.md`](../10-cross-cutting/telemetry-and-privacy.md).
