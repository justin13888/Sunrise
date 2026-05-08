---
status: accepted
---

# Focus Mode

A dedicated mode that takes one task and removes everything else.

## Entering focus

- From any task list: `f` or "Focus" button on a hovered/selected task.
- From a Block: tap a bound task within it.
- From a notification: a reminder push has a "Focus now" action.

## Composition

Full-screen UI showing:

- **The task title** (large).
- **The body** (rich text), if any.
- **A timer** (default Pomodoro: 25 minutes work, 5 minutes break, configurable per Stream).
- **Three actions only**: complete, defer, capture-aside.
- **Sub-task list**, if the task has notes containing a checklist.

## What focus does to the system

- iOS: registers a Live Activity; locks the user into the app via Focus filters if set.
- Android: foreground service with a persistent notification; suppresses other Sunrise notifications.
- Desktop: on macOS, dims other windows (where supported) and hides the tray badge. On Linux/Windows, the dim is a no-op (focus mode still works; just no dim).
- Web: requests page visibility lock where possible; exits gracefully on tab close.
- TUI: takes over the whole pane; restores on exit.

### Notification suppression scope

Focus mode suppresses **reminder** notifications only. Background sync wakeups continue (silent). Suppression starts on focus-mode start and ends 30 s after focus-mode end.

## Capture-aside

While focused, ideas appear. Press `a` to open a tiny capture overlay; it lands in **Inbox** — always, regardless of any current-stream context. Rationale: focus mode is for *not switching context*. Returns to focus immediately. Critical for users who can't context-switch without losing their thread.

## Pomodoro and timer policy

- Default 25/5; user-configurable per Stream and per session.
- Long-break after 4 cycles (configurable).
- Audible cue (off by default; opt-in).
- Visual progress bar.

## What completing a focus session records

- The task gets a focus-session entry in its activity timeline:
  - Started at, ended at, duration.
  - Whether the task was completed in this session.
- Routines: a focus session that completes a routine occurrence increments the routine streak.
- Reviews: weekly review surfaces "time spent in focus" per Stream.

## What we don't do

- We don't gamify focus (no streaks of focus per se, no daily quota that yells at you).
- We don't auto-block notifications system-wide. iOS Focus filters and OS-level DND remain the user's choice.
- We don't track keystrokes or productivity scores.

## Focus from a watch

Apple Watch / Wear OS (when shipped): start/stop focus, see timer. No editing.

## States

Empty / loading / error / conflict states follow the four-state contract in [`../07-clients/shared-ui-system.md`](../07-clients/shared-ui-system.md#four-state-view-contract). (Focus mode itself is never "empty" — it shows an idle screen when no task is selected.)
