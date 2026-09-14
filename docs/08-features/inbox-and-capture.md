---
status: accepted
---

# Inbox and Capture

The capture surface is the most-used surface in the app. It must be uncompromisingly fast.

## Goals

- Time from intent to confirmed-saved: ≤2 seconds, including the time it takes the user to think and type.
- Available from every surface, with no app-launch needed if the user is on a device with a global hotkey or system widget.
- Parses inline annotations so the user can capture *and* triage in one step when they have the context.

## Capture parser

A pure function `parse(input: &str, at: DateTime, tz: Tz, contexts: &[ContextRef], streams: &[StreamRef]) -> TaskDraft`.

Recognized syntax:

| Token | Effect |
|---|---|
| `#name` | Stream — exact match if exists; ambiguous matches → suggestions |
| `@name` | Context — exact match preferred |
| `^when` | Scheduled at; "when" parsed by date library |
| `!N` (1..5) | Priority |
| `~Xm` / `~Xh` | Estimated duration |
| `*due:when*` | Due (less common, more verbose to avoid collision with markdown) |
| Naked text | Title (everything else) |

Examples:

- `Renew passport #travel @errands ^next saturday !1 ~1h`
- `Email Sara about Q3` → unsorted Inbox task
- `Standup ^weekday 9:30 #work-acme` → schedule + stream

Parser runs *as the user types*; an inline preview shows the structured interpretation. Pressing Enter commits the parsed draft.

## Capture UX patterns

- **Single field.** No labels, no required form fields beyond the title.
- **Stream default.** Capture always lands in Inbox **unless** the user is actively typing into a specific Stream's task list at the moment of capture (in which case that Stream is the implicit target — overridable inline with `#inbox`). Capture from outside the app (share sheets, hotkeys, widgets, Siri/Tasker, CLI subcommand) **always** lands in Inbox. There is no per-device "respect current stream" preference.
- **Today default.** The same rule for the field Today selects on. Capturing into the bar at the top of an unfiltered Today gives an otherwise-undated line `scheduled_at = now`, so the row appears where it was typed. An explicit `^` in the line still wins, exactly as an explicit `#stream` overrides the Stream default above. Without this the line is filed in Inbox — correct, and invisible on the screen that just accepted it, with nothing on screen to say where it went. This is the *only* other implicit target; a Today narrowed by contexts offers no capture bar at all, because a captured line carries none of the contexts the filter names and would be written and filtered straight back out.
- **Voice capture** on supporting platforms (iOS Siri, Android voice, watchOS). Voice goes through the same parser.
- **Batch capture.**
  - Web/Desktop: paste of multi-line text auto-detects line-separated batch; a modal preview shows parsed items with checkboxes; user confirms.
  - Mobile: explicit "Batch capture" entry inside the Inbox view (multi-line textarea).
  - Each line becomes a Task (line ≥ 3 non-whitespace chars; shorter lines are skipped).
- **Dedup.** Dedup key is `(account_id, normalized_title, source, captured_within_5_min)`. `normalized_title` = lowercase + strip leading/trailing whitespace + collapse internal whitespace. Duplicate captures are silently dropped; UI shows a brief "Already captured" toast.

## Inbox view

The Inbox is one Stream, under the well-known id
[`INBOX_STREAM_ID`](../../crates/sunrise-domain/src/inbox.rs)
(`str_0000076XBEE9MQ6S9ED5Q64VVR`) — three zero bytes followed by the ASCII
`sunrise.inbox`, not a literal spelling of the word.

Inbox view characteristics:

- Reverse-chronological default (newest first).
- Multi-select to bulk-promote, defer, delete.
- "Triage mode": one-task-at-a-time presentation, one keypress per outcome — keep, promote, schedule, defer, delete.

## Capture from outside the app

The **Status** column is measured against the tree, not promised: this table
is part specification and part description, and which half a row is in matters
more than the row itself.

| Source | Result | Status |
|---|---|---|
| macOS global hotkey | Capture window | **live** — `RegisterEventHotKey` ⌘⇧N, plus the menu bar item |
| Apple URL scheme (`sunrise://capture?text=`) | Capture sheet, pre-filled | **live** on both Apple apps; each registers the scheme in its own `info:` block |
| iOS Siri / Shortcuts | Voice → parser → task | **live** — `CaptureTaskIntent` behind the **Capture Task** App Shortcut, shared with macOS |
| iOS Lock Screen widget | Capture sheet | **not built** — no `WidgetKit` in `apps/apple` and no widget extension target ([#14](https://github.com/justin13888/Sunrise/issues/14)) |
| iOS Share Sheet | Task with attached link/file/text | **not built** — no share extension target ([#31](https://github.com/justin13888/Sunrise/issues/31)) |
| Android Quick Settings tile | Capture sheet | **not built** — deferred client |
| Android share intent | Task with attached link/file/text | **not built** — deferred client |
| Android Tasker | Task with arbitrary fields | **not built** — deferred client |
| Web bookmarklet / extension | Capture sheet pre-filled | **not built** — deferred client |
| CLI subcommand | One-shot commit | **live** — `sunrise capture`, the same parser |
| Email-to-Sunrise | — | deferred to v2 |

## Performance constraints

- The capture window must render and be ready for input ≤100ms after the trigger fires.
- The parse function must run inline without input lag (< 5ms per keystroke on a 100-char line).
- Commit is local; no network is awaited before the user can dismiss.

## States

Empty / loading / error states follow the three-state contract in [`../07-clients/shared-ui-system.md`](../07-clients/shared-ui-system.md#three-state-view-contract).
