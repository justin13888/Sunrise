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

> An earlier revision of this file specified a Tauri 2 + React app across
> macOS, Windows and Linux. That app never existed — there was no `main.rs`, no
> `tauri.conf.json`, and Tauri was not a dependency anywhere. ADR-0019 records
> the replacement and why.

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
| File menu | Import Calendar… (⌘⇧I) and Export Calendar ▸ Today \| This Week, over the seam's iCal pair; Print… (⌘P) and Export as PDF… |
| Import report | A sheet over the window listing what an `.ics` created and updated, and every notice **grouped by code** — an importer whose losses nobody sees is the failure the report exists to prevent |

**Notes fidelity.** The editor works on structured blocks decoded at the seam,
never on raw CBOR. The domain codec reports a `Fidelity` computed by re-encoding
what it decoded and comparing bytes; a body this build cannot reproduce exactly
is rendered **read-only** rather than rewritten. Under entity-level LWW
([ADR-0014](../11-adr/0014-entity-level-lww-merge.md)) that is what stops an
older client from silently flattening a body it does not fully understand.

**Not built here:** the free-standing `Note` entity, stream sharing, and Google
Calendar, all three per [ADR-0020](../11-adr/0020-v1-must-demotions.md).

That list used to carry a fourth entry — **iCal import/export**, the one v1
MUST this client did not meet — and it no longer does: File → Import Calendar…
(⌘⇧I) and Export Calendar ▸ Today | This Week now call the seam's
`import_ical` / `export_ical` through `IcalModel`. **Every one of the 23 macOS
MUSTs is met.** See the status audit in
[`parity-matrix.md`](./parity-matrix.md#v1-status-audit).

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
single step on every push to `master` and on every pull request that touches
something the app is built from, whatever it targets. (A pull request that
touches no Rust, no manifest, no `apps/apple/**` and no `mise.toml` skips it;
the `changes` job decides, and a skipped job still reports a passing check.)
The framework it links is built upstream, once per run, by the
`apple-xcframework` job. Plus a 04:00 UTC nightly on `master` alone, since GitHub fires a
`schedule` only on the repository's default branch, which is `master`. Any
other ref builds on demand through `workflow_dispatch` — `gh workflow run
ci.yml --ref <branch>`. So a Swift-side break is caught.

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
- **built — Quick capture** — a borderless window on a global hotkey (⌘⇧N).
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
  `#if os(…)`. On every **Mac** build this repository can make the probe
  answers `.login`, so both halves are inert on this platform today; see below.
  On iOS it answers `.dataProtection` — the only keychain that platform has —
  which `KeychainDomainTests` pins, and there the migration is a no-op for
  the other reason: one keychain means the source and destination name one
  stored item.

  **The Mac does not honour the class**: without the App Sandbox or a
  keychain-access-group entitlement the app uses the file-based login keychain,
  which stores no protection class at all, so a Mac moved by Migration
  Assistant or restored from Time Machine carries all three items with it. iOS
  enforces the class; see
  [`../03-crypto/recovery.md`](../03-crypto/recovery.md#device-backups-do-not-carry-the-vault-root).
- **built — App Intents / Shortcuts.** Six intents — capture, complete, today,
  inbox, start focus, end focus — plus a `TaskEntity` with an
  `EntityStringQuery` and an `AppShortcutsProvider`, which is what puts them in
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
  `Stream.sort_order` through the core and therefore **syncs** — task order does
  not, because a Task has no ordering facet to write. Files drop onto a task's
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
  parity-matrix **SHOULD** that no longer has to slip to v1.x. The screen is
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
Store is **not** a v1 channel, and v1 ships the direct `.dmg` alone. The
sandbox stays off.

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

So the entitlement is not a setting that can be committed on its own: a build
carrying it will not launch without a real signing identity, and
`mise run macos-app` builds `CODE_SIGNING_ALLOWED=NO` — which is what keeps the
Mac app buildable by a contributor with no Apple account, the same thing
`DEVELOPMENT_TEAM: ""` exists for.

**The migration that has to go with it is built; the entitlement is not.** The
half that does not need a signing identity is in the tree, and the half that
does is not:

- `KeychainDomain` — `.login` and `.dataProtection`, and a memoised
  `probe()` that adds one fixed non-secret byte under a probe-only service in
  `.dataProtection`, keeps the status, deletes whatever it wrote, and answers
  `.dataProtection` only on `errSecSuccess`. The byte goes in under
  `…AfterFirstUnlockThisDeviceOnly`, the class all three stores write under
  rather than the platform default, because "can this binary reach that
  keychain at all" and "can it store a secret there the way this app stores
  secrets" are different questions and only the second decides where a vault
  root ends up. It **never throws**: it fails open to the weaker-but-reachable
  keychain, deliberately the opposite direction from the accessibility raise,
  because a refused raise leaves a readable secret whose guarantee is wrong
  while a refused domain would leave a secret the app cannot see at all.
- Failing open is right *before* a migration and wrong *after* one, so a `load`
  that finds nothing at the resolved domain **reads the other domain before
  reporting nothing** (`KeychainItem.readAcrossDomains`). Once an item has
  moved, a single transient `SecItemAdd` failure at launch would otherwise make
  the app read an empty login keychain and present the lost-vault screen to a
  user whose vault is intact. **`nil` out of that method means both keychains
  answered *not-found*, and nothing else** — a refusal from the other domain is
  **raised**. An earlier revision wrapped the second read in a blanket `try?`,
  and because `nil` is the one answer `SessionModel` reads as absence, a store
  that was reached and *refused* — locked, or a prompt denied — produced the
  lost-vault screen the fallback exists to prevent, for a condition an unlock
  fixes.

  The argument that swallow rested on — a refused read has changed nothing, so
  answering `nil` is no worse than the answer before the fallback existed — is
  true about the **Keychain** and false about the **caller**: before the
  fallback existed there was no second store to be wrong about, and once there
  is one, `nil` asserts something about it. Its companion, that propagating
  would fail a genuine first run, is false outright: not-found is a refusal in
  neither domain, so nothing-anywhere still answers `nil`, and the first read
  keeps `read()`'s contract in full. This page carried both arguments as the
  reason for the shipped code for one round after the code stopped agreeing
  with them.

  The refusal is raised as `KeychainError.otherDomainUnreadable` rather than as
  the bare status underneath, because a bare `unexpected` shows the *other*
  keychain's sentence — "User interaction is not allowed." — under a header
  naming the one that is working, which sends the user to unlock the wrong
  store. `SessionModel` already maps any throw out of a `load` to
  `.locked(.keychainUnavailable)`, the screen that offers Keychain Access, so
  the vault root needed no new handling. `AccountModel.restore` did: it read the
  credential store with `try?`, so a refusal arrived as a *signed-out* session,
  and the one thing that screen offers is a sign-in that writes a second token
  into the resolved domain while the unreadable copy stays where it is — two
  secrets under one name, `.migrationUnverified` on every later launch, signed
  out in silence for good. It now reports the refusal, and what its Try again
  does with it is decided by the *shape* of the refusal: for every shape that
  may have left a copy of the token unread it re-reads the store instead of
  writing that second token, and for `.migrationUnverified` — where both copies
  have already been read and are known to disagree — it goes to the login,
  because there the sign-in's cross-domain write is what collapses the pair.
  Gating on the fact of a throw rather than on its shape left that one shape
  with no remedy at all.

  **The entitlement is not what the second read has to survive**, and an earlier
  revision of this page said it was. Measured on the same ad-hoc Mac as the
  three results above: a `.dataProtection` **query** answers
  `errSecItemNotFound` (-25300). The -34018 refusal is on the *mutating* calls —
  `SecItemAdd`, `SecItemUpdate` and `SecItemDelete`. That is also why the raise
  executes in no test here, item 7 of the seven below: every read this build can
  make is answered rather than refused. It earns its keep on the entitled Mac,
  where one store can be locked or refuse a prompt while the other answers.

  All three stores `clear` across both domains, for that same rule — whatever a
  read can reach, a clear removes, or signing out would leave a live refresh
  token where the next launch looks. Both deletes are *attempted* before either
  status is raised: the delete is itself refused on the domain this build cannot
  address, so stopping at the first failure would skip the one copy that was
  reachable and make the whole cross-domain clear a no-op.

  The two cross-domain **mutations** swallow only the statuses that mean the
  other store was *unreachable* — the missing-entitlement refusal an unentitled
  build gets, and not-found — and raise anything else. A store that was reached
  well enough to refuse on its own terms, a locked keychain or a denied prompt,
  may still be holding the copy the mutation was supposed to remove, and
  reporting that as success is the failure the cross-domain half exists to
  prevent: on an entitled Mac a swallowed delete leaves a refresh token behind a
  Sign out the user was told had worked, and a swallowed cross-domain write
  leaves the two copies that produce `.migrationUnverified` on every later
  launch. This is behaviourally inert on everything this repository builds,
  where the other domain's refusal *is* the missing-entitlement one; it adds one
  throw on an entitled Mac whose other keychain is locked, which is a real
  failure previously reported as success.

  The **write** raises it as a case of its own,
  `KeychainError.writtenButOtherDomainRefused`, and the distinction is
  load-bearing rather than tidy. `writeAcrossDomains` writes before it cleans
  up, so it can throw with the secret already stored, while a throwing write is
  read by every caller as "nothing was stored". Raised as a bare `unexpected`,
  that misreading cost a session twice on an entitled Mac: a silent renewal
  whose fresh token landed left the stale credential in memory and signed the
  user out at expiry — the sign-out then deleting the token that *had* been
  written — and a sign-in reported as failed for a login that had succeeded,
  with the next launch signing the user in anyway. `KeychainCredentialStore.save`
  is the boundary that resolves it: it is the thing whose `Void` return answers
  "was the token stored", so it treats that one case as the success it is and
  rethrows everything else. The caveat is dropped rather than carried because
  `save` has nowhere to carry it — widening the `CredentialStore` protocol to
  disclose a partial success is the shape #255 proposes for the sign-out path.

  What that does **not** fix, and is worth having written down: the stale copy
  survives, so the next `load`'s migration compares a destination holding the
  fresh bytes against a source holding the stale ones and refuses with
  `.migrationUnverified` — a refused load one launch later, reported rather than
  silent, and collapsed by the next successful sign-in. The refused delete
  creates that state whether `save` rethrows or not; rethrowing only adds the
  lost session on top of it. Closing it means teaching
  `KeychainMigration` that a just-written destination is authoritative, which is
  a change to the verify step and not to either cross-domain mutation.

  **Seven lines are declared untestable** rather than left to be re-discovered,
  on the same rule the rest of this page follows. Three of them are in the pair
  of cross-domain mutations described above (1, 2 and 4), one is in the
  re-raise both of those consult (3), one is in the `save` that consumes the
  write (5), one is in `loadMigratingIfNeeded` (6), and one is in the
  cross-domain **read** (7). This is the whole set and the only count of it,
  since an earlier revision of this page named two of the seven here while a
  second record named five:

  1. `writeAcrossDomains`'s raise of `writtenButOtherDomainRefused`. Every
     other-domain delete this repository can build either succeeds or is
     refused with the missing-entitlement status, which is swallowed before it
     reaches the re-label. Its second precondition is item 3: the raise is
     constructed only inside the `catch` that `deleteInOtherDomain()` enters
     when `meansTheOtherStoreWasUnreachable` answers *false*.
  2. The same method's cross-domain **delete effect**, the other domain's copy
     actually being removed. Nothing this suite can stage puts a copy where the
     delete would find it and still lets the delete run: an item addressed at
     the domain this build cannot reach throws out of `write` first, and on iOS
     the guard short-circuits.
  3. `deleteInOtherDomain`'s re-raise — `meansTheOtherStoreWasUnreachable`
     answering *false* **inside a running cross-domain delete**, for a status
     that is neither the missing-entitlement refusal nor not-found. The
     predicate's `false` answer is itself no longer undeclared: it is `internal`
     now, and `KeychainUnreachableStatusTests` asserts it directly for a locked
     keychain, a denied prompt, a cancelled prompt and an I/O failure, which
     pins the decision the production path takes. What still executes in no test
     is the predicate being *asked* that question mid-case: it is consulted only
     when an other-domain delete throws, and every such delete this suite can
     make throw throws one of the two statuses it answers `true` to, so the
     `else { throw error }` arm is never reached.
  4. The cross-domain clear's **tie-break**, that this domain's status wins
     when both deletes refuse. The one case that reaches the other-domain arm
     has that delete *succeed*. Measured rather than inferred: on 2026-09-17
     the `??` was replaced with a plain `failureToRaise = error` and
     `mise run macos-app` run on the result — 586 passed, 0 failed, 0 skipped,
     so the mutation survives.
  5. `KeychainCredentialStore.save`'s `catch`, which the swallowed
     missing-entitlement refusal never reaches. Reached only through item 1's
     raise, so it carries item 1's preconditions and item 3's with them.
  6. `KeychainMigration.loadMigratingIfNeeded`'s destination-step throw, which
     needs a lock or a denial landing between the source reads and either of
     the two destination calls. This is **one item with three branches**, and
     each is declared rather than folded into the line the `catch` starts on:
     the `accessibilityNotRaised` rethrow, which needs
     `upgradeAccessibilityIfNeeded` to reach its `SecItemUpdate` and be refused
     — it returns early on both of this build's routes instead; the
     `guard let migrated else` re-raise, which needs the `do` to throw with
     nothing rescued from the source; and the `return migrated` rescue, which
     needs it to throw with something rescued. The two fallback cases that call
     this address their destination at `.dataProtection`, so neither enters the
     `catch` at all.
  7. `readAcrossDomains`'s **raise** when the other domain refuses the read —
     the `catch` that relabels the status as
     `KeychainError.otherDomainUnreadable`. Every shape this build reaches
     answers that read rather than refusing it: `.dataProtection` returns
     `errSecItemNotFound`, `.login` answers cleanly, and on iOS the
     `domainsAreDistinctStores` guard short-circuits before the `do` is entered,
     so the `catch` is never entered either.

     This item **replaced** an earlier one at the same line rather than leaving
     the set at six, and the distinction matters to anyone auditing the count.
     The line used to be a blanket `try?`, and that swallow was the single line
     collapsing "the other keychain was reached and refused" into "absent" —
     `nil` being the one answer `SessionModel` reads as absence, it reported a
     vault root that exists and is momentarily unreadable as one that is gone,
     and offered a two-machine pairing ceremony for a condition an unlock fixes.
     Raising repaired that. It did not make the line *reachable*: what blocks it
     is the other store refusing a read mid-case, which no configuration this
     repository builds can produce, so the blocker carried over intact along
     with the item's place in the three-way split below.

     The measurement that used to sit here is **deleted rather than carried
     forward**. It recorded that replacing the `try?` with `try` left the macOS
     suite green — and that replacement is now the shipped code, so it would be
     asserting a survivor for a mutant that no longer exists.

  **Where item 4's measurement comes from, and what does not supply it.** It was
  taken by hand in a worktree on 2026-09-17: the one-line mutation applied,
  `mise run macos-app` run, the result read off `out/test-results/macos.xcresult`,
  and the mutation reverted. **No gate checks it.** The repository's
  `Mutation coverage` and `Mutation coverage gate` jobs are
  `schedule || workflow_dispatch` only, so no pull request can make them report,
  and both are pointed at the Rust crates — `cargo-mutants` never sees a Swift
  file. So that sentence is a dated local observation and is written to read as
  one. If the line's surroundings change, the measurement is stale and has to be
  retaken; nothing will fail to tell you so.

  It used to be one of **two**. Item 7 carried the other until the `try?` it
  measured was replaced by the raise described above, at which point the
  measurement's mutant became the shipped code and the sentence was deleted
  rather than reworded. That is the failure mode this paragraph exists to warn
  about, arriving on schedule.

  **The set does not split in two, and an earlier revision of this page said it
  did.** It splits three ways, because two of the items wait on *both* blockers
  rather than on one:

  - **Reach only — item 2.** Its path is a write that succeeds, a cross-domain
    delete that also *succeeds*, and the other domain's copy then gone. No
    refusal is wanted anywhere; what is missing is only a build that can plant a
    copy in the domain this one cannot write into.
  - **Reach *and* a mid-case refusal — items 1 and 5.**
    `writtenButOtherDomainRefused` is constructed at exactly one site, inside
    `writeAcrossDomains`'s `catch` on `deleteInOtherDomain()`. That `catch` is
    entered only when `deleteInOtherDomain()` rethrows, and it rethrows only
    when `meansTheOtherStoreWasUnreachable` answers *false* — which is item 3.
    So **item 1 executing implies item 3 executing**, and item 5, reachable only
    through item 1's raise, implies both. They inherit item 3's blocker whole,
    on top of their own.
  - **A mid-case refusal only — items 3, 4, 6 and 7.** Reach supplies no part of
    these: the hard statuses they wait for are already produced by the ad-hoc
    build this repository makes. What is missing is the lock or the denied
    prompt landing *while a case runs*.

  Why 1 and 5 need reach at all, item by item:

  - **1** needs `write` to *succeed* before the cross-domain delete is even
    attempted, so on an ad-hoc Mac the item's own domain has to be `.login` and
    the other is then necessarily `.dataProtection` — whose mutations answer
    `errSecMissingEntitlement` unconditionally, and
    `meansTheOtherStoreWasUnreachable` swallows that before the re-label.
    Addressing the item the other way round does not help: `try write(data)`
    sits *outside* the `do`, so it throws first and the delete is never
    reached.
  - **2** needs a copy planted in the domain this build cannot write into, by
    the same mechanism.
  - **5** is reached *only* through 1's raise, so it inherits 1's blockers
    exactly — both of them.

  Both keychains are distinct stores on any Mac, entitled or not —
  `domainsAreDistinctStores` is a compile-time `os(macOS)` value
  (`apps/apple/Sunrise/Identity/KeychainDomain.swift:67-72`) — so what reach
  changes is not that there are two stores but *which* of them a write can land
  in, and therefore which one ends up on the far side of the cross-domain
  delete. Today the write forces the item's own domain to be `.login`, which
  pins the other domain to `.dataProtection` and its one swallowed status. A
  build that reaches both can address the item at `.dataProtection` instead,
  putting `.login` on the far side — a store that answers a delete on its own
  terms rather than with the one status the re-label swallows. That is the
  arrangement 1 and 5 need and this build cannot set up. Item 2 wants the mirror of it: plant a copy in
  `.dataProtection`, address the item at `.login`, and watch the delete take
  that copy away.

  **For 3, 4, 6 and 7 an entitled, signed build is the wrong answer** — which is
  what this page used to say and, for those items, said wrongly. A locked login
  keychain or a denied prompt already returns a hard status on the ad-hoc build
  this repository produces, so the statuses those lines wait for are reachable
  here today; what no suite here can drive is the lock or the denial arriving
  mid-case. An entitlement supplies no part of that.

  **Why the suite cannot drive it — the mechanism, in place of the premise this
  page used to give.** The premise was that "the suite holds the keychain
  unlocked for its whole run by construction". It is false, and it was asserted
  in five places and evidenced in none: the suite performs no keychain-state
  call of any kind — it never creates, opens, locks, unlocks or queries the
  status of a keychain — so it holds nothing. It inherits whatever the host
  login session already unlocked, which is a property of the machine asserted
  as a property of the suite. Two things are true instead, and both can be
  checked against the SDK and this target:

  - `SecKeychainLock`, `SecKeychainUnlock` and
    `SecKeychainSetUserInteractionAllowed` *are* still declared in the macOS
    SDK, but as `API_DEPRECATED("SecKeychain is deprecated", macos(10.2,
    10.10))` and `API_UNAVAILABLE(ios, watchos, tvos, macCatalyst)`.
    `SunriseTests/` compiles into the iOS app as well as the Mac one, so a case
    calling them could only ever run in half the targets it is built into.
  - The lock has **no scope smaller than the machine**. Its target is the
    default login keychain — the one running the CI job and the developer's own
    session. Swift Testing parallelizes by default, and `.serialized` is a
    `ParallelizationTrait` applied to the suite that carries it: it orders that
    suite's own cases and constrains nothing outside it. So no trait available
    here keeps a lock taken inside one case away from the cases running beside
    it, several of which write real `.login` items. Getting back out of it
    without a UI prompt needs the keychain's password, which no case here has,
    so a case that failed between lock and unlock would leave the runner's
    login keychain locked for the rest of the job.

  So a spike that locks the keychain to close these items is rejected on the
  second of those rather than the first: it is a machine-global mutation staged
  inside the suite that guards the vault root.

  What puts 3 and 4 on this side of the line rather than with 1 is that
  `deleteAcrossDomains` stores its first failure and *continues* rather than
  throwing: an item addressed at `.dataProtection` still reaches
  `deleteInOtherDomain()` with `.login` as the other domain, and a locked login
  keychain gives a hard status there on an ad-hoc build today. Item 1 has no
  such route, because its write throws before its delete runs.

  Nor, for any of the seven, is the answer a fault-injection seam inside the type
  that holds the vault root, rejected four times on this change for one reason:
  it would be a second implementation of `Security.framework` to get wrong.

  **Separately, and not one of the seven: `theProbeDeletesWhateverItWrote` is
  vacuous on macOS.** That `KeychainDomainTests` case asserts nothing under
  `probeService` is findable in either domain once `probe()` has run — but on
  this build the probe's `SecItemAdd` into `.dataProtection` is refused, so
  nothing is ever written, and deleting or not deleting gives the same answer.
  Removing the cleanup loop from `probe()` altogether leaves the case green on
  every macOS run; it has teeth only on iOS, where the add succeeds. It is not
  in the seven, because it needs no keychain state this machine cannot produce —
  it needs only to run on iOS, which it already does.

  Worse than vacuous, it is a statement about the **machine**: it asks whether
  anything at all sits under `probeService`, so a Sunrise app running beside the
  suite fails it on a tree that is green. An orphan left by a probe whose
  process died between its `SecItemAdd` and its cleanup no longer does — the
  sweep by service reclaims that one on the `probe()` the case itself runs,
  before it looks — and this paragraph named it alongside the racing app for one
  round after the sweep that fixed it landed in the same change.
  `aProbeReclaimsAnOrphanAnEarlierProbeLeftBehind` is the probe-level assertion
  beside it and the one that actually pins the sweep: it plants an item under
  `probeService` with an account no probe will mint again, runs `probe()`, and
  requires it gone. Planting into `.login` is something every build here can do,
  so unlike its neighbour it has teeth on both platforms. It is what made the
  cleanup a sweep by *service* rather than by the account the probe just minted;
  key it back and the planted orphan survives. Sweeping cannot invert the probe,
  because the add's status is captured before any delete runs — a *fixed*
  account would have inverted it, by making a second concurrent probe's add fail
  with `errSecDuplicateItem` and answer `.login` on iOS.

  The **credential** store also `save`s across both, and it is the only one that
  needs to. Its token is rewritten with no user action — `refreshIfNeeded`
  renews at 75% of the token's life — so one launch whose probe failed open
  leaves a fresh token in one keychain and a stale one in the other, and every
  later launch with a correct probe reads two secrets under one name and raises
  `.migrationUnverified`, which refuses the load rather than signing the user
  out in silence — and which this same cross-domain `save`, run by the next
  sign-in, is what collapses. The vault root and the relay device id are written
  once and never rewritten on the ordinary path, so neither can diverge that
  way. `KeychainMigration`'s own write is deliberately
  exempt too: it deletes its source only after the verify step, and a write that
  removed the other domain would take the source out from under it.
- `KeychainMigration` — five resumable steps holding one invariant: **a
  readable copy exists at every instant.** Read the destination, read the
  source, write the destination, read it back and compare byte-for-byte, and
  only then delete the source. A kill between any two steps leaves a state the
  next launch finishes from. It refuses exactly one thing — a destination whose
  bytes differ from the source's, meaning two different secrets claim one
  `(service, account)` — and falls back to the source item for every other
  Security status, because locking a user out over a *destination* problem
  while the secret is perfectly readable where it has always been is a worse
  trade than the raise takes. That fallback's value is **what `load` answers
  with**: `KeychainMigration.loadMigratingIfNeeded` is the single step all three
  stores call, and a caller that discarded it and read the destination instead
  would see nothing and report the lost vault the fallback exists to prevent.
- Each of the three stores migrates **its own** item inside its own `load`.
  There is no launch-time pass over all three: the OIDC credential is keyed per
  account and the other two per vault, so "all three" is not one set.

None of it changes behaviour on any build this repository can produce. The
probe answers `.login` on an unsigned or ad-hoc-signed Mac, and iOS has only
one keychain, so in both cases the migration's source and destination are two
names for one stored item and it does nothing at all — a case the code checks
for explicitly and the tests pin, because a migration that missed it would
verify that item against itself and then delete it.

What is left for whoever holds an Apple team is the entitlements file,
`DEVELOPMENT_TEAM`, turning the probe's answer over on macOS — **and eight
things in the suite: five test assertions to rewrite, and three tests to
write.**
The *shipping* code needs no further change on this side; the suite does, and
"no further code change is needed" said without that qualification is not
exact. Five assertions encode the fact that this build reaches exactly one
domain, and each is a true statement today that a team makes false:

- `theProbeAnswersWhatThisBuildCanActuallyReach` — `KeychainDomainTests`
- `aDestinationThisBuildCannotReachFallsBackToTheSource`
- `theUnreachableDomainRefusesMutationsAndAnswersReadsAsEmpty`
- `aRefusalOnThisDomainDoesNotSpareTheCopyInTheOther`
- `aWriteRefusedInItsOwnDomainIsNotReportedAsAPartialSuccess`

One further case is on this handoff and is **not** one of the five, because it
does not change its answer — it starts running.
`theMigrationsDestinationWriteDoesNotTakeTheSourceWithIt` in
`KeychainMigrationFallbackTests` is guarded with
`.enabled(if: KeychainDomain.current == .dataProtection)` and skipped on every
build this repository can make. It pins the one arrangement where
`KeychainMigration`'s plain `write(_:)` matters: source and destination sharing
a service and an account and differing only by domain, which is the shape all
three stores build and which no `.login` → `.login` case can construct. Swap
that write for `writeAcrossDomains(_:)` and the destination's cross-domain
delete takes the source with it — and until an entitlement lands, nothing
anywhere will say so.

The last four are in the `KeychainMigrationFallbackTests` suite, which is
`macOS`-only — the `KeychainErrorMessageTests` suite sharing its file sits
outside that gate on purpose and is not one of the five.
Each of the five is **rewritten to assert the entitled behaviour** — not
deleted, and not guarded by an availability check. Deleting them drops the
coverage exactly when the path first runs for real, and four of the five are
the only pins on their behaviour; guarding them leaves the entitled
configuration asserting nothing. (The two platform-conditional accessibility
expectations in `VaultRootStoreTests` flip with them and already say so where
they sit; they are constants rather than assertions, and are not part of the
five.)

The other three are not rewrites: they are tests that have to be **written**,
for items **1, 2 and 5** of the untestable set above — `writeAcrossDomains`'s
raise of `writtenButOtherDomainRefused`, the same method's cross-domain delete
effect, and `KeychainCredentialStore.save`'s `catch`. There is no assertion to
correct for any of the three, because no case in this suite claims anything
about what they do; none has executed on any platform this repository builds
for, which is why they sit in the untestable set rather than in a gap someone
forgot to fill.

**Only one of the three is writable the day a team lands**, and an earlier
revision of this page promised all three of them to the entitlement. That one
is item 2: plant a copy in `.dataProtection`, address the item at `.login`, and
assert the other domain's copy gone after the write. It wants reach and nothing
else, because every step of it *succeeds*.

Items 1 and 5 want the mirror — the item addressed at `.dataProtection`, so
that the delete's refusal comes from `.login` rather than from the domain whose
only answer is the swallowed one — and then `writeAcrossDomains` asserted to
have kept what it wrote, and `save` asserted to treat the raise as the success
it is. But that refusal from `.login` is a *second* blocker rather than a
detail of the first: it is `meansTheOtherStoreWasUnreachable` answering false,
which is item 3 of the set. So 1 and 5 need the team **and** the mid-case lock
or denied prompt items 3, 4, 6 and 7 wait on, and an entitlement on its own
buys neither of them a test. The distinction is worth carrying into the work:
the five are expectations that change their answer, item 2 is new coverage an
entitlement unblocks outright, and items 1 and 5 are new coverage it only half
unblocks.

The remaining **four of the seven — items 3, 4, 6 and 7 — gain no test from an
entitlement** and stay declared. They wait on a lock or a denied prompt landing
mid-case, which an Apple team does not supply; reach was never what blocked
them.

The **hardened runtime**, which is a different setting, is on and has to be:
Apple's notary service rejects a submission without it.
`apps/apple/project.yml` sets `ENABLE_HARDENED_RUNTIME: YES` in
`settings.base`, and that is the only place it is set — a local
`xcodebuild archive` therefore produces the same bundle the release pipeline
signs. The app needs no `com.apple.security.cs.*` exception for it: it loads a
statically linked xcframework and no plug-ins, and neither `RegisterEventHotKey`
nor `AXIsProcessTrusted` is restricted by the runtime.

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

**The app cannot yet register itself.** `sunrise_relay_client::bootstrap` — the
`POST /api/v1/accounts` then `POST /api/v1/devices` pair the CLI runs as
`sunrise bootstrap` — is not exposed across the UniFFI seam, so nothing in the
app produces an id. Until it is, the only way an Apple client is device-bound is
the environment override the CLI has for the same case: launch it with
`SUNRISE_SYNC_DEVICE_ID` set to an id registered elsewhere, and the driver
presents and signs for it. That is the same variable name, the same precedence
(override before stored) and the same meaning as
`sunrise_cli::livesync::ENV_SYNC_DEVICE_ID`.

An unbound driver is not a failure state and is not refused: it is what every
self-host relay runs, and a client that would not connect without a binding
could never reach the relay that mints one.

## Pairing

A second device is paired over a **six-leg copy/paste handshake**: the QR/text
code, three Noise XX messages, a SAS comparison, and the sealed vault root.

**The relay's pairing rendezvous does not exist, so the user is the transport.**
The `relay_url` in the QR payload is a routing label for later; nothing dials
it. Five legs move bytes the user copies between the two Macs; the sixth moves
none, because it is the SAS.

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
- Two-column stream comparison is **deferred**, not cut: it was specified for
  the Tauri app, nothing was built, and it is not a v1 MUST.

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

## Telemetry

Off by default. If the user opts in: minimal anonymous metrics (launches, crash
reports). Crash reports never include vault content — see
[`../10-cross-cutting/telemetry-and-privacy.md`](../10-cross-cutting/telemetry-and-privacy.md).
