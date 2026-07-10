# 0011 — Datetime library: jiff

**Status:** accepted

## Context

The domain is timezone-heavy. RRULE expansion must be DST-correct in each Routine's own IANA timezone (see [`../02-domain/routines-and-recurrence.md`](../02-domain/routines-and-recurrence.md), [`../08-features/recurrence-engine.md`](../08-features/recurrence-engine.md)). The design deliberately distinguishes three kinds of time:

- **Absolute instants** — `Task.scheduled_at` / `due_at`.
- **Civil wall-clock** — routine anchors and time-of-day scheduling-constraint windows, which mean "9:00 local" regardless of instant.
- **Floating times** — iCal events imported without a TZID ([`../09-integrations/icalendar.md`](../09-integrations/icalendar.md)).

The workspace previously used `chrono` ad hoc and declared an unused `time` dependency in the workspace `Cargo.toml`. No ADR pinned a datetime library, so type usage drifted.

## Decision

Use **`jiff`** as the sole datetime library across all Rust crates. Pin the `0.2.x` line in the workspace `Cargo.toml`.

Rationale:

- **Type distinction maps onto the design.** `jiff::Timestamp` (absolute instant), `jiff::civil::DateTime` (wall-clock), and `jiff::Zoned` (instant + IANA tz) map 1:1 onto our fixed-instant, floating, and tz-aware semantics.
- **DST-correct zoned arithmetic** with explicit gap/fold disambiguation policies — exactly what recurrence expansion needs.
- **Bundled, up-to-date IANA tzdb**, avoiding OS-tzdb drift across platforms.
- **Modern serde integration** serializing RFC 3339 — value-compatible with the formats already in the op CBOR, and clean across FFI/JSON boundaries (bindings, wasm).

## Alternatives considered

| Option | Why rejected |
|---|---|
| `chrono` (status quo) | Weak timezone story; needs the `chrono-tz` sidecar; ambiguity-prone API around DST transitions |
| `time` | No timezone support at all — which is why the declared dependency sat unused |
| Hand-rolled | Correctness risk in DST and disambiguation edge cases; rejected outright |

## Consequences

- Workspace-wide code migration from `chrono` to `jiff` (tracked in [`../implementation/overview.md`](../implementation/overview.md)).
- `chrono` and `time` are removed from workspace dependencies when the migration lands.
- SQLite storage stays epoch-ms integers; no on-disk change.
- Op CBOR stays RFC 3339 strings — value-compatible, so the wire format is unaffected.
- Routine and scheduling-constraint civil types serialize as civil (wall-clock) strings.
