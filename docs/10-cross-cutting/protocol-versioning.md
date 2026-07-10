# Protocol versioning

`status: accepted (v1)`

Sunrise has four independent versioned surfaces. Each evolves on its own schedule. A single client release pins exact versions of all four. The wire-protocol exchange negotiates the highest version both peers understand at session start; mismatch is a hard failure with a known error code.

This document is the authoritative table of version numbers and the rules for changing them. Any change to any version constant requires a superseding ADR in [`11-adr/`](../11-adr/).

---

## 1. Versioned surfaces

| Surface | Constant | Defined in | What it covers |
|---|---|---|---|
| **Wire protocol** | `WIRE_PROTO_V` | `sunrise-sync/src/proto.rs` | Frame layout, message kinds, error codes, compression rules — see [05-sync/wire-protocol.md](../05-sync/wire-protocol.md). |
| **Document schema** | `DOC_SCHEMA_V` | `sunrise-domain/src/schema.rs` | Per-entity field shapes and CRDT types — see [02-domain/schema-versioning.md](../02-domain/schema-versioning.md). |
| **Crypto suite** | `CRYPTO_SUITE_V` | `sunrise-crypto/src/suite.rs` | AEAD, signature, KDF, HPKE choices and parameters — see [03-crypto/primitives.md](../03-crypto/primitives.md). |
| **Storage schema** | `STORAGE_V` | `sunrise-storage/src/migrations.rs` | SQLite/SQLCipher schema — see [04-storage/migrations.md](../04-storage/migrations.md). |

`STORAGE_V` is purely local; it never appears on the wire. The other three are negotiated.

---

## 2. v1 version constants

```
WIRE_PROTO_V    = 1
DOC_SCHEMA_V    = 1
CRYPTO_SUITE_V  = 1
STORAGE_V       = 1
```

Wire frames, op envelopes, recovery blobs, and storage rows all carry their respective version constants. Decoders MUST refuse to interpret bytes as version `N` if the version field reads anything other than `N`; refusal is a clean error, not a guess.

---

## 3. Magic prefixes

Every persisted or transmitted byte structure begins with a uniform 5-byte magic prefix:

```
0:2   ASCII "SR"  (0x53 0x52)
2:3   kind        (uint8; see table)
3:5   version     (uint16 big-endian; the structure-specific version)
```

| Kind | Hex | Structure | Version source |
|---|---|---|---|
| 1 | `0x01` | Wire frame | `WIRE_PROTO_V` |
| 2 | `0x02` | Op envelope | `DOC_SCHEMA_V` |
| 3 | `0x03` | Recovery blob | recovery format version (v1 = 1) |
| 4 | `0x04` | Snapshot blob | snapshot format version (v1 = 1) |
| 5 | `0x05` | Vault meta record | vault meta version (v1 = 1) |
| 6 | `0x06` | Diagnostic bundle | bundle version (v1 = 1) |
| 7 | `0x07` | Pairing payload (QR contents, base64url JSON inside) | pairing version (v1 = 1) |

All version fields are written big-endian. v1 always emits version `0x0001`.

A reader MUST verify the magic before any further parse. Mismatch → `PROTOCOL_BAD_MAGIC` and discard. Pairing QR codes are JSON strings that include a `magic_v1` field whose value is the 5 bytes encoded as 10 hex characters; the QR JSON's outer object is otherwise the canonical CBOR-via-JSON form described in [03-crypto/pairing-and-onboarding.md](../03-crypto/pairing-and-onboarding.md).

---

## 4. Wire-protocol negotiation

The first frame on any sync connection is `Hello`:

```cddl
Hello = {
  client_app_v:        text,            ; e.g. "1.4.2"
  client_platform:     text,            ; "ios18", "macos15", "linux-x86_64", ...
  wire_proto_supported: [+ uint],        ; ascending list of WIRE_PROTO_V values the client speaks
  doc_schema_min:      uint,            ; lowest DOC_SCHEMA_V the client can produce
  doc_schema_max:      uint,            ; highest DOC_SCHEMA_V the client can read
  crypto_suite_supported: [+ uint],
  capabilities:        uint64,          ; bitfield, see §5
  trace:               text,            ; ULID; logged as the trace id of this session on the server
}
```

The server replies with `HelloAck`:

```cddl
HelloAck = {
  server_app_v:    text,
  wire_proto:      uint,                ; max(intersection(client.wire_proto_supported, server.wire_proto_supported))
  crypto_suite:    uint,                ; same negotiation
  doc_schema_floor: uint,               ; the minimum DOC_SCHEMA_V the server accepts in inbound ops
  capabilities:    uint64,              ; bitfield AND'd with client.capabilities
  server_time_ms:  uint,                ; for clock-skew detection only; not authoritative
}
```

