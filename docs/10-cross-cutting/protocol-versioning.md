---
status: accepted
---

# Protocol versioning

Sunrise has five independent versioned surfaces. Each evolves on its own schedule. A single client release pins exact versions of all five. The wire-protocol exchange negotiates the highest version both peers understand at session start; mismatch is a hard failure with a known error code.

This document is the authoritative table of version numbers and the rules for changing them. Any change to any version constant requires a superseding ADR in [`11-adr/`](../11-adr/).

---

## 1. Versioned surfaces

| Surface | Constant | What it covers |
|---|---|---|
| **Wire protocol** | `WIRE_PROTO_V` | Frame layout, message kinds, error codes, compression rules — see [05-sync/wire-protocol.md](../05-sync/wire-protocol.md). |
| **Envelope container** | `ENVELOPE_FORMAT_V` | `OpEnvelope` field layout, canonical ordering, AAD, signature input — see [ADR-0015](../11-adr/0015-envelope-doc-schema-split.md). |
| **Document schema** | `DOC_SCHEMA_V`, `DOC_SCHEMA_FLOOR` | Per-entity field shapes — see [02-domain/schema-versioning.md](../02-domain/schema-versioning.md). |
| **Crypto suite** | `CRYPTO_SUITE_V` | AEAD, signature, KDF, HPKE choices and parameters — see [03-crypto/primitives.md](../03-crypto/primitives.md). |
| **Storage schema** | `STORAGE_V` | SQLite/SQLCipher schema — see [04-storage/migrations.md](../04-storage/migrations.md). |

**Every one of these constants is defined in exactly one file:
`crates/sunrise-cbor/src/version.rs`.** There is no per-surface constant in
`sunrise-sync`, `sunrise-domain`, `sunrise-crypto` or `sunrise-storage`, and a
second definition MUST NOT be introduced: the magic prefix, the `Hello`
exchange, the envelope header and the migration runner all read the same `u16`,
and two of them disagreeing is a wire break no test would catch. Consumers
import from `sunrise_cbor::version`.

`STORAGE_V` is purely local; it never appears on the wire. The other four are negotiated or carried in the structure they version.

---

## 2. v1 version constants

```
WIRE_PROTO_V      = 1
ENVELOPE_FORMAT_V = 3
DOC_SCHEMA_V      = 4
DOC_SCHEMA_FLOOR  = 1
CRYPTO_SUITE_V    = 1
STORAGE_V         = 16
```

Wire frames, op envelopes, recovery blobs, and storage rows all carry their respective version constants.

**Two different refusal rules, on purpose** (see [ADR-0015](../11-adr/0015-envelope-doc-schema-split.md)):

| Constant | On mismatch | Why |
|---|---|---|
| `WIRE_PROTO_V`, `ENVELOPE_FORMAT_V`, `CRYPTO_SUITE_V` | hard refusal | The reader does not know the LAYOUT. It cannot find the payload, so there is nothing to salvage and a guess would be worse than an error. |
| `DOC_SCHEMA_V` | accept when `>= DOC_SCHEMA_FLOOR` | The reader knows the layout and can authenticate and decrypt the payload; it merely may not understand some fields *inside* it, which §7 says it must preserve rather than reject. |

`ENVELOPE_FORMAT_V` versions the op-envelope container (field layout, canonical ordering, AAD, signature input) and rides in the magic prefix. `DOC_SCHEMA_V` versions the entity shapes and rides in envelope field 12. Collapsing them into one number — which v1 did — meant that adding a field to a Task changed the magic prefix and made every already-signed envelope undecodable, the exact opposite of the rule in §6.

`DOC_SCHEMA_FLOOR` is the lowest schema this build can still interpret, and it moves only when a shape stops being readable — never merely because a newer one exists. It is `1` while `DOC_SCHEMA_V` is `4` because a v1 payload really does still decode: its bare-instant time fields read as `SunriseTime::Instant`, and every change since has been an added field that defaults.

One caveat the "any field is additive" rule does not cover: a new **op variant** is not a new field. `DOC_SCHEMA_V = 3` introduced the `blk_` and `att_` op families, and a build that predates them cannot decode one — it reports an invalid remote op rather than applying it wrongly. That is acceptable only pre-1.0, where no such build exists ([ADR-0018](../11-adr/0018-storage-baseline-reset.md)); after 1.0 a new op family needs a negotiated capability bit, not a schema bump.

