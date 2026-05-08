# 0004 — Crypto primitives selection

**Status:** accepted

## Context

E2EE is a core promise. We need to pick small, well-understood, modern primitives, optimized for the mobile and WASM hot paths.

## Decision

- Identity / device signing: **Ed25519**.
- Identity / device DH: **X25519**.
- AEAD (op envelopes, blobs): **XChaCha20-Poly1305**.
- Hash and KDF (perf path): **BLAKE3** (KDF mode).
- Hash and KDF (interop path): **HKDF-SHA-512**.
- Password KDF: **Argon2id** (m=64MiB, t=3, p=1).
- TLS: **TLS 1.3** only.

All implementations from RustCrypto / dalek-cryptography ecosystems. No hand-rolled crypto.

## Alternatives considered

| Concern | Alternative | Why rejected |
|---|---|---|
| AEAD | AES-256-GCM | Requires hardware AES for fast mobile; cache-timing concerns on lower-end Android; ChaCha is faster on most ARM |
| KDF | scrypt / PBKDF2 | Argon2 is the modern standard; scrypt's parameters are awkward; PBKDF2 is too cheap |
| Signature | RSA, ECDSA P-256 | Curve25519 family is faster, simpler, side-channel-resilient |
| Hash | SHA-256 | BLAKE3 is faster and parallelizable; we use SHA where interop matters |
| PQ-resistant primitives | Hybrid Kyber + X25519, Dilithium + Ed25519 | Adds complexity without urgent need; planned for later (see ADR TBD) |

## Consequences

- One library family (RustCrypto + dalek-cryptography); minimal third-party dep surface.
- Fast on every platform, including WASM.
- Migration path to PQ is documented; envelope format carries algorithm IDs.
- We commit to running the upstream test vectors on every build.
- Crypto code is centralized in `sunrise-crypto`; nothing else calls primitives directly.
