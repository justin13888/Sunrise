# 0015 — The envelope container format is versioned separately from the document schema

**Status:** accepted

**Amends:** [ADR-0009 — Protocol versioning spec](./0009-protocol-versioning-spec.md),
which enumerated four version constants and gave them one refusal rule. This
ADR splits one of them in two and gives the halves different rules.

## Context

`docs/10-cross-cutting/protocol-versioning.md` §6 and §7 promise that adding a
field is a **minor** change: older readers keep working, and the new field
round-trips through them untouched. The code made that impossible.

`OpEnvelope.v` was written from `DOC_SCHEMA_V`, the 5-byte magic prefix carried
`DOC_SCHEMA_V`, and `decode_envelope` refused any envelope whose prefix version
was not exactly `DOC_SCHEMA_V`:

```rust
if prefix.kind != MagicKind::OpEnvelope || prefix.version != DOC_SCHEMA_V {
    return Err(OpEnvelopeError::BadMagic);
}
```

One number, three jobs. Bumping `DOC_SCHEMA_V` to add a field to `Task` would
have changed the magic prefix on every envelope written afterwards **and** made
every envelope written before it undecodable — including the ones already
sitting in the local op log. The documented minor change was in fact the most
destructive operation in the system, and nothing in the code said so.

The two things being conflated are not the same kind of thing:

* The **container format** is where the bytes are: the field layout, the
  canonical CBOR ordering, which fields the AAD covers, which fields the
  signature covers. A reader that does not implement it cannot locate the
  payload. It cannot decrypt, cannot verify, and cannot salvage anything.
* The **document schema** is what the payload means: the shape of a `Task`, a
  `Stream`, a `Routine`. A reader that does not implement it can still find the
  payload, authenticate it, decrypt it, store it, and relay it. It merely may
  not understand some fields inside it — which §7 says it must preserve rather
  than reject.

## Decision

**Two constants, two refusal rules.**

`ENVELOPE_FORMAT_V` (currently `3`) versions the container and rides in the
magic prefix and in envelope field 1. A mismatch is a hard reject at the prefix,
before any CBOR is parsed.

`DOC_SCHEMA_V` (currently `2`) versions the payload and rides in a new envelope
**field 12**. A decoder accepts any value `>= DOC_SCHEMA_FLOOR` and refuses
below it with a typed `DocSchemaTooOld`, distinct from `BadMagic`.

`DOC_SCHEMA_FLOOR` (currently `1`) is the lowest schema this build can still
interpret. It moves only when a shape stops being readable — never merely
because a newer one exists. `Hello` now advertises the floor as
`doc_schema_min` and `DOC_SCHEMA_V` as `doc_schema_max`, instead of the same
number for both, which had been telling every peer that this build could not
read its own predecessor.

### Field 12 is inside the AAD and the signature

Deterministic CBOR orders non-negative integer keys numerically, so field 12
encodes as `0x0c` and sorts **after** field 11 (`sig`). It is nonetheless
covered by both the AEAD associated data and the signature input, because both
are now defined by which fields they **exclude** rather than by an upper bound:

* AAD = the canonical map with fields 10 and 11 removed.
* Signature input = the canonical map with field 11 removed.

This is why the three encodings collapsed into one function parameterised by an
`Omit`. Written as "fields 1..=9" and "fields 1..=10" — which is what they were
— every future field would silently escape both, and a field outside the
signature is a field an attacker can rewrite.

## What we give up

* **A third number to reason about.** `ENVELOPE_FORMAT_V`, `DOC_SCHEMA_V`, and
  `DOC_SCHEMA_FLOOR` where there was one. The cost is real; the alternative is
  a system that cannot honour its own documented compatibility rule.
* **The frozen v1 envelope vectors moved.** Field 12 changes the byte layout,
  so `sunrise-crypto-test-vectors` was re-frozen. This is a container change,
  not a crypto-suite change — the primitives, contexts, and key schedule are
  untouched — so `CRYPTO_SUITE_V` does not move and ADR-0004 is unaffected.
* **Dev vaults written before the split no longer decode.** Sunrise is
  pre-release, and [ADR-0018](./0018-storage-baseline-reset.md) refuses those
  vaults outright rather than leaving them to fail one op at a time.

## Alternatives considered

| Option | Why rejected |
|---|---|
| **Keep one number, relax the check to `>=`** | The prefix version would still change when the doc schema did, so a v1 reader would see a prefix it had never been told about and could not distinguish "newer payload" from "different container". It also leaves the AAD and signature definitions bounded above, so a later field escapes both. |
| **Put `doc_schema_v` outside the envelope** (in the frame, or an unsigned annotation) | The payload's schema is a property of the payload, and anything outside the signature can be rewritten by a relay. Sunrise's threat model treats the relay as untrusted. |
| **Reuse field 1 for both, as a packed pair** | Saves a field id and costs a decoding rule nobody reading the CDDL would guess. Field ids are cheap. |
| **Two constants, field 12, exclusion-defined AAD and signature (chosen)** | Each number has exactly one job and one refusal rule, and the rules differ because the failure modes differ. |

## Consequences

* **Adding a field to an entity is now genuinely minor.** Bump `DOC_SCHEMA_V`;
  older readers keep decoding, preserve the field (see
  [§7 and the forward-compat implementation](../10-cross-cutting/protocol-versioning.md)),
  and re-emit it. [ADR-0017](./0017-sunrise-time-representation.md) is the first
  real exercise of this: it took `DOC_SCHEMA_V` to 2 and left the floor at 1.
* **`ENVELOPE_FORMAT_V` is the only number a reader may refuse on layout
  grounds.** Moving it is a coordinated release, exactly as `WIRE_PROTO_V` is.
* **The exclusion rule is load-bearing.** Anyone adding envelope field 13 must
  not reintroduce an upper bound in `encode_cbor`; the `Omit` enum exists to
  make that hard to do by accident.

## What would force revisiting this

1. **A container change that is not backward-compatible in the prefix** — e.g.
   moving the magic prefix itself. That is a `MagicKind` question, not a version
   question, and needs its own ADR.
2. **A doc-schema change that genuinely cannot be read by an older build.** The
   floor exists for exactly this, but raising it strands every device that has
   not upgraded, so it needs the telemetry gate in §7 and an ADR of its own.