`DOC_SCHEMA_V = 4` is a stronger case still: it changed the **shape of
existing variants**, not just added new ones. All six delete ops —
`TaskDelete`, `StreamDelete`, `ContextDelete`, `RoutineDelete`, `BlockDelete`,
and `AttachmentDelete` — went from carrying a bare `EntityRef` to carrying the
full entity, so a v3 build reading a v4 delete does
not merely miss a field — it cannot decode the variant at all, and a v4 build
reading a v3 delete would be missing the state it now relies on. Neither
direction is salvageable by the additive rule, and the only reason it is
acceptable is the same pre-1.0 licence: there are no deployed builds to strand.
After 1.0 this would need a new variant alongside the old one, not a
redefinition. The change itself is required for correctness — an id-only delete
cannot converge under entity-level LWW
([ADR-0014](../11-adr/0014-entity-level-lww-merge.md)).

`STORAGE_V` is per-device and never appears on the wire. It is `16`; the *floor* is a separate constant, `BASELINE_STORAGE_V = 13`, the pre-1.0 baseline reset ([ADR-0018](../11-adr/0018-storage-baseline-reset.md)), and a vault below **that** is refused rather than upgraded. Three migrations have been appended since the reset — `0014_stream_sort_order.sql`, `0015_entity_extra_columns.sql` and `0016_stream_description_and_default_context.sql` — so the two numbers have parted company and should not be quoted as one — a vault at 13 upgrades, a vault at 12 is refused.

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
- Empty wire-proto intersection → server sends `Error { code: SYNC_PROTOCOL_VERSION_MISMATCH }` and closes. (`ErrorPayload` carries `code` and a diagnostic `reason` only; there are no `server_min` / `server_max` fields on the wire.) Client MUST surface a "please update" UX with a link to the upgrade page.
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
| 8 | `SRV_TOKEN_REFRESH` | Server accepts `0x12 RefreshToken` mid-session and answers `0x13 RefreshTokenAck`. |

Client bits:

| Bit | Name | Meaning when set |
|---|---|---|
| 32 | `CLI_ENTITY_LWW` | Client resolves concurrent writes by entity-level LWW over `(hlc, device_id, seq)`. |
| 33 | `CLI_HLC_TIMESTAMPS` | Client stamps every op with a hybrid logical clock and refuses one beyond the drift window. |
| 34 | `CLI_FORWARD_COMPAT` | Client preserves and re-emits unknown CBOR map keys. |
| 35 | `CLI_FTS5_PORTER_EN` | Client's FTS uses unicode61 + porter. |
| 36 | `CLI_PRESENCE_BEACONS` | Client emits presence beacons. |
| 37 | `CLI_DIAGNOSTIC_MODE` | Client supports diagnostic-mode log uploads. |

**Required in v1.** Client MUST set 32, 33, 34, 35. Server MUST set 3. Missing required bit → session refused with `CAPABILITY_REQUIRED_MISSING`. Any other bit unset is a clean degrade. This is enforced, not aspirational: `REQUIRED_CLIENT_BITS` and `REQUIRED_SERVER_BITS` in `crates/sunrise-wire-protocol/src/capability.rs` hold exactly those bits, `Hello::negotiate` refuses a peer missing any of them, and `the_redefined_client_bits_keep_their_positions` pins 32–35 so the redefinition below cannot become a silent renumbering.

