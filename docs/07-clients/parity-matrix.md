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

Two clients ship in v1: the **macOS** app and the **CLI**. iOS, Android and Web
are **deferred** — specified, not scheduled, and carrying no MUSTs, because a
deferred client cannot regress one. The **TUI** was removed by
[ADR-0019](../11-adr/0019-swiftui-macos-client.md); its column is kept for one
release so the table records what was withdrawn rather than quietly losing it.

| Capability | macOS | CLI | iOS | Android | Web | TUI |
|---|---|---|---|---|---|---|
| | | | *deferred* | *deferred* | *deferred* | *removed* |
| Read/write tasks | MUST | MUST | — | — | — | — |
| Streams, contexts, routines | MUST | MUST (read + capture) | — | — | — | — |
| Today / Inbox / Stream views | MUST | MUST (list form) | — | — | — | — |
| Focus mode | MUST | MUST (`next`, `focus <id>`) | — | — | — | — |
| Time-blocking on calendar grid | MUST | N/A | — | — | — | — |
| Notes (rich text) | MUST (a Task's `body`; scope per [ADR-0020](../11-adr/0020-v1-must-demotions.md)) | MAY | — | — | — | — |
| Attachments — view image/PDF | MUST | N/A | — | — | — | — |
| Attachments — upload | MUST | MAY | — | — | — | — |
| Search (FTS) | MUST | MUST | — | — | — | — |
| Saved searches / views | MUST | MAY | — | — | — | — |
| Keyboard navigation | MUST (full) | N/A (non-interactive) | — | — | — | — |
| Drag-and-drop | MUST | N/A | — | — | — | — |
| Quick capture (global hotkey / system surface) | MUST (global hotkey, menu bar) | MUST (`sunrise capture`) | — | — | — | — |
| Reminders / scheduled local notifications | MUST | N/A (one-shot process) | — | — | — | — |
| Multi-account | MUST | MUST (`SUNRISE_VAULT`) | — | — | — | — |
| Pairing — scan QR | MUST (camera or paste) | MAY (manual code entry) | — | — | — | — |
| Pairing — show QR | MUST | MAY (ASCII QR) | — | — | — | — |
| Sharing — accept invite | *deferred* ([ADR-0020](../11-adr/0020-v1-must-demotions.md)) | MAY | — | — | — | — |
| Sharing — view shared stream as editor | *deferred* ([ADR-0020](../11-adr/0020-v1-must-demotions.md)) | *deferred* ([ADR-0020](../11-adr/0020-v1-must-demotions.md)) | — | — | — | — |
| Calendar integration (Google) | *deferred* ([ADR-0020](../11-adr/0020-v1-must-demotions.md), [#4](https://github.com/justin13888/Sunrise/issues/4)) | MAY | — | — | — | — |
| iCal import / export | MUST | MUST | — | — | — | — |
| Background sync | MUST (while running) | N/A (`sync --once` for cron) | — | — | — | — |
| Menu bar | MUST | N/A | — | — | — | — |
| Lock screen / home screen widget | N/A | N/A | — | — | — | — |
| Watch app | N/A | N/A | — | — | — | — |
| OS automation surface (App Intents / Shortcuts) | MUST | MUST (the CLI *is* one) | — | — | — | — |
| Vim-style modal navigation | SHOULD (opt-in) | N/A | — | — | — | — |
| Mouse | MUST | N/A | — | — | — | — |
| Touch | MAY | N/A | — | — | — | — |
| Print / PDF export | SHOULD | MAY (`export`) | — | — | — | — |
| First-run pairing | MUST | SHOULD | — | — | — | — |

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

**Every MUST in both shipping columns is now met.** The previous revision of
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
| Search (FTS) | met | sidebar / `⌘F` / `⌘K` → `SearchView`, 150 ms debounce |
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
client the row applies to. `apps/macos` now has one: `CoreBridge.importIcal` /
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
produce a title-and-date page with no rows, and both already carry the CSV/JSON
export beside them, which remains the seam's only `ExportFormat` pair.

### CLI — 9 MUSTs

| Capability | Verdict | Reached from |
|---|---|---|
| Read/write tasks | met | `capture` (`CreateTask`), `edit <id>… <tokens>` (`UpdateTask`, plus `PromoteToStream` when the line carries `#stream`), `defer` (`DeferTask`), `done` (`CompleteTask`), `drop` (`DeleteTask`). Two fields stay unwritable: a Task's **title** cannot be changed after capture (`TaskEdit` has no title field and a bare word is refused), and its `body` is the CLI's *Notes* row, which is a MAY |
| Streams, contexts, routines (read + capture) | met | `streams`, `contexts`, `routines`; `#stream` / `@context` resolve **existing** entities in `capture` and warn on an unknown one. Reordering streams is the one write: `streams move <x> before <y>\|last` → `UpdateStream { sort_order }`. The CLI still mints no Stream, Context or Routine — the row asks for read + capture, and that is what it is |
| Today / Inbox / Stream views (list form) | met | `today` (`Query::Today`), `inbox` (`Query::Inbox`), `stream <id\|name>` (`Query::StreamTasks`), and `context <id\|name>` (`Query::ContextTasks`) beside it. Both resolvers take an id, an exact name or a unique prefix, and fail loudly rather than printing an empty list. `today` cannot yet be filtered by context, though `Query::Today` takes the list |
| Focus mode (`next`, `focus <id>`) | met | `next`, `focus <id>`, bare `focus`. Note there is still no way to *end* a session: `Command::EndFocus` has no CLI path, so a session opened here is closed from macOS or not at all |
| Search (FTS) | met | `sunrise search <query>…` |
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

### What is still narrow

Nothing above demotes a mark, and nothing above is graded up past what a user
can reach. What is narrower than the row's prose, recorded rather than smoothed
over:

- **macOS.** No camera QR scanner exists — the *Pairing — scan QR* row's "camera
  or paste" is satisfied by paste alone. Drag-and-drop is missing the Calendar
  block → Task gesture, which the shipped layout cannot express. Print covers
  four surfaces and skips two by decision.
- **CLI.** A Task cannot be re-titled after capture, and its `body` is
  unreachable. A focus session can be started but not ended. Streams, Contexts
  and Routines can be listed and (for Streams) reordered, but none can be
  created, renamed, archived or deleted. `Query::Today`'s context filter has no
  flag. The mode-0600 keystore guarantee is `#[cfg(unix)]`; elsewhere the file
  is written with default permissions.

Every one of these is inside a row graded **met**, because each row asks for a
capability and each capability is reachable. They are written down so that "met"
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
menu bar, CLI subcommand). Platforms MAY implement additional surfaces. There
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
