---
status: accepted
---

# Keyboard

Sunrise must be operable end-to-end with the keyboard alone on every platform that has a keyboard.

## Per-platform default keymaps

Defaults follow platform conventions.

> **There is no TUI column.** This table carried one until
> [ADR-0019](../11-adr/0019-swiftui-macos-client.md) removed the terminal
> client. The `sunrise` CLI that replaced it is **not** a keyboard-driven
> client — it is a set of twenty-three one-shot subcommands (`capture`, `edit`,
> `defer`, `done`, `drop`, `today`, `inbox`, `next`, `focus`, `search`, …), so
> it has no keymap to specify. The Win/Linux and Web columns are unbuilt
> targets: macOS is the only shipping GUI client.

> **Reading the macOS column.** Every binding below is **implemented and
> reachable** in `apps/apple`, transcribed as data in
> `apps/apple/Sunrise/Keyboard/Keymap.swift` and resolved in one of two scopes
> (`.application`, attached to the window; `.list`, attached to the task list).
> The Win/Linux and Web columns are specification for unbuilt targets — nothing
> in them has been implemented, and they should be read as intent.

| Action | macOS | Desktop (Win/Linux) | Web |
|---|---|---|---|
| Quick capture (global) | `Cmd+Shift+N` | `Ctrl+Shift+N` | extension shortcut |
| Quick capture (in-app) | `Cmd+N` | `Ctrl+N` | `n` |
| Today | `Cmd+1` | `Ctrl+1` | `g t` |
| Inbox | `Cmd+2` | `Ctrl+2` | `g i` |
| Search (keeping the query) | `Cmd+F` | `Ctrl+F` | `/` |
| Search (fresh query) | `Cmd+K` | `Ctrl+K` | `/` |
| Open command palette | `Cmd+Shift+P` | `Ctrl+Shift+P` | `Ctrl+Shift+P` |
| Cheat sheet | `?` (in list), `Cmd+/` (menu) | `?` | `?` |
| New stream | `Cmd+Shift+S` | `Ctrl+Shift+S` | — |
| Import calendar (.ics) | `Cmd+Shift+I` | — | — |
| Print the current screen | `Cmd+P` | — | — |
| Morning summary | `Cmd+Opt+M` | — | — |
| End-of-day plan | `Cmd+Opt+E` | — | — |
| Mark done | `X` (when row selected) | same | same |
| Defer | `D` | same | same |
| Schedule | `S` | same | same |
| Move to Stream | `M` | same | same |
| Focus mode | `F` | same | same |
| Up/Down in list | `↑/↓` or `j/k` | same | same |
| Open detail | `Enter` or `→` | same | same |
| Close detail | `Esc` or `←` | same | same |
| Multi-select toggle | `Space` | same | same |
| Multi-select range | `Shift+↑/↓` | same | same |
| Undo | `Cmd+Z` | `Ctrl+Z` | `Ctrl+Z` |
| Redo | `Cmd+Shift+Z` | `Ctrl+Y` | `Ctrl+Y` |

Four entries need their exact behaviour stated, because the obvious reading is
wrong:

- **`Cmd+F` is not a find-in-current-list.** Both `Cmd+F` and `Cmd+K` navigate
  to the one Search surface; the only difference is that `Cmd+K` clears the
  standing query first and `Cmd+F` keeps it. There is no in-place list filter.
- **`?` cannot be a menu key equivalent on macOS**, so the cheat sheet also
  carries `Cmd+/`, which is what appears in the Help menu. `?` works while the
  list has focus.
- **`Cmd+P` prints the screen, not the selection.** It renders whatever the
  detail pane is showing — a task list, search results, the calendar day or
  week grid, or the weekly or daily review. On Review → Trends or → History it
  produces a title-and-date page with no rows, by decision; both carry CSV/JSON
  export instead. File → Export as PDF… is the same document written to a file
  and carries no chord of its own.
- **`Cmd+Shift+I` opens a file picker, not an import dialogue.** It is File →
  Import Calendar…, and it raises the main window first, because the import's
  *report* — what was created, what was updated, and every notice grouped by
  code — is a sheet on that window and is the point of the feature. The
  matching export is a submenu (Today | This Week) with no chord, since a
  shortcut that silently picks a window would be guessing.

**Remapping is not implemented.** An earlier revision of this file said the
defaults were "user remappable in Settings". They are not: `Keymap.bindings` is
a compile-time constant and Settings → Keyboard offers only the vim toggle
below. Remapping is roadmap, not v1.

### Note editor

The rich-text editor on a Task's body carries its own inline-formatting keymap
(`apps/apple/Sunrise/Notes/NoteInlineText.swift`), which the list keymap does
not shadow:

| Action | macOS |
|---|---|
| Bold | `Cmd+B` |
| Italic | `Cmd+I` |
| Underline | `Cmd+U` |
| Strikethrough | `Cmd+X` |
| Code | `Cmd+E` |

## Vim mode (opt-in)

Settings toggle `editor.vim_mode: bool = false`. Persisted as a per-device
local pref (not synced) — `UserDefaults`, under the literal key
`editor.vim_mode`. Reachable from Settings → Keyboard and from the `?` cheat
sheet, which carries the same toggle so the mode is discoverable from the place
that documents it. **Available on macOS only.** Web is unbuilt.

