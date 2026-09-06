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
| Public-key encryption to a recipient (key envelopes, share grants, recovery upload) | HPKE Base mode | Suite ID `0x0020 0x0001 0x0003` (DHKEM(X25519, HKDF-SHA-256), HKDF-SHA-256, ChaCha20-Poly1305) | `hpke` 0.13.x (RustCrypto) |
| Pairing handshake | Noise XX | Pattern `Noise_XX_25519_ChaChaPoly_SHA256` | `snow` 0.9.x |
| Hash | BLAKE3 | 256-bit output unless noted | `blake3` 1.x |
| KDF (internal) | BLAKE3 KDF mode (`derive_key`) | Per-call `context` string (see usage rules) | `blake3` |
| MAC (rare; non-AEAD paths) | BLAKE3 keyed | 32-byte key | `blake3` |
| Password / recovery-code KDF | Argon2id (version 0x13, RFC 9106) | m=65536 KiB (64 MiB), t=3, p=1, salt=16 random bytes, 32-byte output | `argon2` 0.5.x (RustCrypto) |
| RNG | OS CSPRNG | `getrandom`-backed | `rand_core` + `getrandom` |
| TLS | TLS 1.3 only | Cipher suites: `TLS_AES_128_GCM_SHA256`, `TLS_AES_256_GCM_SHA384`, `TLS_CHACHA20_POLY1305_SHA256` | `rustls` 0.23.x |

HKDF-SHA-256 is used **only** as the HPKE-internal KDF; it is not exposed at the spec layer. SHA-256 is used **only** by the pairing Noise pattern.

### Which of these are actually reachable

Every algorithm above is frozen for v1, and most are live. Two rows describe capability that no code exercises, and one is narrower in practice than the row implies:

* **HPKE has one consumer of four roles.** `sunrise-crypto` depends on `hpke` and `hpke_seal.rs` implements the single-shot Base construction; **key envelopes** use it. Share grants, recovery upload and pairing transport still do not — sharing is unbuilt, the recovery blob uses its own AEAD-under-Argon2id seal, and pairing's transport is Noise XX, not HPKE.
* **Argon2id runs on one path, not two.** It is used only to stretch the recovery code in `crates/sunrise-crypto/src/recovery.rs`. There is no passphrase unlock: the vault root is 32 random bytes from a keystore, never derived (see [`identity-and-device-keys.md`](./identity-and-device-keys.md)).
* **Noise XX is implemented** in `crates/sunrise-pairing`, with the exact pattern string in `handshake.rs::NOISE_PARAMS`. Its relay transport is not; see [`pairing-and-onboarding.md`](./pairing-and-onboarding.md).

## Usage rules

### When to use which AEAD

- **All AEAD operations are XChaCha20-Poly1305** with random 24-byte nonces, except:
  - HPKE-internal AEAD operations, which use ChaCha20-Poly1305 with HPKE-derived nonces.
  - Blob chunks, which use deterministically derived 24-byte nonces (see envelope format).

The 192-bit random nonce gives a comfortable safety margin without needing per-key counter coordination across devices.

### When to use which KDF

- **HPKE Base mode** for any single-recipient public-key encryption (key envelopes for sibling devices, share grants for peers, recovery blob upload, pairing transport). The spec layer never composes raw X25519 + KDF + AEAD.
- **BLAKE3 KDF mode** for all internal symmetric derivations. Every `derive_key` call MUST pass a unique, descriptive context string of the form `"sunrise.<purpose>.v<version>"` (e.g. `"sunrise.vault_root.v1"`, `"sunrise.blob_chunk_nonce.v1"`).
- **Argon2id** for stretching low-entropy human secrets (passphrase, recovery code) into 32-byte keys. Algorithm version is **0x13 (RFC 9106)**; parameters are `m = 65536` KiB (64 MiB), `t = 3`, `p = 1`, fixed and platform-uniform. Phones budget ~1 s, desktops ~250 ms (parameters chosen for the slower bound). There is no time-out at the call site; UI shows a progress modal on mobile.

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
- `sunrise-crypto` exposes typed wrappers (`StreamKey`, `OpEnvelope`, `VaultRootKey`, `RecoveryKey`, …). Identity and device key types MUST be distinct, so that an identity private key cannot be passed where a device private key is expected.

  **True in code since [ADR-0024](../11-adr/0024-key-hierarchy.md).** `DeviceSigningKeyPair` and `DeviceDhKeyPair` are newtypes in `crates/sunrise-crypto/src/keys.rs`, not aliases of the identity types, so `DeviceCert::issue` cannot be handed a device key and `sign_envelope` cannot be handed an identity key. Both zeroize on drop and redact in `Debug`.
