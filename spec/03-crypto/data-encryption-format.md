---
status: accepted
---

# Data Encryption Format

The byte-exact wire and storage format for ops, blob chunks, and HPKE single-shot ciphertexts. Encoded as canonical CBOR (RFC 8949 §4.2.1, "deterministic encoding"). All map keys are integers; sorted; no floats; length-prefixed strings; library: `ciborium` in deterministic mode with a thin wrapper that asserts canonical form on decode.

## Op envelope

Every op (in-stream or control) is wrapped in a single envelope type. The envelope is the unit of storage in the `ops` table and the unit of transport on the wire.

```cddl
OpEnvelope = {
    1: uint,            ; v             (envelope version, = 1)
    2: bstr .size 16,   ; stream_id     (raw 128-bit; see below for special values)
    3: bstr .size 16,   ; device_id     (raw 128-bit; signing device)
    4: uint,            ; seq           (per-(stream_id, device_id) monotonic; starts at 1)
    5: uint,            ; ts_ms         (device wall clock; advisory only)
    6: uint,            ; aead_alg      (1 = XChaCha20-Poly1305; 0 = none/control)
    7: uint,            ; sig_alg       (1 = Ed25519)
    8: uint,            ; epoch         (Stream-key epoch used for ciphertext; 0 if aead_alg = 0)
    9: bstr .size 24,   ; nonce         (random 192-bit; ignored if aead_alg = 0)
    10: bstr,           ; ciphertext_or_payload
                        ;   if aead_alg = 1: AEAD output (Op encoded as canonical CBOR, then encrypted)
                        ;   if aead_alg = 0: canonical-CBOR-encoded Op (signed-only control envelope)
    11: bstr .size 64,  ; sig           (Ed25519; see signature rules)
}
```

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
    1: v, 2: stream_id, 3: device_id, 4: seq, 5: ts_ms,
    6: aead_alg, 7: sig_alg, 8: epoch, 9: nonce
})
```

This binds every metadata field to the ciphertext.

### Signature

Always computed (regardless of `aead_alg`):

```
sig_input = "sunrise.op_envelope.v1" || BLAKE3-256(canonical_cbor_without_field_11)
sig       = Ed25519_sign(D_S_priv, sig_input)
```

Where `canonical_cbor_without_field_11` is the envelope encoded with all fields except `11` (sig).

The signature covers the ciphertext (or signed-only payload) bit-for-bit and all metadata, so:

- Signature alone authenticates the message even if the AEAD key is later disclosed.
- AEAD alone authenticates the AAD, which already includes every metadata field.
- Both together give belt-and-suspenders assurance against forgery by anyone who later obtains the Stream key.

### Verification order

A receiver MUST verify in this order:

1. Decode CBOR; assert canonical form.
2. Resolve the signing device's `D_S_pub` from a valid DeviceCert in the local vault-meta log.
3. Verify `sig` over `sig_input`. If invalid: reject; do not decrypt.
4. If `aead_alg = 1`, run AEAD-open with the Stream key for `(stream_id, epoch)`. If the AEAD tag is invalid: reject.
5. Decode the inner Op as canonical CBOR; reject if not canonical.

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
    context = "sunrise.blob_chunk_nonce.v1",
    key_material = blob_key || u32_be(chunk_idx)
)[0..24]

ciphertext = XChaCha20-Poly1305_seal(
    key       = blob_key,
    nonce     = blob_chunk_nonce,
    plaintext = chunk_plaintext,
    aad       = canonical_cbor({1: blob_id, 2: chunk_idx, 3: chunk_count})
)
```

Per-chunk plaintext SHA-256 hashes are recorded in the parent attachment metadata op; receivers verify each chunk after decrypt.

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
