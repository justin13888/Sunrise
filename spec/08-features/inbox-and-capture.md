---
status: draft
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
- **Smart defaults.** If the user is currently viewing Stream X, capture defaults to X (override with `#inbox`).
- **Voice capture** on supporting platforms (iOS Siri, Android voice, watchOS). Voice goes through the same parser.
- **Batch capture.** Multi-line input where each line is a task. Each line is parsed independently. Useful for clearing a paper list at once.

## Inbox view

The Inbox is one Stream: `str_INBOX0000000000000000000000`.

Inbox view characteristics:

- Reverse-chronological default (newest first).
- Multi-select to bulk-promote, defer, delete.
- "Triage mode": one-task-at-a-time presentation, one keypress per outcome — keep, promote, schedule, defer, delete.

## Capture from outside the app

| Source | Result |
|---|---|
| Desktop global hotkey | Capture window |
| iOS Lock Screen widget | Capture sheet |
| iOS Share Sheet | Task with attached link/file/text |
| iOS Siri intent | Voice → parser → task |
| Android Quick Settings tile | Capture sheet |
| Android share intent | Task with attached link/file/text |
| Android Tasker | Task with arbitrary fields |
| Web bookmarklet / extension | Capture sheet pre-filled |
| TUI subcommand | One-shot commit |
| Email-to-Sunrise | (deferred to v2) |

## Performance constraints

- The capture window must render and be ready for input ≤100ms after the trigger fires.
- The parse function must run inline without input lag (< 5ms per keystroke on a 100-char line).
- Commit is local; no network is awaited before the user can dismiss.
