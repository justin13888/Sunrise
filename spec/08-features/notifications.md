---
status: draft
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

## Quiet hours

User-configured per device. During quiet hours, notifications are queued and fire at the next allowed time, OR are dropped (per-category preference).

## Action buttons

Every reminder offers (where the OS supports actions):

- Mark done.
- Defer 1 hour.
- Snooze until tomorrow.
- Open in app.

These are routed through deep links / OS action handlers and run as commands against the local core.

## Multi-device dedup

If two devices schedule the same reminder, the user gets one ping each. Dedup options:

- **Primary device per account.** The user picks "this device handles reminders." Other devices don't fire.
- **First-to-fire wins.** First device that fires emits a "fired" op; other devices see it and suppress their pending reminder. Brief race window where both might fire is acceptable.

Default in v1: primary device, with a "duplicate-tolerant" fallback if the primary is offline or stale.

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
