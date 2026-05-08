---
status: draft
---

# Client Feature Parity Matrix

Marks: **MUST** = ships in v1; **SHOULD** = v1 if feasible, otherwise v1.x; **MAY** = future; **N/A** = doesn't apply on the platform.

| Capability | Desktop | iOS | Android | Web | TUI |
|---|---|---|---|---|---|
| Read/write tasks | MUST | MUST | MUST | MUST | MUST |
| Streams, contexts, routines | MUST | MUST | MUST | MUST | MUST |
| Today / Inbox / Stream views | MUST | MUST | MUST | MUST | MUST |
| Focus mode | MUST | MUST | MUST | MUST | MUST |
| Time-blocking on calendar grid | MUST | MUST | MUST | MUST | SHOULD (compact) |
| Notes (rich text) | MUST | MUST | MUST | MUST | MUST (limited; opens `$EDITOR` for long edits) |
| Attachments — view image/PDF | MUST | MUST | MUST | MUST | MAY |
| Attachments — upload | MUST | MUST | MUST | MUST | MUST |
| Search (FTS) | MUST | MUST | MUST | MUST | MUST |
| Saved searches / views | MUST | MUST | MUST | MUST | MUST |
| Keyboard navigation | MUST (full) | SHOULD (BT keyboard) | SHOULD (BT keyboard) | MUST | MUST (only) |
| Drag-and-drop | MUST | MUST | MUST | MUST | N/A |
| Quick capture (global hotkey / system surface) | MUST (global hotkey) | MUST (lock-screen widget, Shortcut, Siri) | MUST (Quick Settings tile, Tasker) | SHOULD (browser action) | MUST (subcommand) |
| Reminders / scheduled local notifications | MUST | MUST | MUST | SHOULD (Web Push) | MAY (TUI is unlikely to be foreground at the right time) |
| Multi-account | MUST | MUST | MUST | MUST | MUST |
| Pairing — scan QR | MUST (camera or paste) | MUST | MUST | MAY (camera if available) | MAY (manual code entry) |
| Pairing — show QR | MUST | MUST | MUST | MUST | MAY (ASCII QR) |
| Sharing — accept invite | MUST | MUST | MUST | MUST | MUST |
| Sharing — view shared stream as editor | MUST | MUST | MUST | MUST | MUST |
| Calendar integration (Google) | MUST | MUST | MUST | MUST | MAY |
| CalDAV integration | SHOULD | SHOULD | SHOULD | SHOULD | MAY |
| iCal export | MUST | MUST | MUST | MUST | MUST |
| Background sync | MUST (running) | MUST (BGTask) | MUST (WorkManager) | SHOULD (Service Worker) | N/A (foreground tool) |
| Tray / menu bar | MUST | N/A | N/A | N/A | N/A |
| Lock screen widget | N/A | MUST | SHOULD (Glance) | N/A | N/A |
| Home screen widget | N/A | MUST | MUST | N/A | N/A |
| Live Activities (iOS) | N/A | SHOULD | N/A | N/A | N/A |
| Quick Settings tile (Android) | N/A | N/A | SHOULD | N/A | N/A |
| Watch app (Apple Watch / Wear OS) | N/A | MAY | MAY | N/A | N/A |
| OS automation surface (App Intents / Tasker) | MUST (CLI) | MUST | MUST | N/A | MUST (CLI) |
| Vim-style modal navigation | SHOULD (opt-in) | N/A | N/A | SHOULD (opt-in) | MUST (opt-out) |
| Mouse | MUST | N/A | N/A | MUST | MAY |
| Touch | MAY | MUST | MUST | MUST | N/A |
| Print / PDF export | SHOULD | SHOULD | SHOULD | SHOULD | MAY |
| Offline initial setup (no internet) | MUST (LAN pair) | MUST (LAN pair) | MUST (LAN pair) | N/A | MUST |

## Hard rules

- A capability MUST not regress mid-version. A v1.0 → v1.1 release cannot remove a MUST.
- A capability marked N/A is a deliberate choice; if revisited, document the change in [`../11-adr/`](../11-adr/).
- A user can run **without** any specific OS feature (Live Activities, Glance, etc.); fallbacks via plain notifications must exist.
