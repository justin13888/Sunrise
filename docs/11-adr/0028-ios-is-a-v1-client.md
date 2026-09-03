# 0028 — iOS is a v1 client with its own parity column, at SHOULD level

**Status:** accepted

**Amends:** [`../07-clients/parity-matrix.md`](../07-clients/parity-matrix.md)
(one new column, 31 cells, plus an audit section, two hard rules — the iOS
regression rule and the definition of a qualified *met* — and five verdicts in
the MUST-carrying columns regraded to qualified *met*s under that definition)
and
[`./0019-swiftui-macos-client.md`](./0019-swiftui-macos-client.md) (revisit
trigger 1).

## Context

### A revisit trigger fired and nothing was decided

[ADR-0019](./0019-swiftui-macos-client.md) §What would force revisiting this
opens with:

> 1. **iOS shipping.** The spike proved macOS only. An iOS slice needs its own
>    spike before it is planned, not after.

That trigger has fired. The amendment recorded underneath it says so, and then
declines to draw the conclusion: *"**It does not make iOS a v1 client:** the
parity MUSTs are still macOS's, and no MUST has been transferred."* Both halves
of that sentence are true and the second does not follow from the first. A
client can be a v1 client at a level below MUST — that is what the SHOULD mark
exists for, and the parity matrix already uses it for macOS's own vim-mode and
print rows. The amendment answered "did a MUST move?" when the question the
trigger asked was "is this a client we ship?".

This ADR answers the question that was left open.

### What actually ships

Read from the tree rather than from a plan:

- **`apps/apple/project.yml:157-212` defines `SunriseiOS`**, a full application
  target: iOS 26.0 (`:29`, matching the Mac's major so the shared tree needs no
  `@available` forks), `TARGETED_DEVICE_FAMILY: "1,2"` (`:191`, iPhone and
  iPad), a `sunrise://` registration of its own (`:198-201`), and
  `AppIntents.framework` named explicitly (`:173`) so
  `appintentsmetadataprocessor` writes the metadata bundle without which the
  intents link and are never offered.
- **`SunriseiOSTests` (`:273-290`) compiles the same `SunriseTests/` sources a
  second time against the iOS product.** Both app targets pin
  `PRODUCT_MODULE_NAME: Sunrise` precisely so `@testable import Sunrise`
  resolves in either bundle. The claim this buys is not "the iOS app compiles"
  but "the shared half behaves the same on both platforms".
- **`SunriseiOSUITests` (`:250-262`) is not skipped** in the `SunriseiOS`
  scheme (`:337-351`), unlike `SunriseUITests`, which is `skipped: true` in the
  macOS scheme (`:318-319`) because a macOS XCUITest needs
  `sudo DevToolsSecurity -enable` on the machine. Five cases run on the
  simulator on every build (`SunriseiOSUITests/TabShellUITests.swift:24`,
  `:46`, `:77`, `:98`, `:133`). **iOS is the only Apple product where CI proves
  a tap reaches the core.**
- **`.github/workflows/ci.yml:114-158` gates `ios-app` no further than the
  workflow itself** — the job has no `if:` and no path filter, so it runs every
  time CI runs, which the triggers define as pushes to `master` and
  `v1-rewrite` (`:4-5`), pull requests targeting those two branches (`:6-7`),
  the 04:00 UTC nightly (`:8-10`) and manual dispatch (`:11`). The two
  `branches:` filters match different things — `push` matches the ref that was
  pushed, `pull_request` matches the **base** the request targets — and both
  are narrower than "every push and every pull request": a push to a feature
  branch matches neither list, and a pull request stacked on another feature
  branch is filtered out on its base, as the one carrying this ADR was, based
  on `docs-51-architecture-freeze`. Neither the schedule nor `workflow_dispatch`
  is branch-filtered, so any ref can still be built on demand — what the filters
  bound is what happens *automatically*. It runs on the same pinned `macos-26`
  image the macOS job uses, adding both iOS Rust slices to the pinned toolchain
  first (`:138`).
