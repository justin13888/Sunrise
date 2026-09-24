---
status: accepted
---

# Protocol versioning

> **Amended by [ADR-0045](../11-adr/0045-schema-identity-and-feature-gating.md)**,
> which adds four things:
>
> - a fingerprint for every document-schema version
> - envelope field 13
> - parking of verified ops this build cannot understand
> - `vault_requires` feature gating, and a floor for the envelope container
>
> **Amended by [ADR-0044](../11-adr/0044-per-field-ops.md)**, which changes how
> entity ops merge. This document **reserves envelope fields 14 and 15** for
> the commit tree (§7.1), whose design, [ADR-0043](../11-adr/0043-commit-tree.md),
> is proposed.
>
> **Items marked *Today* describe the tree before those ADRs are built.**

Sunrise has five independent versioned surfaces. Each one evolves on its own
schedule, and a single build pins exact values for all five. Compatibility is
carried by these surfaces, the capability bits and the vault's feature ids,
never by the product version ([ADR-0042](../11-adr/0042-v0-forever.md)). Every
rule here serves one invariant:

> **Merging vaults across client versions MUST NEVER break and MUST NEVER lose
> data.**

A version mismatch is therefore a hard failure only where a reader genuinely
cannot locate the bytes: an unknown wire protocol, crypto suite, or container
layout. Everywhere else, the reader keeps what it cannot understand and
degrades.

This document is the authoritative table of version numbers and the rules for
changing them. Any change to any version constant requires a superseding ADR in
[`11-adr/`](../11-adr/).

---

## 1. Versioned surfaces

| Surface | Constant | What it covers |
|---|---|---|
| **Wire protocol** | `WIRE_PROTO_V` | Frame layout, message kinds, error codes, compression rules. See [05-sync/wire-protocol.md](../05-sync/wire-protocol.md). |
| **Envelope container** | `ENVELOPE_FORMAT_V`, `ENVELOPE_FORMAT_FLOOR` | `OpEnvelope` field layout, canonical ordering, AAD, signature input. See [ADR-0015](../11-adr/0015-envelope-doc-schema-split.md) and [ADR-0045](../11-adr/0045-schema-identity-and-feature-gating.md) §5. |
| **Document schema** | `DOC_SCHEMA_V`, `DOC_SCHEMA_FLOOR`, schema fingerprint | Entity, field, enum and op-kind shapes, and the feature registry. See [02-domain/schema-versioning.md](../02-domain/schema-versioning.md). |
| **Crypto suite** | `CRYPTO_SUITE_V` | AEAD, signature, KDF, HPKE choices and parameters. See [03-crypto/primitives.md](../03-crypto/primitives.md). |
| **Storage schema** | `STORAGE_V` | SQLite/SQLCipher schema. See [04-storage/migrations.md](../04-storage/migrations.md). |

**Every one of these constants is defined in exactly one file,
`crates/sunrise-cbor/src/version.rs`.** There is no per-surface constant in
`sunrise-sync`, `sunrise-domain`, `sunrise-crypto` or `sunrise-storage`, and a
second definition MUST NOT be introduced. The magic prefix, the `Hello`
exchange, the envelope header and the migration runner all read the same
`u16`, and if two of them disagreed the result would be a wire break no test
would catch. Consumers import from `sunrise_cbor::version`. The fingerprint
registry is the one exception to "one file". It is data, not a constant, and it
lives under `schemas/doc-schema/`
([ADR-0045](../11-adr/0045-schema-identity-and-feature-gating.md) §2).

Each surface is a monotonic `u16`. Semantic versioning and date versions are
rejected ([ADR-0045](../11-adr/0045-schema-identity-and-feature-gating.md) §1).

`STORAGE_V` is purely local, and it never appears on the wire. The other four
are either negotiated or carried in the structure they version.

Two finer-grained mechanisms sit on top of the surfaces:

- **Capability bits** (§5) describe a *session* between a client and the
  relay.
- **Feature ids in `vault_requires`** (§7) describe the *data* in a vault.

A question about what the relay can do is a capability bit. A question about
whether this build can safely write to this vault is a feature id.

---

## 2. Current version constants

```
WIRE_PROTO_V      = 1
ENVELOPE_FORMAT_V = 3
DOC_SCHEMA_V      = 6
DOC_SCHEMA_FLOOR  = 1
CRYPTO_SUITE_V    = 5
```

