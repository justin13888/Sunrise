---
status: accepted
---

# Push Notifications

> **Implementation status: the APNs path is built; FCM, Web Push, the
> `alert` tier and the `sunrise` payload block are not; `push_key.bin` never
> will be (ADR-0060).**
> With `[push.apns]` configured
> ([`self-hosting.md`](./self-hosting.md) §Config), every op batch
> `POST /sync/ops` stores fresh wakes the account's other devices that hold an
> APNs token and have no event stream open, with the content-less `silent`
> push described under [APNs](#apns), coalesced and capped as
> [Coalescing](#coalescing-implemented) says. The code is `crates/sunrise-server/src/push/`:
> `dispatch.rs` (presence, the queue, coalescing, the cap, retries) and
> `apns.rs` (the provider). Without `[push]` nothing is sent, and the server
> logs `srv.push.disabled` once at startup.
>
> No client registers a token yet: `POST /api/v1/devices/push-tokens` is
> live and nothing in `apps/apple` calls it
> ([#367](https://github.com/justin13888/Sunrise/issues/367)). Sections below
> say per section what is built.

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
| Desktop | OS-native local notifications + the event stream the running app holds open |

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

**Tokens are stored as written, and protected at rest with the rest of the
database.** `push_tokens.token` is a `TEXT` column written verbatim by
`Store::upsert_push_token`; there is no per-token encryption. With
`[storage] encrypt = true` the whole relay database is SQLCipher-encrypted
under a key file the operator keeps outside the data dir, so a copy of the
data dir or a backup of it carries the tokens only as ciphertext
([`self-hosting.md`](./self-hosting.md) §Encryption at rest). With encryption
off, they are plaintext in the file and in every backup of it.

The per-token ChaCha20-Poly1305 scheme this section used to specify, under a
`push_key.bin` excluded from backups, is **superseded** by
[ADR-0060](../11-adr/0060-relay-database-encryption-at-rest.md) and will not be
built. It protected against the same thing, a backup leaked without its key,
and the database key file now does that for every column at once. The relay
must read a token in the clear to hand it to APNs, so no at-rest scheme hides
tokens from the running server.

## Push fanout flow (implemented for APNs)

1. Op arrives at server for receiving device R. `ops` in
   `crates/sunrise-server/src/api/sync/publish.rs` appends it, publishes it to
   open streams, and — for a batch stored fresh, never a re-sent duplicate —
   queues a wake naming the account, the stream and the sending device. The
   queue holds 1024 wakes; a full queue drops the wake and counts
   `sunrise_push_dispatch_total{result="dropped"}`. The append is never
   slowed or failed by push: queueing is a non-blocking send, with no store
   read and no lock the append path holds.
2. R is offline (no open event stream). A device counts as online while a
   `GET /sync/events` it opened is being served, whichever stream it is
   subscribed to. The sending device is never woken.
3. Server enqueues a wake-up push to R's registered tokens. One worker task
   looks up the account's active (unrevoked) devices holding a token for the
   provider's platform, applies [Coalescing](#coalescing-implemented), and hands each
   surviving push to its own delivery task, at most 16 at once.
4. Push provider delivers; OS wakes the app briefly. Each attempt has 10 s.
   A `429`, a `5xx`, a timeout or a transport failure is retried up to three
   attempts in all, waiting 1 s then 2 s. APNs `410` or `400 BadDeviceToken`
   deletes the token row — only while it still holds the token that was sent,
   so a re-registration in the meantime survives. Any other refusal is logged
   and not retried.
5. App establishes a session, drains the stream, may emit a *local* notification if the new state warrants one.

An offline device that is never woken — no token, push not configured, a
dropped wake — still catches up by cursor replay on its next
`GET /sync/events`, so a lost push is latency, never a lost op.

## Priority tiers (only `silent` is sent)

There are exactly **two** tiers:

| Tier | Meaning | APNs `apns-priority` | FCM `priority` | Web Push `Urgency` |
|---|---|---|---|---|
| `alert` | High-priority wake (mention, reminder hand-off). | `10` | `high` | `high` |
| `silent` | Background/sync wake; user-invisible until client renders. | `5` | `normal` | `normal` |

Any low-priority re-sync a previous draft split into a third tier is now folded into `silent`. There is no third tier.

Every push the relay sends today is a `silent` sync wake. No `alert` push has a
sender: mentions and reminder hand-offs are not built.

## Coalescing (implemented)

To avoid push storms, the server coalesces per `(device_id, stream_id, push_kind)` tuple, where `push_kind ∈ {sync, reminder, mention}`; only `sync` has a sender. The coalescing window is **30 s**, opened by a push: the first op for a tuple sends at once, the ops after it inside the window send nothing, and if any arrived, one trailing push goes when the window closes — the device may have synced and slept before they landed. A device that has opened a stream by then is not pushed. Time-zone-agnostic — based purely on `(device, stream, kind)` tuples, never on wall-clock windows. A per-device cap of **10 pushes in any 60 s**, across every stream, prevents pathological storms; a push over it is not sent and is counted as `sunrise_push_dispatch_total{result="rate_limited"}`.

## Push payload format (APNs implemented)

What APNs is sent is the `aps` dictionary below and nothing else: the
`sunrise` block, and every FCM and Web Push format, are design targets.

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

What is sent, byte for byte (`push::APNS_PAYLOAD`), as
`POST /3/device/<token>` over HTTP/2 to `api.push.apple.com` or
`api.sandbox.push.apple.com` with `apns-push-type: background`,
`apns-priority: 5` and `apns-topic` set to the configured bundle id:

```json
{"aps":{"content-available":1}}
```

No stream id, count, timestamp or text. Authentication is a provider token: an
ES256 JWT whose header carries `kid` (the key id) and whose claims are `iss`
(the team id) and `iat`, signed with the `.p8` key, reused for 40 minutes and
re-signed early when APNs answers `ExpiredProviderToken` or
`InvalidProviderToken`.

The design target, not sent: `apns-priority: 10` for `alert`,
`apns-priority: 5` for `silent`, and this payload:

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

FCM data-only messages CAN wake the app on most devices, rate-limited by Android's Doze and battery optimizations. The design accepts occasional latency during deep Doze. An optional foreground service for "always-on" sync as a per-device opt-in is not built (deferred — see [`overview.md`](./overview.md)).

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

Self-hosted servers can omit push entirely — no `[push]` table, which is the
default. Without push, mobile devices fall back to:

- Periodic background pulls when the OS allows (heavily limited on iOS).
- A foreground sync on app open.

Documented as a tradeoff for self-hosters.

## Reminder pushes (local)

Task reminders ("remind me at 3pm") are scheduled by each device that has the responsibility:

- The most-recently-active device (defined above) is the "primary scheduler" for reminders.
- That device registers OS-level local notifications.
- Other devices' schedules of the same reminder are de-duped by `reminder_id`.

Local notifications carry plaintext (because they're rendered on the device that has the keys). The server is not involved.
