---
status: draft
---

# Cryptographic Primitives

The full set of algorithms used in Sunrise. We pick small, well-understood, modern primitives.

| Purpose | Algorithm | Library |
|---|---|---|
| Identity signing key | Ed25519 | `ed25519-dalek` |
| Identity / device DH key | X25519 | `x25519-dalek` |
| AEAD (op envelopes, blobs) | XChaCha20-Poly1305 | `chacha20poly1305` |
| Hash | BLAKE3 | `blake3` |
| KDF | HKDF-SHA-512 | `hkdf` |
| Password-based KDF | Argon2id (m=64MiB, t=3, p=1, salt=16B) | `argon2` |
| MAC (rare cases without AEAD) | BLAKE3-keyed | `blake3` |
| Random | OS CSPRNG via `getrandom` | `rand_core` + `getrandom` |
| TLS | TLS 1.3 only; cipher suites: AES-GCM-128/256, CHACHA20-POLY1305 | `rustls` |

## Why these choices

- **Curve25519 family** (Ed25519, X25519). Fast, side-channel-resilient, no parameter choices.
- **XChaCha20-Poly1305** (extended-nonce). 192-bit random nonces are safe; we don't need a counter scheme. Significantly simplifies multi-device op generation (no shared nonce coordinator).
- **BLAKE3.** Faster than SHA-2, parallelizable, supports keyed mode and KDF mode (handy for op-log Merkle hashing).
- **HKDF-SHA-512.** When we want a strict RFC-compliant KDF. We use BLAKE3-KDF for performance-sensitive paths and HKDF-SHA-512 where interop matters.
- **Argon2id.** The password-hashing standard. Parameters tuned to ~1s on a 2020 mid-range phone. Confirmable on each launch.

## What we explicitly do not use

- **AES-GCM** for app-level AEAD (we use ChaCha20-Poly1305). Reason: AES-NI is universal but ChaCha is faster on modern mobile and avoids cache-timing concerns on platforms without hardware AES.
- **PBKDF2 / scrypt / bcrypt** for password KDF.
- **RSA**.
- **Custom protocols** built on raw primitives without a published protocol layer. Where we use Noise or a similar construction, we cite the spec.

## Post-quantum

Tracked as future work, not v1:

- Identity / device DH: candidate hybrid ML-KEM (Kyber) + X25519.
- Signature: candidate hybrid ML-DSA (Dilithium) + Ed25519.
- Forward secrecy of stored ops: a deeper redesign; deferred.

When PQ is added, the wire and storage formats include explicit algorithm tags so a smooth rotation is possible.

## Library hygiene

- **All crypto goes through `sunrise-crypto`** (a leaf crate inside `sunrise-core`). No app code calls `chacha20poly1305` etc. directly.
- The crate exposes typed wrappers (`StreamKey`, `OpEnvelope`, `IdentityPrivKey`) that prevent misuse (e.g. you cannot accidentally feed an `IdentityPrivKey` to a function expecting a `DeviceKey`).
- `cargo-deny` and `cargo-audit` run in CI; new versions of crypto deps require explicit review.
- We pin specific versions, vendor them for releases, and reproduce builds.

## Test vectors

For every primitive, the crate ships test vectors from the upstream library (re-tested) plus Sunrise-specific compositions (e.g. a frozen identity + device + stream key derivation tree). Any change that perturbs a test vector blocks merge.
