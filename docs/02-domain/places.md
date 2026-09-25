---
status: accepted
---

# Place

A named location the user entered, which a task can require. The decision, the
privacy argument and the alternatives are
[ADR-0051](../11-adr/0051-places.md). Implementation is tracked in [#339](https://github.com/justin13888/Sunrise/issues/339).

## Fields

Shared types are defined in
[`overview.md` §Common CDDL types](./overview.md#common-cddl-types).

```cddl
Place = {
    id:             tstr .regexp "plc_[0-9A-HJKMNP-TV-Z]{26}",
    created_at:     timestamp,
    updated_at:     timestamp,
    name:           text<128>,          ; required, 1..128 chars after trim
    lat_e7:         int,                ; degrees × 10^7, -900000000..900000000
    lon_e7:         int,                ; degrees × 10^7, -1800000000..1800000000
    radius_m:       uint,               ; 50..5000, default 150
    address_label?: text<256>,          ; display text only; never geocoded by Sunrise
    deleted:        bool,
    unknown-fields,
}
```

- **Integers, not floats.** A float has several CBOR encodings, and a signed op
  must have one. 10⁻⁷ degrees is about a centimetre.
- **`radius_m`** is bounded below by what OS region monitoring can resolve
  reliably and above by what still means "here".
- **`address_label`** is whatever the user typed or picked. Sunrise never
  reverse-geocodes a Place, because doing so would send its coordinates to a
  geocoding service.

**Key domain.** Place ops are sealed under the vault-meta key domain, like
Stream and Routine ops ([ADR-0046](../11-adr/0046-optional-stream.md) §2). Every
field is end-to-end encrypted, and the relay sees ciphertext only.

## Commands

`CreatePlace`, `UpdatePlace` (any subset of `name`, coordinates, `radius_m`,
`address_label`) and `DeletePlace`. A Place is created only by an explicit user
act: picking a point on a map, choosing a search result, or tapping "use my
current location" once. The core never creates or moves a Place from a location
reading.

## Requirement

A task or routine requires a place through the `at_place` dimension of a
`SchedulingConstraint` ([`scheduling-constraints.md`](./scheduling-constraints.md)):
one or more `plc_` refs, any of which satisfies it, with `hard` or `soft`
severity.

## Presence (device-local, memory only)

```rust
fn on_place_presence(&self, place: PlaceId, present: bool);   // called by the client
fn evaluate_place(req: &[PlaceId], presence: &Presence) -> PlaceEval;

enum PlaceEval { Satisfied, Violated, Unknown }
```

- Presence is a `Map<PlaceId, bool>` held **in memory only**. It is not written
  to the op log, to an envelope, to the local database, to a log line or to any
  file. It is rebuilt at launch from the OS's current region state.
- `evaluate_place` is `Satisfied` when any required place is present,
  `Violated` when every required place is known absent, and `Unknown` otherwise.
  A place that is deleted, not monitored, or unreadable (a shared stream's task
  naming a Place from another identity's vault) is unknown.
- `Unknown` is never displayed as a violation. With location permission denied,
  every place requirement is unknown and the task behaves as if it had none.

## Region selection

The OS caps monitored regions (20 per app on iOS). The core decides which Places
the client registers:

```rust
fn select_regions(places: &[Place], open_tasks: &[TaskPlaceReq],
                  approx_position: Option<(i32, i32)>, cap: usize) -> Vec<PlaceId>
```

Ranked by: places with a `hard` requirement on an open task; then the count of
open tasks requiring the place, descending; then distance from
`approx_position`, ascending, when given; then id. One slot below the cap is
left for the client's re-selection trigger. `approx_position` is an argument,
held for the call and never stored. The function is pure and table-tested.

## Views and notifications

- **"Here now"** lists open tasks whose place requirement is `Satisfied`.
- **Arrival.** A region enter event may post a local `place_arrival`
  notification for open tasks requiring that place, when
  `notifications.place_arrival.enabled` is on (off by default, device-scoped;
  [`../08-features/notifications.md`](../08-features/notifications.md) #18).
- The planner never places, moves or refuses work by Place; a preview may
  annotate a slot at `Info` and never higher
  ([ADR-0051](../11-adr/0051-places.md) §4,
  [`../08-features/planner.md`](../08-features/planner.md) §Places). No write is
  ever rejected for a place requirement.

## Validation

- `name` MUST be non-empty after trim.
- `lat_e7` and `lon_e7` MUST be in range; `radius_m` MUST be in `50..=5000`.
- A requirement MAY name a deleted Place. It evaluates `Unknown` for that ref.

## Deletion

Deleting a Place tombstones it and nothing else. A task that still names it
keeps the ref, and the ref evaluates `Unknown`, so a task never becomes "not
actionable here" because a Place disappeared. Clients show the ref as
"deleted place" with an action to remove it.

## Merge mapping

| Field | Merge |
|---|---|
| `name`, `lat_e7`, `lon_e7`, `radius_m`, `address_label` | per-field LWW register (ADR-0044) |
| `deleted` | tombstone, per ADR-0044's delete rule |
| presence | never merged; never leaves the device |
