# 0051 — Places are a synced, end-to-end encrypted entity, and presence is evaluated on the device and never stored

**Status:** accepted

**Amends** [`../02-domain/scheduling-constraints.md`](../02-domain/scheduling-constraints.md):
the requirement model gains an `at_place` dimension.

**Depends on** [ADR-0044](./0044-per-field-ops.md) (per-field registers).
**Used by** [ADR-0048](./0048-interactive-planner.md), whose solver never
places, moves or refuses work by Place (§4 below;
[`../08-features/planner.md`](../08-features/planner.md) §Places).

**Specified in** [`../02-domain/places.md`](../02-domain/places.md). **Tracked
by** [#339](https://github.com/justin13888/Sunrise/issues/339).

## Context

A task cannot say where it can be done. The requirement model has three
dimensions, all of them about time
(`crates/sunrise-domain/src/constraint.rs#ScheduleConstraint`). "Buy stamps
when I'm near the post office" and "only at the office" are inexpressible.
Nothing in the tree models a location: there is no `Place` entity kind, and no
crate or app references `CLLocationManager` or `CLRegion`. The contexts spec
suggests location-named contexts like `@office`
([`../02-domain/contexts-and-tags.md`](../02-domain/contexts-and-tags.md)), but
a context is a tag the user switches by hand, not a place the device can detect.

Location is also the most sensitive data a task manager could hold. The design
constraint is not only that the relay must not see it, which end-to-end
encryption already gives, but that **a history of where the user has been must
not exist anywhere**, including on the user's own devices and in the synced
vault.

## Decision

### 1. `Place` is a user-entered, synced entity

```cddl
Place = {
    id:             tstr .regexp "plc_[0-9A-HJKMNP-TV-Z]{26}",
    created_at:     timestamp,
    updated_at:     timestamp,
    name:           text<128>,
    lat_e7:         int,              ; latitude  × 10^7, -900000000..900000000
    lon_e7:         int,              ; longitude × 10^7, -1800000000..1800000000
    radius_m:       uint,             ; 50..5000; default 150
    address_label?: text<256>,        ; free text the user typed or picked; never geocoded by Sunrise
    deleted:        bool,
    unknown-fields,
}
```

- **Fixed-point coordinates.** Degrees × 10⁷ as integers (about 1 cm), not
  floats. Canonical CBOR has several encodings of one float, and a signed op
  must encode one value one way.
- **Every field is end-to-end encrypted.** Place ops are sealed under the
  vault-meta key domain like every other vault-level entity, so the relay sees
  ciphertext and a size.
- **Per-field registers.** Renaming a place on one device and moving its pin
  on another both survive.
- **User-entered only.** A Place is created by the user choosing a point (by
  map pick, by search, or by "use current location" once, as an explicit act).
  The core never creates or moves a Place from a location reading.

### 2. A task can require a place

A new requirement dimension, `at_place: [+ entity-ref]` (one or more `plc_`
refs, satisfied by any of them), joins time of day, days of week and date range
in `SchedulingConstraint`, with the same `hard`/`soft` severity
([`scheduling-constraints.md`](../02-domain/scheduling-constraints.md)).

### 3. Presence is evaluated on the device, in memory, and never written

- **Opt-in.** A client registers regions only after the user grants location
  permission in response to an explicit action ("Remind me when I'm here").
  Denying or revoking permission is a supported state, not an error.
- **The OS does the geometry.** Apple clients register one
  `CLCircularRegion` per selected Place (§5) with `CLLocationManager` region
  monitoring. Other platforms use their equivalent (Android geofencing).
- **One entry point.** Region enter, exit and state callbacks call
  `on_place_presence(place_id, present: bool)` on the core.
- **Presence is memory only.** The core holds `presence: Map<PlaceId, bool>`
  for the life of the process. It is **not** written to the op log, to any
  envelope, to the local database or to any file, and it is rebuilt at launch
  by asking the OS for the current state of each monitored region
  (`requestState(for:)`). A test asserts that no table and no op contains a
  presence value, and that a restart with no OS callbacks yields `unknown`
  for every place.
- **Location never leaves the device**, not as coordinates and not as "at
  place X". No notification, log line, metric, crash report or telemetry event
  carries a place id together with a presence value.
- **Evaluation is pure.** `evaluate_place(requirement, presence) -> Satisfied |
  Violated | Unknown` is a Rust function of its arguments. A place that is not
  monitored, a denied permission, or a place deleted since the requirement was
  written, all give `Unknown`, which is never shown as a violation.

### 4. Place requirements annotate; they do not schedule

A place requirement is about *where the user is*, which is unknown for any time
but now. So:

- `at_place` is **never used to place, move or refuse work**: the planner
  (ADR-0048) does not read it when it chooses positions, and no write is
  rejected for it. A planner preview **may annotate** a slot with
  `PlaceUnknown` or `PlaceMismatch` at `Info` severity, and never higher;
- views may filter to "here now" (every open task whose place requirement
  evaluates `Satisfied`);
- a region **enter** event may fire a local notification for open tasks that
  require that place (the `place_arrival` kind, off by default, in the
  [notification catalog](../08-features/notifications.md));
- a `hard` place requirement evaluating `Violated` marks the task "not
  actionable here" in views. It never blocks a write.

### 5. Region selection respects the OS cap, deterministically

iOS allows 20 monitored regions per app, and macOS enforces a similar limit.
The core chooses which Places to register:

```
select_regions(places, open_tasks, approx_position: Option<(lat_e7, lon_e7)>, cap) -> Vec<PlaceId>
```

ranked by, in order:

1. places required by an open task with a `hard` place requirement;
2. number of open tasks requiring the place, descending;
3. distance from `approx_position`, ascending, when one is supplied;
4. place id, ascending.

One slot is reserved for the client's own re-evaluation trigger (a
significant-location-change subscription or a region around the last
selection point), so selection is re-run when the user moves far enough for
the nearest set to change. `approx_position` is passed in memory for the
duration of the call and is never stored. The function is pure and tested
with a fixed input table.

