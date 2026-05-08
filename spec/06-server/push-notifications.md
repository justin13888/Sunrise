---
status: accepted
---

# Push Notifications

Push is **only** a wake-up signal. The server never sends content in pushes; the client wakes, connects, syncs, and decides whether and what to display.

## Why content-less pushes

Content in pushes would require either:

- Sending plaintext through the push provider (Apple, Google) — violates E2EE.
- Sending ciphertext that the OS can show — requires the app to register a Notification Service Extension (iOS) that decrypts. We do that for *display* of reminders the user set, *not* for content of arriving ops.

So:

- **Sync pushes** (peer made a change): content-less wake-up.
- **Reminder pushes** (the user's own reminder fires): scheduled locally on the device that owns the upcoming reminder; uses local-notification APIs, not server pushes.

## Providers

| Platform | Provider |
|---|---|
| iOS | APNs |
| Android | FCM |
| Web (PWA) | Web Push (VAPID) |
| Desktop | OS-native local notifications + WebSocket maintained by the running app |

Desktop apps are usually running, so they don't need server push for sync wakeups.

## Token registration

Clients register their push tokens via `POST /api/v1/devices/<dev_id>/push_token`. Tokens are encrypted at rest using a server-side key the operator does not back up (so a backup leak doesn't leak push tokens).

## Push fanout flow

1. Op arrives at server for receiving device R.
2. R is offline (no active WS).
3. Server enqueues a wake-up push to R's registered tokens.
4. Push provider delivers; OS wakes the app briefly.
5. App connects WS, drains, may emit a *local* notification if the new state warrants one.

## Coalescing

To avoid push storms, the server coalesces:

- Multiple ops for the same device within a 30-second window → 1 push.
- Per-device rate cap (e.g. 10 pushes/minute).

## Quiet hours

Per-device "do not disturb" windows are set by the user. The client tells the server which hours (in opaque-form: a ciphered preference op). When server fanout would push during a quiet window, server holds the push for the end of the window (best-effort; some providers don't respect server-side delays).

## Self-host without push

Self-hosted servers can omit push entirely. Without push, mobile devices fall back to:

- Periodic background pulls when the OS allows (heavily limited on iOS).
- A foreground sync on app open.

Documented as a tradeoff for self-hosters.

## Reminder pushes (local)

Task reminders ("remind me at 3pm") are scheduled by each device that has the responsibility:

- The device the user uses most (heuristic: most recent activity) is the "primary scheduler" for reminders.
- That device registers OS-level local notifications.
- Other devices' schedules of the same reminder are de-duped by reminder_id.

Local notifications carry plaintext (because they're rendered on the device that has the keys). The server is not involved.
