---
status: accepted
---

# Data Encryption Format

The byte-exact wire and storage format for ops, blob chunks, and HPKE single-shot ciphertexts. Encoded as canonical CBOR (RFC 8949 §4.2.1, "deterministic encoding").

## Canonical CBOR enforcement

Implementations MUST use `ciborium` ≥ 0.2 in deterministic mode wrapped by `sunrise_crypto::canonical_cbor`. The wrapper, on **decode**, verifies:

1. All map keys are sorted by encoded byte form (RFC 8949 §4.2.1 length-then-lex on the key bytes).
2. No indefinite-length items.
3. Integers use minimal encoding (no leading-zero `uint`).
4. No floating-point types appear in any envelope (use scaled integers).
5. Strings and byte strings are length-prefixed in single chunks.

Any violation aborts decryption with `CRYPTO_NON_CANONICAL_CBOR` before any AEAD attempt. The wrapper is the only entry point — direct `ciborium::de::from_reader` on envelope bytes is forbidden by clippy lint.

## Op envelope

Every op (in-stream or control) is wrapped in a single envelope type. The envelope is the unit of storage in the `ops` table and the unit of transport on the wire.

Every persisted/transmitted envelope begins with the unified 5-byte magic prefix from [`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md) §3: `"SR" + kind=2 + version=ENVELOPE_FORMAT_V (uint16 big-endian, currently 3)`. The prefix carries the **container format** version, not the document schema — see [ADR-0015](../11-adr/0015-envelope-doc-schema-split.md). Readers MUST verify the magic before any CBOR parse; mismatch → `PROTOCOL_BAD_MAGIC` and discard. The prefix is not part of the canonical CBOR; the bytes that follow are.

```cddl
OpEnvelope = {
    1: uint,            ; v             (ENVELOPE_FORMAT_V, = 3; matches the magic-prefix version)
    2: bstr .size 16,   ; stream_id     (raw 128-bit; see below for special values)
    3: bstr .size 16,   ; device_id     (raw 128-bit; signing device)
    4: uint,            ; seq           (per-(stream_id, device_id) monotonic; starts at 1)
    5: [uint, uint],    ; hlc           ([physical_ms, logical]; hybrid logical clock)
    6: uint,            ; aead_alg      (1 = XChaCha20-Poly1305; 0 = none/control)
    7: uint,            ; sig_alg       (1 = Ed25519)
    8: uint,            ; epoch         (Stream-key epoch used for ciphertext; 0 if aead_alg = 0)
    9: bstr .size 24,   ; nonce         (random 192-bit; ignored if aead_alg = 0)
    10: bstr,           ; ciphertext_or_payload
                        ;   if aead_alg = 1: AEAD output (Op encoded as canonical CBOR, then encrypted)
                        ;   if aead_alg = 0: canonical-CBOR-encoded Op (signed-only control envelope)
    11: bstr .size 64,  ; sig           (Ed25519; see signature rules)
    12: uint,           ; doc_schema_v  (DOC_SCHEMA_V of the inner Op; >= the reader's floor)
}
```

### Two versions, two rules

Field `1` versions the **container**: the field layout, the canonical ordering, the AAD construction, and the signature input. Field `12` versions the **document schema** of the payload.

A reader treats them differently, because the failure modes differ:

| | Mismatch means | Reader behaviour |
|---|---|---|
| `v` (field 1, and the magic prefix) | "I do not know where the bytes are" | Hard reject — `PROTOCOL_BAD_MAGIC` |
| `doc_schema_v` (field 12) | "I may not know what some payload fields mean" | Accept when `>= DOC_SCHEMA_FLOOR`; reject below it as `DOC_SCHEMA_TOO_OLD` |

This is what makes the documented "adding a field is a minor change" true. Before the split, both roles were played by one number, so bumping `DOC_SCHEMA_V` to add a Task field changed the magic prefix and made every already-signed envelope undecodable.

Field ids are non-negative CBOR integers, so deterministic (RFC 8949 §4.2.1) key ordering is numeric ordering: field `12` encodes as `0x0c` and sorts *after* field `11`. It is still covered by both the AAD and the signature, because both are defined by the fields they **exclude** — see below.

### `hlc` — field 5

A **hybrid logical clock**, not a wall clock: `[physical_ms, logical]`. It is
the ordering key for last-writer-wins, and the reason it is not a bare
`ts_ms` is that a bare `ts_ms` gave a device with a fast clock a permanent veto
over every peer (issue #21). The merge rules, the receive rule, and the
`MAX_DRIFT_MS` bound live in
[`../05-sync/conflict-resolution.md`](../05-sync/conflict-resolution.md) and
[ADR-0016](../11-adr/0016-hlc-timestamps.md).

`hlc.physical_ms` is also the wall-clock reading used by "Signature key
resolution" below — an HLC's physical component only ever runs at or ahead of
the emitting device's true clock, never behind it.

### Special `stream_id` values

| Value | Meaning |
|---|---|
| `0x00…00` (16 zero bytes) | Vault-meta log (per-identity, holds DeviceCerts, share grants, settings) |
| Any other 16-byte value | A regular Stream's op log |

### Per-(stream, device) `seq`

- Starts at `1`; increases by exactly 1 per emitted op.
- A receiver detecting a gap surfaces a sync warning (see [`audit-and-tamper-evidence.md`](./audit-and-tamper-evidence.md)).
- A receiver detecting `seq` reuse with non-identical envelope bytes treats the Stream as compromised and stops sync; user is shown an integrity warning.

### AAD construction

When `aead_alg = 1`, the AEAD AAD is the canonical CBOR encoding of the envelope map **with field `10` (ciphertext) and field `11` (sig) removed**:

```
aad = canonical_cbor({
    1: v, 2: stream_id, 3: device_id, 4: seq, 5: hlc,
    6: aead_alg, 7: sig_alg, 8: epoch, 9: nonce, 12: doc_schema_v
})
```

This binds every metadata field to the ciphertext. The rule is stated by **exclusion**, not as "fields 1..9": any field added in a later container format is inside the AAD by construction, and cannot be rewritten by an attacker who does not hold the Stream key.

### Signature

Always computed (regardless of `aead_alg`):

```
sig_input = "sunrise.op_envelope.v1" || BLAKE3(canonical_cbor_without_field_11, 32)
sig       = Ed25519_sign(D_S_priv, sig_input)
```

Where `canonical_cbor_without_field_11` is the envelope encoded with all fields except `11` (sig) — currently `{1..10, 12}`. As with the AAD, the rule is by exclusion, so a field added later is signed automatically.

The signature covers the ciphertext (or signed-only payload) bit-for-bit and all metadata, so:

- Signature alone authenticates the message even if the AEAD key is later disclosed.
- AEAD alone authenticates the AAD, which already includes every metadata field.
- Both together give belt-and-suspenders assurance against forgery by anyone who later obtains the Stream key.

### Signature key resolution

When verifying an op envelope's signature, select the device cert in force at `envelope.hlc.physical_ms` (written `ts_ms` below):

```
device_certs = vault_meta.device_certs[envelope.device_id]   // 1..N records
candidate    = device_certs
                .filter(c => c.created_at_ms <= envelope.ts_ms)
                .filter(c => no device_revoke r exists with r.device_id = envelope.device_id
                             AND r.effective_at_ms <= envelope.ts_ms)
                .max_by_key(c => c.created_at_ms)
if candidate.is_none(): reject CRYPTO_DEVICE_NOT_TRUSTED
verify with candidate.D_S_pub
```

Key rotation grace: when a device emits `device_rotate(old, new, effective_at)`, the **previous** cert remains the resolution result for any envelope with `ts_ms < effective_at`. There is no flat "24 h grace" — the rotation op carries the explicit cutoff.

The resolution snapshot is captured at signature-verify entry; concurrent vault-meta updates do not change the result of an in-flight verification.

### Verification order

A receiver MUST verify in this order:

1. Verify the 5-byte magic prefix; mismatch → `PROTOCOL_BAD_MAGIC`.
2. Decode CBOR; assert canonical form (or `CRYPTO_NON_CANONICAL_CBOR`).
3. Resolve the signing device's `D_S_pub` per "Signature key resolution" above.
4. Verify `sig` over `sig_input`. If invalid: reject; do not decrypt.
5. If `aead_alg = 1`, run AEAD-open with the Stream key for `(stream_id, epoch)`. If the AEAD tag is invalid: reject.
6. Decode the inner Op as canonical CBOR; reject if not canonical.

Failed verification at any step is a hard reject: the op MUST NOT be applied or forwarded.

## Inner Op

```cddl
Op = {
    1: bstr .size 16,         ; op_id (ULID body)
    2: EntityRef,             ; target
    3: OpKind,                ; kind
    4: any,                   ; payload (kind-specific; see below)
    5: [* bstr .size 16],     ; deps (op_ids of immediate causal predecessors known at emit time)
}

EntityRef = [
    uint,            ; kind tag: 1=Stream 2=Task 3=Context 4=Routine 5=Block 6=Note
                     ;           7=Attachment 8=Person 9=Device 10=Identity
    bstr .size 16    ; entity body bytes
]

OpKind = "create" / "update" / "delete" /
         "key_envelope" / "device_cert" / "device_revoke" /
         "share_grant" / "share_revoke" /
         "snapshot" / "checkpoint" /
         "identity_transition"
```

Payload schema by kind is defined in the domain specs (`02-domain/*.md`) for `create`/`update`/`delete`, and below for control kinds.

### Control op payloads

| Kind | `aead_alg` | Payload sketch |
|---|---|---|
| `device_cert` | 0 | The `DeviceCert` map (see [`identity-and-device-keys.md`](./identity-and-device-keys.md)) |
| `device_revoke` | 0 | `{ revoked_device_id, reason_code, effective_at }` |
| `key_envelope` | 0 | `{ stream_id, epoch, recipient_device_id, hpke_ciphertext }` |
| `share_grant` | 0 | `{ stream_id, epoch, recipient_identity_id, role, expires_at?, hpke_ciphertext, identity_sig }` |
| `share_revoke` | 0 | `{ stream_id, recipient_identity_id, effective_at }` |
| `snapshot` | 1 | `{ covers: { device_id => max_seq }, base_root, state_cbor }` (encrypted under Stream key) |
| `checkpoint` | 1 | `{ root, covers: { device_id => max_seq } }` (encrypted under Stream key) |
| `identity_transition` | 0 | `{ new_ID_S_pub, new_ID_D_pub, old_sig, new_sig, effective_at }` |

`hpke_ciphertext` is the byte string `enc || ct` produced by HPKE single-shot Base mode; see "HPKE single-shot" below.

`identity_sig` on `share_grant` is `Ed25519_sign(ID_S_priv, "sunrise.share_grant.v1" || canonical_cbor(payload_without_identity_sig))`. Verification requires the granting identity's `ID_S_pub`, looked up from the server-published bundle.

## HPKE single-shot

Used wherever a sender encrypts to a single recipient identified by an X25519 public key (`key_envelope`, `share_grant`, recovery upload, pairing transport).

- Suite: `KEM = DHKEM(X25519, HKDF-SHA-256)`, `KDF = HKDF-SHA-256`, `AEAD = ChaCha20-Poly1305` (RFC 9180; suite IDs `0x0020 / 0x0001 / 0x0003`).
- Mode: Base (no PSK, no sender auth — sender authenticity comes from the surrounding op envelope's signature).
- Info string per role:
  - `key_envelope` → `"sunrise.hpke.key_envelope.v1" || stream_id || u32_be(epoch)`
  - `share_grant`  → `"sunrise.hpke.share_grant.v1"  || stream_id || u32_be(epoch) || identity_id`
  - `recovery_upload` → `"sunrise.hpke.recovery_upload.v1" || identity_id`
  - `pairing_transport` → `"sunrise.hpke.pairing.v1" || handshake_hash`
- Output bytes: `enc (32 B) || ciphertext (variable) || tag (16 B)`. Stored as a single `bstr`.

## Blob chunks

Attachments are split into 256 KiB plaintext chunks; the last chunk MAY be shorter. Each chunk is sealed independently with the parent attachment's per-blob key:

```cddl
BlobChunkEnvelope = {
    1: bstr .size 16,   ; blob_id
    2: uint,            ; chunk_idx (0-based)
    3: uint,            ; chunk_count (total)
    4: bstr,            ; ciphertext (XChaCha20-Poly1305 output)
}
```

```
blob_chunk_nonce = BLAKE3.derive_key(
    context      = "sunrise.blob_chunk_nonce.v1",
    key_material = blob_key || u32_be(chunk_idx),
    out_len      = 24
)

ciphertext = XChaCha20-Poly1305_seal(
    key       = blob_key,
    nonce     = blob_chunk_nonce,
    plaintext = chunk_plaintext,
    aad       = canonical_cbor({1: blob_id, 2: chunk_idx, 3: chunk_count})
)
```

**Per-chunk integrity at decrypt time** is provided by the AEAD tag — XChaCha20-Poly1305 fails closed if any byte of `ciphertext` or `aad` is altered. Re-verifying with a separate hash on every read would duplicate that work; we don't.

**Content integrity** for the assembled attachment is recorded once on the parent attachment metadata op as a single BLAKE3 hash over the concatenated plaintext chunks:

```
content_hash = BLAKE3(chunk_0_plaintext || chunk_1_plaintext || … || chunk_{N-1}_plaintext, 32)
```

Receivers compute this once after all chunks decrypt and compare to the value in the metadata op. A mismatch is a hard error and the attachment is treated as corrupted (the user is offered a re-fetch).

The per-blob `blob_key` is a 32-byte random value generated when the attachment is created and recorded inside the attachment's `create` op (which is itself encrypted under the Stream key, so the blob key is end-to-end protected).

## Maximum sizes

- Inner Op (post-CBOR): 1 MiB. Larger user content goes through the blob path.
- DeviceCert: 4 KiB.
- HPKE info string: 256 B.
- Single envelope on the wire (post-CBOR): 1 MiB + 256 B overhead. The wire-protocol layer rejects oversize envelopes.

## Forward compatibility

- Envelope `v` field is checked first. Unknown values are rejected.
- Inner Op is parsed with tag-aware tolerance: unknown `OpKind` values cause the op to be retained verbatim and forwarded but not applied (preserves convergence on future-clients without breaking older ones).
- Unknown fields inside known payloads are preserved verbatim through round-trip.

## Test vectors

A frozen test set in `sunrise-crypto`'s test suite covers:

- Round-trip encode/decode of every `OpKind`.
- Detection of every single-bit flip in a known envelope (must fail at signature step).
- Detection of forged signatures with a swapped device key.
- Constant test vectors for every HPKE role with a frozen recipient keypair.
- Constant test vectors for blob chunk nonce derivation.