### What v1 vim mode is

A **navigational subset**, scoped to the task list, and **additive** rather than
modal: a key vim does not claim falls through to the ordinary list keymap, so
`X` `D` `S` `M` `F` keep working with vim on. There is no Insert mode, and
therefore no Normal mode to return to.

### v1 vim-mode keymap (exhaustive, as shipped)

| Keys | Action |
|---|---|
| `j` `k` | Down / up a row |
| `h` | Close detail (also clears the multi-select set) |
| `l` | Open detail |
| `gg` | Top of list |
| `G` | Bottom of list |
| `u` | Undo |
| `Ctrl-r` | Redo |
| `/` | Search |
| `:` | Command palette |
| `Esc` | Cancel a pending `g`; otherwise falls through to Close detail |

Ten bindings. `gg` is the only prefixed motion. No regex, no macros, no marks.

### Motions deliberately not implemented

An earlier revision of this file specified a full caret-motion set. It is not
implemented, and it is listed here rather than deleted so a reader can tell a
scoping decision from an oversight.

| Keys | Why not |
|---|---|
| `w` `b` `e` `0` `$` | Caret motions need a caret. SwiftUI's `TextField` / `TextEditor` expose no caret position, so there is nothing to move; implementing them means dropping to `NSTextView`. |
| `i` `a` `I` `A` `o` `O` | Insert mode, same reason — and with no Insert mode there is no mode to escape from. |
| `x` `p` `P` | Character-level editing, same reason. |
| `dd` `yy` | These require `d` and `y` to become pending operators. **`d` is already Defer.** A mode that silently turns a one-key defer into the first half of a two-key delete is a mode that loses somebody a task, so the operator-pending state was not built. |

`h` and `l` are also worth calling out: they are bound, but to **Close detail /
Open detail**, not to horizontal motion. There is no horizontal motion in a list.

### Vim-mode conflict mitigation

`Ctrl+Shift+P` is a browser conflict, and macOS is the only shipping client, so
nothing here applies today. Kept as specification for the unbuilt web client:
when vim mode is on, the browser-default `Ctrl+Shift+P` would be intercepted
only inside the Sunrise web-app surface; outside (DevTools open, browser chrome
focused) the browser keeps the binding.

## Discoverability

- `?` in any view opens a contextual cheat sheet — on macOS. It is inert on
  iOS; see [Mobile keyboards](#mobile-keyboards).
- Command palette shows the current binding next to every command.
- New users see an opt-in "show keyboard tips" coachmark.

## Accessibility

- All keyboard shortcuts have a visible UI affordance — no hidden-only commands.
- VoiceOver / TalkBack / NVDA traversal is keyboard-equivalent.
- Focus indicators are *always* visible (no `outline: none` overrides). Contrast is WCAG 2.1 AA minimum (3:1 against adjacent colors). Every interactive element MUST have a visible focus indicator.

## Mobile keyboards

- **An iPad with an attached keyboard runs the list keymap and the vim subset,
  and nothing above them.** `onKeyChord` is applied in exactly one place in
  `apps/apple` — the shared `TaskListView` — so every row-scoped binding in the
  macOS column above works unchanged on iOS, and every `⌘` binding does not:
  those are delivered by the Mac's `Commands` scene, which lives in `macOS/`
  and the iOS target never compiles. Concretely, `⌘N` on iOS is the toolbar
  **Capture** button rather than a chord; the command palette (`⌘⇧P`) and the
  cheat sheet are handed inert closures in the tab shell, so `?` in a list does
  nothing there. An iPad that draws a system menu bar gets only the system's
  own items, for the same reason. Android tablets are a deferred client.
- **Software keyboards are specified here and not yet configured in the app.**
  The intent is a `text` input mode with autocorrect off, so that `#`, `@`,
  `^`, `!` and `~` are typed as-is and predictive text cannot rewrite a token
  the parser is about to read. Nothing in `apps/apple` sets it: neither capture
  field carries `autocorrectionDisabled`, `textInputAutocapitalization` or
  `keyboardType`, so on iOS both get the system defaults today. This paragraph
  is the requirement, not a description.
- **What a phone does have** is the affordances a hardware keyboard made
  unnecessary: an explicit **Done** in the capture bar's toolbar, because there
  is no Escape key to hand focus back with, and **Cancel** / **Add** buttons in
  the capture sheet, because the Mac's panel commits on Return and closes on
  Escape and a phone can do neither visibly.
- The requirement level is in
  [`../07-clients/parity-matrix.md`](../07-clients/parity-matrix.md): iOS
  *Keyboard navigation* is a **SHOULD** scoped to the list keymap, and the
  audit grades it `met *(list keymap)*`.

## Conflict policy

Where a keyboard shortcut conflicts with a user-installed system shortcut: the user's wins; we silently no-op our handler if the OS reports the binding is intercepted.

### `Ctrl+Shift+P` on web

The web app intercepts `Ctrl+Shift+P` only when focus is inside the app's main element (not in browser chrome / DevTools). Documented in user-facing help. There is no rebinding: no client ships a Settings → Keyboard surface, and the bindings in this document are fixed.
