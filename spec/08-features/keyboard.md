---
status: accepted
---

# Keyboard

Sunrise must be operable end-to-end with the keyboard alone on every platform that has a keyboard.

## Per-platform default keymaps

Defaults follow platform conventions; user remappable in Settings.

| Action | Desktop (mac) | Desktop (Win/Linux) | Web | TUI |
|---|---|---|---|---|
| Quick capture (global) | `Cmd+Shift+N` | `Ctrl+Shift+N` | extension shortcut | `c` |
| Quick capture (in-app) | `Cmd+N` | `Ctrl+N` | `n` | `c` |
| Today | `Cmd+1` | `Ctrl+1` | `g t` | `g t` |
| Inbox | `Cmd+2` | `Ctrl+2` | `g i` | `g i` |
| Search | `Cmd+F` (in view), `Cmd+K` (global) | `Ctrl+F` / `Ctrl+K` | `/` | `/` |
| Open command palette | `Cmd+Shift+P` | `Ctrl+Shift+P` | `Ctrl+Shift+P` | `:` |
| New stream | `Cmd+Shift+S` | `Ctrl+Shift+S` | — | `:stream new` |
| Mark done | `X` (when row selected) | same | same | `x` |
| Defer | `D` | same | same | `d` |
| Schedule | `S` | same | same | `s` |
| Move to Stream | `M` | same | same | `m` |
| Focus mode | `F` | same | same | `f` |
| Up/Down in list | `↑/↓` or `j/k` | same | same | `j/k` |
| Open detail | `Enter` or `→` | same | same | `Enter` or `l` |
| Close detail | `Esc` or `←` | same | same | `Esc` or `h` |
| Multi-select toggle | `Space` | same | same | `Space` |
| Multi-select range | `Shift+↑/↓` | same | same | `V` then move |
| Undo | `Cmd+Z` | `Ctrl+Z` | `Ctrl+Z` | `u` |
| Redo | `Cmd+Shift+Z` | `Ctrl+Y` | `Ctrl+Y` | `Ctrl+r` |

## Vim mode (opt-in)

Settings toggle `editor.vim_mode: bool = false`. Persisted as a per-device local pref (not synced). Available on Desktop and Web. The TUI is vim-style by default (opt-out).

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
