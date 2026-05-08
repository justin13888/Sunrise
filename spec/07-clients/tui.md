---
status: accepted
---

# TUI Client

A first-class terminal user interface. Same Rust core; UI built with Ratatui + Crossterm. Single binary `sunrise` (or `sr`), no external runtime.

The TUI is **not** a debug tool. It's intended for daily use by users who live in terminals (researchers, ops folks, developers). It must be capable of replacing the GUI for those users.

## Targets

- macOS, Linux, FreeBSD, Windows (modern Terminal). 256-color minimum; truecolor preferred. Unicode wide-glyph support assumed.

## Why a TUI

- Multi-stream operators often work on remote machines via SSH; a server-resident TUI lets them capture and review without leaving the terminal.
- Keyboard-only navigation is faster for power users.
- Smaller surface to maintain than yet another GUI variant.

## Layout

Split-pane layout, vim-flavored navigation:

```
┌──────────────────┬───────────────────────────────────┐
│ Streams          │ Today                              │
│  > Inbox     [3] │                                    │
│    Work A    [12]│  ☐ Pay invoice           !1 @home  │
│    Family    [4] │  ☐ Walk        @errands  ~30m      │
│    Travel    [0] │                                    │
│                  │  ─── Scheduled ───                 │
│ Contexts         │  ☐ 09:00 Standup       Work A      │
│  @home       [6] │  ☐ 10:00 Review PR     Work A      │
│  @errands    [3] │                                    │
│  @deep-work  [2] │  ─── Overdue ───                   │
│                  │  ☐ Renew passport      Travel  -2d │
└──────────────────┴───────────────────────────────────┘
 NORMAL  [c]apture [/]search [s]ched [f]ocus [q]uit       Sync ✓ • Fri 09:14
```

## Modes (vim-style)

- **Normal**: navigate, run commands. `j/k`, `gg`, `G`, `/` for search, `:` for command palette.
- **Insert**: text input, when editing a title, body, etc. `Esc` returns to Normal.
- **Visual**: multi-select. `V` enters; `j/k` extends; `d` deletes the selection, etc.
- **Command**: `:` prompt. Completion-driven. `:capture`, `:focus`, `:share`, `:device list`, …

## Keybindings (defaults)

| Key | Action |
|---|---|
| `c` | Quick capture |
| `j`/`k` | Down/up |
| `h`/`l` | Collapse/expand or navigate panes |
| `gg` / `G` | Top / bottom |
| `Enter` | Open detail |
| `x` | Toggle done |
| `d` | Defer (prompt) |
| `s` | Schedule (prompt) |
| `m` | Move to Stream |
| `f` | Focus mode |
| `/` | Search |
| `:` | Command |
| `?` | Help |
| `Tab` | Switch pane |
| `q` | Quit |

User-configurable via `~/.config/sunrise/keys.toml`.

## Vault sharing with desktop

If both the desktop client and TUI run on the same machine for the same account, they share the vault dir. Coordination:

- Both processes use SQLite WAL with appropriate file locks.
- Loro CRDT writes go through a single core process via a Unix socket "core daemon" — only one TUI/desktop attaches as the writer; the other becomes a read-only view via subscription.
- Run separately on different machines (laptop + remote box) — each has its own vault and syncs through the server normally.

## SSH-friendly behavior

- Render at the terminal's reported size; respond to resize.
- Mouse support optional; works without it.
- True color and Unicode glyph fallbacks for monochrome terminals or terminals without UTF-8.

## Capture from anywhere

- `sunrise capture "Buy milk #errands"` — non-interactive, parses, commits, exits 0. Useful in scripts, vim shortcuts, tmux popups.
- `sunrise focus next` — picks next Today task and enters focus.
- `sunrise sync --once` — drain outbox and exit (for cron / CI).

## Pairing

- The TUI displays the QR via Unicode block art (works in most modern terminals).
- Recovery code entry is a focused prompt with paste detection (preserves spaces).

## Notifications

- The TUI emits desktop notifications via libnotify (Linux), `terminal-notifier` (macOS), or BurntToast/PowerShell (Windows) when reminders fire while the TUI is the foreground sync agent.
- A `--silent` mode suppresses for SSH sessions where notifications would land somewhere useless.

## Limitations

- No drag-and-drop (use `m` / `s` commands instead).
- Inline images / attachments: filename + size shown; `:open <id>` saves to disk and opens in the platform default viewer.
- Long-form note editing: `e` opens the note body in `$EDITOR` (vim/helix/nano), saves on exit.

## Daemon mode

`sunrise daemon` runs headless (no UI) and:

- Maintains the WS sync connection.
- Schedules local notifications.
- Listens on a Unix socket for `sunrise capture …` and other one-shot commands.

Useful on a remote box where the user wants Sunrise running across SSH sessions.
