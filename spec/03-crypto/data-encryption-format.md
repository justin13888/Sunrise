---
status: draft
---

# Data Encryption Format

Every CRDT op leaves the device wrapped in an envelope. This spec is the wire+storage format.

## Op envelope

```
OpEnvelope = {
    version:      uint,                  ; format version (= 1)
    stream_id:    bstr .size 16,         ; binary form of str_…
    device_id:    bstr .size 16,
    seq:          uint,                  ; per-(stream, device) monotonic counter
    ts_ms:        uint,                  ; device wall clock; advisory only
    nonce:        bstr .size 24,         ; XChaCha20 nonce; random
    aad:          bstr,                  ; additional auth data (see below)
    ciphertext:   bstr,                  ; AEAD output of the inner Op
    sig:          bstr .size 64,         ; Ed25519 sig over hash(everything else)
}
```

- AEAD: `XChaCha20-Poly1305` with 32-byte stream key, 24-byte nonce, plaintext is the canonical CBOR-encoded inner `Op`.
- AAD includes `version || stream_id || device_id || seq || ts_ms`. Tampering with metadata is detected.
- `sig` is `Ed25519_sign(D_S_priv, BLAKE3(version || stream_id || device_id || seq || ts_ms || nonce || aad || ciphertext))`. Provides authenticity even if the AEAD key is later compromised; signed-then-encrypted *and* signed over ciphertext gives both authentication and unforgeability against the holder of the AEAD key.

## Inner op

```cddl
Op = {
    op_id:       bstr .size 16,         ; ULID, generated client-side
    target:      EntityRef,
    kind:        OpKind,
    payload:     any,                   ; CRDT-typed mutation
    deps:        [* bstr],              ; op_ids this op causally depends on
}

OpKind = "create" / "update" / "delete" / "membership_add" / "membership_remove" / "key_envelope" / "device_cert" / "share_grant" / "share_revoke" / …
```

`deps` is the set of immediate causal predecessors *known to this device at op-creation time*. Used for partial-order ack and op-log tamper detection.

## Special envelopes (unencrypted metadata)

Some envelopes carry metadata the server *needs* to do its job (e.g. routing). These are signed but not encrypted:

- **Pairing requests** between devices of the same identity (signed by inviting device, contains DH ephemeral).
- **Share invites** (signed by inviting identity, contains recipient identity reference).
- **Device revocations** (signed by identity).

These are clearly tagged `category: "control"` and can carry no user content.

## Blob envelope (for attachments)

Blobs are streamed in chunks:

```
BlobChunkEnvelope = {
    blob_id:    bstr .size 16,
    chunk_idx:  uint,
    chunk_count: uint,
    nonce:      bstr .size 24,           ; deterministic: HKDF(stream_blob_key, "chunk", chunk_idx)
    ciphertext: bstr,
}
```

Chunk size 256 KiB. Last chunk may be shorter. The blob's per-blob key is wrapped in the parent attachment metadata op.

## Determinism rules for the codec

- Canonical CBOR (RFC 8949 deterministic encoding) for the inner op and for the AAD construction.
- Sorted maps; no floats; explicit length-prefixed strings.
- Library: `ciborium` with deterministic mode + a thin wrapper that asserts canonical form on decode.

## Forward compatibility

- Envelope version byte is checked first. Unknown versions: reject.
- Inner op format uses tagged unions; unknown variants are *preserved* (for round-trip) but not interpreted.
- Unknown fields inside known variants are preserved verbatim.

## Maximum sizes

- Envelope payload: 1 MiB (anything larger goes through blob upload + a metadata op).
- Single op AAD: 4 KiB.

## Test vectors

A frozen test set in `sunrise-crypto`'s test suite covers:

- Round-trip encode/decode of every `OpKind`.
- Detection of every tampered byte position in a known envelope.
- Detection of forged signatures.
- Constant test vectors for the empty Stream key and a known-fixed nonce.
