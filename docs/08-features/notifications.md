---
status: accepted
---

# Notifications

Notifications are *local* — scheduled by the device that "owns" the upcoming reminder. The server only sends content-less wake-ups when peer ops arrive (see [`../06-server/push-notifications.md`](../06-server/push-notifications.md)).

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
