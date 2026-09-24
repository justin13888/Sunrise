---
status: accepted
---

# Schema Versioning

> **Amended by [ADR-0044](../11-adr/0044-per-field-ops.md) and
> [ADR-0045](../11-adr/0045-schema-identity-and-feature-gating.md).** Entity
> ops write individual fields, and each field merges by its declared CRDT
> type. A schema version has a fingerprint. Nothing that verifies is dropped.
> A vault declares the features it requires. The per-device hash chain and
> state digest in [ADR-0043](../11-adr/0043-commit-tree.md) are **proposed**,
> and nothing here depends on them.
>
> **Rules marked *Today* describe the tree before those ADRs are built.** Each
> one names the issue that closes the gap.

Domain entities evolve, and devices on different builds have to keep syncing
the same vault. Every rule on this page serves one invariant:

> **Merging vaults across client versions MUST NEVER break and MUST NEVER lose
> data.**

"Never break" means an older build never errors out, never stops syncing, and
never refuses a vault because a newer build has written to it. "Never lose"
means every field a writer set is still in the merged state of every replica,
unless a later concurrent write to *that same field* won under its CRDT rule.

## Versioned layers

| Layer | Constant | Bump on |
|---|---|---|
| **Wire protocol** | `WIRE_PROTO_V` | Sync protocol changes: new message kinds, framing |
| **Envelope container format** | `ENVELOPE_FORMAT_V` (floor `ENVELOPE_FORMAT_FLOOR`) | A change to the `OpEnvelope` field layout, canonical ordering, AAD or signature input. An additive field does not bump it ([ADR-0045](../11-adr/0045-schema-identity-and-feature-gating.md) §5). |
| **Document schema** | `DOC_SCHEMA_V` (floor `DOC_SCHEMA_FLOOR`, identity = fingerprint) | Any change to an entity, a nested struct, an enum's variant set, an op kind, a field's CRDT type, or the feature registry |
| **Local DB schema** | `STORAGE_V` | SQLite tables and indexes for the materialized projection |