- **`apps/apple/iOS/` is 647 lines** of shell — a five-tab `TabView` with
  `.tabViewStyle(.sidebarAdaptable)` so a phone gets a tab bar and an iPad a
  sidebar from one declaration — over the same `apps/apple/Sunrise/` views the
  Mac compiles. `mise run ios-run` builds it, boots the simulator, installs and
  launches it.

### Why an all-dash column is a claim, not a neutral placeholder

The matrix opens by saying what its marks are — requirement levels, not
status, with a separate audit underneath for what is actually reachable. Its
§Hard rules close with **"A deferred client has no MUSTs. When one is
scheduled, its column is filled in and the fill-in is the commitment"**.

Put together, a column of `—` under a `*deferred*` header states that nothing
whatsoever is required of this client. For a client that is scheduled, built,
run in CI on every pull request into `master` or `v1-rewrite`, and the *only*
one whose UI tests execute, that is not a placeholder — it is a false
statement, and it is the one that makes it impossible to say that any iOS
behaviour has regressed. Four open issues
([#14](https://github.com/justin13888/Sunrise/issues/14),
[#31](https://github.com/justin13888/Sunrise/issues/31),
[#40](https://github.com/justin13888/Sunrise/issues/40) and
[#42](https://github.com/justin13888/Sunrise/issues/42)) already target iOS
surfaces and have no row to attach to. For two of them that is exactly what the
dash column costs: widgets and the platform surfaces `mobile-ios.md` specifies
are iOS behaviour this table currently requires nothing of, and filling the
column is what gives them somewhere to land. The other two do not land, and
both are worth naming as exceptions rather than counted as wins. **#40** —
`sunrise://focus` and `sunrise://share` unparsed — gains visibility and not a
row: the matrix grades no URL scheme in any column, which is why the
Consequences below say it is "unchanged and now visible". **#42** is the same
shape one layer down: the vault root's Keychain accessibility class,
`Keychain.swift:62`, measured against
[`../07-clients/mobile-ios.md`](../07-clients/mobile-ios.md) §OS keystore
(`:185-188`). It targets an iOS surface, but the row it lacks is missing from
**every** column — the matrix grades no key storage anywhere, and the constant
is shared, so macOS is affected identically. Filling the iOS column gives
neither a home, and this ADR adds neither row.

**What makes this an ADR is not the N/A rule.** The rules do say a cell marked
N/A may only be revisited with a record, but no such cell is revisited here.
On the base revision the widget row's six cells read
`N/A | N/A | — | — | — | —` under `macOS | CLI | iOS | Android | Web | TUI`:
the iOS cell this ADR fills was a dash, and the two N/A cells beside it stay
exactly where they are — Decision 3 below says so outright. What forces the
record is
[ADR-0019](./0019-swiftui-macos-client.md)'s own revisit trigger, quoted above:
it fired, its amendment declined to draw the conclusion, and answering a
question another ADR raised and left open is what this directory is for. The
requirement levels in 31 cells change, the column header has to cite
something, and Decision 5 adds a rule to the matrix's own §Hard rules
([`../07-clients/parity-matrix.md`](../07-clients/parity-matrix.md#hard-rules)),
which is a change to the table's standing terms and wants a reason on the
record. What the matrix keeps in [`../11-adr/`](../11-adr/) is narrower — the
reason an N/A or a *deferred* mark moved — and neither moves here.

## Decision

**iOS / iPadOS is a v1 client. Its parity column is filled in, at SHOULD level,
and it carries no MUSTs until an iOS release ships.**

1. **The matrix gains a filled iOS column**, 31 cells, replacing 31 dashes. The
   header's `*deferred*` mark becomes **v1**, citing this ADR.

2. **Every row the shell reaches is a SHOULD** — "v1 if feasible, otherwise
   v1.x", as the matrix's own legend defines it. SHOULD is assigned by
   *reachability from the shipped shell*, not by code provenance: a row is a
   SHOULD because a user can get to the capability, whether the code behind it
   is shared with the Mac or written for iOS. Twenty-three rows qualify.

3. **Rows the shell cannot reach are N/A or *deferred*, never a silent dash.**
   - **N/A exactly once: Menu bar.** The row means the macOS `MenuBarExtra`
     status item (`macOS/SunriseMacApp.swift:63`) — a persistent, glanceable
     surface outside the app's own window. iOS has no status-item equivalent;
     the nearest thing is a widget, which is its own row. (An iPad that draws a
     system menu bar gets only the system's own items. The `.commands { … }`
     scene modifier that would populate it is at
     `macOS/SunriseMacApp.swift:30-61`; `macOS/AppCommands.swift` holds the
     `View`s it hangs in those menus — `AppMenuItems`, `CommandMenuItem`,
     `NewMenuItems`, `IcalMenuItems`, `PrintMenuItems`, `GoMenuItems` — and
     declares no `Commands`-conforming type, as nothing in the tree does. The
     `SunriseiOS` target's `sources:` are `Sunrise` and `iOS`
     (`project.yml:160-164`), so it compiles neither file. That is a
     keyboard-navigation narrowness, recorded there, not a second menu-bar
     row.)
   - ***deferred*** four times: the three [ADR-0020](./0020-v1-must-demotions.md)
     capabilities (both sharing rows and Google Calendar) inherit their
     capability-level deferral unchanged, and **Lock screen / home screen
     widget** becomes deferred with
     [#14](https://github.com/justin13888/Sunrise/issues/14) as its tracker.
     Widgets are *specified* in [`../07-clients/mobile-ios.md`](../07-clients/mobile-ios.md)
     and not scheduled, which is exactly what *deferred* means; MAY would say
     "unspecified future", which is false of a surface with a written spec.
     **The macOS and CLI cells on that row stay N/A** — nothing here revisits
     them.
   - Three rows are **MAY**: Watch app, Mouse and Print / PDF export.

4. **iOS does not enter the matrix's "every MUST is met" sentence.** That
   sentence is about MUSTs, the MUSTs live in two columns, and iOS carries
   none. Its SHOULDs are graded in a section of their own, measured the same
   way — reachability from a running binary.

5. **A regression rule, weaker than the MUST rule and deliberately so.** An iOS
   SHOULD graded **met** in the audit may not become unmet *silently*: the pull
   request that causes the regression updates the audit row in the same pull
   request. No ADR is required — there is no release to protect, so this cannot
   be the MUST rule, and a decision record for every SHOULD would price the
   rule out of being followed. What it does protect is the audit: a green row
   is a claim that somebody traced a surface to a seam, and deleting the
   surface without touching the row throws that away and leaves the table
   lying. Recorded as a hard rule in the matrix.

6. **Promotion to MUST parity is a separate ADR**, written when an iOS release
   is cut. This one deliberately does not pre-commit to it.

## Alternatives considered

**Leave iOS deferred.** Rejected as false by the file's own definition:
*deferred* is "specified, not scheduled for v1", and iOS is built and tested
on every pull request into `master` or `v1-rewrite`. The matrix additionally
forbids using *deferred* "to make this table agree with the code after the
fact"; leaving it in place would
be that same failure pointed the other way — keeping the table disagreeing with
the code because moving it is work.

**MUST parity with macOS.** Rejected. No iOS release exists, and three MUSTs
would be unmet the moment they were written:

- **Saved searches / views.** `SavedViewsModel` is built by the shared
  `VaultModels`, and `iOS/VaultTabs.swift:456` even loads it — but
  `SavedViewsMenu` is instantiated in exactly one place,
  `macOS/VaultWindow.swift:89`. The model runs on iOS and nothing shows it.
- **iCal import / export.** The URL-taking halves of `importIcal` / `exportIcal`
  are shared, but the picker-driven entry points are inside
  `#if os(macOS)` (`Sunrise/Ical/AppSurfaces+Ical.swift:52-75`), the
  `IcalSurfaces` modifier is applied only at `macOS/VaultWindow.swift:141`, and
  the File-menu items live in `macOS/AppCommands.swift:104-118`.
- **Keyboard navigation (full).** `onKeyChord` is applied in exactly one place
  in the whole tree, `Sunrise/Views/TaskListView.swift:118`, with
  `scope: .list`. Every `.application`-scoped binding in
  `Sunrise/Keyboard/Keymap.swift:177-199` — ⌘N, ⌘⇧N, ⌘1, ⌘2, ⌘F, ⌘K, ⌘⇧P,
  ⌘⇧S, ⌘Z, ⌘⇧Z, ⌘P, ⌘/ — reaches a user only through the Mac's `Commands`
  scene. The command palette and the cheat sheet are handed inert closures on
  iOS (`iOS/VaultTabs.swift:283-288`).

Writing three MUSTs that are unmet on the day they are written is how a
requirement level stops meaning anything.

**MAY for every row.** Rejected: it says less than the tree already proves. MAY
means "future". A capability that a UI test drives on a simulator in CI on
every pull request into `master` or `v1-rewrite` is not future.

**SHOULD.** Chosen. It is the only mark that is true of a client which ships,
is tested, and has not been released.

## Consequences

- **The matrix gains an `### iOS — 23 SHOULDs` audit section.** It grades **21
  met, 2 unmet**. The two unmet rows — saved views and iCal — share one shape:
  a working, tested shared model with no iOS caller. They stay **SHOULD** and
  are not demoted to *deferred*; "nobody has built it" is status, and status
  belongs in the audit, not in the requirement level. That is the distinction
  the matrix's own "the marks are requirement levels, not status" callout
  draws.

- **Five qualified verdicts**, and the matrix now defines the vocabulary for
  them: search is `met *(plain-text half)*` (it inherits the macOS FTS
  narrowness, [#28](https://github.com/justin13888/Sunrise/issues/28)),
  keyboard navigation is `met *(list keymap)*`, pairing-scan is
  `met *(paste half)*` (no camera scanner exists on **either** platform),
  background sync is `met *(frontmost only)*`, and drag-and-drop is
  `met *(six of eight cells)*` — iOS reaches six of the eight cells in
  [`../07-clients/interaction-patterns.md`](../07-clients/interaction-patterns.md)'s
  matrix where macOS reaches seven, *Task → Calendar block* being a **No** here
  for want of a screen that shows both of its ends. The same definition then
  applies to the MUST-carrying columns, swept over the rows whose shortfall the
  audit or the narrow notes already record — five of them: macOS drag-and-drop
  `met *(seven of eight cells)*` and macOS iCal `met *(windowed, no
  round-trip)*`; the CLI's `today` view (`met *(today, no context filter)*`),
  multi-account (`met *(0600 on unix only)*`) and iCal. Seven of eight is a
  scope note as much as six is, and a rule the table visibly broke in its
  better-served columns would stop being a rule. **Every one stays a met**: the
  23 macOS MUSTs and the 9 CLI MUSTs are all still met, because a qualifier
  scopes a verdict rather than demoting it.

- **[#14](https://github.com/justin13888/Sunrise/issues/14) gains a cell.** The
  widgets row's iOS cell is where widget work now lands. Grepping `apps/apple`
  finds no `WidgetKit`, `BGAppRefreshTask`, `ActivityKit`, `WatchConnectivity`
  or `SecureEnclave`, and `project.yml` declares no widget, share or watch
  extension target — as
  [#31](https://github.com/justin13888/Sunrise/issues/31) says and
  [`../07-clients/mobile-ios.md`](../07-clients/mobile-ios.md) §Platform
  surfaces repeats. Several source comments in `iOS/` name "the Control Center
  control, the widget" as capture routes; those describe the intended set, not
  the built one.

- **[#31](https://github.com/justin13888/Sunrise/issues/31) gains a bound.**
  Background sync is a SHOULD graded `met *(frontmost only)*`:
  `iOS/VaultTabs.swift:388-394` starts the live driver and `:448-457` keeps it
  for the life of the shell, and there is no `BGAppRefreshTask` anywhere. The
  row now says what the gap is against.

- **[#40](https://github.com/justin13888/Sunrise/issues/40) is unchanged and
  now visible.** `Sunrise/Notifications/DeepLink.swift` is shared and
  `iOS/TabRoute.swift:83-116` routes every destination it produces to a tab and
  a stack — but `sunrise://focus` and `sunrise://share` are still unparsed on
  both platforms.

- **[#12](https://github.com/justin13888/Sunrise/issues/12) doubles in size.**
  Every user-facing string in `apps/apple/Sunrise/` now ships on two platforms.
  `Platform.deviceName` (`Sunrise/Platform/PlatformKit.swift:163-183`) exists
  for exactly this and has **two** callers (`Views/LockedView.swift:72`,
  `Views/OnboardingView.swift:32`); **twenty-eight** further lines across six
  shared files still put "Mac" in a string the user reads —
  `Pairing/PairingModel.swift` (11), `Views/PairingView.swift` (8),
  `Views/AccountView.swift` (5), `Sync/SyncPresentation.swift` (2),
  `Notifications/NotificationAuthorization.swift` and
  `Keyboard/CommandPaletteView.swift` (1 each). Comments, the `addThisMac`
  intent case and the `#if os(macOS)` arm of `deviceName` itself are excluded;
  the count is of user-facing text. Among them are the pairing sheet's own
  title (`Views/PairingView.swift:32`, driven by that intent case) and the vim
  toggle's caption (`Views/AccountView.swift:222`, "Stored on this Mac only").
  A phone tells its user it is a Mac. This is filed as narrowness in the
  matrix; it has no issue of its own yet.

- **A layout defect is now inside a graded row rather than outside the table.**
  Seven shared sheets an iOS user can reach carry unconditional Mac-sized
  frames — the settings `Form` is `.frame(width: 520)`
  (`AccountView`, `Views/AccountView.swift:101`), the pairing sheet 560×520
  (`PairingView`, `Views/PairingView.swift:25`), the task editor 460
  (`TaskEditorView`, `Views/TaskEditorView.swift:97`), the routine editor 440
  (`RoutineEditorView`, `Views/RoutineEditorView.swift:132`), and three in
  `Views/BlockEditorView.swift`, which holds three sheets and not one: the
  block draft sheet 420 (`BlockDraftSheetView`, `:35`), the block editor 460
  (`BlockEditorView`, `:155`) and the conflict adjuster 620
  (`AdjustBlocksView`, `:280`) — all wider than an iPhone, none behind an
  `#if`. The reference for "an iPhone" is the device the UI tests actually run
  on: the `iPhone 17 Pro` simulator `mise.toml` pins as `ios_sim` (`:46`, and
  [`../07-clients/mobile-ios.md`](../07-clients/mobile-ios.md) §Run it), about
  402 points wide in portrait against a narrowest-of-the-seven of 420. The
  bound is sensitive to that choice and the number is not a law of nature — on
  a 440-point Pro Max only five of the seven are still too wide, and on an
  iPad none of them is. Those seven are every fixed width over that reference
  that the tab shell can put on screen — the tree holds plenty of narrower ones
  (`TaskListView.swift:276`, `StreamEditorView.swift:77`, `ReviewView.swift:353`
  at 380; `CalendarView.swift:44` at 140), and a frame that fits is not a
  defect. Two further things the sentence does not claim. It is not every
  oversized frame in a shared file: the rest are either unreachable from the
  tab shell (`Views/IcalView.swift:35`, behind the
  `IcalSurfaces` modifier applied only at `macOS/VaultWindow.swift:141`;
  `Keyboard/CommandPaletteView.swift:20` and `:182`, behind a palette iOS hands
  an inert closure at `iOS/VaultTabs.swift:286`) or already guarded
  (`Views/QuickCaptureView.swift:88-91`). And it does not count `minWidth:`
  floors, which are not fixed widths: `Views/AttachmentsView.swift:31` sets one
  at 420, shared and unguarded on a surface graded **met**, but its only
  instantiation is `Views/TaskEditorView.swift:93` — inside the task editor,
  whose fixed 460 at `:97` is already in the list — so it widens nothing the
  list does not already carry. Multi-account and first-run pairing are graded
  **met** because the capability is reachable; the width is recorded
  under *What is still narrow* rather than allowed to sink a verdict it does
  not change.

- **ADR-0019's revisit trigger 1 is superseded on this point only.** Its
  amendment's remaining claims stand: the seam held, a second Apple platform
  cost UI work and not a second core, and no MUST was transferred.

- **Android and Web are untouched.** Both remain deferred clients with no
  MUSTs — Web by [ADR-0012](./0012-web-wasm-deferred.md), Android by
  [ADR-0027](./0027-v1-self-host-first.md). The TUI column is untouched.

- **Twelve documents are amended** so the tree and the docs stop disagreeing:
  the parity matrix, `07-clients/{overview,shared-ui-system,interaction-patterns,mobile-ios}.md`,
  `08-features/{keyboard,inbox-and-capture}.md`,
  `01-architecture/{overview,shared-core}.md`, `02-domain/notes.md`,
  `implementation/overview.md` and the README. No code changes.

## What would force revisiting this

1. **An iOS release shipping.** Everything above is levelled on there being no
   release yet: SHOULD is chosen because this is a client that ships, is tested
   and has *not* been released; the regression rule is weakened for that exact
   reason (Decision 5); and Decision 6 already reserves promotion to MUST
   parity for its own record. Cutting a release fires all three at once, and
   the re-entry point is that promotion ADR, not an edit to this one.
2. **`ios-app` ceasing to prove that a tap reaches the core.** The case against
   *deferred* and against MAY is evidential — a UI test drives the shell on a
   simulator in CI on every pull request into `master` or `v1-rewrite`. Delete
   the job, put an `if:` or a path filter on it, or mark `SunriseiOSUITests`
   `skipped: true` the way the macOS scheme marks its own
   (`project.yml:319`), and every row here falls back to "it compiles", which
   this file says is not evidence.
3. **A widget, share or watch extension target appearing in `project.yml`.**
   Decision 3 defers the widget row, and grades *Watch app* MAY, on the
   strength of there being no such target — today `targets:` declares six, all
   applications and test bundles, with `aggregateTargets:` adding `SunriseFFI`
   (`project.yml:50-51`). One landing moves the widget row off
   *deferred*, and the macOS and CLI cells on that row **are** N/A, so unlike
   this ADR that edit does meet the N/A rule and needs its own record.
4. **The shared tree ceasing to be shared.** SHOULD is assigned by reachability
   from the shipped shell over one `Sunrise/`, and ADR-0019's surviving claim
   is that a second Apple platform cost UI work and not a second core. Rows
   starting to fork behind `#if os(iOS)`, or an iOS-only model growing beside a
   shared one, would make reachability a per-platform measurement and this
   column a second implementation rather than a second shell.
5. **The audit going stale anyway.** Decision 5 traded an ADR gate for an
   obligation on the pull request that regresses a row, on the argument that a
   record of decision per SHOULD would price the rule out of being followed. If
   green rows outlive the surfaces behind them, that trade was wrong and the
   rule should be re-cut at MUST strength.
