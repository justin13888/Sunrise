---
status: accepted
---

# Interaction Patterns

Cross-platform user-facing behaviors. Implemented natively per platform; consistent in *what* the user can do.

## Quick capture

| Trigger | Platform |
|---|---|
| Global hotkey (default `Ctrl/Cmd + Shift + N`) | Desktop |
| Lock-screen widget tap | iOS |
| Quick Settings tile | Android |
| Browser keyboard shortcut (when extension installed) | Web |
| `s capture` subcommand or `:` in app | TUI |

Capture syntax (parsed by core):

- `Buy milk` → task in Inbox
- `Buy milk #errands` → task in stream "errands" (creates if missing — confirms first time)
- `Buy milk @home` → task with context "home"
- `Buy milk ^tomorrow 9am` → scheduled
- `Buy milk !2` → priority 2
- `Buy milk ~30m` → estimated 30 min
- Multiple: `Buy milk #errands @home ^tomorrow 9am !2`

Parser is a single deterministic function in core (string in, structured task draft out).

## Mark done

- Click checkbox / tap checkbox / press `x` in TUI / press `Space` in keyboard mode.
- Confetti? No. We don't gamify.

## Defer

- Right-click → "Defer to…" / long-press → menu / keyboard `d` then a date picker.
- The system increments `deferred_count`; weekly review surfaces serial deferrers.

## Promote (Inbox → Stream)

- Drag onto a Stream / `m` then pick stream.
- One-shot inline: type `#stream` in the task title.

## Reorder

- Drag within a list (cross-platform).
- Keyboard: `Alt+↑/↓` to move within parent.

## Schedule

- Drag onto Today / drag onto a calendar block / open detail and edit.
- Keyboard: `s` to open scheduler, then natural-language input ("tomorrow 9am").

## Focus mode

- Pick a task → "Focus" button or `f`.
- Renders task full-screen with optional Pomodoro timer.
- One key to: complete (`x`), defer (`d`), capture-aside (`a`), exit (`Esc`).
- Breaks (5 min default after 25 min focus) are surfaced as a soft suggestion.

## Multi-select

- Cmd/Shift-click on desktop and web.
- Long-press + tap-to-extend on mobile.
- Visual marker in TUI; `V` to enter visual mode (vim-like).

## Undo

- `Cmd/Ctrl+Z` / `u` in TUI.
- Stack depth: last 50 user actions in this session.
- Time-bounded: undo of an op older than 5 minutes shows a confirm ("undo from 12 minutes ago?").

## Drag-and-drop matrix

| From → To | Desktop | iOS | Android | Web | TUI |
|---|---|---|---|---|---|
| Task → Stream | Yes | Yes | Yes | Yes | Use `m` |
| Task → Calendar block | Yes | Yes | Yes | Yes | Use `s` |
| Calendar block → Task | Yes | Yes | Yes | Yes | N/A |
| File → Task (attach) | Yes | Yes (Files app) | Yes | Yes | Use `attach <path>` |
| Task → Task (reorder) | Yes | Yes | Yes | Yes | `Alt+↑/↓` |

## Notification interactions

- "Mark done" action button on every reminder push.
- "Defer 1 hour" action.
- "Snooze until tomorrow" action.

These run via local OS APIs (deep links into the app for desktop; native action handlers for iOS / Android; Web Push action buttons for web).

## URL scheme

`sunrise://` deep links for:

- `sunrise://entity/<EntityRef>` — open entity in detail.
- `sunrise://capture?text=…&stream=…` — open quick capture pre-filled.
- `sunrise://focus/<TaskId>` — start focus on a task.
- `sunrise://share/<token>` — accept a share invite.

All deep links are validated; unknown shapes are ignored (no shell injection).

## Conflict-of-shortcut handling

Where platform-standard shortcuts disagree (e.g. `Cmd+T` is "new tab" on a browser), the web app yields and uses `Cmd+Shift+T` for new task; the desktop app uses `Cmd+N`. No global "consistent across all platforms" mandate that fights the OS.
