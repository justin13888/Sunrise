---
status: accepted
---

# Push Notifications

> **Implementation status: nothing in this document is built except token
> storage.** `crates/sunrise-server/src/push.rs` declares a `PushProvider`
> trait, a `PushIntent`, a `PushRegistration` and one implementation,
> `LoggingProvider`, whose `send` increments a counter and returns `Ok(())`.
> `LoggingProvider` is never constructed outside its own unit test, and it is
> not held by `ServerState`. **The relay never builds a `PushIntent` at all** —
> the only `PushIntent { .. }` literal in the workspace is in that test — so no
> code path leads from an arriving op to a wake-up. No APNs, FCM or Web Push
> client is in the dependency tree (`apns2`, `fcm` and `web-push` appear in no
> `Cargo.toml`). What *is* live: `POST /api/v1/devices/push-tokens` stores a
> token, and revoking a device deletes its tokens.
>
> Everything below — fanout, priority tiers, coalescing, payload formats,
> `push_key.bin` — is a design target. Read it as a specification, not as a
> description.

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

## Token registration (implemented)

Clients register their push tokens via `POST /api/v1/devices/push-tokens` with a
`PushRegistration` body — `{ device_id, platform, token }` — not via a
device-scoped path. The `device_id` is validated against the caller's own
account (`Store::active_device`), so a token cannot be filed under a device the
caller does not actively own; a revoked device's registration is a
`403 AUTH_DEVICE_NOT_OWNER`. See [`api.md`](./api.md) §devices.

Tokens live in the `push_tokens` table keyed `(device_id, platform)`, upserted
on re-registration, and deleted in the same transaction that revokes the device
— a revoked device silently stops being wakeable.

**Tokens are stored in plaintext.** `push_tokens.token` is a `TEXT` column
written verbatim by `Store::upsert_push_token`; there is no encryption at rest.
The ChaCha20-Poly1305 scheme below is **not implemented**: no `push_key.bin` is
generated or read anywhere, and the relay's SQLite database is not SQLCipher-keyed
(see [`relay-and-blob-storage.md`](./relay-and-blob-storage.md)). An operator's
backup of the data dir therefore *does* carry push tokens today.

The target, unchanged: server-side push-token encryption using
**ChaCha20-Poly1305 with a 32-byte key** generated at server initialization and
written to `<data_dir>/push_key.bin` (mode 0600). Operators **MUST** exclude
this file from backups; a backup leak then does not leak push tokens. There is
no key rotation in v1 — key loss invalidates all stored push tokens, and clients
re-register on next sync.

## Push fanout flow (not implemented)

No step below runs. `ws.rs` appends an arriving batch to the durable relay log
and fans it out to connected sessions; an offline receiver is simply not
delivered to, and nothing consults `push_tokens`.

1. Op arrives at server for receiving device R.
2. R is offline (no active WS).
3. Server enqueues a wake-up push to R's registered tokens.
4. Push provider delivers; OS wakes the app briefly.
5. App connects WS, drains, may emit a *local* notification if the new state warrants one.

## Priority tiers (not implemented)

There are exactly **two** tiers:

| Tier | Meaning | APNs `apns-priority` | FCM `priority` | Web Push `Urgency` |
|---|---|---|---|---|
| `alert` | High-priority wake (mention, reminder hand-off). | `10` | `high` | `high` |
| `silent` | Background/sync wake; user-invisible until client renders. | `5` | `normal` | `normal` |

Any low-priority re-sync a previous draft split into a third tier is now folded into `silent`. There is no third tier.

## Coalescing (not implemented)

To avoid push storms, the server coalesces per `(device_id, stream_id, push_kind)` tuple, where `push_kind ∈ {sync, reminder, mention}`. The coalescing window is **30 s**: multiple ops in the same window collapse to one push, and the payload reflects the latest event. Time-zone-agnostic — based purely on `(device, stream, kind)` tuples, never on wall-clock windows. A per-device rate cap (e.g. 10 pushes/minute) prevents pathological storms.

## Push payload format (not implemented)

Note that `account_id_salt` below does not exist: there is no salt column on
`accounts`, and the server's live hashing (`logging::account_h` / `id_h`) is a
deliberately unsalted BLAKE3 truncation — see
[`observability.md`](./observability.md) for why.

`stream_h` is the per-account stream hash:

```
stream_h = BLAKE3(stream_id || account_id_salt, 4)   # lowercase hex, 8 chars
```

`account_id_salt` is **server-generated per-account**, stored in the account row, used **only** for log/push grouping. It lets the server group coalescing without ever seeing real `stream_id` values.

### APNs

Sent with `apns-priority: 10` for `alert`, `apns-priority: 5` for `silent`. Payload:

```json
{
  "aps": {
    "content-available": 1,
    "mutable-content": 1
  },
  "sunrise": {
    "v": 1,
    "kind": "sync",
    "stream_h": "abc12345",
    "ts_ms": 1715123456789
  }
}
```

### FCM

Sent with `priority: high` for `alert`, `priority: normal` for `silent`. FCM data values must be strings. The `notification` field is **never** set; the client constructs the user-visible notification from the data after decrypting the latest op.

```json
{
  "data": {
    "v": "1",
    "kind": "sync",
    "stream_h": "abc12345",
    "ts_ms": "1715123456789"
  }
}
```

### Web Push

Sent with `Urgency: high` for `alert`, `Urgency: normal` for `silent`. Body is the same JSON as the APNs `sunrise` block, encrypted to the client's VAPID-managed key per RFC 8291:

```json
{
  "v": 1,
  "kind": "sync",
  "stream_h": "abc12345",
  "ts_ms": 1715123456789
}
```

### Android data-only without foreground service

FCM data-only messages CAN wake the app on most devices, rate-limited by Android's Doze and battery optimizations. v1 accepts occasional latency during deep Doze. v1.x adds an optional foreground service for "always-on" sync as a per-device opt-in (deferred — see [`overview.md`](./overview.md)).

## Quiet hours

- Quiet hours are stored in the user's vault as encrypted preferences (an ordinary op).
- The server **cannot** read quiet hours.
- Enforcement is **client-side**: the client decides whether to surface a notification; the server always sends the push.
- "Server-side coalescing" above is time-zone-agnostic — based purely on `(device, stream, kind)` tuples.

## Reminder target device (not implemented)

`devices.last_seen_at_ms` is maintained (`Store::touch_device` stamps it on
every authenticated request), so the input exists; the selection below does not.
There is no `device_state` heartbeat frame in the wire protocol.

"Most-recently-active device" = the device whose `last_seen_at_ms` is highest among devices that:

1. Have `last_seen_at_ms > now - 24h`, AND
2. Are not in client-reported "do not push to me" mode (sent in the `device_state` heartbeat).

Tie-break by lex `device_id`.

## Self-host without push

Self-hosted servers can omit push entirely. Without push, mobile devices fall back to:

- Periodic background pulls when the OS allows (heavily limited on iOS).
- A foreground sync on app open.

Documented as a tradeoff for self-hosters.

## Reminder pushes (local)

Task reminders ("remind me at 3pm") are scheduled by each device that has the responsibility:

- The most-recently-active device (defined above) is the "primary scheduler" for reminders.
- That device registers OS-level local notifications.
- Other devices' schedules of the same reminder are de-duped by `reminder_id`.

Local notifications carry plaintext (because they're rendered on the device that has the keys). The server is not involved.
