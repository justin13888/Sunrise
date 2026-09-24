# 0050 — Preferences are a vault-synced typed entity with a device overlay, and the day schedule is one of them

**Status:** accepted

**Amends** [`../08-features/notifications.md`](../08-features/notifications.md),
whose rule that notification settings "are configured per device and never
synced" is replaced by per-key scope (§2).

**Depends on** [ADR-0044](./0044-per-field-ops.md) (per-field registers) and
ADR-0045 (lossless unknown values). **Used by**
[ADR-0047](./0047-deadlines-and-lateness.md) (`stale_after_days`, the end of a
planner day) and [ADR-0048](./0048-interactive-planner.md) (the planner day).

**Specified in** [`../02-domain/preferences.md`](../02-domain/preferences.md)
and [`../02-domain/day-schedule.md`](../02-domain/day-schedule.md). **Tracked
by** [#337](https://github.com/justin13888/Sunrise/issues/337) (Preferences) and [#338](https://github.com/justin13888/Sunrise/issues/338) (day schedule), and closes the
precondition of #301 (wind-down notification).

## Context

Every setting a user can change lives in the Apple client's `UserDefaults`:
the relay URL and OIDC configuration
(`apps/apple/Sunrise/Identity/AppSettings.swift#AppSettings`), the notification
master switch, primary-device flag, lead time and quiet hours
(`apps/apple/Sunrise/Notifications/NotificationPreferences.swift#NotificationPreferences`),
and the per-list task order. Nothing in Rust models a preference. The values
the core needs are hard-coded: the week starts on Monday in the core
(`crates/sunrise-core/src/engine/block.rs#query_week_blocks`) and in the client
(`apps/apple/Sunrise/Calendar/CalendarModel.swift#dayStartMs`).

That has three costs:

- **Nothing follows the user.** A week start, a stale threshold or a quiet-hours
  window set on the Mac has to be set again on the iPhone.
- **Every client reinvents storage.** A second client would pick its own keys,
  types and defaults, and they would drift.
- **The core cannot honour what it cannot read.** Lateness (ADR-0047), the
  planner (ADR-0048) and "Today" all need values that only a client holds.

A day is also undefined beyond civil midnight. `civil_span`
(`crates/sunrise-core/src/engine/block.rs#civil_span`) is the only notion of a
day in the core, so a task done at 00:30 by someone who sleeps at 01:00 lands on
tomorrow, and #301 ("remind me 60 minutes before I go to bed") has no bedtime to
count back from.

## Decision

### 1. `Preferences` is one synced entity per vault, and every key is its own register

- **Singleton.** Its id is fixed per vault (`prf_` followed by the zero body),
  so two devices that write their first preference concurrently write to the
  same entity rather than creating two.
- **Ordinary `Patch` ops, one register per key.** A device's first write is an
  idempotent create of the singleton under that deterministic id (a `Patch`
  with `"create": true`, ADR-0044 §4); every later write is a plain `Patch`
  setting keys in the `values` map, and clearing a key is a `Patch` that sets
  it to `null`. Each key is its own per-field LWW register, so two devices
  editing different keys never conflict. Map-valued keys (the day schedule's
  per-weekday, per-month-day and per-date entries) are **maps of registers**:
  each map entry is its own register, so overrides for two different dates set
  on two devices both survive.
- **Sealed under the vault-meta key domain**, like Stream and Routine ops. A
  preference is configuration of the vault, not content of a stream, and it is
  never shared with another identity.
- **Typed in Rust.** The schema is a Rust table of keys, each with a type, a
  default, a scope and validation. Clients read and write through the core and
  never parse a value themselves. DTOs are generated from the table (C10).
- **Lossless.** A key this build does not know, or a value that fails this
  build's type (a newer build widened it), is preserved byte for byte and
  written back unchanged. Resolution treats it as absent and uses the default
  (ADR-0045).

### 2. Each key declares its scope, and one resolver reads all three

| Scope | Stored in | Synced | Meaning |
|---|---|---|---|
| `vault` | the `Preferences` entity | yes | one value for the whole account |
| `vault_overridable` | the entity, plus an optional device overlay entry | the vault value only | an account default that one device may override |
| `device` | the device overlay only | never | a fact about this install |

```
resolve(key) =
    scope ∈ {device, vault_overridable} and overlay[key] is set  → overlay[key]
    scope ∈ {vault, vault_overridable}  and vault[key] is valid  → vault[key]
    otherwise                                                    → schema default
```

The resolver is a pure Rust function. A write names its target explicitly
(`SetPreference { key, value, target: Vault | Device }`), and a target the
key's scope does not allow is rejected.

**The device overlay** is a table in the device's local encrypted database,
never an op, never synced. Keys that must be readable **before the vault is
unlocked** (the relay URL and the OIDC issuer and client id, which are needed to
find and unlock it) are marked `bootstrap` and live in a small plaintext file
the core manages beside the database. A `bootstrap` key MUST be `device`-scoped
and MUST hold no user content.

The initial key set, with types, defaults and scopes, is normative in
[`preferences.md`](../02-domain/preferences.md), except the notification keys,
which the notification catalog owns
([`../08-features/notifications.md`](../08-features/notifications.md)
§Preference keys). Two defaults are decided here: **`week_start = Sunday`**,
and **`notifications.timezone_changed.enabled = false`**.

### 3. The day schedule is a vault-scoped preference

Optional but recommended. It is specified in
[`day-schedule.md`](../02-domain/day-schedule.md); the decisions are:

- **Levels.** Wake and sleep defaults per **weekday**, optional defaults per
  **day of month** (1–31, and `-1` for the last day), and **per-date
  overrides**. A per-date override may also say "no schedule this day".
- **Precedence** is first match of: date override, then day of month, then
  weekday, then unset. It is a pure function of `(schedule, civil date)`.
- **Civil and zone-less.** Wake and sleep are civil times, resolved in the
  **reader's** zone, so the schedule follows the user when they travel.
- **Sleep may cross midnight.** A sleep time at or before the wake time falls
  on the next civil day. Equal wake and sleep is rejected.
- **It defines the planner day.** Every instant belongs to exactly one planner
  day, `[boundary(d), boundary(d+1))`; the boundary between two days is the
  later of the next day's wake (or civil midnight when unset) and the previous
  day's sleep. The wake-to-sleep span inside it is the day's **plannable
  window**, which is where the planner places work. With no schedule at all,
  both are the civil day. Today, Upcoming, lateness, the briefs
  and the planner all read `day_bounds(date, zone)`, and `civil_span` survives
  only where a civil day is meant.
- **It drives wind-down.** The wind-down notification fires
  `notifications.wind_down.lead_s` (default 3600 s) before the resolved sleep
  time, on the primary notification device, when
  `notifications.wind_down.enabled` is on. It is
  computed in the core and scheduled by the client through the notification
  catalog.

## Alternatives considered

| Option | Why not |
|---|---|
| **Keep settings device-local** | What exists. Fails "configure once" and leaves the core blind to values it needs. |
| **One synced blob of all settings** | A single register: two devices editing different settings lose one edit, which is the full-state LWW failure ADR-0044 removes. |
| **Sync everything, no overlay** | "Which relay does this install talk to?" and "is notification on for this install?" are per-install facts. (Which device is the notification primary is synced, as one `vault` register holding a device id, precisely so that exactly one device can hold it; a per-device boolean would let every device claim it.) |
| **Two scopes only (vault, device)** | Quiet hours and category toggles are usually account-wide and occasionally per device (a work laptop that should be silent at night). Without the overridable scope that case needs a second key per setting. |
| **Settings in platform stores, mirrored into the core** | Two sources of truth per value with a sync between them inside one device. |
| **Day schedule as its own entity** | It is one account-wide value with per-entry merge, which is exactly a map-valued preference. A second entity kind adds a registry entry and nothing else. |
| **Day boundary = sleep time** | Leaves the night between sleep and the next wake belonging to no day, and "Today" at 03:00 would be undefined. Attributing every instant to one day keeps every query total. |

## Consequences

- `Command::SetPreference` and `Command::ClearPreference` (emitting `Patch`
  ops for a vault target), and
  `Query::Preferences` returning resolved values with their source (overlay,
  vault, default), so a settings screen can show "set on this device".
- The Apple `AppSettings` and `NotificationPreferences` become views over the
  core. Existing `UserDefaults` values migrate once into the overlay (for
  `device` keys) or the vault (for `vault` keys, only if the vault has no value
  yet), and the `UserDefaults` keys are then deleted.
- `week_start` is read by the core and every client. The Monday constants go.
- A new feature id, `preferences.entity`, is declared in `vault_requires`
  (ADR-0045).
- #301 becomes a notification-catalog entry over a value the core already
  computes.

## What would force revisiting this

1. **A preference that must differ per stream** (a stream-specific quiet
   window). That is a Stream field, as `reminder_lead_s` already is, and not a
   fourth scope.
2. **A day schedule that is not a daily wake/sleep pair** (shift work that
   rotates on a multi-week cycle). Day-of-month and per-date overrides cover
   rotations poorly; a rule-based level (an `RRule` per window) would be the
   extension.
