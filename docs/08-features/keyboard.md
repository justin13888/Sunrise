---
status: accepted
---

# Keyboard

Sunrise must be operable end-to-end with the keyboard alone on every platform that has a keyboard.

## Per-platform default keymaps

Defaults follow platform conventions; user remappable in Settings.

> **There is no TUI column.** This table carried one until
> [ADR-0019](../11-adr/0019-swiftui-macos-client.md) removed the terminal
> client. The `sunrise` CLI that replaced it is **not** a keyboard-driven
> client — it is a set of one-shot subcommands (`capture`, `today`, `inbox`,
> `next`, `focus`, `done`, …), so it has no keymap to specify. The Win/Linux
> and Web columns are unbuilt targets: macOS is the only shipping GUI client.

| Action | macOS | Desktop (Win/Linux) | Web |
|---|---|---|---|
| Quick capture (global) | `Cmd+Shift+N` | `Ctrl+Shift+N` | extension shortcut |
| Quick capture (in-app) | `Cmd+N` | `Ctrl+N` | `n` |
| Today | `Cmd+1` | `Ctrl+1` | `g t` |
| Inbox | `Cmd+2` | `Ctrl+2` | `g i` |
| Search | `Cmd+F` (in view), `Cmd+K` (global) | `Ctrl+F` / `Ctrl+K` | `/` |
| Open command palette | `Cmd+Shift+P` | `Ctrl+Shift+P` | `Ctrl+Shift+P` |
| New stream | `Cmd+Shift+S` | `Ctrl+Shift+S` | — |
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

## Vim mode (opt-in)

Settings toggle `editor.vim_mode: bool = false`. Persisted as a per-device local pref (not synced). Available on Desktop and Web.

### v1 vim-mode keymap (exhaustive)

| Keys | Action |
|---|---|
| `h` `j` `k` `l` | Move left / down / up / right |
| `w` `b` `e` | Forward word / back word / end of word |
| `gg` | Top of list/document |
| `G` | Bottom of list/document |
| `0` | Start of line |
| `$` | End of line |
| `i` `a` | Insert before / after cursor |
| `I` `A` | Insert at start / end of line |
| `o` `O` | Open new line below / above |
| `x` | Delete character under cursor |
| `dd` | Delete current line / row |
| `yy` | Yank current line / row |
| `p` `P` | Paste after / before cursor |
| `u` | Undo |
| `Ctrl-r` | Redo |
| `/` | Search-in-list |
| `:` | Command palette |
| `Esc` | Return to Normal mode |

No regex, no macros, no marks in v1.

### Vim-mode conflict mitigation

When vim mode is on, the browser-default `Ctrl+Shift+P` is intercepted only inside the Sunrise web-app surface; outside (DevTools open, browser chrome focused, etc.) the browser keeps the binding. See `Ctrl+Shift+P` conflict notes below.

## Discoverability

- `?` in any view opens a contextual cheat sheet.
- Command palette shows the current binding next to every command.
- New users see an opt-in "show keyboard tips" coachmark.

## Accessibility

- All keyboard shortcuts have a visible UI affordance — no hidden-only commands.
- VoiceOver / TalkBack / NVDA traversal is keyboard-equivalent.
- Focus indicators are *always* visible (no `outline: none` overrides). Contrast is WCAG 2.1 AA minimum (3:1 against adjacent colors). Every interactive element MUST have a visible focus indicator.

## Mobile keyboards

- iPad / Android tablet with attached keyboard: same shortcuts as desktop where they make sense (e.g., `Cmd+N` for new task).
- Software keyboards: capture sheet uses input mode `text` + autocorrect off; supports Smart Punctuation toggle.
- Predictive text doesn't interfere with the capture parser (`#`, `@`, `^` etc. are typed as-is).

## Conflict policy

Where a keyboard shortcut conflicts with a user-installed system shortcut: the user's wins; we silently no-op our handler if the OS reports the binding is intercepted.

### `Ctrl+Shift+P` on web

The web app intercepts `Ctrl+Shift+P` only when focus is inside the app's main element (not in browser chrome / DevTools). Documented in user-facing help; users can rebind via Settings → Keyboard.
