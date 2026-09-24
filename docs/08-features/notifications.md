---
status: accepted
---

# Notifications

The catalog of every notification Sunrise sends: what triggers it, what it
offers, whether it is on by default, the preference that controls it, how it is
delivered, and which clients deliver it today. A notification kind that is not
in this catalog does not exist, and a client that delivers one of these differently
from this page is wrong. [#347](https://github.com/justin13888/Sunrise/issues/347) implements the catalog; it absorbs
[#301](https://github.com/justin13888/Sunrise/issues/301) (wind-down).

## Rules

1. **Scheduling is computed in Rust.** Every kind is produced by the core as a
   `ReminderIntent` — what, when, which category, which actions, which deep
   link — by the pure `sunrise_domain::notify` functions (today
   `crates/sunrise-domain/src/notify.rs:254#plan_reminders`), so the rules are the
   same on every client and testable without a device. A client only registers
   intents with the OS and routes the responses back as commands. It never
   decides whether or when something fires.
2. **Content is rendered on the device.** The server never sees a notification's
   content. It sends only content-less wake-ups
   ([`../06-server/push-notifications.md`](../06-server/push-notifications.md)).
3. **Parity per device class.** Every client in a device class (desktop:
   macOS, Windows, Linux; phone and tablet: iOS, iPadOS, Android) MUST support
   every kind in this catalog, with the same trigger, actions, default and
   preference. Where an OS lacks a mechanism (an interruption level, an action
   button), the kind degrades in *delivery*, as noted per platform below; it is
   never dropped. A kind is not "done" until every shipping client in its class
   delivers it.
4. **One toggle per kind.** Each kind has its own preference, except security
   events, which cannot be turned off.
5. **Nothing silent.** A notification that could not be delivered as specified
   (permission denied, OS setting off) is reflected in Settings → Notifications
   on that device, next to the kind.

## Catalog

**Preference keys** are listed by name; their types, defaults and scopes are in
§Preference keys, which is the list of record for every notification key.
**Delivery**: *scheduled* — the fire time is known ahead and handed to the OS
scheduler; *on event* — raised when the device observes the event, which needs
the app running or woken by a background refresh or content-less push.
**Level** is the Apple interruption level; other platforms map it per §Platform
mechanisms.

| # | Kind | Trigger | Actions (besides tap-to-open) | Default | Preference keys | Delivery | Level |
|---|---|---|---|---|---|---|---|
| 1 | Task reminder | A task's `planned_at` time minus its resolved lead time (§Lead times). Not for date-only plans. | Mark Done · Snooze 1 h · Snooze until tomorrow | on | `notifications.task_reminder.enabled` | scheduled | active |
| 2 | Deadline | A task's `hard_due_at`: lead time before a timed deadline (default 60 min); morning-brief time on the day of a date-only one. Never for `target_at`. | Mark Done · Defer… · Snooze 1 h | on | `notifications.deadline.enabled`, `.lead_s` | scheduled | active |
| 3 | Routine due | A routine occurrence's `planned_at` time minus its lead time. Replaces #1 for routine tasks, never adds to it. | Mark Done · Skip · Snooze 1 h | on | `notifications.routine_due.enabled` | scheduled | active |
| 4 | Block start | A block's start minus its lead time (default 15 min). | Start Focus · Snooze 5 min | on | `notifications.block_start.enabled` | scheduled | time-sensitive |
| 5 | Morning brief | Wake + 15 min per the day schedule; 08:00 without one. | Plan My Day · Open Triage (when the queue is non-empty) | on | `notifications.morning_brief.enabled`, `.offset_s` | scheduled | active |
| 6 | Late-task triage nudge | Once a day, when the triage queue ([ADR-0047](../11-adr/0047-deadlines-and-lateness.md)) is non-empty and has changed since the last nudge. Fires at the morning-brief time, **folded into the brief** when #5 is on. | Open Triage | on | `notifications.triage_nudge.enabled` | scheduled (content refreshed on each re-plan) | passive |
| 7 | Evening plan | Sleep − 120 min per the day schedule; 20:00 without one. | Move Open Tasks to Tomorrow · Open | on | `notifications.evening_plan.enabled`, `.offset_s` | scheduled | active |
| 8 | Wind-down | Sleep − `lead_s` (default 60 min) per the day schedule ([#301](https://github.com/justin13888/Sunrise/issues/301)). Never fires without a sleep time. | See Tomorrow · Snooze 15 min | on once a sleep time is set | `notifications.wind_down.enabled`, `.lead_s` | scheduled | time-sensitive; see §Wind-down |
| 9 | Focus session end / break | A focus session's planned end, and a break's end. | +5 min · Start Break / End Session | on | `notifications.focus_end.enabled` | scheduled at session start, cancelled if the session ends early | time-sensitive |
| 10 | Time zone changed | The device's zone differs from the zone it last observed. | Review Times · Keep | **off** | `notifications.timezone_changed.enabled` | on event | passive |
| 11 | Weekly review | The review cadence ([`reviews-and-stats.md`](./reviews-and-stats.md)); default Friday 16:00. | Start Review · Snooze until tomorrow | on | `notifications.weekly_review.enabled`, `review.cadence` | scheduled | active |
| 12 | Sync stalled | Local ops unacknowledged by the relay for 6 h while the device had network, or a terminal refusal from the relay. | Open Sync Status | on | `notifications.sync_stalled.enabled` | on event | active |
| 13 | New device paired | A device joins the account. Every other device. | Review Devices | **always** | — | on event | time-sensitive |
| 14 | Device revoked | A device is revoked. Every remaining device, and the revoked one ("This Mac was removed from your account"). | Review Devices | **always** | — | on event | time-sensitive |
| 15 | Recovery code used or changed | The account's recovery blob is fetched, or a new recovery code is sealed. Every device. | Review Security | **always** | — | on event (needs a server-originated account event) | time-sensitive |
| 16 | Calendar reconnect needed | This device's calendar token is rejected; or, on the primary device only, no device has fetched an account for 24 h ([ADR-0049](../11-adr/0049-calendar-integrations-per-device-oauth.md)). | Reconnect | on | `notifications.calendar_reconnect.enabled` | on event | active |
| 17 | Shared change | A peer completes or edits a task in a shared Stream. | Open | on | `notifications.shared_change.enabled` | on event | passive |
| 18 | Place arrival | A region **enter** event for a monitored Place that an open task requires ([ADR-0051](../11-adr/0051-places.md) §4). Evaluated and delivered **on this device only**; the device's presence never leaves it. Requires the `place.entity` feature and location permission. | Open "Here now" · Mark Done (when one task) | **off** | `notifications.place_arrival.enabled` | on event | active |

Not notifications, by decision:

- **Attachment download finished or failed.** A download is started from a
  visible pane and reports in that pane. A notification would duplicate it.
- **Target date passed.** A soft deadline slipping is what the triage nudge
  (#6) batches; a notification per task would be noise.
- **Import finished.** The import report is a sheet on the window that started
  it.

## Status by platform

"Built" means the kind is scheduled from a core intent and delivered with its
actions. macOS and iOS/iPadOS are the shipping clients; Android, Windows and
Linux have no client yet ([`../07-clients/parity-matrix.md`](../07-clients/parity-matrix.md)).

| # | Kind | macOS | iOS / iPadOS | Android | Windows | Linux |
|---|---|---|---|---|---|---|
| 1 | Task reminder | **built** (actions: Mark Done, Snooze 1 h, Snooze until tomorrow) | **built** | no client | no client | no client |
| 2 | Deadline | not built | not built | no client | no client | no client |
| 3 | Routine due | not built (delivered as #1; `ReminderKind::Routine` is never produced) | not built (same) | no client | no client | no client |
| 4 | Block start | **built** (open only; no actions) | **built** (same) | no client | no client | no client |
| 5 | Morning brief | not built (the view and `sunrise://morning` exist; nothing schedules it) | not built (same) | no client | no client | no client |
| 6 | Triage nudge | not built | not built | no client | no client | no client |
| 7 | Evening plan | not built (the view and `sunrise://evening` exist) | not built (same) | no client | no client | no client |
| 8 | Wind-down | not built | not built | no client | no client | no client |
| 9 | Focus end / break | not built | not built | no client | no client | no client |
| 10 | Time zone changed | not built | not built | no client | no client | no client |
| 11 | Weekly review | not built | not built | no client | no client | no client |
| 12 | Sync stalled | not built | not built | no client | no client | no client |
| 13 | New device paired | not built | not built | no client | no client | no client |
| 14 | Device revoked | not built | not built | no client | no client | no client |
| 15 | Recovery code used or changed | not built; blocked on a server account event | not built; same | no client | no client | no client |
| 16 | Calendar reconnect | not built; blocked on [#4](https://github.com/justin13888/Sunrise/issues/4) | not built; same | no client | no client | no client |
| 17 | Shared change | not built; blocked on sharing ([#133](https://github.com/justin13888/Sunrise/issues/133)) | not built; same | no client | no client | no client |
| 18 | Place arrival | not built; blocked on Places ([#339](https://github.com/justin13888/Sunrise/issues/339)) | not built; same | no client | no client | no client |

What exists today, read from the tree:

- **The core** produces intents for two sources, a task's `scheduled_at` and a
  block's start (`crates/sunrise-core/src/engine/notify.rs#query_reminder_intents`).
  `ReminderKind` (`crates/sunrise-domain/src/notify.rs:108#ReminderKind`) has a
  `Routine` variant that only a test constructs. The morning summary and
  end-of-day plan are queries
  (`crates/sunrise-core/src/engine/notify.rs:22#query_morning_summary`,
  `crates/sunrise-core/src/engine/notify.rs:43#query_end_of_day_plan`) that
  nothing schedules.
- **The Apple clients** share one scheduler: `ReminderScheduler` follows the
  change feed and reconciles against the OS's pending requests, and two
  categories are registered (`apps/apple/Sunrise/Notifications/ReminderPlan.swift:10`).
  Settings has one master switch
  (`apps/apple/Sunrise/Notifications/NotificationPreferences.swift:31`), not a
  toggle per kind. No request sets an interruption level, and nothing observes a
  time-zone change.
- **On iOS**, on-event kinds need a background wake, which does not exist yet
  ([#367](https://github.com/justin13888/Sunrise/issues/367)); until it does they fire when the app next runs.

## Details

### Morning brief and triage nudge

The brief opens the morning summary (`sunrise://morning`). When the triage queue
is non-empty, the brief's body names it ("3 tasks need a decision") and offers
**Open Triage**, and no separate nudge fires. With the brief off, the nudge fires
alone at the same time. The nudge fires at most once per civil day and only when
the queue changed since the last one, so an untouched queue does not nag.

### Wind-down

Its purpose is to reach the user *as they wind down*, which on most phones is
when a Sleep Focus or Bedtime mode is already on. So:

- **Apple.** The request uses the `.timeSensitive` interruption level (the app
  carries the Time Sensitive Notifications entitlement). A Sleep Focus that
  silences time-sensitive notifications still holds it, and the app cannot grant
  itself an exception. When the user enables wind-down, Sunrise explains this
  and opens the system Focus settings so the user can add Sunrise to Sleep's
  allowed apps. Settings shows whether time-sensitive delivery is allowed
  (`UNNotificationSettings.timeSensitiveSetting`). Critical alerts are never
  used.
- **Android.** A dedicated channel `wind_down` with `CATEGORY_REMINDER` and high
  importance. Only the user can let a channel bypass Do Not Disturb or Bedtime
  mode; enabling wind-down deep-links to that channel's settings with the same
  explanation.
- **Windows.** A toast with `scenario="reminder"`, which Focus Assist holds
  unless Sunrise is on its priority list; enabling wind-down points there.
- **Linux.** A desktop notification with urgency `critical` where the
  notification server honours it; otherwise a normal one.

### Time zone changed

Off by default (`notifications.timezone_changed.enabled = false`,
[ADR-0050](../11-adr/0050-preferences-and-day-schedule.md) §2). When on,
the core compares the device's zone with the zone it last observed on this
device (device-local state) at launch, on a significant-time-change event, and
on the platform's zone-change notification. The notification names the new
zone and how many upcoming timed items have fixed (zoned) times, and **Review
Times** opens Upcoming. Floating and all-day items follow the new zone without
comment ([`../10-cross-cutting/time.md`](../10-cross-cutting/time.md)). With the
toggle off the re-plan still happens; only the notification is skipped.

### Security events

Kinds 13–15 are always on, go to **every** device regardless of the primary
device rule, are never suppressed by quiet hours or focus, and cannot be
snoozed. They are raised when the device applies the op that records the event
(a `device_cert` publication for a new device, a `device_revoke`), so a device
learns of them on its next sync. Kind 15 needs an event the relay originates,
because the recovery blob is fetched from the relay and no op records the fetch;
until the server emits one, it is not deliverable.

### Sync stalled

Device-local. Raised once per stall, cleared silently when the relay
acknowledges again. A terminal refusal (revoked device, account gone) raises it
at once with the refusal's own message.

## Lead times

- Task and routine reminders default to 0 (fire at `planned_at`); block starts
  to 15 minutes; timed deadlines to 60 minutes.
- **Hierarchy:** per task → per Stream → global, first non-null wins. `null` and
  `0` are different answers: `0` means "fire at the time", which is the default,
  so it stops the fallback like any other value. The per-task and per-Stream
  values are entity fields (`Task.reminder_lead_s`, `Stream.reminder_lead_s`)
  because they describe the work; the global value is the preference
  `notifications.task_reminder.lead_s` (§Preference keys), which routine
  occurrences share.
- The UI labels each value with where it came from.

## Quiet hours

A vault default that any device may override
(`vault_overridable`, [`../02-domain/preferences.md`](../02-domain/preferences.md)
§Scope and resolution): set once, quiet hours follow the user to every device,
and a work laptop can keep its own. The window is
`notifications.quiet_hours.window` (`{ start, end }` civil times, one register
so a concurrent edit never pairs one device's start with another's end; `end <
start` wraps midnight). During quiet hours, notifications are queued and fire
at the next allowed time, or are dropped, per
`notifications.quiet_hours.policy = "queue" | "drop"`, default `queue`. A queued notification
fires at the first minute outside quiet hours, capped at 4 hours after its
original time; if still inside quiet hours then, it is dropped with a
`notif.queued.dropped` log line. Several queued notifications collapse into one.
The core applies quiet hours before it returns an intent
(`crates/sunrise-domain/src/notify.rs#apply_quiet_hours`), and an intent moved by
them carries `deferred_from` so the client can say "delayed from 06:15" only
when that is true.

Security events (13–15) ignore quiet hours. Wind-down and focus end are
scheduled into quiet hours only if the user placed them there (a sleep time
inside quiet hours means wind-down fires anyway, because the user asked for it
at that time).

## Focus suppression

During an in-app focus session ([`focus-mode.md`](./focus-mode.md)), only that
session's own end and break notifications (#9) and security events fire. Every
other kind is queued under the quiet-hours rule until 30 s after the session
ends.

## Multi-device dedup

One device per account is the **primary**; it delivers every kind except
security events (every device) and the device-local kinds (#12, #18, and #16's
token-rejected case, which fire on the device concerned). Other devices stay
silent.

- **One register.** `notifications.primary_device` is a `vault`-scope register
  holding a device id. It is unset by default; the first device that enables
  notifications while it is unset claims it by writing its own id. Because it is
  one LWW register, exactly one primary exists by construction: two concurrent
  claims converge on one id. The user moves it from Settings on any device. A
  register naming a revoked device reads as unset.
- **The core enforces it.** `ReminderIntents` returns no primary-only intent
  on a device whose id is not the one the register holds.

When the primary goes quiet (no check-in for 1 hour), the relay tells every
active device to take over; users may briefly see duplicates. This is
deliberately simple: no first-to-fire suppression op.

## Actions

Every action is routed through the deep-link and OS action handlers
([`../07-clients/interaction-patterns.md`](../07-clients/interaction-patterns.md))
and runs as a command against the local core. Actions that move time (Snooze,
Defer, Move Open Tasks to Tomorrow, Plan My Day) go through the planner
([`planner.md`](./planner.md)), so a notification action and a drag produce the
same result. **Defer…** and **Move Open Tasks to Tomorrow** are deferrals: each
writes `planned_at` and `+1` on `deferred_count` per task, the same ops as
`Command::Triage` with `Defer` ([ADR-0047](../11-adr/0047-deadlines-and-lateness.md)
§4). An action the OS cannot show as a button remains reachable from the view
the notification opens.

## Settings → Notifications

Lists every kind in catalog order with its toggle (security events shown as
always on), its timing where it has one, and this device's delivery state
(permission, time-sensitive allowed). Also: global lead times, quiet hours, and
the primary-device choice. On desktop, each row's action shows its shortcut
hint where one exists ([`keyboard.md`](./keyboard.md)).

## Preference keys

This page owns every notification preference key; the Preferences entity
([ADR-0050](../11-adr/0050-preferences-and-day-schedule.md),
[`../02-domain/preferences.md`](../02-domain/preferences.md)) stores them and
does not define them. Per-kind keys are `notifications.<kind>.<field>`. Scopes
are the three of `preferences.md`: `vault` (synced), `vault_overridable` (synced
account default a device may override) and `device` (this install only).

| Kind | `.enabled` default | Other fields | Scope |
|---|---|---|---|
| `task_reminder` | `true` | `.lead_s`: `uint`, default `0`; the global floor of the lead-time chain (§Lead times), shared by `routine_due` | `.enabled` `vault_overridable`; `.lead_s` `vault_overridable` |
| `deadline` | `true` | `.lead_s`: `uint`, default `3600` | `.enabled` `vault_overridable`; `.lead_s` `vault` |
| `routine_due` | `true` | — | `vault_overridable` |
| `block_start` | `true` | `.lead_s`: `uint`, default `900` | `.enabled` `vault_overridable`; `.lead_s` `vault` |
| `morning_brief` | `true` | `.offset_s`: `int`, seconds after wake, default `900` | `.enabled` `vault_overridable`; `.offset_s` `vault` |
| `triage_nudge` | `true` | — | `vault_overridable` |
| `evening_plan` | `true` | `.offset_s`: `int`, seconds before sleep, default `7200` | `.enabled` `vault_overridable`; `.offset_s` `vault` |
| `wind_down` | `true` (fires only once a sleep time is set) | `.lead_s`: `uint` 0..14400, default `3600` | `.enabled` `vault_overridable`; `.lead_s` `vault` |
| `focus_end` | `true` | — | `vault_overridable` |
| `timezone_changed` | **`false`** | — | `vault_overridable` |
| `weekly_review` | `true` | — (the time is `review.cadence`, owned by [`reviews-and-stats.md`](./reviews-and-stats.md)) | `vault_overridable` |
| `sync_stalled` | `true` | — | `device` |
| `calendar_reconnect` | `true` | — | `vault_overridable` |
| `shared_change` | `true` | — | `vault_overridable` |
| `place_arrival` | **`false`** | — | `device` (location permission is per device) |

Security events (13–15) have no key. Account-level keys:

| Key | Type | Default | Scope |
|---|---|---|---|
| `notifications.enabled` | `bool` | `true` | `device`: the master switch on this install |
| `notifications.primary_device` | device id, or unset | unset; the first device that enables notifications claims it (§Multi-device dedup) | `vault` |
| `notifications.quiet_hours.window` | `{ start: civil-time, end: civil-time }`, or absent; `start` MUST NOT equal `end` | absent | `vault_overridable` |
| `notifications.quiet_hours.policy` | `"queue"` / `"drop"` | `"queue"` | `vault_overridable` |

A kind this catalog adds is a new row here, and its keys follow the same
pattern.

## Platform mechanisms

| Level | Apple | Android | Windows | Linux |
|---|---|---|---|---|
| passive | `.passive` | channel importance low | toast, no sound | urgency low |
| active | `.active` | channel importance default | toast | urgency normal |
| time-sensitive | `.timeSensitive` (entitlement) | channel importance high | toast `scenario="reminder"` | urgency critical |

Android channels are one per kind, named as in the catalog, so users manage
them in system settings. Every platform's scheduled kinds use absolute-time
triggers (a calendar trigger on Apple, `setExactAndAllowWhileIdle` or its
successor on Android), so a laptop that slept through the fire time delivers on
wake rather than restarting a countdown.

## Testing

- **Rust, per kind:** fire time; quiet-hours queueing; focus suppression; a DST
  day; a sleep time past midnight; a disabled kind yields no intent; the
  primary-device rule.
- **Clients:** the reconcile schedules every kind the core returns, with its
  category and actions; a deep-link test per kind with a destination.
- *Target:* an end-to-end test that a reminder set 1 minute ahead fires within
  ±5 s. What is covered today is the scheduling arithmetic
  (`a_task_reminder_fires_at_its_scheduled_time_by_default` in
  `crates/sunrise-domain/src/notify.rs`), not delivery.
- A manual regression check on each major OS release, because notification APIs
  change often.
