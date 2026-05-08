---
status: draft
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
- Desktop: dims other windows (where supported); hides the tray badge.
- Web: requests page visibility lock where possible; exits gracefully on tab close.
- TUI: takes over the whole pane; restores on exit.

## Capture-aside

While focused, ideas appear. Press `a` to open a tiny capture overlay; it lands in Inbox. Returns to focus immediately. Critical for users who can't context-switch without losing their thread.

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
