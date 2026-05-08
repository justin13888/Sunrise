---
status: draft
---

# Identity and Device Keys

## Identity

A user has **one** Identity. The identity is the cryptographic anchor:

- Long-lived; restored from recovery if all devices are lost.
- Two keypairs:
  - **Identity signing key** (Ed25519) — `ID_S_pub`, `ID_S_priv`.
  - **Identity DH key** (X25519) — `ID_D_pub`, `ID_D_priv`.
- The user's "Identity ID" is `idn_<base32(BLAKE3(ID_S_pub) truncated to 16 bytes)>`.

## Devices

Each device has **its own** keypairs:

- **Device signing key** (Ed25519) — `D_S_pub`, `D_S_priv`. Used to sign every op.
- **Device DH key** (X25519) — `D_D_pub`, `D_D_priv`. Used during pairing to wrap stream keys.

A `DeviceCert` binds a device key to the identity:

```
DeviceCert = sign( ID_S_priv,
    blake3( D_S_pub || D_D_pub || device_id || created_at || nickname ) )
```

A device's authority to participate is checked by:

1. Validating `DeviceCert` against the user's `ID_S_pub`.
2. Confirming the device hasn't been revoked (no revocation entry in the op log; see [`key-rotation.md`](./key-rotation.md)).

## Vault root key

Used to encrypt at-rest material on a single device.

```
vault_root = HKDF-SHA-512(
    salt = device_salt,
    ikm  = unlock_secret,             // passphrase-derived OR keystore-released
    info = "sunrise.vault_root.v1"
)
```

`unlock_secret` is whichever applies on this device:

- **OS keystore** holds an opaque random secret released after biometric/passcode authentication.
- **Passphrase** is run through Argon2id to produce a key.

Devices SHOULD prefer the OS keystore. The passphrase mode is a fallback (Linux without TPM, kiosk web, etc.).

## Stream keys

Per-Stream symmetric keys used as AEAD keys for ops within that Stream.

- Generated when a Stream is created.
- Wrapped under the vault root key in the local DB.
- Wrapped under each authorized device's `D_D_pub` and stored in a "key envelope" entity in the op log so that other paired devices can decrypt.
- Wrapped under each shared peer's `ID_D_pub` for sharing (see [`sharing-with-others.md`](./sharing-with-others.md)).

## Why per-Stream, not per-Task?

- Per-Stream keys give us efficient bulk decrypt on initial sync.
- Per-Task keys would be too granular (key envelope explosion).
- Per-vault key would prevent selective sharing.

Stream is the **sharing unit** in the domain model; matching the crypto unit to the sharing unit is the right tradeoff.

## Storage at rest

| Item | On-device storage |
|---|---|
| Identity private keys | OS keystore (non-extractable on iOS/macOS Secure Enclave + Android StrongBox); fallback: encrypted file |
| Device private keys | Same as above (separate slot) |
| Vault root key | RAM only; rebuilt at unlock |
| Stream keys | SQLite, encrypted by vault root key (column-level XChaCha20-Poly1305) |
| Wrapped stream keys for other devices | In the op log (encrypted to that device) |
| Wrapped stream keys for shared peers | In the op log (encrypted to that peer's identity DH key) |

## Public-key publication

A user's `ID_S_pub` and `ID_D_pub` are published to the server when the account is created so other users can share with them by user handle / email lookup. Server-stored profile blobs are integrity-protected by `ID_S_priv` so the server cannot substitute keys.

Out-of-band verification (QR / numeric fingerprint) is offered for users who want to confirm a peer's keys haven't been MITM'd by a hostile server.
