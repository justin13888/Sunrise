# 0045 — A schema version has a fingerprint, nothing verified is dropped, and a vault declares the features it requires

**Status:** accepted

**Amends** [ADR-0009](./0009-protocol-versioning-spec.md). ADR-0009 names the
versioned surfaces and their refusal rules. This record keeps the surfaces and
the integers. It adds:

- an identity for each document-schema version
- a feature granularity below that version
- a tolerance rule that replaces "report it as an invalid remote op" for
  anything a build cannot understand

**Amends** [ADR-0015](./0015-envelope-doc-schema-split.md). Envelope field 13
is added. The container version gains a floor, so an additive container change
no longer strands older readers.

**Depends on** [ADR-0042](./0042-v0-forever.md), which retires the pre-release
licence to break compatibility and puts this invariant in its place:

> **Merging vaults across client versions MUST NEVER break and MUST NEVER lose
> data.**

**Tracked by** these issues, all in phase P1 of
[`../roadmap.md`](../roadmap.md):

- [#320](https://github.com/justin13888/Sunrise/issues/320): parking
- [#321](https://github.com/justin13888/Sunrise/issues/321): lossless enums
- [#322](https://github.com/justin13888/Sunrise/issues/322): unknown maps at every nesting level
- [#323](https://github.com/justin13888/Sunrise/issues/323): fingerprint and registry
- [#324](https://github.com/justin13888/Sunrise/issues/324): `vault_requires`
- [#329](https://github.com/justin13888/Sunrise/issues/329): container floor

The cross-version harness that proves all of them together is [#326](https://github.com/justin13888/Sunrise/issues/326).

## Context

Four ways a verified op, or part of one, is lost today. Each is traced in its
issue.

1. **An unknown op kind is dropped as corruption.**
   `crates/sunrise-core/src/inner_op.rs#decode_inner_op` is a plain serde
   decode of an externally tagged enum. When it fails inside
   `crates/sunrise-core/src/engine/sync.rs#apply_remote_all`, the error becomes
   `RemoteOpInvalid`. `crates/sunrise-core/src/sync_driver.rs#is_corruption`
   classes that as link damage. The op never reaches `ops`, and it never comes
   back after an upgrade ([#320](https://github.com/justin13888/Sunrise/issues/320)).
2. **An unknown enum value is written back as its fallback.**
   `lossy_enum!` in `crates/sunrise-domain/src/unknown.rs` says so in as many words:
   "a degraded value is written back as its fallback". Under full-state ops,
   that fallback then overwrites the real value on every replica ([#321](https://github.com/justin13888/Sunrise/issues/321)).
3. **Unknown fields survive only at the top level.** Nested structs such as
   `ScheduleConstraint`, `RRule` and `TaskTemplate` have no unknown map, so
   serde discards the keys it does not know. An unknown `SunriseTime` kind fails
   the enclosing op ([#322](https://github.com/justin13888/Sunrise/issues/322)).
4. **A container version one step newer is refused outright.**
   `crates/sunrise-crypto/src/op_envelope.rs#decode_envelope` requires both the
   magic-prefix version and field 1 to equal `ENVELOPE_FORMAT_V`.
   `crates/sunrise-cbor/src/envelope_header.rs#decode_envelope_header` applies
   the same exact match at the relay. This holds even though the decoder
   already preserves the unknown fields it meets, and both the AAD and the
   signature already cover them ([#329](https://github.com/justin13888/Sunrise/issues/329)).

Two further gaps make the fixes above impossible to operate:

- **`DOC_SCHEMA_V` is a bare integer.** Nothing ties it to the shapes it
  names. Two branches can each ship `DOC_SCHEMA_V = 7` with different shapes
  ([#323](https://github.com/justin13888/Sunrise/issues/323)).
- **Nothing tells an older client that a vault uses a feature it lacks.**
  Capability bits describe a session with the relay, not the data in a vault
  ([#324](https://github.com/justin13888/Sunrise/issues/324)).

## Decision

### 1. Keep one monotonic integer per surface. No semver, no dates.

The five surfaces stay:

- `WIRE_PROTO_V`
- `ENVELOPE_FORMAT_V`
- `DOC_SCHEMA_V`
- `CRYPTO_SUITE_V`
- `STORAGE_V`

Each is a `u16` that only increases, is defined once in
`crates/sunrise-cbor/src/version.rs`, and has its own refusal rule
([`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md)
§2).

**Semantic versioning is rejected.** Its major/minor/patch split is already
expressed, more precisely, by mechanisms that act on the bytes:

- *Major*, "a reader of the old version cannot read this", is
  `DOC_SCHEMA_FLOOR`, or `ENVELOPE_FORMAT_FLOOR` (§5).
- *Minor*, "adds something an old reader can ignore", is a `DOC_SCHEMA_V` bump,
  a capability bit, or a feature id (§7).
- *Patch* has no meaning for bytes on the wire. A byte layout either changed or
  it did not.

A semver string would add a second, coarser statement of what those already
say. Readers would start comparing it instead of the precise ones.

**ISO-8601 dates are rejected**, for three reasons:

- A date carries no compatibility meaning.
- Two branches can pick the same day.
- A date implies an order across branches that do not share history.

The failure the fingerprint closes is exactly two branches choosing the same
next integer. Dates make that failure more likely, not less.

**The real gaps are elsewhere**, and each has its own section below:

| Gap | Section |
|---|---|
| The *identity* of a version | §2, the fingerprint |
| The *granularity* below a version | §7, feature ids |
| *Tolerance* of what a build cannot understand | §3, §4 and §6 |
| *Container evolution* | §5 |

### 2. Every document-schema version has a fingerprint

- **The canonical schema.** A machine-readable description of every entity,
  nested struct, enum (with its variant names), op kind, field (name, value
  type, CRDT type per [ADR-0044](./0044-per-field-ops.md), default) and feature
  id (§7). It is **generated** from the entity registry ([#328](https://github.com/justin13888/Sunrise/issues/328)), never
  written by hand. It is committed as `current.json` in a new `schemas/doc-schema/`
  directory.
- **The fingerprint** is computed as:

  ```
  fp = BLAKE3::derive_key("sunrise.doc_schema.fingerprint.v1", JCS(schema))
  ```

  `JCS` is the RFC 8785 canonical JSON form, the same canonicalization
  [ADR-0022](./0022-device-signature-canonical-json.md) uses. So the committed
  file's formatting does not affect the hash.
- **The registry**, `registry.json` in the same directory, maps every
  `DOC_SCHEMA_V` that has ever shipped to the lowercase hex of its full 32-byte
  fingerprint. It is append-only.
- **Two tests pin all of it:**
  - One recomputes the fingerprint of the generated schema. It fails unless
    that fingerprint equals the registry entry for the build's `DOC_SCHEMA_V`.
    Any change to a shape therefore fails the build until `DOC_SCHEMA_V` is
    bumped and a new entry is appended.
  - One fails if any existing registry entry changes or disappears.
- **Versions before the fingerprint** (1 through the current 6) have no entry.
  Their envelopes carry no field 13, and they are read under the legacy rules
  in §3.

### 3. The envelope binds `(doc_schema_v, fingerprint prefix)`

A new envelope field, **13**, carries the first 8 bytes of the payload's schema
fingerprint:

```cddl
OpEnvelope = {
    1: uint,            ; v             writer's ENVELOPE_FORMAT_V (§5)
    2: bstr .size 16,   ; stream_id
    3: bstr .size 16,   ; device_id
    4: uint,            ; seq
    5: [uint, uint],    ; hlc
    6: uint,            ; aead_alg
    7: uint,            ; sig_alg
    8: uint,            ; epoch
    9: bstr .size 24,   ; nonce
    10: bstr,           ; ciphertext_or_payload
    11: bstr .size 64,  ; sig
    12: uint,           ; doc_schema_v
    ? 13: bstr .size 8, ; schema_fp     first 8 bytes of fingerprint(doc_schema_v)
    * uint => any       ; later fields, preserved verbatim (ADR-0015)
}
```

- **Field 13 is covered by the AAD and the signature** through the exclusion
  rule ADR-0015 made load-bearing:
  - AAD = the map minus fields 10 and 11.
  - Signature input = the map minus field 11.

  A relay cannot strip or rewrite it.
- **Field 13 is not a container bump.** Every build in the field already
  decodes an unknown envelope field into `OpEnvelope.unknown`, re-emits it in
  its canonical position, and includes it in the AAD and the signature. See the
  `other =>` arm of
  `crates/sunrise-crypto/src/op_envelope.rs#decode_envelope`. An old build
  therefore reads, verifies, stores and relays an envelope carrying field 13
  without change.
- **Field 13 is required from the first fingerprinted version.** Call that
  version `N_fp`. A writer at `doc_schema_v ≥ N_fp` MUST emit field 13.
- **8 bytes is enough.** The prefix detects *accidental* disagreement: two
  branches, or a missed bump. The writer signs the whole envelope, so an
  adversarial collision buys an attacker nothing they could not already sign.

**What a receiver does with it:**

| Envelope | Receiver's registry | Outcome |
|---|---|---|
| `doc_schema_v < N_fp`, no field 13 | any | Legacy: read under today's rules. |
| `doc_schema_v` known, field 13 matches | has the entry | Apply normally. |
| `doc_schema_v` known, field 13 **differs** or is **missing** | has the entry | **Park** (§4) with reason `schema_fp_mismatch`. The writer and this build disagree about what version `N` means, and applying the op would be a guess. Log `core.op.schema_fp_mismatch` at `warn`. |
| `doc_schema_v` newer than this build | no entry | Accept. Unknown fields are preserved (§6). Unknown op kinds and field-op kinds are parked (§4). Missing features are gated (§8). |

Field 13 is cleartext to the relay, like the rest of the header. It reveals the
schema version the payload was written against, and field 12 already reveals
that.

**Consequence for a committed fixture.** Once field 13 is assigned,
`FUTURE_SMALL_FIELD` (`crates/sunrise-crypto/tests/forward_compat.rs:23`) stops
naming an unknown field. The `forward-compat/v1-reads-v2.cbor` fixture is
regenerated with an unassigned single-byte id (23) in its place, in the same
change that assigns 13.

### 4. Nothing that verifies is dropped: parked ops

An envelope that passes the signature check and the AEAD open is **retained
forever** once this record lands. It is either applied or **parked**. Only
envelope-level failures are corruption: a bad magic below the floor, a
non-canonical encoding, a failed signature, or a failed AEAD tag.

- **Parking reasons.**
  - The inner op kind is unknown.
  - A field-op kind is unknown (ADR-0044 §8).
  - Inner decode fails at a `doc_schema_v` newer than this build.
  - The schema fingerprint mismatches (§3).
  - Inner decode fails at a known version whose fingerprint matches. That is a
    writer bug. It is parked all the same, because the invariant does not
    exempt bugs, and it is logged at `error`.
- **Where a parked op lives.** It is written to `ops` like any applied op, with
  the same `UNIQUE (stream_id, device_id, seq)`, and marked parked. A
  `parked_ops (op_id, reason, doc_schema_v, schema_fp, parked_at_ms)` side
  table indexes it. It has **no TTL and no eviction cap**, unlike
  `deferred_ops`, which holds envelopes that could not yet be *opened* and
  can therefore be re-fetched from the relay.
- **Cursor.** A parked op counts as received. It advances the contiguous
  `(stream, device)` prefix in
  `crates/sunrise-core/src/engine/oplog.rs#upsert_sync_cursor`, so the relay
  does not re-send it forever. It is not reported as `LossEvidence`, and it
  does not start a resync.
- **Replay.** On `Core::open`, when this build's `DOC_SCHEMA_V` or registry
  differs from the one that last opened the vault, every parked op is retried
  through the full apply path, in `(hlc, device_id, seq)` order. A retry that
  succeeds clears the marker. A retry that still cannot apply stays parked with
  its reason updated. Replay is idempotent, because apply is.
- **Visibility.** The count of parked ops by reason crosses the UniFFI seam.
  Clients show it in diagnostics, and next to the read-only banner of §8 when
  the reason is a missing feature.
- **The digest and snapshots include parked ops.** They are part of the op
  set that [ADR-0043](./0043-commit-tree.md)'s state digest (proposed) covers,
  and part of what a compaction snapshot must carry ([#330](https://github.com/justin13888/Sunrise/issues/330)).

### 5. The envelope container has a floor

`ENVELOPE_FORMAT_FLOOR` joins `crates/sunrise-cbor/src/version.rs`. Two numbers
now describe a container:

- **Field 1 (`v`)** is the writer's `ENVELOPE_FORMAT_V`: the newest layout the
  writer knows.
- **The magic-prefix version** is the writer's `ENVELOPE_FORMAT_FLOOR`: the
  oldest container a reader may implement and still read this envelope
  correctly.

A reader accepts an envelope when:

```
prefix.version ≤ reader.ENVELOPE_FORMAT_V   and   v ≥ prefix.version
```

It preserves every field it does not know, as it does today. The client
decoder and the relay's header decoder share one function for this rule.

- **An additive container change** bumps `ENVELOPE_FORMAT_V` and leaves the
  floor alone. An additive change is a new field whose absence has a defined
  meaning, and which the exclusion-defined AAD and signature already cover.
- **A change that alters the meaning of an existing field, the AAD
  construction or the signature input MUST raise the floor.** A test enforces
  this by asserting that fields 1–12, the `Omit` set and `SIG_DOMAIN` are
  unchanged whenever the floor is not.
- **Transition.** Builds in the field compare both numbers for equality. So
  writers MUST keep emitting `v = 3` and prefix `3` until `core.envelope_floor`
  is in `vault_requires` (§7), and the relay has agreed server capability bit
  9, `SRV_ENVELOPE_FLOOR`, in `HelloAck`. The relay's header decoder is the
  other exact-match reader in the field. Field 13 does not need a container
  bump at all (§3), and neither would fields 14–15, which
  [`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md)
  holds as **reserved** for the commit tree. So this constraint holds nothing
  back.

### 6. Unknowns are lossless at every level

- **Lossless enums.** Every enum that crosses the wire or storage gains an
  `Unknown(raw)` arm that holds the original value.
  - *Logic* reads it as the named safe fallback. An unknown task state reads as
    `todo`, and an unknown severity reads as `soft`.
  - *Encoding* writes back `raw`, byte for byte. Storage keeps `raw` too, so a
    database round trip does not degrade it.
  - A command that sets the field explicitly replaces it. Every other write
    carries it through.
  - The UniFFI mirrors expose the unknown case, so clients render "Unknown"
    rather than the fallback.
  - This covers the seven `lossy_enum!` users and the wire enums that still
    reject: `StreamColor`, `Frequency` and `Weekday`. An unknown `Frequency` or
    `Weekday` no longer fails the op. The routine reads as generating no
    occurrences, and it is flagged. That keeps the old intent ("silently
    recurring on the wrong schedule is worse than failing") without dropping
    the routine.
- **Unknown maps at every nesting level.** Every struct that crosses the wire
  carries `#[serde(flatten)] unknown: Unknowns` and re-emits it byte-exact.
  That includes every nested value type (`ScheduleConstraint`, `TimeOfDayRange`,
  `DateRange`, `RRule`, `TaskTemplate`) and the `Patch` op's own map. `Copy` is
  dropped where the map requires it. The one exception stays `Interruption`,
  whose whole value is its key.
- **`SunriseTime` gains `Unknown { kind, raw }`.** It orders by its `index_ms`
  when the raw value carries one, round-trips unchanged, and never fails the
  enclosing op. Storage keeps the raw value rather than degrading it to
  `Instant`.
- **An `extra` blob that cannot be parsed MUST be kept opaque, not emptied.**
  This is the target, not today's behaviour:
  `crates/sunrise-core/src/engine/ids.rs#decode_unknowns` currently decodes
  the blob with `decode_lenient(..).ok().unwrap_or_default()`, so an
  undecodable blob reads as an empty map and the next write of the row drops
  it. The fix keeps the blob as bytes, writes it back unchanged, and logs the
  event ([#322](https://github.com/justin13888/Sunrise/issues/322)).

Under [ADR-0044](./0044-per-field-ops.md), an older build no longer re-emits
fields it did not edit. That reduces how often these rules are exercised. It
does not remove the need for them, because the fields an older build *does*
edit can still hold unknown values.

### 7. `vault_requires` and the feature registry

- **A feature** is a stable id. A structural feature is named
  `core.<feature>`, for example `core.field_ops` or `core.envelope_floor`. An
  entity-scoped feature is named `<entity>.<feature>`, for example
  `task.optional_stream`, `task.deadlines_v2`, `preferences.entity`,
  `place.entity`, `external_event.entity` or `attachment.thumbnail`. The
  feature registry is part of the canonical schema (§2), and it records for
  each feature:
  - the op kinds it introduces
  - the field names it introduces
  - the field-op kinds it introduces
  - its **scope**: a list of entity kinds, or `structural` for the whole vault
  - the `DOC_SCHEMA_V` it arrived in

  Ids are never reused or renamed.
- **`vault_requires`** is a new signed control op family on the vault-meta
  stream (`stream_id = 0x00…00`), sealed like the other control ops in
  `crates/sunrise-core/src/control_op.rs`:

  ```cddl
  ; InnerOp variant: { "VaultRequires": vault-requires }
  vault-requires = {
    "features": [+ feature-id],
    unknown-fields
  }
  feature-id = tstr .regexp "[a-z][a-z0-9_]*(\\.[a-z0-9_]+)+"
  ```

  The vault's required set is the **union** of every applied `VaultRequires`,
  a grow-only set. Concurrent enables converge, and no op removes a feature. A
  feature that falls out of use stays required. Dropping it would need its own
  ADR.
- **Emission order.** A client MUST apply and emit `VaultRequires` naming a
  feature before it emits the first op that uses the feature. Cross-stream
  delivery order is not guaranteed, so this is a signal, not a lock. A build
  that meets an op it cannot read parks it (§4) whether or not the signal has
  arrived yet.
- **Devices advertise what they support.** A second control op,
  `DeviceFeatures { "features": [+ feature-id] }`, is emitted by each device on
  the vault-meta stream whenever its supported set changes. It is read as the
  latest op per device, by `(hlc, seq)`. A device that has never emitted one
  supports nothing.
- **Enabling a feature.** A client MUST NOT add a feature to `vault_requires`
  while any non-revoked device lacks it in its latest `DeviceFeatures`, unless
  the user confirms. The confirmation names the devices, for example "Your
  iPhone needs an update first". It says that those devices become read-only
  for the affected data until they update. This protects every build that
  predates §4, which cannot park. They never advertise, so no feature is ever
  enabled over them silently.

### 8. A client missing a required feature degrades to read-only, not to broken

When `vault_requires` contains a feature this build does not have:

1. **It keeps syncing.** Inbound ops apply or park, outbound ops it already
   queued still upload, and cursors advance.
2. **It refuses local writes on the affected scope.** For an entity-scoped
   feature, that is every command that would write an entity of those kinds.
   For a structural feature, it is every entity command in the vault. The
   refusal is a typed error `DOC_FEATURE_MISSING { feature }`. Control ops
   needed for safety (revocation, key rotation, pairing) stay allowed, because
   refusing them would harm the vault more than any feature mismatch.
3. **It keeps reading.** Entities the build can represent are shown, and
   unknown values are shown as unknown (§6).
4. **It tells the user.** The state crosses UniFFI as the list of missing
   features with their scopes. The macOS and iOS apps show a persistent banner,
   **"Update Sunrise to edit"**, and disable the affected edit actions rather
   than letting them fail.
5. **After upgrade, nothing has been lost.** Parked ops replay (§4), and the
   banner clears when no required feature is missing.

## Alternatives considered

| Option | Why rejected |
|---|---|
| **Semver or date versions** | See §1. They add a coarser statement of what the integers, floors and feature ids already say. |
| **Refuse the whole vault on any newer schema** (`DOC_SCHEMA_TOO_NEW`) | It breaks the invariant's "never break" half. An old laptop would stop syncing entirely because a phone upgraded. |
| **Negotiate features per session with the relay (capability bits)** | The relay cannot see payloads and does not own the vault's data. Two clients that never share a session still share a vault. |
| **Drop unknown ops but ask the relay to re-send after upgrade** | The relay log is bounded (30 days or 256 MiB per channel), so an op evicted before the upgrade is gone. The replica already held the verified bytes. Parking keeps them. |
| **Park in `deferred_ops`** | That table evicts by design, because what it holds can be re-fetched. Parked ops have advanced the cursor and cannot be. |
| **A full 32-byte fingerprint in every envelope** | It costs 24 more bytes per op and detects nothing more. The threat is accidental disagreement, not forgery by a signer. |
| **Bump `ENVELOPE_FORMAT_V` for field 13** | Every build in the field would refuse the envelope at the prefix, beneath parking. The field is additive, and existing decoders already preserve it. |

## Consequences

- **The losses [#320](https://github.com/justin13888/Sunrise/issues/320), [#321](https://github.com/justin13888/Sunrise/issues/321), [#322](https://github.com/justin13888/Sunrise/issues/322) and [#329](https://github.com/justin13888/Sunrise/issues/329) record are closed.**
  Overwrite by full-state ops is [ADR-0044](./0044-per-field-ops.md)'s to
  close, and detecting divergence at all is
  [ADR-0043](./0043-commit-tree.md)'s (proposed).
- **`DOC_SCHEMA_FLOOR` stays at 1 indefinitely.** Old ops stay in logs and on
  relays, and their bytes remain the source of truth for rebuild. Raising the
  floor would strand them, so it needs its own ADR showing no live vault or
  snapshot still holds an op below the new floor.
- **`DOC_SCHEMA_TOO_NEW` is reserved and MUST NOT be used as a refusal.** A
  newer schema is always accepted, per §3.
- **New op families are allowed again**, provided each arrives with a feature
  id, a registry entry and a
  [#326](https://github.com/justin13888/Sunrise/issues/326) harness case.
  [ADR-0042](./0042-v0-forever.md) withdrew any pre-release licence to break
  compatibility, so none of them may rely on one.
- **Two new control op families** (`VaultRequires` and `DeviceFeatures`) and
  one new error code (`DOC_FEATURE_MISSING`) arrive in the same schema bump as
  field 13.
- [`../02-domain/schema-versioning.md`](../02-domain/schema-versioning.md) and
  [`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md)
  are rewritten to this record.

## What would force revisiting this

1. **A feature that must be revoked from a vault**, for example a withdrawn
   integration. The grow-only rule would need a signed, versioned removal, and
   a statement of what happens to the data the feature wrote.
2. **Parked-op volume becoming a storage problem**, for example a long-lived
   old device in a fast-moving vault. A bound would need to preserve the op
   bytes somewhere, because dropping them is not an option under the
   invariant.
3. **A second schema generator**, for example a non-Rust core. The canonical
   schema would then need to be the source rather than a generated artefact.
