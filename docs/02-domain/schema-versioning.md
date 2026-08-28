---
status: accepted
---

# Schema Versioning

Domain entities evolve. Devices on different versions must keep syncing.

## Three layers, three versions

| Layer | Constant | Bump on |
|---|---|---|
| **Wire protocol** | `WIRE_PROTO_V` | Sync protocol changes (new message kinds, framing) |
| **Envelope container format** | `ENVELOPE_FORMAT_V` | The `OpEnvelope` field layout, canonical ordering, AAD, or signature input |
| **Document schema** | `DOC_SCHEMA_V` | Entity/field additions or removals |
| **Local DB schema** | `STORAGE_V` | SQLite tables/indexes for materialized views |

These are independent. Adding a new task field bumps `DOC_SCHEMA_V` only. Switching from WebSocket to QUIC bumps `WIRE_PROTO_V` only.

The envelope container is versioned **separately from the schema it carries**
([ADR-0015](../11-adr/0015-envelope-doc-schema-split.md)), and the two have
opposite failure rules: a container mismatch is a hard reject, because the
reader cannot find the payload, while a newer document schema is *accepted*,
because the reader can find, authenticate and decrypt the payload and merely may
not understand every field inside it. Before the split both roles were played by
one number, so bumping the schema to add a field changed the magic prefix and
made every already-signed envelope undecodable — the exact opposite of the
"infinite forward compat" promised below.

`DOC_SCHEMA_FLOOR` is the lowest schema this build can still interpret. It moves
only when a shape stops being *readable*, never merely because a newer one
exists.

## Compatibility windows

- **Wire protocol:** N and N-1 must interoperate. N-2 is rejected with a clear "please update your client" error.
- **Doc schema:** forward compat down to `DOC_SCHEMA_FLOOR`. Newer fields on older clients are *preserved* but ignored, and re-emitted verbatim. This is implemented, not aspirational: every entity carries `#[serde(flatten)] unknown: Unknowns`, `tasks.extra` persists it, and `encode_canonical` sorts map keys so the preserved field is re-emitted in the position its author put it in — without which byte-exact re-emission is impossible. See [`../10-cross-cutting/protocol-versioning.md` §7](../10-cross-cutting/protocol-versioning.md#7-document-schema-forward-compat) for the precise rules and the `forward-compat/v1-reads-v2.cbor` fixture.
- **DB schema:** local-only; runs migrations in-place on first launch of a new version.

## Adding a field

1. Add to the CDDL spec in `02-domain/`.
2. Add a Rust struct field with `#[serde(default)]` and an `Option`/sensible default.
3. Append a migration under `crates/sunrise-storage/migrations/` (for materialized indexes if needed).
4. Nothing to do for round-tripping: the `unknown` map already preserves the field on every older client.
5. Bump `DOC_SCHEMA_V`. Leave `DOC_SCHEMA_FLOOR` alone — the older shape is still readable.

## Removing a field

1. Mark deprecated in CDDL **and on the Rust field** with a comment and a deadline (≥2 minor versions). `Routine.skip_dates` is the worked example: deprecated in favour of `skipped_keys`, stage A complete, removal dated at `DOC_SCHEMA_FLOOR = 3` — the **floor**, not the current version. Stage B is "stop writing", and a field may only stop being written once no reader below the floor can still need it; `DOC_SCHEMA_V` moving past 3 says nothing about that.
2. Stop reading. Continue writing for the deprecation window so older clients that need it still get it.
3. After the window, stop writing. Older clients fall back to the field's default.
4. Bump `doc_schema_version`.

## Renaming a field

Treated as add + remove. Don't reuse names.

## Breaking changes

Avoid. If unavoidable:

- Coordinate a flag-day version that both halves understand for a transition period.
- Provide an automatic migration path on the user's data.
- Coordinate a managed-cloud release that gates older clients to read-only until they update.

## What this means for the v1 launch

- The CDDL specs in `02-domain/` are the authoritative shape at `DOC_SCHEMA_V = 2`.
  Version 1 differed only in the five `SunriseTime` fields
  ([ADR-0017](../11-adr/0017-sunrise-time-representation.md)), which were bare
  instants and still decode as `instant`.
- We expect rapid iteration in the first 6 months. Therefore, and these are
  implemented rather than planned:
  - Every entity carries an `unknown` map (`#[serde(flatten)]`) that preserves
    and re-emits fields it does not model. The one exception is `Interruption`,
    whose whole value is its primary key.
  - Every enum that rides the wire has an unknown fallback, so a future
    `state: "delegated"` degrades instead of rejecting the op it arrived in.
    The fallback is chosen to be the SAFE reading — an unknown task state is
    `todo`, never `done`; an unknown constraint severity is `soft`, never
    `hard` — because rejecting one field's value rejects the whole op, and two
    replicas would then diverge permanently over one string. `RRule`'s
    `Frequency` and `Weekday` are deliberately excluded: silently recurring on
    the wrong schedule is worse than failing the routine.

## Encryption granularity

Encryption is at the **op envelope** level, not the field level. The entire CRDT op (any field set, any payload) is encrypted as one unit under the Stream key. There are no field-level encryption sub-keys; "indexable plaintext metadata" does not exist on the server. See [`../03-crypto/data-encryption-format.md`](../03-crypto/data-encryption-format.md) for the envelope structure. Schema additions therefore never expand the server's ciphertext-visibility surface.
