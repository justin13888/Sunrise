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
| `1`–`7`, `g t` / `g i` / `g s` / `g /` / `g f` / `g r` / `g v` | Switch view |
| `c` | Quick capture |
| `A` | Annotate facets (`!1 %high ~30m @ctx due:friday`; `-` clears) |
| `j`/`k` | Down/up |
| `h`/`l` | Sidebar / tasks pane |
| `gg` / `G`, `Home`/`End`, `PgUp`/`PgDn`, `^D`/`^U` | Movement |
| `Enter` | Open detail |
| `x` | Toggle done (re-opens a completed task) |
| `Space` | Mark a row (non-contiguous multi-select); `V` for a range |
| `d` | Defer (prompt) |
| `s` | Schedule (prompt) — skip the next occurrence in the Routines view |
| `m` | Move to Stream |
| `b` / `B` | Marked tasks block this one / clear its blockers |
| `e` / `E` | Edit title / note body in `$EDITOR` — recurrence / rename in Routines |
| `S` / `C` / `R` | New Stream / Context / Routine |
| `a` / `p` | Archive / pause the selected sidebar row |
| `D` | Delete (confirms) |
| `L` | Activity feed |
| `u` / `Ctrl-r` | Undo / redo |
| `f` / `F` | Focus view / start a focus session |
| `t` | Triage the Inbox |
| `/` | Search (re-runs as you type) |
| `:` | Command line (Tab completes, `↑` recalls) |
| `?` | Help — keys for the current mode and view, plus every command |
| `q` | Quit (`Esc` backs out; it does not quit) |

Prompts are a full single-line editor: `←`/`→`, `Home`/`End`, `^A`/`^E`,
`M-b`/`M-f`, `^W`, `^U`, `^K`, and bracketed paste.

## Commands

| Command | Action |
|---|---|
| `:view <name>` | Switch view |
| `:capture <text>` | Capture without leaving the view |
| `:filter @ctx…` | Narrow every list to those contexts (bare clears) |
| `:focus plan\|stats\|energy <l\|m\|h>\|length <p\|e\|u>` | Planner and calibration |
| `:export <trends\|activity\|focus\|streaks> [json\|csv] [path]` | Write a stats dataset |
| `:open <tsk_…>` | Jump to a task by id |
| `:devices` | List paired devices |
| `:preview <path>` | Render an image inline (Focus view) |
| `:save <name>` / `:go <name>` / `:views` / `:unsave <name>` | Saved views (view + query + filter), stored in `~/.config/sunrise/views.toml` |
| `:help` | The full reference |

User-configurable via `~/.config/sunrise/keys.toml`.

## Vault sharing with desktop

If both the desktop client and TUI run on the same machine for the same account, they share the vault dir. Coordination:

- Both processes use SQLite WAL with appropriate file locks.
- Loro CRDT writes go through a single core process via a Unix socket "core daemon" — only one TUI/desktop attaches as the writer; the other becomes a read-only view via subscription.
- Run separately on different machines (laptop + remote box) — each has its own vault and syncs through the server normally.

## Daemon coordination

- A `core daemon` listens on `$XDG_RUNTIME_DIR/sunrise/core.sock` (Linux/macOS); Windows uses a named pipe.
- The TUI process attaches via the socket. Acquisition order: TUI checks for an existing socket; if absent, TUI forks the daemon and waits for socket readiness (≤ 2 s) before attaching.
- The `core.lock` file is held by the daemon. UI processes are not lock holders.
- Daemon crash detection: socket close. UI displays `"Sunrise daemon stopped — reconnecting…"` and respawns the daemon up to 3 times in 60 s, then prompts the user.
- Daemon idle shutdown: after 5 minutes with no attached UI, the daemon exits cleanly.

## Minimum terminal size

Minimum 80 × 24. Below that, the TUI displays `"Sunrise needs at least 80 × 24"` and refuses to render. Redraw on resize is debounced 50 ms.

## SSH-friendly behavior

- Render at the terminal's reported size; respond to resize.
- Mouse support is opt-in (`SUNRISE_MOUSE=1`): capturing the mouse takes the
  terminal's own text selection, which is a bad default for a client used
  inside tmux. When on, the wheel scrolls and a click moves the cursor —
  clicks never mutate, because a cell-sized target with no hover feedback will
  eventually be off by one.
- True color and Unicode glyph fallbacks for monochrome terminals or terminals without UTF-8.

## Capture from anywhere

- `sunrise capture "Buy milk #errands"` — non-interactive, parses, commits, exits 0. Useful in scripts, vim shortcuts, tmux popups.
- `sunrise focus next` — picks the planner's top actionable task and opens a session on it; `sunrise next` lists the picks without starting one.
- `sunrise sync --once` — drain outbox and exit (for cron / CI). Bounded, not a loop: a scheduled job that hangs because the relay is down is worse than one that fails.
- `sunrise done <id>…`, `today`, `inbox`, `search`, `streams`, `contexts`, `routines`, `review`, `export <dataset> [json|csv] [path]` — the same reads and writes, scriptable. `export` goes to stdout unless given a path, so it pipes into `jq`.

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
