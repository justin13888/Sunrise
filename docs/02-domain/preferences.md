---
status: accepted
---

# Preferences

The user's settings, as one typed entity per vault plus a device-local overlay.
The decision and its alternatives are
[ADR-0050](../11-adr/0050-preferences-and-day-schedule.md). Implementation is
tracked in [#337](https://github.com/justin13888/Sunrise/issues/337).

## Shape

```cddl
; One per vault. The id is fixed: "prf_" followed by the zero ULID body, so every
; device writes to the same entity.
Preferences = {
    id:         "prf_00000000000000000000000000",
    updated_at: timestamp,
    values:     { * pref-key => any },   ; one per-field LWW register per key (ADR-0044)
    unknown-fields,
}

; The first segment is a lowercase name; later segments may also hold upper
; case, digits and "-", so map entries such as day_schedule.weekday.MO,
; day_schedule.date.2026-12-25 and day_schedule.month_day.-1 are keys.
pref-key = tstr .regexp "[a-z][a-z0-9_]*(\\.[A-Za-z0-9_-]+)*"
```

**Ops.** Preferences use the `Patch` op family of
[ADR-0044](../11-adr/0044-per-field-ops.md), sealed under the vault-meta key
domain. There is no preference-specific op.

- **Create, idempotently.** A device's first write, while it has not seen the
  entity's create, is a `Patch` with `"create": true` under the fixed
  `prf_…` id, carrying only the keys it sets. Two devices that both create it
  merge (ADR-0044 §4, singleton rule), so no device needs to know whether
  another already made it.
- **Then patch.** Every later write is a plain `Patch` whose `values` field is
  a `map-op`: one `set` per key written.
- **Clear** is a `Patch` that sets the key to `null`, which unsets it; the key
  then resolves to the schema default.

```
Patch { ref: prf_…, fields: { "values": { "map": { "week_start": { "set": "MO" } } } } }
Patch { ref: prf_…, fields: { "values": { "map": { "week_start": { "set": null } } } } }   ; clear
```

- **Map-valued keys are maps of registers.** A key whose type is a map (the day
  schedule's per-weekday, per-month-day and per-date entries) is written one
  entry at a time with a dotted key, for example `day_schedule.date.2026-12-25`.
  Each entry is its own register, so two devices editing two entries both keep
  their edit.
- **Lossless.** A key this build does not know, or a value whose type this build
  cannot read, is kept byte for byte and re-emitted unchanged. The resolver
  treats it as absent.

## Scope and resolution

Every key declares one scope:

| Scope | Where the value lives | Synced |
|---|---|---|
| `vault` | the entity | yes |
| `vault_overridable` | the entity, and optionally the device overlay | the vault value only |
| `device` | the device overlay only | never |

The **device overlay** is a table in the local encrypted database:
`device_preferences(key TEXT PRIMARY KEY, value BLOB)`. It is never an op.
Keys marked **bootstrap** are needed before the vault can be unlocked; they
live instead in a plaintext file the core manages beside the database, and they
MUST be `device`-scoped and MUST NOT hold user content.

```
resolve(key) =
    scope ∈ {device, vault_overridable} and overlay has key  → overlay value
    scope ∈ {vault, vault_overridable}  and vault value valid → vault value
    otherwise                                                → default
```

`Query::Preferences` returns each resolved value with its source (`overlay`,
`vault`, `default`). `Command::SetPreference { key, value, target }` and
`Command::ClearPreference { key, target }` name `Vault` or `Device`, and a target
the scope does not permit is rejected with `VALIDATION_PREFERENCE_SCOPE`. With
target `Vault` they emit the `Patch` ops above; with target `Device` they write
the overlay.

## Initial keys

Types use the CDDL names from [`overview.md`](./overview.md) and
[`../10-cross-cutting/time.md`](../10-cross-cutting/time.md).

### Calendar and time

| Key | Type | Default | Scope | Read by |
|---|---|---|---|---|
| `week_start` | `Weekday` | `"SU"` | `vault` | week views, the planner, reviews |
| `home_timezone` | IANA zone name, or absent | absent | `vault` | dual-zone display when travelling ([`time.md`](../10-cross-cutting/time.md) §7) |
| `time_format` | `"locale"` / `"h12"` / `"h24"` | `"locale"` | `vault_overridable` | every time label |
| `day_schedule` | `DaySchedule` ([`day-schedule.md`](./day-schedule.md)) | empty (unset) | `vault` | planner day, Today, lateness, wind-down |

### Tasks, triage and reviews

| Key | Type | Default | Scope | Read by |
|---|---|---|---|---|
| `stale_after_days` | `uint` 0..365 | `14` | `vault` | lateness; `0` disables staleness ([ADR-0047](../11-adr/0047-deadlines-and-lateness.md)) |
| `capture_default_stream` | `entity-ref` (`str_`) or absent | absent (no stream) | `vault` | capture with no `#stream` |
| `review.cadence` | `{ day: Weekday, at: civil-time }` | `{ day: "FR", at: "16:00:00" }` | `vault` | the weekly review and its notification ([`../08-features/reviews-and-stats.md`](../08-features/reviews-and-stats.md)) |

### Planner and views

| Key | Type | Default | Scope | Read by |
|---|---|---|---|---|
| `planner.snap_s` | `uint` 60..3600 | `900` | `vault` | the planner's grid snap ([`../08-features/planner.md`](../08-features/planner.md) §Inputs) |
| `planner.default_task_duration_s` | `uint` 60..86400 | `1800` | `vault` | the planner, for a task with no estimate |
| `planner.min_gap_s` | `uint` 0..3600 | `0` | `vault` | the planner's gap between placed items |
| `views.upcoming.span_days` | `7` / `14` / `30` | `7` | `device` | Upcoming ([`../08-features/planning-views.md`](../08-features/planning-views.md)) |
| `search.content_language` | BCP 47 language tag | the creating device's language, written at vault creation | `vault` | the search tokenizer ([ADR-0052](../11-adr/0052-search-v2.md), [`../08-features/search.md`](../08-features/search.md)) |

### Notifications

Every notification key, including the lead-time floor, quiet hours and the
primary device, is defined in
[`../08-features/notifications.md`](../08-features/notifications.md)
§Preference keys, under the one prefix `notifications.`. That table is the list
of record for their types, defaults and scopes, and this page does not repeat
it; the Rust key table is generated from the catalog.

### Device and connection

| Key | Type | Default | Scope | Bootstrap | Read by |
|---|---|---|---|---|---|
| `sync.relay_url` | URL string | absent | `device` | yes | sync |
| `auth.oidc_issuer` | URL string | absent | `device` | yes | sign-in |
| `auth.oidc_client_id` | `tstr` | absent | `device` | yes | sign-in |
| `attachments.auto_fetch_on_cellular` | `bool` | `false` | `device` | no | blob fetch ([ADR-0053](../11-adr/0053-attachment-thumbnails-and-native-rendering.md) §6) |
| `attachments.cache_limit_bytes` | `uint`, bytes, 100 000 000..50 000 000 000 | 1 000 000 000 (1 GB) on desktop, 200 000 000 (200 MB) on phone and tablet | `device` | no | the blob LRU cache ([ADR-0053](../11-adr/0053-attachment-thumbnails-and-native-rendering.md) §5) |
| `keyboard.vim_mode` | `bool` | `false` | `device` | no | list navigation |

`account.email` is not a preference: it is a property of the signed-in
identity and is read from it.

## Validation

- A value MUST match its key's type and range. An out-of-range write is
  rejected; an out-of-range value from a peer is preserved and resolves to the
  default.
- `home_timezone` MUST be a zone name the bundled tzdb resolves at write time.
- A `bootstrap` key MUST be `device`-scoped.
- Per-key rules for notification keys are in
  [`../08-features/notifications.md`](../08-features/notifications.md)
  §Preference keys.

## Merge mapping

| Part | Merge |
|---|---|
| the entity's create | idempotent; concurrent creates of the fixed id merge (ADR-0044 §4) |
| each scalar key | per-field LWW register on `(hlc, device_id, seq)` |
| each entry of a map-valued key | its own LWW register |
| the device overlay | not merged; local only |
| unknown keys | preserved registers, merged like any other |
