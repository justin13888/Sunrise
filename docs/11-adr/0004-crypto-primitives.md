# 0004 — Crypto primitives

**Status:** accepted

## Context

E2EE is a core promise. We need a small, well-understood, modern set of primitives, optimized for the mobile and WASM hot paths.

## Decision

The complete frozen set, normative across the v1 wire and storage formats.

| Role | Primitive |
|---|---|
| Identity / device signing | Ed25519 |
| Identity / device DH | X25519 |
| Op-envelope, blob, at-rest AEAD | XChaCha20-Poly1305 |
| Public-key encryption to a single recipient | HPKE Base mode, suite `DHKEM(X25519, HKDF-SHA-256) / HKDF-SHA-256 / ChaCha20-Poly1305` (RFC 9180) |
| Pairing handshake | Noise XX, pattern `Noise_XX_25519_ChaChaPoly_SHA256` |
| Hash, KDF, MAC (internal paths) | BLAKE3 (KDF mode, keyed mode) |
| Password / recovery-code KDF | Argon2id, parameters `m=64MiB, t=3, p=1, salt=16B, out=32B` |
| TLS | TLS 1.3 only (`AES-128/256-GCM`, `CHACHA20-POLY1305`) |

All implementations from RustCrypto and dalek-cryptography. No hand-rolled crypto outside `sunrise-crypto`.

The op envelope carries `aead_alg`, `sig_alg`, `epoch` fields so future rotation is a clean version transition; see [`../03-crypto/data-encryption-format.md`](../03-crypto/data-encryption-format.md).

## Alternatives considered

| Concern | Alternative | Why rejected |
|---|---|---|
| AEAD | AES-256-GCM | Slower than ChaCha on most ARM without AES hardware; cache-timing concerns on lower-end Android |
| Public-key encryption | Hand-rolled X25519 + KDF + AEAD | HPKE is RFC-standardized with formal analysis; loses nothing in performance; we use it everywhere a single recipient is targeted |
| KDF for internal paths | HKDF-SHA-512 | BLAKE3-KDF is faster, parallel, and our test surface is one library; HKDF-SHA-256 is still pulled in via HPKE |
| Password KDF | scrypt / PBKDF2 | Argon2id is the modern standard; scrypt has awkward tunables; PBKDF2 is too cheap |
| Signature | RSA, ECDSA P-256 | Curve25519 family is faster, simpler, side-channel-resilient |
| Pairing | TLS-based, custom | Noise XX with SAS is well-trodden, off-the-shelf in `snow` |
| PQ resistance now | Hybrid Kyber + X25519, Dilithium + Ed25519 | Adds complexity without urgent need; envelope's algorithm IDs preserve a clean future path |

## Consequences

- Single library family (RustCrypto + dalek + `snow` + `hpke`); minimal third-party dep surface.
- Fast on every platform, including WASM.
- Migration path to PQ is documented; envelope format already carries algorithm IDs.
- Frozen test vectors block any inadvertent change.
- Crypto code is centralized in `sunrise-crypto`; nothing else calls primitives directly.
- The pre-rewrite mention of HKDF-SHA-512 is dropped; no hand-rolled X25519+KDF+AEAD construction is permitted.
