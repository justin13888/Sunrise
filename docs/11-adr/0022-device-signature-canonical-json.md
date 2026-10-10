# 0022 — `header_sig_v2` signs canonical JSON (RFC 8785), not the received bytes

**Status:** accepted

**Amended (2026-10):** the server canonicalizes the JSON value it *received*,
not the `T` it parsed, and the sync request bodies stop refusing unknown fields.
"Request bodies reject unknown fields" was what made re-canonicalising `T`
sound; verifying over the received value makes the parse irrelevant to the
signature, so the rule is no longer load-bearing and the sync surface gives it
up. See [Amendment](#amendment-2026-10-the-received-value-is-what-is-verified).

**Forced by** [ADR-0021](./0021-kynos-openapi-server.md).
**Amends** [`docs/06-server/auth.md`](../06-server/auth.md), whose device-binding
section describes the v1 construction.

## Context

`X-Sunrise-Device-Sig` binds a request to a registered device by signing

```
sunrise-device-sig-v1\n<METHOD>\n<path?query>\n<Date>\n<blake3-hex(body)>
```

with the device's Ed25519 key, inside a ±300 s window measured against an
injected clock. It works, it is tested, and the byte layout is defined **only in
`crates/sunrise-server/src/auth/device_sig.rs`** — the module says so itself:
"The document does not pin a byte layout, so this does."

The construction hashes the body *as received*. That is why every authenticated
handler takes `body: Bytes`: deserialising first would leave the signature
checking a re-serialisation rather than the bytes the device signed.

[ADR-0021](./0021-kynos-openapi-server.md) removes raw-body access. `kynos` has
no `Request`, `Body` or `HeaderMap` extractor by design, and its `Unchecked<T>`
escape hatch marks an operation's generated documentation non-authoritative —
which would apply to exactly the signed routes. So v1's construction and an
authoritative document cannot both survive.

## Decision

**`header_sig_v2` signs the RFC 8785 JSON Canonicalization Scheme form of the
request *value*, not of the octets that carried it.**

```
sunrise-device-sig-v2\n<METHOD>\n<path?query>\n<Date>\n<blake3-hex(JCS(value))>
```

Both sides compute `JCS(value)` from the typed value: the client from the value
it is sending, the server from the `T` that kynos parsed. Transport-level
variation — key order, whitespace, escaping — stops mattering, which is what JCS
is for. `METHOD`, `path?query` and `Date` stay inside the signature, so the
replay window and the method/path binding are unchanged.

**Request bodies on the API surface reject unknown fields** (`serde(deny_unknown_fields)`).
This is the load-bearing half of the decision, and it is what makes
re-canonicalisation sound: a signature over a re-serialisation verifies only if
the parse is lossless, and a silently-dropped field is exactly a lossy parse.
Rejecting the field turns a would-be signature mismatch into a typed 400 that
names the offending member.

That is the opposite of the rule the op log follows, deliberately. The two
layers have different shapes:

* **Ops** are end-to-end encrypted peer data that an *older* client must merge
  without destroying a newer one's fields, so unknown keys round-trip verbatim
  (`sunrise-domain`'s `unknown` map). A reader there is routinely behind a writer.
* **API requests** are a versioned client/server contract in which the server is
  never behind the client — it is the thing being deployed to. An undocumented
  member is a client bug or an attack, not a newer peer, and the OpenAPI document
  is authoritative about what exists.

Floats stay forbidden, matching `sunrise_cbor::CborValue`'s existing refusal, so
JCS's number-serialisation rules are never exercised on a value the codebase
treats as inadmissible anyway.

The canonical string is specified in [`docs/06-server/auth.md`](../06-server/auth.md)
as a byte layout rather than left to the implementation, correcting the gap
`device_sig.rs` recorded against v1.

### Bodies that are not JSON

A chunk upload is raw ciphertext and has no JSON value to canonicalize. The
rule generalises rather than needing a second scheme: what is hashed is the
body's **canonical form**, and for a binary body the bytes already are it —
there is no key order, whitespace or escaping to normalise away. JSON bodies
reach the same hash through JCS first.

So a chunk's signature covers those exact bytes, which is the property the blob
store's own content hashing already asserts from the other direction.

## Amendment (2026-10): the received value is what is verified

### What the original decision said

> Both sides compute `JCS(value)` from the typed value: the client from the value
> it is sending, the server from the `T` that kynos parsed.

> **Request bodies on the API surface reject unknown fields** (`serde(deny_unknown_fields)`).
> This is the load-bearing half of the decision […] An undocumented member is a
> client bug or an attack, not a newer peer.

And in Consequences, that `deny_unknown_fields` makes adding a request field a
breaking change for clients that send it to an older server.

### What changed

The premise "the server is never behind the client" does not hold for the sync
surface. A self-hoster's relay lags its clients by months, and the fields a newer
client is expected to add next to the capability bitfield — ADR-0045's schema
fingerprint, a `vault_requires` list, a relay floor — are exactly the ones an
older relay has never heard of. With every sync body refusing them, no such
field could ever be added without breaking sync against every relay not yet
upgraded (#370). [protocol-versioning.md](../10-cross-cutting/protocol-versioning.md)
§4 and §6 now make the sync bodies open to extension.

Opening the bodies alone is not enough, and is unsound under the original
construction: `T` drops the unknown field, so `JCS(T)` no longer equals what the
client signed, and every device-signed request carrying one is refused as
`AUTH_DEVICE_SIG_INVALID` — on every relay that requires the binding.

### The amended rule

* **The server canonicalizes the JSON value as received.** `Signed<T>` and
  `SignedBootstrap<T>` read the body as a JSON value, verify the signature over
  `JCS(value)`, and only then read `T` *from that same value*. The client already
  signs the value it sends (`sunrise-sync`'s transport signs the
  `serde_json::Value` it posts), so both sides canonicalize the same thing by
  construction, and nothing the handler acts on lies outside what was verified.
  An unknown field is therefore *inside* the signature: ignored by the handler,
  but not alterable in flight.
* **Strictness is per body, and no longer a signature concern.** The sync bodies
  ignore unknown fields. The account, device, pairing and blob bodies keep
  `deny_unknown_fields`, because the original reasoning still holds for them —
  they are not the negotiation path, and a misspelt member there should fail
  loudly. Either choice now verifies correctly.
* **Everything else is unchanged:** the canonical string, JCS, the floats rule,
  binary bodies, and verification after parsing (a malformed body is still a
  400 before it is a 401, and a body that does not fit `T` still a 422).

### What it costs

A body with a duplicate member is read last-wins, as the JSON value holds it,
rather than refused by `T`'s derive. The signature and the handler both see the
same last-wins value, so it is no route to acting on something unsigned; it is
only a lost 422 for a body no conforming client produces.

## Alternatives considered

| Option | Why not |
|---|---|
| **`Unchecked<T>` on signed routes, keeping v1** | No wire change and no client work, and it marks the account, device and blob operations non-authoritative in the generated document — the subset a client most needs described. It spends ADR-0021's entire benefit on its most important routes. |
| **Verify raw bytes in a layer beneath kynos** | Keeps v1 byte-for-byte, but requires a layer that consumes and replays the body before kynos parses it, and leaves two components disagreeing about what the request *is*. It also reintroduces the untyped surface one level down rather than removing it. |
| **Canonical CBOR bodies instead of JSON** | Consistent with the envelope encoding, and it already has a canonical form the workspace enforces. But OpenAPI 3.2 tooling, `spargen`, and the document's own schema vocabulary are JSON-shaped; CBOR request bodies would be described as opaque strings, recreating the ADR-0021 problem in a new place. |
| **Sign a field subset the server names** | Smallest signed payload, and it means an unsigned field can be tampered with in flight. Rejected outright. |

## Consequences

* Signature verification moves after parsing, so a malformed body now fails as a
  400 before it fails as a 401. The ordering is stated in `auth.md` because it is
  observable.
* ~~`deny_unknown_fields` makes adding a request field a breaking change for
  clients that send it to an older server — correct for a client/server contract,
  and the reason the OpenAPI document is versioned.~~ *Amended (2026-10): still
  true of the account, device, pairing and blob bodies; the sync bodies accept
  unknown fields, and the signature covers them. See the amendment above.*
* The client half must implement JCS. It is small and well-specified, and both
  sides share it through one crate rather than reimplementing per platform.
* v1 signatures are not accepted. Nothing is deployed, so there is no window to
  support both, and supporting both would mean retaining raw-body access — the
  thing this ADR exists to remove.
* `POST /accounts` and `POST /devices` keep their bootstrap exemption: a device
  cannot sign before it exists. That reasoning is unchanged by the construction.