Negotiation rules:
- Empty wire-proto intersection → server sends `Error { code: PROTOCOL_VERSION_MISMATCH, server_min: …, server_max: … }` and closes. Client MUST surface a "please update" UX with a link to the upgrade page.
- Empty crypto-suite intersection → same as above with `CRYPTO_SUITE_MISMATCH`. **Crypto suite is not optional — there is no fallback path.**
- `doc_schema_floor > client.doc_schema_max` → `DOC_SCHEMA_TOO_OLD`; client MUST update.
- `client.doc_schema_min > server's max known` → server accepts the connection but tags inbound ops `forward-from-newer` and treats unknown fields per [§7](#7-document-schema-forward-compat).

Once negotiated, the chosen versions are immutable for the lifetime of the connection. A client that wants to upgrade reconnects.

---

## 5. Capability bitfield

Capabilities express optional features that don't justify a full version bump. The bitfield is 64 wide; bits 0–31 are server-side features, bits 32–63 are client-side features.

Bits are allocated contiguously. Bits 0–31 are server-side; bits 32–63 are client-side. Bits not listed below MUST be 0 in v1.

Server bits:

| Bit | Name | Meaning when set |
|---|---|---|
| 0 | `SRV_PUSH_APNS` | Server can deliver APNs pushes. |
| 1 | `SRV_PUSH_FCM` | Server can deliver FCM pushes. |
| 2 | `SRV_PUSH_WEB` | Server can deliver Web Push. |
| 3 | `SRV_BLOB_PRESIGN` | Server signs URLs for blob upload/download. |
| 4 | `SRV_RELAY_PAIR` | Server forwards Noise-XX pairing transport. |
| 5 | `SRV_INTEGRATION_GCAL` | Server provides Google Calendar OAuth proxy. |
| 6 | `SRV_BILLING_STRIPE` | Server enforces Stripe-backed quotas. |
| 7 | `SRV_DIAGNOSTIC_UPLOAD` | Server accepts opt-in diagnostic bundles. |

Client bits:

| Bit | Name | Meaning when set |
|---|---|---|
| 32 | `CLI_LORO_LWW_REGISTER` | Client uses Loro LWW Register for scalar fields. |
| 33 | `CLI_LORO_OR_SET` | Client uses Loro OR-Set semantics. |
| 34 | `CLI_FRACTIONAL_INDEX` | Client emits fractional-index sort keys. |
| 35 | `CLI_FTS5_PORTER_EN` | Client's FTS uses unicode61 + porter. |
| 36 | `CLI_PRESENCE_BEACONS` | Client emits presence beacons. |
| 37 | `CLI_DIAGNOSTIC_MODE` | Client supports diagnostic-mode log uploads. |

**Required in v1.** Client MUST set 32, 33, 34, 35. Server MUST set 3. Missing required bit → session refused with `CAPABILITY_REQUIRED_MISSING`. Any other bit unset is a clean degrade.

Unknown bits set by the peer MUST be ignored (forward compatibility) and MUST NOT be echoed back in `HelloAck.capabilities`.

---

## 6. Wire-protocol evolution rules

For `WIRE_PROTO_V`:

1. **Minor change** (no version bump) — adding a new optional field to an existing message, adding a new error code, adding a new capability bit. Old peers ignore unknown fields/codes/bits.
2. **Major change** (version bump) — anything that changes how an existing field is interpreted, removes a field, changes frame layout, changes compression scheme, changes error semantics. A version bump requires:
   - A superseding ADR.
   - Server keeps the previous version listed in `server.wire_proto_supported` for at least **two minor server releases AND ≥ 90 days** after the new version goes live in production. Telemetry `wire_proto_v` gauge tracks how many sessions still negotiate down; deprecation cannot complete while > 0.5% of daily active devices are still on the old version.
   - Client release notes call out the protocol bump.
3. Frame headers and the magic prefix are immutable. A new wire-protocol generation that needs to change them gets a new magic and is treated as a separate transport (clients dial both, server listens on both).

The negotiated `wire_proto` is logged on every session in field `proto.wire`. A server-side metric `sunrise_sync_session_total{wire_proto="…"}` records the distribution.

---

## 7. Document-schema forward compat

`DOC_SCHEMA_V` follows the rules in [02-domain/schema-versioning.md](../02-domain/schema-versioning.md). This section adds the wire-side rules.

CRDT codec rule: **unknown CBOR map keys round-trip unchanged.** A v1 client receiving a v2 op preserves the unknown keys verbatim in storage and re-serializes them on outbound merges. This means:
- v2-only fields persist on v1-only devices.
- v1-only constraints (e.g., "title must be non-empty") are validated on **inbound user edits only**, not on inbound merged ops. A v1 client merging an op that produces an empty title silently accepts it and surfaces a `db.merge.invariant_violated` `warn` log; the UI shows the field as `<untitled>` rather than refusing the merge.

The pair `(doc_schema_floor, client.doc_schema_max)` defines the fence:
- Server's `doc_schema_floor` is the lowest schema the server is willing to relay. Server raises this only when telemetry shows < 0.1% of inbound ops use schemas below the proposed new floor.
- Client refuses to accept a session with `doc_schema_floor > its max`; user is prompted to upgrade.

---

## 8. Crypto suite evolution

`CRYPTO_SUITE_V` is the most conservative surface.

- A new suite means new `aead_alg` / `sig_alg` / `kdf_alg` ids in the op envelope (see [03-crypto/data-encryption-format.md](../03-crypto/data-encryption-format.md)).
- Suite migration is **per-Stream** via key-rotation epoch (see [03-crypto/key-rotation.md](../03-crypto/key-rotation.md)). A client that supports both suites can read both; rotation re-encrypts new ops under the new suite while old ops remain readable until compaction GCs them.
- Suite deprecation requires:
  - All Streams the user owns have rotated past the old suite (client surfaces the list).
  - At least 12 months since the new suite shipped to all client platforms.
  - A superseding ADR.
- The negotiated suite is logged as `proto.crypto`.

There is no per-session crypto-suite mixing. A session uses exactly one suite for transport-level handshake; ops within the session may carry envelopes encrypted under any suite the recipient supports.

---

## 9. Storage schema migrations

`STORAGE_V` is per-device. See [04-storage/migrations.md](../04-storage/migrations.md). Wire/server is unaware of `STORAGE_V`.

Forward compat for storage: a device running schema `N` MUST refuse to open a database whose `STORAGE_V > N`; the user is prompted to upgrade the binary. Downgrade across `STORAGE_V` is supported only if the migration was explicitly marked reversible in its ADR.

---

## 10. Error codes for version mismatch

Defined in [error-handling.md](./error-handling.md). v1 codes related to versioning:

| Code | When |
|---|---|
| `PROTOCOL_BAD_MAGIC` | Magic prefix mismatch on parse. |
| `PROTOCOL_VERSION_MISMATCH` | Wire-proto intersection empty. |
| `CRYPTO_SUITE_MISMATCH` | Crypto-suite intersection empty. |
| `DOC_SCHEMA_TOO_OLD` | Server's `doc_schema_floor` exceeds client's `doc_schema_max`. |
| `DOC_SCHEMA_TOO_NEW` | Client opens a vault written by a binary with `doc_schema_v > local`. |
| `STORAGE_V_TOO_NEW` | Local database file is `STORAGE_V > binary`. |
| `STORAGE_V_TOO_OLD` | Local database needs migration; user prompted. |
| `CAPABILITY_REQUIRED_MISSING` | Negotiation succeeded on versions but a required capability bit is unset on the peer. |

All version-mismatch errors are **permanent** in the sense of [error-handling](./error-handling.md): the session does not retry. The client's task is to surface a clear "update the app" UX, not to back-off.

---

## 11. Logging and metrics

Every log record includes `proto: { wire, doc, crypto }` (see [logging.md](./logging.md) §3). Server-side metrics:

```
sunrise_sync_session_total{wire_proto, crypto_suite, result}
sunrise_sync_session_capabilities{bit}      # counter; per-session OR of capabilities
sunrise_op_envelope_total{aead_alg, sig_alg}
sunrise_storage_migration_total{from_v, to_v, result}
```

These are the data the operator uses to decide when a deprecation window can close.

---

## 12. Test fixtures

The crypto spec describes byte-exact test vectors. This spec adds:

- `tests/fixtures/hello/v1.cbor` — canonical `Hello` and `HelloAck` byte fixtures.
- `tests/fixtures/version-mismatch/*.cbor` — every error path above produces a fixture; clients and server both run a vector test that confirms decode + correct error.
- `tests/fixtures/forward-compat/v1-reads-v2.json` — synthetic v2 op (with extra fields) decoded by a v1 codec; round-trip MUST preserve the v2 fields byte-for-byte.

These fixtures are checked into the repo. Any change to the fixtures is a version-bump ADR.
