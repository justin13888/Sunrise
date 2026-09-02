---
status: accepted
---

# Client Feature Parity Matrix

Marks: **MUST** = ships in v1; **SHOULD** = v1 if feasible, otherwise v1.x;
**MAY** = future; **N/A** = doesn't apply on the platform; ***deferred*** =
specified, not scheduled for v1, with the reason recorded in
[`../11-adr/`](../11-adr/).

> **The marks are requirement levels, not status.** A MUST says "v1 does not
> ship without this"; it does not claim the capability exists today. The
> [v1 status audit](#v1-status-audit) below is the separate, measured record of
> what is actually reachable from a running binary, and it is the one to read if
> the question is "is it built yet". Keeping the two apart is deliberate: a
> requirement edited to match the tree stops being a requirement.

**Three** clients ship in v1: the **macOS** app, the **iOS / iPadOS** app and
the **CLI**. macOS and the CLI carry the MUSTs. iOS carries **SHOULDs and no
MUSTs** until a release ships — [ADR-0028](../11-adr/0028-ios-is-a-v1-client.md)
is the record of why, and it is also why iOS has a filled column here rather
than a row of dashes. **Android** and **Web** are ***deferred*** — specified,
not scheduled, and carrying no MUSTs, because a deferred client cannot regress
one. The **TUI** was removed by
[ADR-0019](../11-adr/0019-swiftui-macos-client.md); its column is kept for one
release so the table records what was withdrawn rather than quietly losing it.

| Capability | macOS | CLI | iOS | Android | Web | TUI |
|---|---|---|---|---|---|---|
| | | | **v1** ([ADR-0028](../11-adr/0028-ios-is-a-v1-client.md)) | *deferred* | *deferred* | *removed* |
| Read/write tasks | MUST | MUST | SHOULD | — | — | — |
| Streams, contexts, routines | MUST | MUST (read + capture) | SHOULD | — | — | — |
| Today / Inbox / Stream views | MUST | MUST (list form) | SHOULD | — | — | — |
| Focus mode | MUST | MUST (`next`, `focus <id>`) | SHOULD | — | — | — |
| Time-blocking on calendar grid | MUST | N/A | SHOULD | — | — | — |
| Notes (rich text) | MUST (a Task's `body`; scope per [ADR-0020](../11-adr/0020-v1-must-demotions.md)) | MAY | SHOULD (a Task's `body`; scope per [ADR-0020](../11-adr/0020-v1-must-demotions.md)) | — | — | — |
| Attachments — view image/PDF | MUST | N/A | SHOULD | — | — | — |
| Attachments — upload | MUST | MAY | SHOULD | — | — | — |
| Search (FTS) | MUST | MUST | SHOULD | — | — | — |
| Saved searches / views | MUST | MAY | SHOULD | — | — | — |
| Keyboard navigation | MUST (full) | N/A (non-interactive) | SHOULD (the list keymap, on an attached keyboard) | — | — | — |
| Drag-and-drop | MUST | N/A | SHOULD (long-press) | — | — | — |
| Quick capture (global hotkey / system surface) | MUST (global hotkey, menu bar) | MUST (`sunrise capture`) | SHOULD (capture sheet, inline bar, App Shortcut) | — | — | — |
| Reminders / scheduled local notifications | MUST | N/A (one-shot process) | SHOULD | — | — | — |
| Multi-account | MUST | MUST (`SUNRISE_VAULT`) | SHOULD | — | — | — |
| Pairing — scan QR | MUST (camera or paste) | MAY (manual code entry) | SHOULD (camera or paste) | — | — | — |
| Pairing — show QR | MUST | MAY (ASCII QR) | SHOULD | — | — | — |
| Sharing — accept invite | *deferred* ([ADR-0020](../11-adr/0020-v1-must-demotions.md)) | MAY | *deferred* ([ADR-0020](../11-adr/0020-v1-must-demotions.md)) | — | — | — |
| Sharing — view shared stream as editor | *deferred* ([ADR-0020](../11-adr/0020-v1-must-demotions.md)) | *deferred* ([ADR-0020](../11-adr/0020-v1-must-demotions.md)) | *deferred* ([ADR-0020](../11-adr/0020-v1-must-demotions.md)) | — | — | — |
| Calendar integration (Google) | *deferred* ([ADR-0020](../11-adr/0020-v1-must-demotions.md), [#4](https://github.com/justin13888/Sunrise/issues/4)) | MAY | *deferred* ([ADR-0020](../11-adr/0020-v1-must-demotions.md), [#4](https://github.com/justin13888/Sunrise/issues/4)) | — | — | — |
| iCal import / export | MUST | MUST | SHOULD | — | — | — |
| Background sync | MUST (while running) | N/A (`sync --once` for cron) | SHOULD (while frontmost) | — | — | — |
| Menu bar | MUST | N/A | N/A | — | — | — |
| Lock screen / home screen widget | N/A | N/A | *deferred* ([#14](https://github.com/justin13888/Sunrise/issues/14)) | — | — | — |
| Watch app | N/A | N/A | MAY | — | — | — |
| OS automation surface (App Intents / Shortcuts) | MUST | MUST (the CLI *is* one) | SHOULD | — | — | — |
| Vim-style modal navigation | SHOULD (opt-in) | N/A | SHOULD (opt-in; attached keyboard) | — | — | — |
| Mouse | MUST | N/A | MAY (iPadOS pointer) | — | — | — |
| Touch | MAY | N/A | SHOULD | — | — | — |
| Print / PDF export | SHOULD | MAY (`export`) | MAY | — | — | — |
| First-run pairing | MUST | SHOULD | SHOULD | — | — | — |

## The v1 scoping pass (ADR-0020)

Five cells above changed as part of defining v1 — four lose a MUST, one keeps it
and gains a scope note. [ADR-0020](../11-adr/0020-v1-must-demotions.md) is the
record of why.

**This is not a regression, and a later reader should not read it as one.** The
hard rule below governs a *released* capability: a v1.0 → v1.1 release cannot
remove a MUST. v1 has not shipped. These marks are a pre-1.0 v1 definition being
set once, before anything was promised to a user — no shipped capability is
being withdrawn, because none of these ever shipped. Had v1.0 been out, the
answer would have been to build them.

- **Sharing — accept invite** (macOS) and **Sharing — view shared stream as
  editor** (macOS and CLI) → *deferred*. The crypto and domain designs are
  specified in full and the underlying primitives are frozen and tested, but the
  entity the design operates on does not exist: nothing anywhere reads or writes
  the `persons` table, and no `share_grant` op is implemented. Closing it is a
  second, security-critical epic — a sharing model that is almost right in an
  end-to-end-encrypted product is a vulnerability, not a partial feature.
- **Calendar integration (Google)** (macOS) → *deferred*. Already decided:
  [issue #4](https://github.com/justin13888/Sunrise/issues/4) was deferred out of
  the v1 epic, and two accepted specs cannot disagree about whether it ships. The
  provider itself is implemented and tested; what is missing is the wiring and
  storage around it.
- **Notes (rich text)** (macOS) stays a **MUST**, and it is now **met** — this is
  a scope clarification, not a deferral. The row means a **Task's `body`**:
  persisted, FTS-indexed, and editable across the seam as structured blocks
  rather than as bytes the client parses. It does **not** mean the free-standing
  `Note` entity (specified, never wired), and it does not mean collaborative
  text — a body merges as one last-writer-wins unit
  ([ADR-0014](../11-adr/0014-entity-level-lww-merge.md)), so two devices editing
  one body concurrently leave one survivor, not a merge.

  The editor shipped with a safety interlock worth naming here, because it is
  what keeps the LWW rule from being lossy in the ordinary case: the domain
  codec reports a `Fidelity`, computed by **re-encoding the decoded blocks and
  comparing bytes**. A body this build cannot reproduce exactly is rendered
  **read-only** instead of being rewritten, so an editor that does not
  understand a future block shape cannot silently flatten it on the next save.

## v1 status audit

Re-measured on branch `v1-rewrite` by tracing each capability from a
**user-reachable surface** — a view something presents, a menu command, a
subcommand, an OS entry point — down to a real seam or core call. A file that
compiles is not evidence; an unreachable correct implementation counts as unmet,
which is the whole point of grading this way.

Verdicts: **met** / **partial** (reachable, narrower than the row) / **unmet**.

**Every MUST is met.** The MUSTs live in two columns — macOS and the CLI. iOS
ships too and carries none: [ADR-0028](../11-adr/0028-ios-is-a-v1-client.md) puts it at
SHOULD level until a release, and its rows are graded in an **iOS** section of
[this audit](#v1-status-audit) below. The previous revision of
this audit recorded one unmet macOS MUST (iCal import/export), one partial macOS
MUST (drag-and-drop) and three partial CLI MUSTs (read/write tasks, the Stream
view, multi-account); all five were closed in code, and each was re-traced from
a surface rather than taken on report. What remains narrower than the prose
around it is recorded in the cells below and in
[What is still narrow](#what-is-still-narrow) — an audit whose every row says
"met" is worth nothing if the narrowness is not written down beside it.

### macOS — 23 MUSTs

| Capability | Verdict | Reached from |
|---|---|---|
| Read/write tasks | met | sidebar → `TaskListView` → `TaskListModel` → `submit`/`query` |
| Streams, contexts, routines | met | `BrowseSidebar` CRUD; `RoutinesView` → `RoutineEditorView` |
| Today / Inbox / Stream views | met | `Destination.fixed` + per-stream tags → `TaskListView` |
| Focus mode | met | sidebar → `FocusView` → `FocusModel` (plan, start, interrupt, cascade) |
| Time-blocking on calendar grid | met | sidebar → `CalendarView` → `CalendarModel`; day and week |
| Notes (rich text) | met | task editor → Notes pane → `NoteBodyEditor`; scope per ADR-0020 |
| Attachments — view image/PDF | met | task editor → Attachments pane; `PDFKit` inline, images inline |
| Attachments — upload | met | `Attach…` file importer **and** a drop target on the pane |
| Search (FTS) | met *(plain-text half)* | sidebar / `⌘F` / `⌘K` → `SearchView`, 150 ms debounce. The query that reaches FTS5 is a literal AND of quoted terms over tasks; the operator grammar, negation and by-kind grouping in [search.md](../08-features/search.md) are specified and not built ([#28](https://github.com/justin13888/Sunrise/issues/28)) |
| Saved searches / views | met | toolbar → `SavedViewsMenu`; the same `views.toml` the CLI reads |
| Keyboard navigation | met | every binding in [keyboard.md](../08-features/keyboard.md)'s macOS column, transcribed as data in `Keymap.swift`, plus the palette and the cheat sheet |
| Drag-and-drop | met | seven of the eight rows in [interaction-patterns.md](./interaction-patterns.md#drag-and-drop-matrix)'s matrix: task → stream, task → context, task → calendar block, block move/resize on the grid, task → task reorder, stream reorder, file → attachments. The eighth (Calendar block → Task) is not built — the window is a sidebar plus one detail pane, so a grid and a task list are never both on screen and the gesture has no two surfaces to connect |
| Quick capture (hotkey / menu bar) | met | Carbon `RegisterEventHotKey` ⌘⇧N + `MenuBarExtra`; both via `previewCapture` |
| Reminders / local notifications | met | `ReminderScheduler` follows the change feed, reconciles against pending requests, snooze targets from the domain |
| Multi-account | met | Settings → vault picker → `SessionModel.switchTo`, teardown before reopen |
| Pairing — scan QR | met *(paste half)* | `PairingView` paste-accept → `DevicePairing.accept`. **No camera scanner exists**; the row's "camera or paste" is satisfied by paste |
| Pairing — show QR | met | `QRCode.image` (CoreImage) rendered on the code leg, with copyable text beside it |
| iCal import / export | met | File → Import Calendar… (⌘⇧I) and Export Calendar ▸ Today \| This Week → `AppSurfaces` → `IcalModel` → `CoreBridge.importIcal` / `.exportIcal` → the seam's `import_ical` / `export_ical` |
| Background sync (while running) | met | `startSync` spawns a live driver for the life of the window; off when no relay URL is set |
| Menu bar | met | `MenuBarExtra` with real Today / Inbox / sync data off the change feed |
| OS automation (App Intents) | met | six intents + `AppShortcutsProvider` + `TaskEntity`/`EntityStringQuery`; `IntentVault` counted lease |
| Mouse | met | standard AppKit/SwiftUI controls, plus double-click-to-open and context menus |
| First-run pairing | met | `OnboardingView` "Pair with that device", and the same route out of `LockedView` |

**iCal import / export was the one unmet macOS MUST, and it is now met.** The
gap was never in the core: `SunriseCore::import_ical` / `::export_ical` and the
`IcalImportReport` / `IcalNotice` DTOs were correct and tested, and
`sunrise-cli` already consumed them — what was missing was a caller on the
client the row applies to. `apps/apple` now has one: `CoreBridge.importIcal` /
`.exportIcal`, an `IcalModel` holding the report as a value, and the two File
menu items. The import's notices are **shown to the user, grouped by code**,
rather than counted — an importer that silently drops a `VTODO` is the failure
the notice list exists to prevent, and a notice nobody sees is the same failure
one layer up. Export covers Today and This Week; a Stream-scoped export is
still not built (see [icalendar.md](../09-integrations/icalendar.md)).

**Print / PDF export** is macOS **SHOULD**, and it is now **met** as well: ⌘P
and File → Export as PDF…, rendering through `ImageRenderer` into a paginated
`PDFDocument` and then either `PDFDocument.printOperation` or a save panel. It
covers the four surfaces with a paper shape — task lists, search results, the
calendar day and week grids, and the weekly and daily reviews. It deliberately
does **not** cover Review → Trends (a chart) or Review → History (links); both
already carry the CSV/JSON export beside them, which remains the seam's only
`ExportFormat` pair. On those two the menu item is **disabled and says why** —
the reason is a sentence used as both `.help` and `.accessibilityHint`, because
[`../10-cross-cutting/accessibility.md`](../10-cross-cutting/accessibility.md)
forbids a state carried by appearance alone. A screen that *can* print but is
empty right now stays enabled and beeps; that is a different condition, and a
menu item flickering as tasks come and go would explain less.

### CLI — 9 MUSTs

| Capability | Verdict | Reached from |
|---|---|---|
| Read/write tasks | met | `capture` (`CreateTask`), `edit <id>… <tokens>` (`UpdateTask`, plus `PromoteToStream` when the line carries `#stream`), `defer` (`DeferTask`), `done` (`CompleteTask`), `drop` (`DeleteTask`), `retitle <id> <text>…` (`UpdateTask` with a title patch). Retitle is its own verb rather than an `edit` token because a title is free text that will eventually contain a `#` or a `!`, and inside the annotate grammar a bare word would be ambiguous between title text and a malformed token — which would force `edit` to weaken its rule that one bad token rejects the whole line. The one field still unwritable is a Task's `body`, which is the CLI's *Notes* row, and that is a MAY |
| Streams, contexts, routines (read + capture) | met | `streams`, `contexts`, `routines`; `#stream` / `@context` resolve **existing** entities in `capture` and warn on an unknown one. Reordering streams is the one write: `streams move <x> before <y>\|last` → `UpdateStream { sort_order }`. The CLI still mints no Stream, Context or Routine — the row asks for read + capture, and that is what it is |
| Today / Inbox / Stream views (list form) | met | `today` (`Query::Today`), `inbox` (`Query::Inbox`), `stream <id\|name>` (`Query::StreamTasks`), and `context <id\|name>` (`Query::ContextTasks`) beside it. Both resolvers take an id, an exact name or a unique prefix, and fail loudly rather than printing an empty list. `today` cannot yet be filtered by context, though `Query::Today` takes the list |
| Focus mode (`next`, `focus <id>`) | met | `next`, `focus <id>`, bare `focus`, and `focus end [--done]` (`EndFocus`). End resolves the session through `Query::RunningFocusSessions` rather than taking an `fcs_` id, because neither `focus` nor `next` ever prints one — and it closes every running session, since two devices can each mint a valid one |
| Search (FTS) | met *(plain-text half)* | `sunrise search <query>…` — the same literal-AND FTS query the app issues; the operator grammar is [#28](https://github.com/justin13888/Sunrise/issues/28) |
| Quick capture (`sunrise capture`) | met | the full token syntax, same parser as every other surface |
| Multi-account (`SUNRISE_VAULT`) | met | each vault directory mints its own 32-byte root from the injected RNG on first open and keeps it in the keystore (`SUNRISE_KEYSTORE`), one mode-0600 file per vault, **outside** the vault directory; `vaults` lists them. Two vaults share no SQLCipher key and no Stream keys. Still no passphrase — the root is random and something local holds it |
| iCal import / export | met | `sunrise ical import <path\|->` and `sunrise ical export [today\|day\|week] [path]` |
| OS automation surface | met | stdout is the script contract, notes to stderr, `-` reads stdin, meaningful exit codes |

The CLI also carries surfaces this table has no row for: `login` / `logout` /
`whoami` (OIDC + PKCE, token stored mode-0600 and device-bound), `review`,
`export` as an *analytics* export, `vaults`, and device trust via
`SUNRISE_TRUST_CERT_FILE`, which reaches `Command::TrustDevice` on every
subcommand. The last is security-relevant and unrowed.

`SUNRISE_VAULT_ROOT` is the other unrowed surface, and it is the one to read
carefully: it supplies a root outright and touches no keystore, which is how two
vaults are told to be one account until pairing lands, and how a vault created
before per-vault keys existed is opened. Such a vault is **refused** with a
typed `PreMultiAccount` error rather than opened by guessing the old constant —
and the refusal quotes that constant, so the data can still be read out once and
moved. Refusing and then telling the user exactly how to proceed is the point:
guessing would have left every such vault readable by anyone holding a copy of
`sunrise`.

### iOS — 23 SHOULDs

Measured the same way, and against the same two trees the iOS product compiles:
`apps/apple/iOS/` for the shell, and the shared `apps/apple/Sunrise/` for
everything below it. **21 met, 2 unmet.** iOS carries no MUSTs
([ADR-0028](../11-adr/0028-ios-is-a-v1-client.md)), so nothing here is a v1 release gate;
it is the record of what a user can actually reach on a phone.

| Capability | Verdict | Reached from |
|---|---|---|
| Read/write tasks | met | Today tab → the shared `TaskListView` (`iOS/VaultTabs.swift:125-139`); the row menu's **Edit…** (`TaskListView.swift:217`) opens the Mac's own `TaskEditorView`; swipe actions for Delete and Tomorrow (`:199-203`) |
| Streams, contexts, routines | met | Browse tab → `BrowseSidebar`: the section headers' **+** (`:139`) → `StreamEditorView` / `ContextEditorView` (`:67-80`), and per-row Edit / Pause / Archive / Delete (`:186-194`, `:224-229`). More → Routines → `RoutinesView`'s **New routine** (`:44`) |
| Today / Inbox / Stream views | met | Today is a tab root (`VaultTabs.swift:138`); the Inbox, a stream and a context push onto Browse instead, so the list they came from stays behind them (`iOS/TabRoute.swift:97-98`) |
| Focus mode | met | Focus tab (`VaultTabs.swift:80-86`) → the shared `FocusView`, and **Start focus session** on any row (`TaskListView.swift:225`) |
| Time-blocking on calendar grid | met | Calendar tab (`VaultTabs.swift:70-76`) → the shared `CalendarView`: drag-to-create (`:220`), block move (`:408`) and resize (`:445`), `BlockEditorView` on tap |
| Notes (rich text) | met | task editor → Notes pane → `NoteBodyEditor` (`TaskEditorView.swift:92`). Checklist ticks draw as filling circles rather than switches, because the iOS default for a `Toggle` says "this setting is on" where a checklist means "this is done" (`PlatformKit.swift:187-212`). Scope per ADR-0020 |
| Attachments — view image/PDF | met | task editor → Attachments pane (`TaskEditorView.swift:93`); `PDFView` bridged through `UIViewRepresentable` (`AttachmentsView.swift:165-169`) |
| Attachments — upload | met | `.fileImporter` (`AttachmentsView.swift:38`) and a URL drop target beside it (`:32`) |
| Search (FTS) | met *(plain-text half)* | the `.search`-role tab (`VaultTabs.swift:90-92`) → the shared `SearchView`. It issues the identical literal-AND FTS query the Mac does and inherits the identical narrowness ([#28](https://github.com/justin13888/Sunrise/issues/28)) |
| Saved searches / views | **unmet** | `SavedViewsModel` is built by the shared `VaultModels` and the shell even loads it (`VaultTabs.swift:456`) — but `SavedViewsMenu` is instantiated in exactly one place in the tree, `macOS/VaultWindow.swift:89`. A working, tested model with no iOS surface |
| Keyboard navigation | met *(list keymap)* | `onKeyChord(scope: .list…)` on the shared `TaskListView` (`:118`) — every row-scoped binding in [keyboard.md](../08-features/keyboard.md), on an attached keyboard. **Nothing above it**: `onKeyChord` is applied in that one place in the whole tree, so every `.application`-scoped chord (`Keymap.swift:177-199`) reaches a user only through the Mac's `Commands` scene, and the palette and the cheat sheet are handed inert closures (`VaultTabs.swift:283-288`) |
| Drag-and-drop | met | the same seven of eight cells macOS has, from shared files with no platform fork: `.draggable` on the row (`TaskRowView.swift:74`), drops on stream and context rows (`BrowseSidebar.swift:175`, `:218`), on task rows (`TaskListView.swift:192`), on the calendar grid (`CalendarView.swift:221`) and on the attachments pane (`AttachmentsView.swift:32`), plus `ForEach.onMove` for stream order (`BrowseSidebar.swift:45`). The gesture is a long-press drag rather than a click-drag. The eighth cell (Calendar block → Task) is unreachable for the same reason as on the Mac, and more so: a tab shell shows one screen at a time |
| Quick capture (system surface) | met | a **Capture** toolbar button on every screen (`VaultTabs.swift:318-324`), routed to the inline bar where the list has one and to the sheet where it does not (`:339-362`, `:471-498`); `sunrise://capture?text=`, registered by the iOS target in its own right (`project.yml:198-201`); and the **Capture Task** App Shortcut (`SunriseShortcuts.swift:23-33`) |
| Reminders / local notifications | met | `ReminderScheduler` follows the change feed for the life of the shell (`VaultTabs.swift:454`); the category, its three buttons and the response delegate are one shared file (`NotificationCenterClient.swift:60-105`); Settings asks for authorization (`VaultTabs.swift:374`) |
| Multi-account | met | More → Settings (`VaultTabs.swift:364-386`) → the vault picker → `SessionModel.switchTo`, teardown before reopen (`AccountView.swift:131-145`) |
| Pairing — scan QR | met *(paste half)* | `PairingView`'s paste field (`:233-240`), reached from Settings → **Add a device…** (`AccountView.swift:159`) and from `LockedView` (`:62`). **No camera scanner exists on either platform**; the row's "camera or paste" is satisfied by paste, as it is on macOS |
| Pairing — show QR | met | `QRCode.image` (`QRCode.swift:29-48`) through `PlatformImage`'s `UIImage` branch (`PlatformKit.swift:42-52`), with the copyable text beside it |
| iCal import / export | **unmet** | the picker-driven `importIcal()` / `exportIcal(_:)` are inside `#if os(macOS)` (`Sunrise/Ical/AppSurfaces+Ical.swift:52-75`), the `IcalSurfaces` modifier is applied only at `macOS/VaultWindow.swift:141`, and the File menu items live in `macOS/AppCommands.swift:104-118`. The URL-taking halves and `CoreBridge.importIcal` / `.exportIcal` are shared and have no iOS caller — the same shape as saved views |
| Background sync | met *(frontmost only)* | `startSync` on the shell's `.task` and again on every relay-URL change (`VaultTabs.swift:388-394`, `:448-457`), exactly as the Mac's window does it. There is no `BGAppRefreshTask` anywhere in `apps/apple`, so sync stops when the app leaves the foreground ([#31](https://github.com/justin13888/Sunrise/issues/31)) |
| OS automation (App Intents) | met | `Sunrise/Intents/` compiles into both products; the iOS target names `AppIntents.framework` (`project.yml:173`), which is what makes Xcode write the metadata bundle without which the intents link and are never offered; six `AppShortcut`s (`SunriseShortcuts.swift:22-83`); the live vault is adopted at `AppSurfaces.swift:162` so an intent fired while the app is open is answered rather than refused |
| Vim-style modal navigation | met | the same ten-binding subset behind the same toggle — `onKeyChord(… vim:)` (`TaskListView.swift:118`) and Settings → Keyboard → **Vim-style motions** (`AccountView.swift:216-228`). Needs an attached keyboard, which is the row's own scope |
| Touch | met | tap-to-select on tagged rows, which iOS does **not** give for free and which left every `BrowseSidebar` entry inert until it was added (`PlatformKit.swift:133-159`); swipe actions (`TaskListView.swift:199-203`); a **Done** toolbar to put the software keyboard away, since a phone has no Escape (`CaptureBar.swift:59+`); **Cancel** / **Add** in the capture sheet, where the Mac has only Return and Escape (`QuickCaptureView.swift:66-85`); haptic refusal feedback where the Mac beeps (`PlatformKit.swift:122-128`) |
| First-run pairing | met | `OnboardingView`'s **Pair with that device** (`:63`), and the same route out of `LockedView` (`:62`) |

**The three MAYs are in prose because none of them is a v1 ask.** *Watch app*
is unbuilt: `apps/apple` contains no `WatchConnectivity` and `project.yml`
declares no watch extension target. *Mouse* is met by inheritance rather than
by intent — an iPad with a pointer gets the shared controls, the row's
double-click-to-open (`TaskRowView.swift:69`) and the context menus, none of
which were written for a pointer. *Print / PDF export* is unbuilt on iOS, and
the split is exact: `Sunrise/Print/PrintDocument.swift` builds and paginates
the document and is shared, while the rendering, the print panel and the save
panel are all in `macOS/PrintJob.swift`.

### What is still narrow

Nothing above demotes a mark, and nothing above is graded up past what a user
can reach. What is narrower than the row's prose, recorded rather than smoothed
over:

- **macOS.** No camera QR scanner exists — the *Pairing — scan QR* row's "camera
  or paste" is satisfied by paste alone. Drag-and-drop is missing the Calendar
  block → Task gesture, which the shipped layout cannot express. Print covers
  four surfaces and skips two by decision. Search reaches FTS5 on tasks only,
  as a literal AND of quoted terms: every operator
  [search.md](../08-features/search.md) specifies is currently matched as a
  literal word, and the by-kind grouping does not exist.
- **CLI.** A Task's `body` is unreachable — Notes is a CLI **MAY**, and what
  plain stdin should become as structured `NoteBlock`s is a design question
  rather than a gap. Streams, Contexts and Routines can be listed and (for
  Streams) reordered, but none can be created, renamed, archived or deleted;
  the row asks for *read + capture*, and that is what it has.
  `Query::Today`'s context filter has no flag. The mode-0600 keystore guarantee
  is `#[cfg(unix)]`; elsewhere the file is written with default permissions.
  `search` issues the same query the app does, so it inherits the same
  narrowness — the operator grammar
  [search.md](../08-features/search.md) specifies is matched literally; see
  the macOS note above.
- **iOS.** Two SHOULDs are unmet, and both for the same shape of reason — a
  working, tested shared model with no iOS caller: **saved views** and **iCal
  import / export**. Background sync runs only while the app is frontmost.
  Keyboard navigation is the list keymap and nothing above it, because
  `onKeyChord` is applied in exactly one place in the tree; an iPad that draws
  a system menu bar therefore gets only the system's own items, since the
  `Commands` scene that would fill it is under `macOS/`. *File → Task* drag is
  iPad-only, because dragging in from another app needs two apps on screen.

  Two further things are narrow that no row is about, and they are worth
  naming here rather than losing. **The shared sheets are Mac-shaped**:
  unconditional frames of 520 points on the settings `Form`
  (`AccountView.swift:101`), 460 on the task editor
  (`TaskEditorView.swift:97`), 420 on the block editor
  (`BlockEditorView.swift:35`) and 560×520 on the pairing sheet
  (`PairingView.swift:25`) — none behind an `#if`, all wider than an iPhone.
  **And the copy still calls the device a Mac**: `Platform.deviceName`
  (`PlatformKit.swift:163-183`) exists for exactly this and has two callers,
  while seventeen further lines across six shared files say "Mac" outright —
  among them the pairing sheet's own title (`PairingView.swift:32`) and the
  vim toggle's caption (`AccountView.swift:222`, "Stored on this Mac only").
  Neither sinks a verdict, because the rows they sit in are reachable. Both
  are real, and neither has an issue of its own yet.

Every one of these is inside a row graded **met** — the two iOS rows named
unmet above are the exception — because each row asks for a capability and
each capability is reachable. They are written down so that "met"
never has to be re-derived from scratch to find out what it covered.

## Hard rules

- A capability MUST not regress mid-version. A v1.0 → v1.1 release cannot
  remove a MUST.
- A capability marked N/A is a deliberate choice; if revisited, document the
  change in [`../11-adr/`](../11-adr/).
- A capability marked *deferred* carries no MUST, exactly as a deferred client
  does. A capability may only become *deferred* **before** the version that
  would have carried it ships, and only with the reason recorded in
  [`../11-adr/`](../11-adr/) — never to make this table agree with the code
  after the fact.
- A user can run **without** any specific OS feature (Live Activities,
  Spotlight, etc.); fallbacks via plain notifications must exist.
- **A deferred client has no MUSTs.** When one is scheduled, its column is
  filled in and the fill-in is the commitment — not this table's history.
- **A v1 client below MUST level has no MUSTs either — and its *met* SHOULDs
  still cannot regress.** An iOS SHOULD graded **met** in the audit above may
  not become unmet without an ADR. Deliberately weaker than the MUST rule,
  because there is no release to protect; a rule at all, because a green audit
  row is a claim that somebody traced a surface down to a seam, and deleting
  the surface silently throws that work away ([ADR-0028](../11-adr/0028-ios-is-a-v1-client.md)).
- **A qualified *met* is still a met.** A verdict written with a parenthetical
  qualifier — `met *(paste half)*`, `met *(plain-text half)*`,
  `met *(list keymap)*`, `met *(frontmost only)*` — discharges the row's
  requirement. The qualifier names which part of the specified capability is
  reachable, and it is repeated under
  [What is still narrow](#what-is-still-narrow) so the narrowness never has to
  be re-derived. A qualifier is **not** a *partial*: *partial* means a user
  cannot complete the row's core action.

## What the CLI is and is not

The CLI is a **capture, triage, review and automation** surface, not a second
interactive client. It is where the SSH and scripting story lives now that the
TUI is gone, and the honest boundary is: everything one-shot works over SSH;
living in the app does not.

It is also the reason the core stays provable without a UI. `cargo test -p
sunrise-cli` drives the real binary against a real vault and an in-process
relay. That is a standing requirement — the day it stops covering the stack is
the day the core's tests stop describing a usable system.

## Capture-surface portability

Each platform MUST implement its native capture surface (macOS global hotkey +
menu bar; on iOS the capture sheet, the inline bar and the capture App
Shortcut; CLI subcommand). Platforms MAY implement additional surfaces. There
is no requirement for cross-platform parity *of capture surfaces*; the
requirement is parity of *capture semantics* — the resulting Task is identical
regardless of capture origin, because every surface calls the same parser
(`sunrise_domain::capture`).

## Vim-mode opt-in

- Settings toggle `editor.vim_mode: bool = false`. Persisted as a per-device
  local pref (not synced). Reachable from Settings → Keyboard and from the `?`
  cheat sheet.
- Available on macOS, and **implemented as a navigational subset** — ten
  bindings over the task list (`h j k l`, `gg`, `G`, `u`, `⌃R`, `/`, `:`),
  additive rather than modal, with no Insert mode and no caret motions.
  `dd` and `yy` are deliberately absent: `d` is already Defer, and an
  operator-pending `d` would turn a one-key defer into the first half of a
  delete. The full list, and the reason for each omission, is in
  [`../08-features/keyboard.md`](../08-features/keyboard.md).
- The row is **SHOULD**, and the shipped subset meets it.