`ENVELOPE_FORMAT_FLOOR` does not exist yet. ADR-0045 §5 introduces it at `3`,
equal to `ENVELOPE_FORMAT_V` ([#329](https://github.com/justin13888/Sunrise/issues/329)).

Wire frames, op envelopes, recovery blobs and storage rows all carry their
respective version constants.

**The refusal rules differ on purpose** (see
[ADR-0015](../11-adr/0015-envelope-doc-schema-split.md) and
[ADR-0045](../11-adr/0045-schema-identity-and-feature-gating.md)):

| Constant | Accept when | Otherwise | Why |
|---|---|---|---|
| `WIRE_PROTO_V`, `CRYPTO_SUITE_V` | The peers' supported lists intersect | Hard refusal at session start | The reader does not know the layout or the primitives, so there is nothing to salvage. |
| `ENVELOPE_FORMAT_V` | The magic-prefix version (the writer's floor) is ≤ the reader's `ENVELOPE_FORMAT_V`, **and** field 1 (`v`, the writer's `ENVELOPE_FORMAT_V`) is ≥ the prefix version: `prefix.version ≤ reader.ENVELOPE_FORMAT_V and v ≥ prefix.version` (ADR-0045 §5). Unknown fields are preserved. | Refuse the envelope: `PROTOCOL_BAD_MAGIC` | The reader can locate the payload whenever it implements the writer's floor. *Today:* both the prefix and field 1 must equal `3` exactly (`crates/sunrise-crypto/src/op_envelope.rs#decode_envelope`, `crates/sunrise-cbor/src/envelope_header.rs#decode_envelope_header`). |
| `DOC_SCHEMA_V` | Whenever it is `>= DOC_SCHEMA_FLOOR` | Refuse the envelope (`DOC_SCHEMA_TOO_OLD`) | The reader can authenticate and decrypt the payload. Whatever it cannot understand inside is preserved, or the whole op is **parked** (§7). It is never dropped. |

`ENVELOPE_FORMAT_V` versions the op-envelope container and is carried in
envelope field 1. `DOC_SCHEMA_V` versions the entity shapes and is carried in
envelope field 12. The first envelope format used one number for both jobs.
That meant adding a field to a Task changed the magic prefix, and made every
already-signed envelope undecodable.

`DOC_SCHEMA_FLOOR` is the lowest schema this build can still interpret. It
moves only when a shape stops being readable, never merely because a newer one
exists. It is `1` while `DOC_SCHEMA_V` is `6`, because a schema-1 payload
really does still decode: its bare-instant time fields read as
`SunriseTime::Instant`. Every op ever written stays in logs and on relays and
is the source of truth for a rebuild. So the floor MUST NOT be raised above any
`doc_schema_v` that a live vault or snapshot may still hold, and raising it
needs its own ADR.

**A new op variant is not a new field.** At `DOC_SCHEMA_V` 3, 5 and 6, new op
families shipped (`blk_`, `att_`, three control families and
`IdentityTransition`) that an older build cannot decode. `DOC_SCHEMA_V = 4` was
stronger still: it changed the shape of the six existing delete variants from
a bare `EntityRef` to the full entity, so neither direction was readable by the
other build. The change itself was required for correctness, because an
id-only delete cannot converge under entity-level LWW
([ADR-0014](../11-adr/0014-entity-level-lww-merge.md)). All four changes relied
on a licence that [ADR-0042](../11-adr/0042-v0-forever.md) withdraws: that no
older build existed.

From now on:

- **A new op kind or field-op kind** carries a feature id. Older builds park it
  (§7).
- **Changing an existing variant's shape** is a new variant alongside the old
  one. The old one stays readable forever.

*Today:* an older build reports an unknown variant as `RemoteOpInvalid`, which
is classed as corruption, and drops it ([#320](https://github.com/justin13888/Sunrise/issues/320)).

`STORAGE_V` is per-device, never appears on the wire, and is deliberately not
named here. It moves with every migration, and a number that has to be
hand-edited on every append will be wrong most of the time. It lives in
`crates/sunrise-cbor/src/version.rs`, and a unit test asserts
`current_storage_v()` equals it, so the constant and the migration list cannot
part company.

What is worth stating, because it is *frozen*, is the floor. That is a separate
constant, `BASELINE_STORAGE_V = 13`, from the one-time baseline reset
([ADR-0018](../11-adr/0018-storage-baseline-reset.md)). A vault at 13 upgrades.
A vault at 12 is refused, with a typed `STORAGE_V_PRE_BASELINE`, rather than
upgraded. The two numbers are not interchangeable and should never be quoted as
one.

---

## 3. Magic prefixes

Every persisted or transmitted byte structure begins with a uniform 5-byte
magic prefix (`crates/sunrise-cbor/src/magic.rs#MagicKind`):

```
0:2   ASCII "SR"  (0x53 0x52)
2:3   kind        (uint8; see table)
3:5   version     (uint16 big-endian; the structure-specific version)
```

| Kind | Hex | Structure | Version in the prefix | Current value |
|---|---|---|---|---|
| 1 | `0x01` | Wire frame | `WIRE_PROTO_V` | 1 |
| 2 | `0x02` | Op envelope | The writer's `ENVELOPE_FORMAT_FLOOR`, i.e. the oldest container a reader may implement (ADR-0045 §5). *Today* this is `ENVELOPE_FORMAT_V`, the same value. It is **not** `DOC_SCHEMA_V`, which is in field 12. | 3 |
| 3 | `0x03` | Recovery blob | recovery format version | 1 |
| 4 | `0x04` | Snapshot blob | snapshot format version (reserved; nothing writes one, see [04-storage/compaction.md](../04-storage/compaction.md)) | 1 |
| 5 | `0x05` | Vault meta record | vault meta version | 1 |
| 6 | `0x06` | Diagnostic bundle | bundle version | 1 |
| 7 | `0x07` | Pairing payload (QR contents, base64url JSON inside) | pairing version | 1 |

All version fields are written big-endian.

A reader MUST verify the magic before any further parse. A mismatch returns
`PROTOCOL_BAD_MAGIC` and the structure is discarded.

- **Pairing QR codes** are JSON strings. They include a `magic_v1` field whose
  value is the 5 bytes encoded as 10 hex characters. The name is a protocol
  identifier and is never renamed ([ADR-0042](../11-adr/0042-v0-forever.md)
  §5). Apart from that field, the QR JSON's outer object is the canonical
  CBOR-via-JSON form described in
  [03-crypto/pairing-and-onboarding.md](../03-crypto/pairing-and-onboarding.md).
- **For an op envelope**, the version check is the floor rule in §2, not
  equality. Every other kind still requires an exact match.

---

## 4. Wire-protocol negotiation

Session establishment carries a `Hello`. Since
[ADR-0023](../11-adr/0023-sse-sync-transport.md), it is the JSON body of
`POST /api/v1/sync/session` (`crates/sunrise-server/src/api/sync/credential.rs`),
and its shape is:

```cddl
Hello = {
  client_app_v:        text,            ; e.g. "0.4.2"; diagnostic only (ADR-0042 §1)
  client_platform:     text,            ; "macos15", "ios18", "linux-x86_64", ...
  wire_proto_supported: [+ uint],       ; ascending list of WIRE_PROTO_V values the client speaks
  doc_schema_min:      uint,            ; the client's DOC_SCHEMA_FLOOR: lowest schema it can read
  doc_schema_max:      uint,            ; the client's DOC_SCHEMA_V: highest schema it writes
  crypto_suite_supported: [+ uint],
  capabilities:        uint64,          ; bitfield, see §5
  trace:               text,            ; ULID; logged as the trace id of this session on the server
}
```

The server replies with `HelloAck`, plus the session id:

```cddl
HelloAck = {
  server_app_v:    text,                ; diagnostic only
  wire_proto:      uint,                ; max(intersection(client.wire_proto_supported, server.wire_proto_supported))
  crypto_suite:    uint,                ; same negotiation
  doc_schema_floor: uint,               ; the minimum DOC_SCHEMA_V the server accepts in inbound ops
  capabilities:    uint64,              ; bitfield AND'd with client.capabilities
  server_time_ms:  uint,                ; for clock-skew detection only; not authoritative
}
```

Negotiation rules:

- **Empty wire-proto intersection.** The server refuses with
  `SYNC_PROTOCOL_VERSION_MISMATCH`. `ErrorPayload` carries `code` and a
  diagnostic `reason` only; there are no `server_min` / `server_max` fields on
  the wire. The client MUST surface an "update Sunrise" prompt with a link to
  the upgrade page.
- **Empty crypto-suite intersection.** The same, with `CRYPTO_SUITE_MISMATCH`.
  The crypto suite is not optional, and there is no fallback path.
- **`doc_schema_floor > client.doc_schema_max`.** `DOC_SCHEMA_TOO_OLD`, and the
  client MUST update.
- **A client whose `doc_schema_max` is newer than the server knows** is
  accepted normally. The relay never decodes payloads, and it forwards any
  envelope at or above its floor. It MUST NOT refuse, tag or rewrite ops
  because their schema is newer than its own.
- **`client_app_v` and `server_app_v` are diagnostics.** Neither side gates on
  them.

Once negotiated, the chosen versions are immutable for the lifetime of the
session. A client that wants to upgrade opens a new session.

*Today:* `SessionRequest` and `SessionResponse` are
`#[serde(deny_unknown_fields)]`. So a new `Hello` field is refused by a server
that predates it, and is **not** the minor change §6 describes. A new `Hello`
field needs either a capability bit agreed in an earlier exchange, or the deny
relaxed on both sides first.

---

## 5. Capability bitfield

Capabilities express optional features of a *session* that don't justify a full
version bump. They do not describe a vault's data; that is `vault_requires`
(§7). The bitfield is 64 wide. Bits 0–31 are server-side features, and bits
32–63 are client-side features. Bits are allocated contiguously. Bits not
listed below MUST be 0.

Server bits:

| Bit | Name | Meaning when set |
|---|---|---|
| 0 | `SRV_PUSH_APNS` | Server can deliver APNs pushes. |
| 1 | `SRV_PUSH_FCM` | Server can deliver FCM pushes. |
| 2 | `SRV_PUSH_WEB` | Server can deliver Web Push. |
| 3 | `SRV_BLOB_PRESIGN` | Server signs URLs for blob upload and download. |
| 4 | `SRV_RELAY_PAIR` | Server forwards Noise-XX pairing transport. |
| 5 | `SRV_INTEGRATION_GCAL` | Server provides a Google Calendar OAuth proxy. |
| 6 | *(reserved)* | Was `SRV_BILLING_STRIPE`, "server enforces Stripe-backed quotas". ADR-0027 took quotas out of scope, so the **name is retired but the position is not**. A peer that ever set bit 6 asserted the Stripe meaning, and reissuing the bit would make that honest claim read as a new one. Nothing above it is renumbered. |
| 7 | `SRV_DIAGNOSTIC_UPLOAD` | Server accepts opt-in diagnostic bundles. |
| 8 | `SRV_TOKEN_REFRESH` | Server accepts `0x12 RefreshToken` mid-session and answers `0x13 RefreshTokenAck`. |
| 9 | `SRV_ENVELOPE_FLOOR` | *Allocated by ADR-0045 §5, not yet built.* The relay's header decoder applies the envelope floor rule (§2) rather than exact match. |

Client bits:

| Bit | Name | Meaning when set |
|---|---|---|
| 32 | `CLI_ENTITY_LWW` | The client resolves concurrent writes over `(hlc, device_id, seq)`. Under [ADR-0044](../11-adr/0044-per-field-ops.md) the resolution is per field. The bit's meaning to the relay is unchanged. |
| 33 | `CLI_HLC_TIMESTAMPS` | The client stamps every op with a hybrid logical clock and refuses one beyond the drift window. |
| 34 | `CLI_FORWARD_COMPAT` | The client preserves and re-emits unknown CBOR map keys. |
| 35 | `CLI_FTS5_PORTER_EN` | The client's FTS uses unicode61 + porter. |
| 36 | `CLI_PRESENCE_BEACONS` | The client emits presence beacons. |
| 37 | `CLI_DIAGNOSTIC_MODE` | The client supports diagnostic-mode log uploads. |

**Required bits.** The client MUST set 32, 33, 34 and 35. The server MUST set
3. A missing required bit refuses the session with
`CAPABILITY_REQUIRED_MISSING`. Any other unset bit is a clean degrade.

This is enforced, not aspirational:

- `REQUIRED_CLIENT_BITS` and `REQUIRED_SERVER_BITS` in
  `crates/sunrise-wire-protocol/src/capability.rs` hold exactly those bits.
- `Hello::negotiate` refuses a peer missing any of them.
- `the_redefined_client_bits_keep_their_positions` pins 32–35, so the
  redefinition below cannot become a silent renumbering.

Read bit 34 alongside [§7](#7-document-schema-forward-compat). A client that
asserts `CLI_FORWARD_COMPAT` has promised to re-emit unknown map keys
byte-for-byte. An entity whose table cannot persist them breaks that promise on
the first restart. That is why §7's `extra` column is a MUST and not a nicety.
A build that cannot keep the promise MUST NOT set the bit, and a build that
does not set it is refused.

> **Bits 32–34 were redefined.** They originally asserted three Loro CRDT
> capabilities: `CLI_LORO_LWW_REGISTER`, `CLI_LORO_OR_SET` and
> `CLI_FRACTIONAL_INDEX`. [ADR-0014](../11-adr/0014-entity-level-lww-merge.md)
> replaced CRDT merge with entity-level LWW and deleted Loro, so these REQUIRED
> bits asserted that a client implemented three things nothing in the codebase
> did. A peer setting them was telling the truth about nothing, and a peer
> refusing them was refused for the wrong reason. They now name what a client
> is actually required to do. The bit POSITIONS are unchanged. The
> redefinition was safe only because nothing that read the old meanings had
> shipped. Any future redefinition of a bit is a `WIRE_PROTO_V` bump.

> **Why bit 8 is negotiated rather than assumed.** A server that accepts a
> `0x12` and a server that has never heard of it are both *silent*, and the
> client's correct response differs: keep the session, or stop trusting the
> credential and let the session die. A client MUST NOT send `0x12` unless it
> saw bit 8 come back agreed in `HelloAck.capabilities`. Without it, renewal
> happens by reconnecting, which every server understands. Adding the bit and
> the `0x13` frame is a **minor** change under §6: a new capability bit, and a
> frame sent only when negotiated. So `WIRE_PROTO_V` stays at 1.

Unknown bits set by the peer MUST be ignored, for forward compatibility, and
MUST NOT be echoed back in `HelloAck.capabilities`.

---

## 6. Wire-protocol evolution rules

For `WIRE_PROTO_V`:

1. **Minor change (no version bump).** Adding a new optional field to an
   existing message, a new error code, or a new capability bit. Old peers
   ignore unknown fields, codes and bits. The exception is the session
   request and response, which today reject unknown fields (§4).
2. **Major change (version bump).** Anything that changes how an existing
   field is interpreted, removes a field, changes the frame layout, changes the
   compression scheme, or changes error semantics. A version bump requires:
   - A superseding ADR.
   - The server keeps the previous version listed in
     `server.wire_proto_supported` for at least **two server releases AND
     ≥ 90 days** after the new version goes live in production. Deprecation
     cannot complete while > 0.5% of daily active devices are still on the old
     version. The telemetry that would measure this does not exist yet (see
     [§11](#11-logging-and-metrics)), so today the window is enforced by the
     calendar alone.
   - Client release notes call out the protocol bump.
3. **Frame headers and the magic prefix are immutable.** A new wire-protocol
   generation that needs to change them gets a new magic and is treated as a
   separate transport: clients dial both, and the server listens on both.

The negotiated `wire_proto` is **not** logged per session, and no per-session
record carries it. `wire_v` / `doc_v` / `crypto_v` appear once per process, on
the `srv.start` line. `crates/sunrise-log/src/proto.rs` records the trade:
restating three constants that cannot change while the process lives would cost
roughly 50 bytes on every record to say what one line already says.
`srv.sync.session_open` (`crates/sunrise-server/src/api/sync/credential.rs`)
carries `account_h` and nothing about the protocol.

For today's server that loses nothing, because the server offers exactly one
wire version. `Hello::negotiate` is called with `&[WIRE_PROTO_V]`, so the
negotiated value is the compiled one, and the startup line already reports it.
That stops being sufficient the day the server lists two, which is the same day
§6's deprecation window needs measuring. The per-version metric that would let
an operator read the distribution without parsing logs is target state
([§11](#11-logging-and-metrics)). It should land with the second version, not
after it.

---

## 7. Document-schema forward compat

`DOC_SCHEMA_V` follows the rules in
[02-domain/schema-versioning.md](../02-domain/schema-versioning.md). This
section adds the wire-side rules. The design of record is
[ADR-0045](../11-adr/0045-schema-identity-and-feature-gating.md), and the rule
it serves is that **nothing that verifies is dropped**.

### 7.1 Schema identity

Every envelope at a fingerprinted `DOC_SCHEMA_V` carries field 13: the first 8
bytes of that version's schema fingerprint. The field is covered by the AAD
and the signature under ADR-0015's exclusion rule.

A receiver checks field 13 against its committed registry:

| Case | Outcome |
|---|---|
| A known version whose fingerprint matches | Applied. |
| A known version whose fingerprint differs, or is missing | Parked. |
| A newer version | Accepted. |
| A version older than the first fingerprinted one | Read as legacy. |

Field 13 is additive: every build in the field already preserves an unknown
envelope field and includes it in the AAD and the signature. So it needs no
`ENVELOPE_FORMAT_V` bump.

**Fields 14 and 15 are reserved** for the commit tree. No build writes them,
and no other use may claim them. A reader that meets either preserves it like
any unknown field. Their meaning is assigned only when
[ADR-0043](../11-adr/0043-commit-tree.md), now proposed, is accepted; being
additive, they will need no `ENVELOPE_FORMAT_V` bump either. *Today:* there is no field 13 and no registry
([#323](https://github.com/justin13888/Sunrise/issues/323)).

### 7.2 Parking

An envelope that verifies and decrypts, but whose payload this build cannot
fully apply, is **parked**. The reasons are:

- an unknown op kind
- an unknown field-op kind
- a decode failure at a newer schema
- a fingerprint mismatch
- a decode failure at a known version whose fingerprint matches. That is a
  writer bug; it is parked all the same, because the invariant does not exempt
  bugs, and it is logged at `error` (ADR-0045 §4)

A parked op is stored in `ops` with a parked marker and no TTL. It advances the
`(stream, device)` cursor, so the relay does not re-send it forever. It is not
loss evidence and does not trigger a resync. After an upgrade it is replayed
through the full apply path, in `(hlc, device_id, seq)` order.

Only envelope-level failures are corruption: a bad magic, non-canonical CBOR, a
failed signature, or a failed AEAD tag. *Today:* a decode failure is
`RemoteOpInvalid`, and `crates/sunrise-core/src/sync_driver.rs#is_corruption`
drops it ([#320](https://github.com/justin13888/Sunrise/issues/320)).

### 7.3 Unknown map keys round-trip unchanged, at every level

A client receiving an op with fields it does not model preserves the unknown
keys verbatim in storage and re-serializes them on any outbound write. This
needs three things, and it fails without any one of them:

1. **Every struct that crosses the wire carries
   `#[serde(flatten)] unknown: Unknowns`** (`sunrise_domain::unknown`). That
   means every entity, every nested value type, and the `Patch` op's own map.
   An unfamiliar field is then kept rather than discarded by serde's default
   behaviour. The one exception is `Interruption`, whose whole value is its
   primary key. *Today:* this holds at the top level of an entity only
   ([#322](https://github.com/justin13888/Sunrise/issues/322)).
2. **Every synced entity's table persists that map in its own `extra BLOB`
   column**, so the field survives materialization rather than living for one
   transaction. This is a contract, not a per-table convenience. The envelope
   signature covers every field the sender wrote. A table added for a synced
   entity MUST carry `extra`. An `extra` blob that cannot be parsed is kept as
   opaque bytes, never treated as empty.
3. **`encode_canonical` sorts map keys by their encoded bytes**, so a
   preserved field is re-emitted in the position its author put it in. Without
   sorting, byte-exact re-emission is impossible.

Under per-field ops ([ADR-0044](../11-adr/0044-per-field-ops.md)), a build
never re-emits a field it did not edit, so a newer field cannot be overwritten
by a writer that does not know it. Field ops are self-describing, so a
`Patch` that names an unknown field still merges correctly (ADR-0044 §8).

### 7.4 Enums are lossless

Every enum that crosses the wire or storage keeps an `Unknown(raw)` arm.

- **Logic** reads it as a named fallback chosen to be the SAFE reading. An
  unknown task state is `todo`, never `done`. An unknown constraint severity is
  `soft`, never `hard`.
- **Encoding and storage** write back `raw` unchanged.
- **`Frequency` and `Weekday`** are no longer the exception. An unknown value
  no longer fails the op: the routine generates no occurrences, and it is
  flagged.
- **`SunriseTime`** gains `Unknown { kind, raw }`.

*Today:* `lossy_enum!` writes the fallback back
(`lossy_enum!` in `crates/sunrise-domain/src/unknown.rs`), and `StreamColor`,
`Frequency` and `Weekday` reject unknown values ([#321](https://github.com/justin13888/Sunrise/issues/321)).

### 7.5 Invariants are read-time

Local invariants are validated on **local commands only**, never on inbound
merged ops. A client merging an op that produces an empty title accepts it, and
shows the task as *Untitled* rather than refusing the merge. Refusing would make
two replicas diverge permanently over one string. Cross-field and cross-entity
invariants are derived at read time
([ADR-0044](../11-adr/0044-per-field-ops.md) §6).

*Today:* the acceptance is silent. `Engine::apply_remote` emits no diagnostic
when a merged entity violates a local invariant. A structured
`core.merge.invariant_derived` event SHOULD be added at `debug` and registered
in [log-events.md](./log-events.md), so an operator can tell "never violated"
from "silently derived".

### 7.6 `vault_requires`: feature gating

A vault records the features its data depends on in a signed, grow-only
`vault_requires` control op set on the vault-meta stream. Each device
advertises the features it supports with a `DeviceFeatures` control op. A
feature id is `<entity>.<feature>` for an entity-scoped feature (for example
`task.optional_stream` or `place.entity`) and `core.<feature>` for a structural
one (for example `core.field_ops`); see
[02-domain/schema-versioning.md](../02-domain/schema-versioning.md).

A client that lacks a required feature:

- keeps syncing
- parks what it cannot read
- keeps reading
- refuses local writes on the feature's scope with `DOC_FEATURE_MISSING`. The
  scope is its entity kinds, or the whole vault for a structural feature.
- shows **"Update Sunrise to edit"**

A feature MUST NOT be added to `vault_requires` while a non-revoked device has
not advertised it, unless the user confirms
([ADR-0045](../11-adr/0045-schema-identity-and-feature-gating.md) §7–§8).
*Today:* nothing records which features a vault uses ([#324](https://github.com/justin13888/Sunrise/issues/324)).

### 7.7 The relay's fence

The pair `(doc_schema_floor, client.doc_schema_max)` defines the fence:

- The server's `doc_schema_floor` is the lowest schema it is willing to relay.
  It is `DOC_SCHEMA_FLOOR` (`1`), and it follows the same no-raise rule as the
  client floor (§2).
- A client refuses a session whose `doc_schema_floor` exceeds its max, and the
  user is prompted to update.

---

## 8. Crypto suite evolution

`CRYPTO_SUITE_V` is the most conservative surface.

- A new suite means new `aead_alg` / `sig_alg` / `kdf_alg` ids in the op envelope (see [03-crypto/data-encryption-format.md](../03-crypto/data-encryption-format.md)). It also means *any* change to a domain-separation string, an AAD prefix, a KDF context or a wrapped-blob length — none of which travels, so none of which any negotiation could catch. [03-crypto/key-rotation.md](../03-crypto/key-rotation.md) §What the format freeze covers lists which of those are pinned by a frozen vector or by the committed keychain vault, and which are still residue.
- Suite migration is **per-Stream** via key-rotation epoch (see [03-crypto/key-rotation.md](../03-crypto/key-rotation.md)). A client that supports both suites can read both; rotation encrypts new ops under the new suite while old ops stay readable under the old one.
- Suite deprecation requires:
  - All Streams the user owns have rotated past the old suite (client surfaces the list).
  - At least 12 months since the new suite shipped to all client platforms.
  - A superseding ADR.
  - Deprecation stops a suite being *written*, never being *read*. A build MUST keep verify and decrypt support for every suite used by any op a live vault or snapshot may still hold, or the invariant's "never lose" half fails on the next rebuild from the log.
- The negotiated suite is **not logged anywhere**. Negotiation is per session (`max_intersection` in `crates/sunrise-wire-protocol/src/negotiation.rs`), and its result is discarded for observability purposes: `srv.sync.session_open` (`crates/sunrise-server/src/api/sync/credential.rs`) carries `account_h` and nothing about the suite. The `crypto_v` on the startup line is a different number — the binary's compiled `CRYPTO_SUITE_V`, what this process *supports*, not what any session *chose*. Nor do the metrics cover it: `sunrise_sync_session_total` is incremented once per session but is unlabelled. The registry's `render` passes a name containing `{` through verbatim, so a labelled series is expressible as a string, but nothing constructs one — the single `incr` site at `api/sync/credential.rs` would have to build the name (see [§11](#11-logging-and-metrics)).

There is no per-session crypto-suite mixing. A session uses exactly one suite for transport-level handshake; ops within the session may carry envelopes encrypted under any suite the recipient supports.

---

## 9. Storage schema migrations

`STORAGE_V` is per-device. See
[04-storage/migrations.md](../04-storage/migrations.md), whose §Migration rigor
carries the backup, integrity-check and rebuild rules. The wire and the server
are unaware of `STORAGE_V`.

Forward compat for storage: a device running schema `N` MUST refuse to open a
database whose `STORAGE_V > N`, and the user is prompted to update the binary.
Refusing to *open* a local file is not a merge failure. The vault's data is
intact on disk and on the relay, and the next build opens it.

Downgrade across `STORAGE_V` is supported only if the migration was explicitly
marked reversible in its ADR. None is.

---

## 10. Error codes for version mismatch

These are defined in [error-handling.md](./error-handling.md). The codes
related to versioning:

| Code | When |
|---|---|
| `PROTOCOL_BAD_MAGIC` | A magic prefix mismatch on parse. For an op envelope, that means a prefix version above this reader's `ENVELOPE_FORMAT_V` (§2). |
| `SYNC_PROTOCOL_VERSION_MISMATCH` | The wire-proto intersection is empty. It is spelled with the `SYNC_` prefix in `crates/sunrise-error/src/codes.rs`; there is no `PROTOCOL_VERSION_MISMATCH`. |
| `CRYPTO_SUITE_MISMATCH` | The crypto-suite intersection is empty. |
| `DOC_SCHEMA_TOO_OLD` | The server's `doc_schema_floor` exceeds the client's `doc_schema_max`, or an envelope's `doc_schema_v` is below this build's floor. |
| `DOC_SCHEMA_TOO_NEW` | **Reserved. It MUST NOT be used as a refusal**, because a newer schema is always accepted (§2, §7). The code exists in `crates/sunrise-error/src/codes.rs`, and nothing emits it. |
| `DOC_FEATURE_MISSING` | *Allocated by ADR-0045 §8, not yet in `codes.rs`.* A local command would write to a scope whose feature this build lacks. The vault stays readable, and it keeps syncing. |
| `STORAGE_V_TOO_NEW` | The local database file is at a `STORAGE_V` newer than the binary. |
| `STORAGE_V_TOO_OLD` | The local database predates the baseline. `STORAGE_V_PRE_BASELINE` maps here. |
| `CAPABILITY_REQUIRED_MISSING` | Negotiation succeeded on versions, but a required capability bit is unset on the peer. |

The session-level errors are **permanent** in the sense of
[error-handling](./error-handling.md): the session does not retry, and the
client shows a clear "update Sunrise" prompt rather than backing off. They are:

- `SYNC_PROTOCOL_VERSION_MISMATCH`
- `CRYPTO_SUITE_MISMATCH`
- `DOC_SCHEMA_TOO_OLD`
- `CAPABILITY_REQUIRED_MISSING`

`DOC_FEATURE_MISSING` is not a session error at all. Sync continues, and only
the refused local write fails.

---

## 11. Logging and metrics

`proto` is **not** on every log record. `wire_v` / `doc_v` / `crypto_v` are
emitted once per process on the binary's startup event (`srv.start` /
`ui.start`) — [logging.md](./logging.md) §3 records the change under *Dropped
from the original schema*, and [§6](#6-wire-protocol-evolution-rules) above
gives the trade: restating three constants that cannot change while the process
lives would cost roughly 50 bytes on every line to say what one line already
says. Correlating a later record with them is a join on the process, not a
field read.

**Server-side metrics as implemented.** `crates/sunrise-server/src/metrics.rs`
is an in-process `BTreeMap<String, AtomicU64>` behind a mutex, rendered as
Prometheus text at `/metrics`. It has one operation — increment a counter by
name — and therefore **no labels, no histograms and no gauges**. Every series it
emits is a bare counter name:

<!-- Extracted from the tree; do not edit by hand. Re-run and reconcile:
     grep -rhoE '"sunrise_[a-z0-9_]+"' crates/sunrise-server/src | sort -u
     Last extracted: 7fa82b54 -->

```
sunrise_sync_{negotiate_refused,session,refresh,stream,resume_conflict}_total
sunrise_relay_append_failed_total       sunrise_relay_cursor_gap_total
sunrise_relay_batch_{duplicate,overlap}_total
sunrise_device_sig_rejected_total       sunrise_account_create_total
sunrise_devices_{list,register,revoke}_total
sunrise_blob_{init,chunk,finalize,fetch}_total
sunrise_blob_hash_mismatch_total
sunrise_recovery_blob_fetch_total       sunrise_recovery_step_up_refused_total
sunrise_push_register_total             sunrise_push_{apns,fcm,web}_total
```

Twenty-five names. The full list with its provenance lives in
[`../06-server/observability.md`](../06-server/observability.md) §Metrics.

The registry's `render` passes a name containing `{` through verbatim, so a
labelled series is *expressible* as a string, but nothing constructs one.

**Target state**, and what a deprecation decision actually needs:

```
sunrise_sync_session_total{wire_proto, crypto_suite, result}
sunrise_sync_session_capabilities{bit}      # counter; per-session OR of capabilities
sunrise_op_envelope_total{aead_alg, sig_alg}
sunrise_storage_migration_total{from_v, to_v, result}
```

None of these four exist. Until they do, no version deprecation under [§6](#6-wire-protocol-evolution-rules) can be justified by the "> 0.5% of daily active devices" or "< 0.1% of inbound ops" thresholds this document sets — the numerator is not measured. `metrics.rs`'s own header says production swaps in a real client (`prometheus`, `metrics`) without changing the exposition contract; that is where labelled series arrive.

---

## 12. Test fixtures

The crypto spec describes byte-exact test vectors. This spec adds:

- `tests/fixtures/hello/v1.cbor` and `tests/fixtures/hello/ack_v1.cbor` — canonical `Hello` and `HelloAck` byte fixtures.
- `tests/fixtures/version-mismatch/*.cbor` — every negotiation error path has a fixture, and the test decodes the FIXTURE (not a freshly built value) and asserts the error it produces: `wire-mismatch`, `crypto-mismatch`, `doc-schema-too-old`, `capability-missing`.
- `tests/fixtures/forward-compat/v1-reads-v2.cbor` — a synthetic envelope at `DOC_SCHEMA_V + 1` carrying two envelope fields this build does not know (ids 13 and 40, the second above 23 so its CBOR key needs two bytes). The round trip preserves them byte-for-byte. When [ADR-0045](../11-adr/0045-schema-identity-and-feature-gating.md) assigns field 13, the small id moves to 23, an unassigned single-byte key, in the same change.

  CBOR, not the JSON this section originally named: the artefact under test is a signed, canonically encoded envelope, and JSON cannot represent one without a re-encoding step that would be the thing actually being tested.

These fixtures are checked into the repo. Any change to them must be a deliberate version bump — which is why regeneration sits behind `SUNRISE_REGEN_FIXTURES=1` rather than happening automatically.

Two more committed binaries sit outside this list and follow the same rule under their own switches: `sunrise-storage`'s old-vault fixtures (`mise run storage-fixtures`), which pin the migration chain, and `sunrise-core`'s keychain vault (`mise run keychain-fixture`), which pins the key hierarchy's wrapping domains. The second is regenerated only for a `CRYPTO_SUITE_V` bump; see [03-crypto/key-rotation.md](../03-crypto/key-rotation.md) §The vault that has to open.

Three of them are built from the live constants and therefore move with a
version bump *by construction*: `hello/*.cbor` and the `version-mismatch`
fixtures embed `doc_schema_max`, and `forward-compat/v1-reads-v2.cbor` is
defined as `DOC_SCHEMA_V + 1`. Regenerating them alongside a bump is expected
and is not the drift this guard exists to catch; regenerating them without one
is. The same distinction applies to the two whole-envelope crypto vectors,
which carry the document schema in field 12 — a doc-schema bump moves their
bytes with no crypto change, and no other frozen vector has that excuse.

The forward-compat fixture also pins the reason preservation is not optional: the envelope signature covers every field the sender wrote, so a decoder that DROPS an unknown field cannot re-emit an envelope that still verifies. `dropping_an_unknown_field_breaks_the_senders_signature` asserts exactly that.
