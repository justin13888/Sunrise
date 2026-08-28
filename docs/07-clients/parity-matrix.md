---
status: accepted
---

# Client Feature Parity Matrix

Marks: **MUST** = ships in v1; **SHOULD** = v1 if feasible, otherwise v1.x;
**MAY** = future; **N/A** = doesn't apply on the platform.

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
| Notes (rich text) | MUST | MAY | — | — | — | — |
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
| Sharing — accept invite | MUST | MAY | — | — | — | — |
| Sharing — view shared stream as editor | MUST | MUST | — | — | — | — |
| Calendar integration (Google) | MUST | MAY | — | — | — | — |
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

## Hard rules

- A capability MUST not regress mid-version. A v1.0 → v1.1 release cannot
  remove a MUST.
- A capability marked N/A is a deliberate choice; if revisited, document the
  change in [`../11-adr/`](../11-adr/).
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
  local pref (not synced).
- Available on macOS. Full motion list lives in
  [`../08-features/keyboard.md`](../08-features/keyboard.md).
