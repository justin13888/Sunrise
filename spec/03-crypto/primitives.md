---
status: accepted
---

# Cryptographic Primitives

The complete and frozen set of algorithms used in Sunrise v1. Any change requires a new ADR superseding [`../11-adr/0004-crypto-primitives.md`](../11-adr/0004-crypto-primitives.md) and a wire-format version bump.

## Algorithm table

| Purpose | Algorithm | Parameters | Library |
|---|---|---|---|
| Identity / device signing | Ed25519 | RFC 8032 | `ed25519-dalek` 2.x |
| Identity / device DH | X25519 | RFC 7748 | `x25519-dalek` 2.x |
| Op-envelope AEAD | XChaCha20-Poly1305 | 32-byte key, 24-byte nonce, 16-byte tag | `chacha20poly1305` (RustCrypto) |
| Blob-chunk AEAD | XChaCha20-Poly1305 | Same as above; nonce derived (see [`data-encryption-format.md`](./data-encryption-format.md)) | Same |
| At-rest wrap (Stream key, recovery blob, keystore-fallback file) | XChaCha20-Poly1305 | Same | Same |
| Public-key encryption to a recipient (key envelopes, share grants, recovery upload) | HPKE Base mode | Suite ID `0x0020 0x0001 0x0003` (DHKEM(X25519, HKDF-SHA-256), HKDF-SHA-256, ChaCha20-Poly1305) | `hpke` 0.11.x (RustCrypto) |
| Pairing handshake | Noise XX | Pattern `Noise_XX_25519_ChaChaPoly_SHA256` | `snow` 0.9.x |
| Hash | BLAKE3 | 256-bit output unless noted | `blake3` 1.x |
| KDF (internal) | BLAKE3 KDF mode (`derive_key`) | Per-call `context` string (see usage rules) | `blake3` |
| MAC (rare; non-AEAD paths) | BLAKE3 keyed | 32-byte key | `blake3` |
| Password / recovery-code KDF | Argon2id | m=64 MiB, t=3, p=1, salt=16 random bytes, 32-byte output | `argon2` 0.5.x (RustCrypto) |
| RNG | OS CSPRNG | `getrandom`-backed | `rand_core` + `getrandom` |
| TLS | TLS 1.3 only | Cipher suites: `TLS_AES_128_GCM_SHA256`, `TLS_AES_256_GCM_SHA384`, `TLS_CHACHA20_POLY1305_SHA256` | `rustls` 0.23.x |

HKDF-SHA-256 is used **only** as the HPKE-internal KDF; it is not exposed at the spec layer. SHA-256 is used **only** by the pairing Noise pattern.

## Usage rules

### When to use which AEAD

- **All AEAD operations are XChaCha20-Poly1305** with random 24-byte nonces, except:
  - HPKE-internal AEAD operations, which use ChaCha20-Poly1305 with HPKE-derived nonces.
  - Blob chunks, which use deterministically derived 24-byte nonces (see envelope format).

The 192-bit random nonce gives a comfortable safety margin without needing per-key counter coordination across devices.

### When to use which KDF

- **HPKE Base mode** for any single-recipient public-key encryption (key envelopes for sibling devices, share grants for peers, recovery blob upload, pairing transport). The spec layer never composes raw X25519 + KDF + AEAD.
- **BLAKE3 KDF mode** for all internal symmetric derivations. Every `derive_key` call MUST pass a unique, descriptive context string of the form `"sunrise.<purpose>.v<version>"` (e.g. `"sunrise.vault_root.v1"`, `"sunrise.blob_chunk_nonce.v1"`).
- **Argon2id** for stretching low-entropy human secrets (passphrase, recovery code) into 32-byte keys. Parameters are fixed and platform-uniform; phones budget ~1 s, desktops ~250 ms (parameters chosen for the slower bound).

### When to sign

- **Every op envelope** (in-stream and control) is signed by the originating device's `D_S_priv`.
- **Every share grant** is additionally signed by the granting identity's `ID_S_priv`.
- **Every DeviceCert** is signed by the identity's `ID_S_priv`.

## Excluded by policy

- AES-GCM at the application layer (consistency with mobile/WASM perf and to avoid AES-NI dependency).
- PBKDF2, scrypt, bcrypt for password KDF.
- RSA, ECDSA over NIST curves, DSA.
- HKDF-SHA-512 (the previous draft used it; HPKE-internal HKDF-SHA-256 + BLAKE3-KDF cover all needs).
- Any cipher mode without authenticated encryption.
- Any hand-rolled "X25519 + KDF + AEAD" construction at the spec layer (HPKE is mandatory for that role).

## Library hygiene

- All crypto goes through the leaf crate `sunrise-crypto`. App code MUST NOT call `chacha20poly1305`, `ed25519-dalek`, `hpke`, `snow`, `blake3`, `argon2`, etc., directly.
- `sunrise-crypto` exposes typed wrappers (`StreamKey`, `OpEnvelope`, `IdentityPrivKey`, `DevicePrivKey`, `RecoveryKey`, …). Type confusion at the API level is impossible (e.g. an `IdentityPrivKey` cannot be passed where a `DevicePrivKey` is expected).
- All key types implement zeroize-on-drop.
- Versions are pinned in `Cargo.lock` and vendored at release; reproducible builds are required for tagged releases.
- `cargo-deny` and `cargo-audit` run in CI; new versions of crypto deps require explicit review by a designated reviewer (see `CODEOWNERS`).

## Test vectors

`sunrise-crypto`'s test suite includes:

- Upstream library test vectors (re-tested per build).
- Sunrise-specific frozen vectors:
  - A canonical identity → device → Stream key derivation tree.
  - A canonical op envelope (encode + decode round-trip, AAD construction, signature).
  - A canonical HPKE single-shot ciphertext for each role (key envelope, share grant, recovery upload).
  - A canonical blob chunk sequence (chunked nonce derivation).
  - A canonical Argon2id derivation from a fixed recovery code.
- Tampering tests: every byte of a known envelope is flipped and the result must fail decode/verify.

Any change that perturbs a frozen vector blocks merge and requires a wire-format version bump.

## Algorithm IDs (wire-level)

The op envelope carries algorithm tags so a future rotation is a clean version transition. Defined values for v1:

| Field | Value | Meaning |
|---|---|---|
| `aead_alg` | `1` | XChaCha20-Poly1305 |
| `aead_alg` | `0` | none (control envelope; signed-only) |
| `sig_alg` | `1` | Ed25519 |
| `kem_alg` (HPKE) | `0x0020` | DHKEM(X25519, HKDF-SHA-256) |
| `kdf_alg` (HPKE) | `0x0001` | HKDF-SHA-256 |
| `aead_alg` (HPKE) | `0x0003` | ChaCha20-Poly1305 |

Receivers reject unknown values. New algorithms get new IDs in a future spec version; no in-place reinterpretation.