Bit 34 is the one to read alongside [§7](#7-document-schema-forward-compat). A client that asserts `CLI_FORWARD_COMPAT` has promised to re-emit unknown map keys byte-for-byte, and an entity whose table cannot persist them breaks that promise on the first restart — which is why §7's `extra` column is a MUST and not a nicety. A build that cannot keep the promise MUST NOT set the bit, and a build that does not set it is refused.

> **Bits 32–34 were redefined.** They originally asserted three Loro CRDT
> capabilities — `CLI_LORO_LWW_REGISTER`, `CLI_LORO_OR_SET`,
> `CLI_FRACTIONAL_INDEX`. [ADR-0014](../11-adr/0014-entity-level-lww-merge.md)
> replaced CRDT merge with entity-level LWW and deleted Loro, so for the whole
> of v1 these REQUIRED bits asserted that a client implemented three things
> nothing in the codebase does: a peer setting them was telling the truth about
> nothing, and a peer refusing them was refused for the wrong reason. They now
> name what v1 actually requires. The bit POSITIONS are unchanged, and the
> redefinition is safe only because nothing ever shipped that read the old
> meanings — a redefinition after 1.0 would be a `WIRE_PROTO_V` bump.

> **Why bit 8 is negotiated rather than assumed.** A server that accepts a
> `0x12` and one that has never heard of it are both *silent*, and the client's
> correct response differs: keep the session, or stop trusting the credential
> and let the session die. A client MUST NOT send `0x12` unless it saw bit 8
> come back agreed in `HelloAck.capabilities`; without it, renewal happens by
> reconnecting, which every server understands. Adding the bit and the `0x13`
> frame is a **minor** change under §8.1 — a new capability bit and a frame
> sent only when negotiated — so `WIRE_PROTO_V` stays at 1.

Unknown bits set by the peer MUST be ignored (forward compatibility) and MUST NOT be echoed back in `HelloAck.capabilities`.

---

## 6. Wire-protocol evolution rules

For `WIRE_PROTO_V`:

1. **Minor change** (no version bump) — adding a new optional field to an existing message, adding a new error code, adding a new capability bit. Old peers ignore unknown fields/codes/bits.
2. **Major change** (version bump) — anything that changes how an existing field is interpreted, removes a field, changes frame layout, changes compression scheme, changes error semantics. A version bump requires:
   - A superseding ADR.
   - Server keeps the previous version listed in `server.wire_proto_supported` for at least **two minor server releases AND ≥ 90 days** after the new version goes live in production. Deprecation cannot complete while > 0.5% of daily active devices are still on the old version. The telemetry that would measure this does not exist yet — see [§11](#11-logging-and-metrics) — so today the window is enforced by the calendar alone.
   - Client release notes call out the protocol bump.
3. Frame headers and the magic prefix are immutable. A new wire-protocol generation that needs to change them gets a new magic and is treated as a separate transport (clients dial both, server listens on both).

The negotiated `wire_proto` is **not** logged per session, and no per-session
record carries it. `wire_v` / `doc_v` / `crypto_v` appear once per process, on
the `srv.start` line, which is the trade `crates/sunrise-log/src/proto.rs`
records: restating three constants that cannot change while the process lives
would cost roughly 50 bytes on every record to say what one line already says.
`srv.sync.session_open` (`crates/sunrise-server/src/api/sync.rs`) carries
`account_h` and nothing about the protocol.

For today's server that loses nothing, because the server offers exactly one
wire version — `Hello::negotiate` is called with `&[WIRE_PROTO_V]` — so the
negotiated value is the compiled one and the startup line already reports it.
It stops being sufficient the day the server lists two, which is the same day
§6's deprecation window needs measuring. The per-version metric that would let
an operator read the distribution without parsing logs is target state
([§11](#11-logging-and-metrics)), and it is what should land with the second
version rather than after it.

---

## 7. Document-schema forward compat

`DOC_SCHEMA_V` follows the rules in [02-domain/schema-versioning.md](../02-domain/schema-versioning.md). This section adds the wire-side rules.

Codec rule: **unknown CBOR map keys round-trip unchanged.** A v1 client receiving a v2 op preserves the unknown keys verbatim in storage and re-serializes them on outbound merges.

This is implemented in three places, and it needs all three:

1. Every entity carries `#[serde(flatten)] unknown: Unknowns` (`sunrise_domain::unknown`), so an unfamiliar field is kept rather than discarded by serde's default behaviour. The one exception is `Interruption`, whose whole value is its primary key.
2. **Every synced entity's table persists that map in its own `extra BLOB` column**, so the field survives materialization rather than living for one transaction. This is a contract, not a per-table convenience: an entity whose table drops `extra` re-emits a truncated map on its next outbound merge, and — because the envelope signature covers every field the sender wrote — the re-emitted envelope no longer verifies. A table added to `0013_baseline.sql` for a synced entity MUST carry `extra`.
3. `encode_canonical` sorts map keys by their encoded bytes, so the preserved field is re-emitted in the position its author put it in. Without sorting, byte-exact re-emission is impossible: the writer put its new field where it declared it, and a reader can only append.

Every enum that rides the wire also degrades rather than rejecting: an unrecognised variant reads as a named fallback chosen to be the SAFE reading (an unknown task state is `todo`, never `done`; an unknown constraint severity is `soft`, never `hard`). Rejecting one field's value would reject the whole op, and two replicas would then diverge permanently over one string. `RRule`'s `Frequency` and `Weekday` are deliberately NOT lossy: silently recurring on the wrong schedule is worse than failing the routine.

This means:
- v2-only fields persist on v1-only devices.
- v1-only constraints (e.g., "title must be non-empty") are validated on **inbound user edits only**, not on inbound merged ops. A v1 client merging an op that produces an empty title accepts it and shows the field as `<untitled>` rather than refusing the merge.

  It accepts it **silently**: `Engine::apply_remote` emits no diagnostic when a merged entity violates a local invariant, and no `db.merge.invariant_violated` event exists anywhere in the tree. That is a gap, not a design: the acceptance is correct — refusing would diverge two replicas permanently over one string — but an operator currently cannot distinguish "no invariant was ever violated" from "every one was swallowed". The event SHOULD be added at `warn` on the `apply_remote` path and registered in [log-events.md](./log-events.md).

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
- The negotiated suite is **not logged anywhere**. Negotiation is per session (`max_intersection` in `crates/sunrise-wire-protocol/src/negotiation.rs`), and its result is discarded for observability purposes: `srv.sync.session_open` (`crates/sunrise-server/src/api/sync.rs`) carries `account_h` and nothing about the suite. The `crypto_v` on the startup line is a different number — the binary's compiled `CRYPTO_SUITE_V`, what this process *supports*, not what any session *chose*. Nor do the metrics cover it: `sunrise_sync_session_total` is incremented once per session but is unlabelled. The registry's `render` passes a name containing `{` through verbatim, so a labelled series is expressible as a string, but nothing constructs one — the single `incr` site at `api/sync.rs` would have to build the name (see [§11](#11-logging-and-metrics)).

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
| `SYNC_PROTOCOL_VERSION_MISMATCH` | Wire-proto intersection empty. Spelled with the `SYNC_` prefix in `crates/sunrise-error/src/codes.rs`; there is no `PROTOCOL_VERSION_MISMATCH`. |
| `CRYPTO_SUITE_MISMATCH` | Crypto-suite intersection empty. |
| `DOC_SCHEMA_TOO_OLD` | Server's `doc_schema_floor` exceeds client's `doc_schema_max`. |
| `DOC_SCHEMA_TOO_NEW` | Client opens a vault written by a binary with `doc_schema_v > local`. |
| `STORAGE_V_TOO_NEW` | Local database file is `STORAGE_V > binary`. |
| `STORAGE_V_TOO_OLD` | Local database needs migration; user prompted. |
| `CAPABILITY_REQUIRED_MISSING` | Negotiation succeeded on versions but a required capability bit is unset on the peer. |

All version-mismatch errors are **permanent** in the sense of [error-handling](./error-handling.md): the session does not retry. The client's task is to surface a clear "update the app" UX, not to back-off.

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
     Last extracted: 310e377 -->

```
sunrise_sync_{negotiate_refused,session,refresh,stream}_total
sunrise_relay_append_failed_total       sunrise_relay_cursor_gap_total
sunrise_device_sig_rejected_total       sunrise_account_create_total
sunrise_devices_{list,register,revoke}_total
sunrise_blob_{init,chunk,finalize,fetch}_total
sunrise_blob_hash_mismatch_total
sunrise_push_register_total             sunrise_push_{apns,fcm,web}_total
```

Twenty names. The full list with its provenance lives in
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
- `tests/fixtures/forward-compat/v1-reads-v2.cbor` — a synthetic envelope at `DOC_SCHEMA_V + 1` carrying two envelope fields this build does not know (ids 13 and 40, the second above 23 so its CBOR key needs two bytes). The round trip preserves them byte-for-byte.

  CBOR, not the JSON this section originally named: the artefact under test is a signed, canonically encoded envelope, and JSON cannot represent one without a re-encoding step that would be the thing actually being tested.

These fixtures are checked into the repo. Any change to them must be a deliberate version bump — which is why regeneration sits behind `SUNRISE_REGEN_FIXTURES=1` rather than happening automatically.

Three of them are built from the live constants and therefore move with a
version bump *by construction*: `hello/*.cbor` and the `version-mismatch`
fixtures embed `doc_schema_max`, and `forward-compat/v1-reads-v2.cbor` is
defined as `DOC_SCHEMA_V + 1`. Regenerating them alongside a bump is expected
and is not the drift this guard exists to catch; regenerating them without one
is. The same distinction applies to the two whole-envelope crypto vectors,
which carry the document schema in field 12 — a doc-schema bump moves their
bytes with no crypto change, and no other frozen vector has that excuse.

The forward-compat fixture also pins the reason preservation is not optional: the envelope signature covers every field the sender wrote, so a decoder that DROPS an unknown field cannot re-emit an envelope that still verifies. `dropping_an_unknown_field_breaks_the_senders_signature` asserts exactly that.