- All key types implement zeroize-on-drop.
- Versions are pinned in `Cargo.lock` and vendored at release.
- `cargo-deny` and `cargo-audit` run in CI; new versions of crypto deps require explicit review by a designated reviewer (see `CODEOWNERS`).

## Constant-time guarantees

All signature-verify, AEAD-tag-verify, and HPKE-decrypt code paths MUST be constant-time. The `sunrise-crypto` crate uses:

| Library | Role | CT property |
|---|---|---|
| `ed25519-dalek` | Ed25519 sign/verify | constant-time by default |
| `chacha20poly1305` | XChaCha20-Poly1305 AEAD | constant-time |
| `hpke` | HPKE Base mode | constant-time |
| `subtle` | tag/MAC/hash equality | `ConstantTimeEq` |
| `argon2` | passphrase / recovery KDF | does not require CT (it's a deliberate-cost KDF over a passphrase, not a comparison primitive) |

Crypto-typed equality MUST go through `subtle::ConstantTimeEq`. An earlier revision of this spec claimed a clippy lint **`sunrise::ct_compare`** enforced that mechanically; **no such lint exists** — `grep` finds `ct_compare` nowhere in the tree, and the workspace `[workspace.lints.clippy]` block in the root `Cargo.toml` carries no custom lint. The rule is real and currently rests on review, not on the compiler.

## Reproducible builds

Tagged releases are to be reproducible:

- `rustc` pinned via `rust-toolchain.toml`. **Implemented** — the file exists and pins 1.91.1 ([ADR-0026](../11-adr/0026-msrv-bump.md)), byte-identical to `rust-version` in the root `Cargo.toml` and to `ARG RUST_VERSION` in the `Dockerfile`. The first step of CI's `rust` job asserts all four — the toolchain file, `rust-version`, the Dockerfile's `ARG RUST_VERSION`, and the `rustc` a checkout actually resolves to — so the match is enforced rather than observed.
- `Cargo.lock` committed. **Implemented.**
- `RUSTFLAGS="-C codegen-units=1 -C link-arg=-Wl,--build-id=none"`. **Not implemented.** `[profile.release]` sets `codegen-units = 1`, but no workflow sets `RUSTFLAGS`.
- `SOURCE_DATE_EPOCH` set to the release-commit timestamp. **Not implemented** — the string appears in neither `.github/workflows/ci.yml` nor `release.yml`.

The goal stands: two builds from the same commit with the same toolchain should produce bit-identical artifacts. **The CI check that rebuilds the previous tag and diffs does not exist yet**, so nothing currently fails on drift.

## Test vectors

`sunrise-crypto`'s test suite includes:

- Upstream library test vectors (re-tested per build).
- Sunrise-specific frozen vectors, in `crates/sunrise-crypto-test-vectors` and asserted by `crates/sunrise-crypto/tests/frozen_vectors.rs`:
  - `identity_id_from_pub`, cross-checked against the longhand `derive_key("sunrise.identity_id.v1", …)` formula so a context-string change is caught even if the vector is regenerated. **Implemented.**
  - BLAKE3-KDF vectors, per context string. **Implemented.**
  - Both whole op envelopes — signed-only and sealed — byte-exact, plus a round-trip-and-verify. **Implemented.**
  - Blob-chunk nonce/AAD vectors and one byte-exact sealed chunk. **Implemented.**
  - Per-Stream Merkle root init/step. **Implemented** — and these tests are the only callers of `merkle.rs`; see [`audit-and-tamper-evidence.md`](./audit-and-tamper-evidence.md).
  - A canonical identity → device → Stream key derivation tree. **Not applicable.** [ADR-0024](../11-adr/0024-key-hierarchy.md) removed the derivation it would have frozen: Stream keys are random per `(stream_id, epoch)` and reach a device through a `key_envelope`, so there is no tree to derive. What is frozen instead is `stream_key_id` — `derive_key("sunrise.stream_key_id.v1", stream_key, 8)` — as a KDF vector.
  - A canonical HPKE single-shot ciphertext for each role. **Implemented for `key_envelope`** — `crypto-test-vectors`'s `key_envelope` module pins the recipient keypair, the Stream key, the info string and the sealed bytes, produced under a seeded RNG because HPKE encapsulation is randomised; the unconditional half opens it and checks `enc.len() == 32`. The other three roles have no implementation to freeze.
  - A canonical Argon2id derivation from a fixed recovery code. **Not implemented**; `recovery.rs` has round-trip tests but no frozen vector.
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
