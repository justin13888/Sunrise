---
status: accepted
---

# Preferences

The user's settings, as one typed entity per vault plus a device-local overlay.
The decision and its alternatives are
[ADR-0050](../11-adr/0050-preferences-and-day-schedule.md). The core implements
it ([#337](https://github.com/justin13888/Sunrise/issues/337)): the key table,
its codec and the resolver are `crates/sunrise-domain/src/preferences/`
(`PREFERENCE_KEYS`, `resolve`), and the commands, the query, the projection and
the bootstrap file's codec are `crates/sunrise-core/src/engine/preferences.rs`.
§Status says what is not built yet.

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
domain. There is no preference-specific op: the registry entry (`prf_`, tag
`preferences`) has no op family, and its op-log kind is `preferences.patch`.

**Gate.** The entity is the feature `preferences.entity`
([ADR-0045](../11-adr/0045-schema-identity-and-feature-gating.md) §7). A
device's first vault write adds it to `vault_requires`, which is refused with
`FeatureUnsupportedByDevices` while another non-revoked device has not
advertised it, and the seal guard refuses any `prf_` op sealed before the vault
requires it. A build older than the entity cannot decode a `prf_` ref, so this
is what keeps the op from reaching one. ADR-0044 §9's `core.field_ops` gate does
not apply here: it protects entities a build without per-field merge would
overwrite with a full-state op, and this entity has none.

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
`device_preferences(key TEXT PRIMARY KEY, value BLOB)`, the value canonical
CBOR. It is never an op. The synced entity projects to
`preferences(id, values_cbor, …)` (migration `0037_preferences.sql`).

Keys marked **bootstrap** are needed before the vault can be unlocked; they
live instead in a plaintext file the core manages beside the vault databases,
and they MUST be `device`-scoped and MUST NOT hold user content. The file is
`preferences.bootstrap.json` in the directory the client keeps its vaults in
(on Apple, `~/Library/Application Support/Sunrise/`): one JSON object of string
values, written to a sibling file and renamed into place. A key the file holds
that this build does not know is kept on every write; a file that is not a JSON
object reads as empty and is replaced by the next write, so a device pointed at
an unreadable relay configuration stays repairable. It is read and written
without a vault, through the FFI's `bootstrap_preferences` and
`set_bootstrap_preference`, never through a command or `Query::Preferences`.
The core holds only its codec (`BootstrapPreferences`); the file I/O is the
platform layer's, because the core touches no file outside its storage handle
([`../01-architecture/shared-core.md`](../01-architecture/shared-core.md) rule 2).

```
resolve(key) =
    scope ∈ {device, vault_overridable} and overlay has key  → overlay value
    scope ∈ {vault, vault_overridable}  and vault value valid → vault value
    otherwise                                                → default
```

A value that does not decode under its key's type is never read, wherever it
is stored: it resolves as if absent. A `vault` key's overlay value and a
`device` key's vault value are never read either.

`Query::Preferences` returns every key but the bootstrap ones, in table order,
each with its resolved value, its source (`overlay`, `vault`, `default`) and
whether the vault holds a value for it at all. `Command::SetPreference { key,
value, target }` and `Command::ClearPreference { key, target }` name `Vault` or
`Device`, and a target the scope does not permit is rejected with
`VALIDATION_PREFERENCE_SCOPE`. An unknown key, a bootstrap key, or a value that
does not fit its key is rejected with `VALIDATION_FIELD`. With target `Vault`
they emit the `Patch` ops above; with target `Device` they write the overlay and
emit nothing.

A default that differs by device class (`attachments.cache_limit_bytes`) is
read for this device's class, from the platform its certificate names: `ios`,
`ipados` and `android` are phone and tablet, anything else is desktop.

## Initial keys

Types use the CDDL names from [`overview.md`](./overview.md) and
[`../10-cross-cutting/time.md`](../10-cross-cutting/time.md).

### Calendar and time

| Key | Type | Default | Scope | Read by |
|---|---|---|---|---|
| `week_start` | `Weekday` | `"SU"` | `vault` | week views, the planner, reviews |
| `home_timezone` | IANA zone name, or absent | absent | `vault` | dual-zone display when travelling ([`time.md`](../10-cross-cutting/time.md) §7) |
| `time_format` | `"locale"` / `"h12"` / `"h24"` | `"locale"` | `vault_overridable` | every time label |
| `day_schedule` | `DaySchedule` ([`day-schedule.md`](./day-schedule.md)) | empty (unset) | `vault` | planner day, Today, lateness, wind-down. Not in the key table yet: [#338](https://github.com/justin13888/Sunrise/issues/338) adds it with its map entries. |

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
| `search.content_language` | BCP 47 language tag | the creating device's language, written at vault creation; absent until a build that does so writes it (the key table's default is absent) | `vault` | the search tokenizer ([ADR-0052](../11-adr/0052-search-v2.md), [`../08-features/search.md`](../08-features/search.md)) |

### Notifications

Every notification key, including the lead-time floor, quiet hours and the
primary device, is defined in
[`../08-features/notifications.md`](../08-features/notifications.md)
§Preference keys, under the one prefix `notifications.`. That table is the list
of record for their types, defaults and scopes, and this page does not repeat
it. The Rust key table (`PREFERENCE_KEYS`) is written by hand, and a test
(`the_key_table_is_the_documented_one`) holds its keys and scopes equal to this
page's tables and that catalog's.

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

## Value encodings

Each type has exactly one CBOR form, in the entity, in the overlay and on the
wire:

| Type | CBOR |
|---|---|
| `bool` | bool |
| `uint`, `int` (with their ranges) | integer |
| enumerated spellings, URL, `tstr`, IANA zone, BCP 47 tag, `entity-ref`, device id | text; a device id is its `dev_` ref |
| `Weekday` | text, `"MO"` … `"SU"` |
| `{ day, at }` (`review.cadence`) | `{ "at": "HH:MM:SS", "day": Weekday }` |
| `{ start, end }` (quiet hours) | `{ "end": "HH:MM:SS", "start": "HH:MM:SS" }` |

The two `notifications.*.offset_s` keys are `int` in −86 400..86 400, and the
lead times without a stated range are `uint` up to 2³² − 1, which is what the
reminder planner reads them as.

## Merge mapping

| Part | Merge |
|---|---|
| the entity's create | idempotent; concurrent creates of the fixed id merge (ADR-0044 §4) |
| each scalar key | per-field LWW register on `(hlc, device_id, seq)` |
| each entry of a map-valued key | its own LWW register |
| the device overlay | not merged; local only |
| unknown keys | preserved registers, merged like any other |

## Status

Built in the core: the key table, the resolver, the entity and its gate, the
overlay, the bootstrap store, `Command::SetPreference`,
`Command::ClearPreference` and `Query::Preferences`, and their FFI mirrors
(`CoreCommand::SetPreference`, `CoreQuery::Preferences`, `PreferenceItem`).

Not built yet:

- **Clients read and write only the two attachment keys through it.** Apple's
  Settings → Storage writes `attachments.cache_limit_bytes` and
  `attachments.auto_fetch_on_cellular` to this device's overlay
  ([#346](https://github.com/justin13888/Sunrise/issues/346)). The Apple
  `AppSettings` and `NotificationPreferences` still hold their values in
  `UserDefaults`, and the one-time migration out of it has not run. That is
  [#489](https://github.com/justin13888/Sunrise/issues/489).
- **The attachment cache is the only core reader.** It resolves the two
  attachment keys when it enforces its limit and when the fetch drain decides
  what to fetch on cellular (`crates/sunrise-core/src/blob_cache.rs`).
  `week_start` reaches the week views with
  [#336](https://github.com/justin13888/Sunrise/issues/336), the notification
  keys reach the reminder planner with
  [#347](https://github.com/justin13888/Sunrise/issues/347), and `day_schedule`
  is [#338](https://github.com/justin13888/Sunrise/issues/338).
- **No cross-version run covers the entity.** The baseline build the
  cross-version harness drives ([ADR-0057](../11-adr/0057-cross-version-merge-harness.md))
  predates `prf_`, and a vault write is gated until every device supports it,
  so no op of the entity can reach that baseline. The lossless rule is tested
  in process instead, with this build as the older peer for a key and a value
  it cannot read (`crates/sunrise-core/src/engine/tests/preferences.rs`).
