---
status: accepted
---

# Notifications

Notifications are *local* — scheduled by the device that "owns" the upcoming reminder. The server only sends content-less wake-ups when peer ops arrive (see [`../06-server/push-notifications.md`](../06-server/push-notifications.md)).

## What the core provides

Notification *content* and *timing* are computed on-device, after decryption,
by three reads on the core:

| Query | Returns |
|---|---|
| `MorningSummary { now_ms }` | Tasks completed since the previous calendar date, the unfiled Inbox queue to triage, and what lands today. |
| `EndOfDayPlan { now_ms }` | Today's still-open tasks (overdue included), the seven civil days ahead, and the unscheduled backlog to plan from. |
| `ReminderIntents { now_ms, horizon_ms, settings }` | Everything to hand the OS scheduler in that window, lead time and quiet hours already applied. |

The first two are the dedicated views issue #9 asks a notification to open
into; the client's job is to render them, not to recompute them. The rules
themselves are the pure `sunrise_domain::notify` functions, so they are the
same on every client and testable without a device.

`settings` carries the **per-device** half of the configuration — the global
lead time, the quiet window, and whether this device is the primary one. Those
are configured per device and never synced, so they arrive with the query
rather than living on a replicated entity that would push one device's quiet
hours onto another. The per-task and per-Stream lead times *are* entity fields
(`Task.reminder_lead_s`, `Stream.reminder_lead_s`), because they describe the
work rather than the device.

Two sources produce intents in v1: a Task's `scheduled_at` and a Block's start.
A routine occurrence is already a Task by the time it is due — that is what
materialization produces — so it needs no source of its own, and having one
would double every routine reminder.

## Sources of notifications

| Source | Trigger | Content |
|---|---|---|
| Reminder | Task with `scheduled_at` minus user's lead-time | Task title + actions |
| Block start | A Block's start time minus travel-buffer | Block title + bound tasks |
| Routine due | Routine occurrence falling in next hour, undone | Routine name |
| Shared peer change | Op from a peer marking a shared task done / mentioning user | Stream + brief |
| Weekly review | The user's review cadence triggers | "Time to review" |

All notifications are **rendered locally**. The server cannot read content; even shared-peer notifications are computed on-device after decryption.

## Lead times

- Default: 0 (fire at scheduled time).
- Configurable per task and per Stream (default).
- Block reminders: 15 min before (default).

### Lead-time hierarchy

Per-task → per-Stream → global, in that order. The first non-null wins. UI labels values inline so users see where the value comes from.

`null` and `0` are different answers: `0` means "fire at the scheduled time",
which is the documented default, so it stops the fallback like any other value.
That is why both entity fields are optional rather than defaulted integers.
Block reminders take 15 minutes as their *global* level, not as an override —
a Task or Stream value still wins over it.

## Quiet hours

User-configured per device. During quiet hours, notifications are queued and fire at the next allowed time, OR are dropped (per-category preference).

`quiet_hours.policy = "queue" | "drop"`, default `queue`. Queue: notification fires at the next minute outside quiet hours, capped to 4 hours after the original time; if still in quiet hours then, drop with a `notif.queued.dropped` log line. Multiple queued notifications collapse using the coalescing rule (see [`../06-server/push-notifications.md`](../06-server/push-notifications.md)).

## Action buttons

Every reminder offers (where the OS supports actions):

- Mark done.
- Defer 1 hour.
- Snooze until tomorrow.
- Open in app.

These are routed through deep links / OS action handlers and run as commands against the local core.

## Multi-device dedup

The user picks a **primary** device per account: that device handles reminders; other devices stay silent.

Enforced in the core: `ReminderIntents` returns an empty list when
`settings.is_primary_device` is false, before it reads a row. The takeover
fallback below is a server-side decision — the relay is the only party that can
see a device stop checking in — so a client that has been told to take over
simply passes `true`.

Fallback when the primary goes quiet: if the primary push device hasn't checked in for 1 hour, ALL active devices receive the push. Users may see duplicates briefly; they can mute on devices that don't need them. This is intentionally simple — no per-device duplicate-tolerance setting, no first-to-fire suppression op.

## Notification channels (Android)

Channels per Stream + per category. User-managed in OS settings.

## Notification settings UI

A `Settings → Notifications` page that lets the user:

- Enable/disable categories.
- Set lead times globally and per Stream.
- Configure quiet hours.
- Choose primary device.

## Testing

- Shipped E2E test that verifies a reminder set 1 minute in the future fires within ±5s.
- Manual regression check on each iOS/Android major version (notification APIs change frequently).
