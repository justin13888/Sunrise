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
single step on every push and PR to `master` and `v1-rewrite`, plus a 04:00 UTC
nightly on `master` alone, since GitHub fires a `schedule` only on the
repository's default branch. Any other ref builds on demand through
`workflow_dispatch` — `gh workflow run ci.yml --ref <branch>` — which carries
no branch filter at all. So a Swift-side break is caught.

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
  overwriting the first.
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
uses most. A Mac App Store build would have to trade that away; it is under
evaluation, not committed.

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
- Only the 32-byte vault root crosses, not the full `PairingPayload` the crypto
  spec describes, so each side still submits `Command::TrustDevice` separately.
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

**Specified, not built.** Sparkle-style signed updates over the direct channel,
applied on next launch — a running session is never interrupted by an update.
Channels: `stable`, `beta`. There is no Sparkle dependency in the project today
and no update path of any kind.

## Telemetry

Off by default. If the user opts in: minimal anonymous metrics (launches, crash
reports). Crash reports never include vault content — see
[`../10-cross-cutting/telemetry-and-privacy.md`](../10-cross-cutting/telemetry-and-privacy.md).
