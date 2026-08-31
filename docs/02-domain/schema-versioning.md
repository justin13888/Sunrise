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
- **Doc schema:** forward compat down to `DOC_SCHEMA_FLOOR`. Newer fields on older clients are *preserved* but ignored, and re-emitted verbatim. See [`../10-cross-cutting/protocol-versioning.md` §7](../10-cross-cutting/protocol-versioning.md#7-document-schema-forward-compat) for the precise rules and the `forward-compat/v1-reads-v2.cbor` fixture.

  The rule has three halves and **all three are required**, because two of them
  hold on their own and still lose the field:

  1. **Every entity carries `#[serde(flatten)] unknown: Unknowns`**, so an
     unmodelled key survives decode. The sole exception is `Interruption`,
     whose whole value is its primary key.
  2. **Every entity persists that map**, so the key survives the projection as
     well as the decode. On an entity-level LWW model
     ([ADR-0014](../11-adr/0014-entity-level-lww-merge.md)) this is not
     cosmetic: every op is full-state, so a device that merely reads and
     re-saves an entity emits the truncated version, and that op wins on every
     peer. A struct field with no column is a field that survives exactly one
     transaction.
  3. **`encode_canonical` sorts map keys**, so the preserved field is
     re-emitted in the position its author put it in — without which
     byte-exact re-emission, and therefore the signature, is impossible.

  Persistence is per-entity storage, not one mechanism: `tasks.extra`,
  `blocks.extra` and `attachments.extra` are dedicated `BLOB` columns;
  `review_snapshots.body` holds the whole entity's canonical CBOR and so
  carries its unknowns without a column of its own. The remaining five
  column-projected entities — `Stream`, `Context`, `Routine`, `FocusStart`
  and `FocusEnd` — get their `extra BLOB` from
  `crates/sunrise-storage/migrations/0015_entity_extra_columns.sql`. Naming
  `tasks.extra` alone, as an earlier revision of this line did, describes one
  column and implies a mechanism that was not universal.
- **DB schema:** local-only; runs migrations in-place on first launch of a new version.

## Adding a field

1. Add to the CDDL spec in `02-domain/`.
2. Add a Rust struct field with `#[serde(default)]` and an `Option`/sensible default.
3. Append a migration under `crates/sunrise-storage/migrations/` (for materialized indexes if needed).
4. Nothing to do for round-tripping — provided the entity's `unknown` map has a
   column behind it. That is the universal rule, not a per-entity favour: an
   entity whose reader hardcodes `Unknowns::new()` silently destroys the field
   on the next full-state op it emits. See §Compatibility windows.
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

- **The CDDL specs in `02-domain/` are authoritative again.** All nine were
  re-derived against `crates/sunrise-domain/src/` at `DOC_SCHEMA_V = 4`. An
  earlier revision of this section recorded eight of the nine as drifted;
  every item on that list is closed, and the repairs are listed below so the
  closure can be re-checked rather than taken on trust.

  | Spec | Was | Repair |
  |---|---|---|
  | `scheduling-constraints.md` | matched | Left alone, except the tiebreak key, which ADR-0016 changed to `(hlc, device_id, seq)`. Still pinned by `serde_json_shape_matches_cddl` in `constraint.rs`. |
  | `tasks.md` | drifted | Added `reminder_lead_s`; `estimated_duration` → `estimated_duration_s` (seconds); dropped the `blocked` state and the `blocks_others` field, neither of which is serialized; `deferred_count` is `int`, not `uint`. |
  | `streams.md` | drifted | Added `reminder_lead_s`; deleted the `integrations` map and the `StreamIcon`/`IntegrationConfig` types; `icon` is a free `tstr`; `review_cadence` is required. |
  | `contexts-and-tags.md` | drifted | Deleted the `color` field and `ContextColor`. |
  | `routines-and-recurrence.md` | drifted | `rrule` is the structured `RRule` map, with `Frequency`/`Weekday`; added all six streak/forgiveness fields and `skipped_keys`; `estimated_duration_s` on `TaskTemplate`. |
  | `time-blocks.md` | drifted | CDDL block now carries only the ten modelled fields; `stream_id` required; no `timezone`; the eight unmodelled fields moved to a separate, explicitly-not-on-the-wire block. |
  | `attachments.md` | drifted | Same split: four thumbnail fields moved out of the live block. `parent` narrowed to Task, which is what validation enforces. |
  | `notes.md` | drifted | `NoteBody` is declared as the `bstr` it is, with the block grammar relabelled a renderer contract; the `Note` entity gained a CDDL block. |
  | `people-and-sharing.md` | drifted | `linked_identity` → `identity_id`; deleted `handle`/`avatar`/`notes`/`contact_methods` and `ContactMethod`. |

  The global drift is closed too: every persisted entity's CDDL block now
  declares `unknown-fields`, defined once in
  [`overview.md` §Common CDDL types](./overview.md#common-cddl-types) along
  with `entity-ref`, `timestamp` and the civil types. `Interruption` correctly
  declares none, and neither do the two value types (`SchedulingConstraint`,
  `stime`).

  Two conventions changed in the process, both because the old spelling was
  wrong rather than merely terse. `tdate` became `timestamp`: `tdate` is the
  prelude's *tagged* date and nothing in the domain emits a CBOR tag. And
  `[A-Z0-9]{26}` became `[0-9A-HJKMNP-TV-Z]{26}`: ULIDs are Crockford base32,
  which excludes `I`, `L`, `O` and `U`, so the old character class matched ids
  that cannot exist.

  **What remains unpinned rather than undrifted.** Only
  `scheduling-constraints.md` has a test that fails when the Rust and the CDDL
  disagree. The other eight are now correct and will stay correct only as long
  as someone re-reads them. Extending the `serde_json_shape_matches_cddl`
  pattern is the fix — it is one `to_value` and a handful of `assert_eq!` per
  entity, and it catches exactly the class of drift this section spent two
  revisions describing. It is not done here because this stream owns `docs/`
  and not `crates/`; it is worth a follow-up issue.

  Version 1 differed from 2 only in the five `SunriseTime` fields
  ([ADR-0017](../11-adr/0017-sunrise-time-representation.md)), which were bare
  instants and still decode as `instant`. Versions 3 and 4 added op families and
  changed the delete ops to full-state
  ([ADR-0014](../11-adr/0014-entity-level-lww-merge.md)); neither is reflected
  in the CDDL — they are op-family and op-shape changes, not entity-field
  changes, so the entity blocks above are unaffected by them.
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

Encryption is at the **op envelope** level, not the field level. The entire op (any field set, any payload) is encrypted as one unit under the Stream key. There are no field-level encryption sub-keys; "indexable plaintext metadata" does not exist on the server. See [`../03-crypto/data-encryption-format.md`](../03-crypto/data-encryption-format.md) for the envelope structure. Schema additions therefore never expand the server's ciphertext-visibility surface.