## Alternatives considered

| Option | Why not |
|---|---|
| **Location contexts (`@office`) the user toggles** | What exists. Works, and asks the user to do the one thing the device can do better. It stays available for people who decline location permission. |
| **Store presence in the local database** | Makes a location history exist on disk, which is the thing §3 forbids. The OS already knows the current state of each region and can be asked on launch. |
| **Sync presence so other devices know "I'm at the office"** | Useful for a phone-to-laptop handoff, and it writes the user's movements into the vault on every device. Rejected on the privacy constraint. |
| **Evaluate geofences in the core from raw GPS** | Continuous location access, battery cost, and the core holding coordinates. The OS does this better and the core never needs to see a fix. |
| **Floating-point coordinates** | Non-canonical encodings under a signature (see §1). |
| **Place as a property on Context** | A context is a many-to-many tag with no geometry; a place has geometry and no tagging semantics. Merging them makes both worse. |

## Consequences

- A new entity kind with prefix `plc_`, CRUD commands, and a new feature id
  `place.entity` in `vault_requires` (ADR-0045).
- `SchedulingConstraint` gains `at_place`, preserved by older builds through
  the nested unknown-field map (C4).
- The Apple clients gain a location-permission flow and a region manager that
  does nothing until the user opts in.
- Imported calendar events' `LOCATION` stays free text on `Block.location`
  ([`time-blocks.md`](../02-domain/time-blocks.md)). It is not matched to
  Places automatically.
- A shared stream's task that requires a Place the recipient cannot read (the
  Place lives in the owner's vault-meta domain) evaluates `Unknown` for the
  recipient.

## What would force revisiting this

1. **Place-aware planning** ("schedule errands when I'm usually in town"). That
   needs a model of where the user tends to be, which is a location history by
   another name, and this ADR forbids it.
2. **More places than the OS can monitor, all equally relevant.** Selection
   would need a smarter trigger than distance, or a server-side service, which
   the privacy constraint rules out.