The layers are independent. Adding a task field bumps `DOC_SCHEMA_V` only.
Switching transports bumps `WIRE_PROTO_V` only. The full table of constants,
and the rules for each, is
[`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md).

Every layer is a monotonic `u16`. Semantic versioning and date versions are
rejected, for the reasons in
[ADR-0045](../11-adr/0045-schema-identity-and-feature-gating.md) §1. The
product version (`0.MINOR.PATCH`, per [ADR-0042](../11-adr/0042-v0-forever.md))
carries no compatibility meaning and MUST NOT be consulted.

The envelope container is versioned **separately from the schema it carries**
([ADR-0015](../11-adr/0015-envelope-doc-schema-split.md)), and the two have
opposite failure rules:

- **A container this reader cannot implement is refused**, because the reader
  cannot find the payload.
- **A newer document schema is accepted**, because the reader can find,
  authenticate and decrypt the payload. It merely may not understand every
  part of it.

`DOC_SCHEMA_FLOOR` is the lowest schema this build can still interpret. It
moves only when a shape stops being *readable*, never merely because a newer
one exists. Every op ever written stays in logs and on relays and remains the
source of truth for a rebuild. So the floor stays at `1`, and raising it needs
its own ADR showing that no live vault or snapshot still holds an op below the
new floor.

## Schema identity: the fingerprint

A `DOC_SCHEMA_V` names exactly one schema. The canonical schema covers:

- every entity, nested struct and enum, with its variant set
- every op kind
- every field, with its name, value type, CRDT type and default
- every feature id

It is generated from the entity registry ([#328](https://github.com/justin13888/Sunrise/issues/328)), not written by hand. It is
committed under `schemas/doc-schema/`. Its fingerprint is:

```
BLAKE3::derive_key("sunrise.doc_schema.fingerprint.v1", JCS(schema))
```

- **A committed registry** maps every shipped version to its fingerprint, and
  it is append-only.
- **A test fails** when the generated schema's fingerprint differs from the
  registry entry for the build's `DOC_SCHEMA_V`. Changing a shape without a
  bump therefore cannot pass CI.
- **Every envelope binds the fingerprint.** It carries
  `(doc_schema_v, first 8 bytes of the fingerprint)` in fields 12 and 13, and
  both fields are covered by the signature and the AEAD associated data.
- **A receiver compares them against its registry.** A known version whose
  fingerprint disagrees is parked, not applied.

The details are in
[ADR-0045](../11-adr/0045-schema-identity-and-feature-gating.md) §2–§3.

*Today:* there is no canonical schema, no fingerprint and no field 13. The
integer is bound under the signature but means only what the doc comment on
`DOC_SCHEMA_V` in `crates/sunrise-cbor/src/version.rs` says ([#323](https://github.com/justin13888/Sunrise/issues/323)).

## How fields merge

Each field has a CRDT type, declared in the entity registry
([ADR-0044](../11-adr/0044-per-field-ops.md) §3):

| CRDT type | Used for | Merge |
|---|---|---|
| LWW register | scalars, optionals, and every nested value type as one value | the greatest `(hlc, device_id, seq)` wins, field by field; the register also records whether that write was by generation or by the user |
| Map | map-valued fields such as `Preferences.values` | one LWW register per key; the key set grows, and a key is removed by a tombstone |
| OR-set | `Task.contexts`, `Task.blocked_by`, `Block.tasks`, `Routine.skipped_keys`, `Routine.streak_keys` (`Task.blocks` is derived from `Block.tasks`) | add-wins, observed-remove |
| PN-counter | `Task.deferred_count` | sum of deltas |

- **An op carries only the fields its command wrote.** It is a `Patch` of
  field ops. Invariants that relate two fields or two entities are evaluated
  at read time and never reject a merged op (ADR-0044 §6).
- **Full-state ops already in logs stay readable forever**, as writes to every
  field they carry (ADR-0044 §7).
- **A field's CRDT type never changes.** Changing it is a new field under a new
  name.

*Today:* every entity merges by entity-level LWW over full-state ops
([ADR-0014](../11-adr/0014-entity-level-lww-merge.md), superseded). A
concurrent edit to a different field, a set addition or a counter increment
can be lost ([#319](https://github.com/justin13888/Sunrise/issues/319)).

## Compatibility rules

- **Wire protocol.** N and N−1 must interoperate. N−2 is refused with a clear
  "please update" error.
- **Document schema: nothing that verifies is dropped.** An envelope that
  passes the signature check and the AEAD open is either applied or
  **parked**. A parked op is stored durably in `ops`, has no TTL, advances the
  cursor, and is replayed through the full apply path after an upgrade. The
  reasons to park are:
  - an unknown op kind
  - an unknown field-op kind
  - a payload this build cannot decode at a newer `doc_schema_v`
  - a schema-fingerprint mismatch

  See [ADR-0045](../11-adr/0045-schema-identity-and-feature-gating.md) §4.
  *Today:* an unknown op kind is reported as `RemoteOpInvalid`, classed as
  corruption and dropped ([#320](https://github.com/justin13888/Sunrise/issues/320)).
- **Document schema: unknowns are lossless at every level.** Three rules, all
  required:
  1. **Every struct that crosses the wire keeps an unknown map.** That means
     every entity, every nested value type, and the `Patch` op's own map. Each
     carries `#[serde(flatten)] unknown: Unknowns`, and re-emits the map
     byte-exact through `encode_canonical`'s sorted keys. The sole exception is
     `Interruption`, whose whole value is its primary key.
  2. **Every enum that crosses the wire or storage keeps an `Unknown(raw)`
     arm.** Logic reads it as the named safe fallback: an unknown task state is
     `todo`, never `done`, and an unknown constraint severity is `soft`, never
     `hard`. Encoding and storage write back `raw` unchanged. An unknown
     `Frequency` or `Weekday` makes the routine generate no occurrences, and
     flags it, rather than failing the op or recurring on a wrong schedule.
     `SunriseTime` gains `Unknown { kind, raw }` in the same way.
  3. **Every entity persists its unknown state.** An `extra` blob that cannot
     be parsed is kept as opaque bytes, never read as "no unknowns".

  Persistence is per entity:
  - `tasks.extra`, `blocks.extra` and `attachments.extra` are dedicated `BLOB`
    columns.
  - `review_snapshots.body` holds the whole entity's canonical CBOR, so it
    carries its unknowns without a column of its own.
  - `Stream`, `Context`, `Routine`, `FocusStart` and `FocusEnd` get their
    `extra BLOB` from
    `crates/sunrise-storage/migrations/0015_entity_extra_columns.sql`.

  *Today:* rule 1 holds at the top level of an entity only ([#322](https://github.com/justin13888/Sunrise/issues/322)). Rule 2 is
  lossy, because `lossy_enum!` writes the fallback back
  (`lossy_enum!` in `crates/sunrise-domain/src/unknown.rs`, [#321](https://github.com/justin13888/Sunrise/issues/321)). `StreamColor`,
  `Frequency` and `Weekday` still reject unknown values.
- **Document schema: a missing feature makes a build read-only, not broken.**
  A vault lists the features its data requires in a signed, grow-only
  `vault_requires` set. A build that lacks one of them:
  - keeps syncing
  - parks what it cannot read
  - refuses local writes to the affected entity kinds, or to the whole vault
    for a structural feature, with `DOC_FEATURE_MISSING`
  - shows **"Update Sunrise to edit"**

  A feature is only added to `vault_requires` once every non-revoked device
  has advertised it, or once the user confirms. See
  [ADR-0045](../11-adr/0045-schema-identity-and-feature-gating.md) §7–§8.
  *Today:* nothing records which features a vault uses ([#324](https://github.com/justin13888/Sunrise/issues/324)).
- **DB schema.** It is local only. Migrations run in place on the first launch
  of a new build, under the rules in
  [`../04-storage/migrations.md`](../04-storage/migrations.md) §Migration rigor.

## Adding a field

1. Declare it in the entity registry, with a name that has never been used,
   its value type, its CRDT type and its default. Add it to the CDDL block in
   the entity's `02-domain/` spec.
2. Add the Rust field with `#[serde(default)]` and an `Option` or a sensible
   default.
3. Add a storage migration if the projection needs a column. Until then, the
   field lives in the entity's `extra`.
4. Decide whether it needs a **feature id**. It does when an older build
   editing the entity's *other* fields would produce wrong data because it
   ignores this one. Otherwise an older build merges and preserves the field
   without understanding it (ADR-0044 §8), and no gate is needed.
   - A field that changes what an existing field means needs a feature id. For
     example, a new `planned_at` that takes over part of `scheduled_at`'s role
     does (`task.deadlines_v2`).
   - A purely additive attribute does not.
5. Bump `DOC_SCHEMA_V`, regenerate the canonical schema, and append the new
   fingerprint to the registry. Leave `DOC_SCHEMA_FLOOR` alone.
6. Add a case to the cross-version harness ([#326](https://github.com/justin13888/Sunrise/issues/326)): an older build merges
   ops that set the field, and the field survives.

## Adding an op kind or a field-op kind

A new op kind, or a new field-op kind such as a text CRDT, **always** needs a
feature id. Older builds park it. A new op kind that writes an existing entity
kind has that entity kind as its scope. A new entity kind has itself as its
scope. Anything that changes how every entity merges is `structural`. Emit
`vault_requires` before the first op of the new kind.

Feature ids follow one scheme. An entity-scoped feature is
`<entity>.<feature>`, and a new entity kind is `<entity>.entity`: for example
`task.optional_stream`, `task.deadlines_v2`, `preferences.entity`,
`place.entity`, `external_event.entity` and `attachment.thumbnail`. A
structural feature is `core.<feature>`, for example `core.field_ops`
([ADR-0045](../11-adr/0045-schema-identity-and-feature-gating.md) §7).

## Adding an enum variant

Bump `DOC_SCHEMA_V`. Older builds keep the raw value and read it as the safe
fallback. Choose the fallback reading when the enum is first defined, not when
the variant is added, because older builds are already compiled against it. If
the fallback would make an older build's *writes* wrong, the variant needs a
feature id with the entity kind as its scope.

## Removing a field

A field is never removed from data. It is removed from what builds *write*.

1. Mark it deprecated in the CDDL **and on the Rust field**, with the
   replacement named. `Routine.skip_dates` is the worked example. It is
   deprecated in favour of `skipped_keys`, and stage A is complete.
2. Stop reading it for new logic. Keep writing it for as long as any build
   below the floor might need it.
3. Stop writing it. Values already written stay in the merged state and in the
   entity's unknown state after the field leaves the registry. Nothing deletes
   them.
4. Bump `DOC_SCHEMA_V`. The name stays reserved forever.

## Renaming a field

A rename is an add plus a removal. Names are never reused.

## Breaking changes

A change that an older build cannot read goes through parking and feature
gating. Nothing else is allowed.

- There are no flag days. Older builds keep syncing and go read-only for the
  affected scope.
- There is no version-stage licence. "No older build exists" is never a
  justification ([ADR-0042](../11-adr/0042-v0-forever.md) §2).
- A change that cannot be expressed as an additive field, a gated op kind or a
  gated field-op kind needs its own ADR. That ADR must show how both halves of
  the invariant still hold.

## Compatibility testing

- **Cross-version merge ([#326](https://github.com/justin13888/Sunrise/issues/326)).** Two different builds, a pinned older tag
  and `HEAD`, run as replicas over an in-process relay under a property test.
  - *No break:* the older build never errors, never classes an op as
    corruption, and never stops syncing.
  - *No loss:* after the older build upgrades and replays its parked ops, both
    projections equal a `HEAD`-only run of the same op set.
- **Per-entity round trip.** Each entity is decoded and re-encoded with random
  unknown keys injected at every nesting level, and must produce identical
  bytes.
- **Per-enum round trip.** Any string must survive decode, storage and
  re-encode unchanged.
- **Schema drift.** The fingerprint test above. The older per-entity
  `serde_json_shape_matches_cddl` pattern (in `constraint.rs`) is subsumed once
  the CDDL blocks are generated from the same registry.

## Record: the CDDL re-derivation at `DOC_SCHEMA_V` 4–6

The CDDL specs in `02-domain/` were re-derived against
`crates/sunrise-domain/src/` at `DOC_SCHEMA_V = 4`, and re-checked at `5` and
`6` without change. Versions 5 and 6 added only control op families, which
carry no domain entity. The repairs made then are listed so the result can be
re-checked:

| Spec | Was | Repair |
|---|---|---|
| `scheduling-constraints.md` | matched | Left alone, except the tiebreak key, which ADR-0016 changed to `(hlc, device_id, seq)`. Still pinned by `serde_json_shape_matches_cddl` in `constraint.rs`. |
| `tasks.md` | drifted | Added `reminder_lead_s`. `estimated_duration` → `estimated_duration_s` (seconds). Dropped the `blocked` state and the `blocks_others` field, neither of which is serialized. `deferred_count` is `int`, not `uint`. |
| `streams.md` | drifted | Added `reminder_lead_s`. Deleted the `integrations` map and the `StreamIcon`/`IntegrationConfig` types. `icon` is a free `tstr`. `review_cadence` is required. |
| `contexts-and-tags.md` | drifted | Deleted the `color` field and `ContextColor`. |
| `routines-and-recurrence.md` | drifted | `rrule` is the structured `RRule` map, with `Frequency`/`Weekday`. Added all six streak/forgiveness fields and `skipped_keys`. `estimated_duration_s` on `TaskTemplate`. |
| `time-blocks.md` | drifted | The CDDL block carries only the ten modelled fields. `stream_id` is required. No `timezone`. The unmodelled fields moved to a separate, explicitly-not-on-the-wire block. |
| `attachments.md` | drifted | Same split: four thumbnail fields moved out of the live block. `parent` narrowed to Task, which is what validation enforces. |
| `notes.md` | drifted | `NoteBody` is declared as the `bstr` it is, with the block grammar relabelled a renderer contract. The `Note` entity gained a CDDL block. |
| `people-and-sharing.md` | drifted | `linked_identity` → `identity_id`. Deleted `handle`/`avatar`/`notes`/`contact_methods` and `ContactMethod`. |

Every persisted entity's CDDL block declares `unknown-fields`, defined once in
[`overview.md` §Common CDDL types](./overview.md#common-cddl-types), along with
`entity-ref`, `timestamp` and the civil types.

- `tdate` became `timestamp`, because nothing in the domain emits a CBOR tag.
- `[A-Z0-9]{26}` became `[0-9A-HJKMNP-TV-Z]{26}`, because ULIDs are Crockford
  base32.

Version history, for reading old ops:

| `DOC_SCHEMA_V` | What changed |
|---|---|
| 1 | The five `SunriseTime` fields were bare instants. They still decode, as `instant`. |
| 2 | `SunriseTime` tagged ([ADR-0017](../11-adr/0017-sunrise-time-representation.md)). |
| 3 | The `blk_` and `att_` op families were added. |
| 4 | The six delete ops changed shape to full-state. |
| 5 | Added the control families `KeyEnvelope`, `DeviceRevoke` and `DeviceCertPublish` ([ADR-0024](../11-adr/0024-key-hierarchy.md)). |
| 6 | Added the control family `IdentityTransition`. |

Versions 3, 4, 5 and 6 each shipped a change an older build could not decode.
Each relied on a pre-release licence that
[ADR-0042](../11-adr/0042-v0-forever.md) withdraws. The next such change goes
through parking and `vault_requires`.

## Encryption granularity

Encryption is at the **op envelope** level, not the field level. The entire op
is encrypted as one unit under the Stream key, whatever field set or payload it
carries. There are no field-level encryption sub-keys, and "indexable plaintext
metadata" does not exist on the server. See
[`../03-crypto/data-encryption-format.md`](../03-crypto/data-encryption-format.md)
for the envelope structure.

Schema additions never expand what the server can see inside the ciphertext.
The envelope header is cleartext, and it gains only the schema-fingerprint
prefix (field 13). That prefix reveals no more than `doc_schema_v`, which the
header already carries.
