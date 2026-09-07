---
status: accepted
---

# Cryptography — Overview

Sunrise is end-to-end encrypted: the server holds ciphertext and metadata; only paired user devices hold the keys to read content.

This document is the index. Each linked spec is normative for its area; **all crypto specs in this directory are accepted (frozen) for v1**.

**Accepted is not the same as implemented.** [ADR-0024](../11-adr/0024-key-hierarchy.md) records that the documented cryptosystem and the one in `crates/` were different systems sharing a vocabulary, and adopts the documented hierarchy — the one drawn below — as the target. Each spec in this directory opens with an implementation-status section naming what the tree does today. The short version: the **op envelope, blob chunking, recovery-blob seal, Noise XX handshake and SAS** are byte-exact; the **key hierarchy is now built** — identity keypair, identity-signed DeviceCerts, random per-`(stream_id, epoch)` Stream keys, HPKE `key_envelope` distribution, device revocation and epoch rotation; what remains unbuilt is **identity rotation**, **`share_grant` / `share_revoke`** and the **checkpoint / snapshot** families.

## Goals (in priority order)

1. **Confidentiality.** Plaintext content is unreadable to the server, network attackers, or anyone without an authorized device.
2. **Integrity.** Tampering with stored or in-flight ops is detected on decrypt.
3. **Authenticity.** Each op is bound to the device that produced it.
4. **Recoverability.** A user with no surviving device can recover with a recovery code.
5. **Revocability.** A lost/stolen device can be revoked, after which it cannot read new content.
6. **Selective sharing.** Sharing a Stream to another identity does not leak unrelated content.

## Non-goals (v1)

- **Forward secrecy of stored ops.** Compromise of a current Stream key reveals all past ops encrypted under it. (TLS 1.3 provides session-level FS for the transport.)
- **Plausible deniability** of identity ownership.
- **Post-quantum resistance.** Envelope algorithm IDs are versioned to support a future PQ rotation; no PQ primitives ship in v1.
- **Anonymity from the relay.** The server learns account email, device IDs, op counts, IPs, and the sharing graph.
- **Defense against compelled disclosure** of user secrets.

## Trust anchors

```
                  ┌──────────────────────┐
                  │   Identity keypair   │  ← long-lived; restored from recovery
                  │   (Ed25519 + X25519) │
                  └──────────┬───────────┘
                             │ signs
              ┌──────────────┼──────────────┐
              ▼              ▼              ▼
      ┌─────────────┐ ┌─────────────┐ ┌─────────────┐
      │ Device key  │ │ Device key  │ │ Device key  │  ← per device; bound by DeviceCert
      │  (Phone A)  │ │ (Laptop B)  │ │  (Desk C)   │
      └──────┬──────┘ └──────┬──────┘ └──────┬──────┘
             │               │               │
             ▼               ▼               ▼
       ┌──────────────────────────────────────────┐
       │         Vault root key (per-device)      │  ← wraps at-rest material
       └──────────────────┬───────────────────────┘
                          │
            ┌─────────────┼──────────────┐
            ▼             ▼              ▼
       ┌─────────┐   ┌─────────┐    ┌──────────┐
       │ Stream A│   │ Stream B│    │ Inbox    │  ← per-Stream content key
       │   key   │   │   key   │    │   key    │   (epoch-versioned)
       └────┬────┘   └────┬────┘    └────┬─────┘
            │             │              │
            ▼             ▼              ▼
        Encrypted    Encrypted      Encrypted ops + blobs
            ops          ops             …
```

**That diagram is what the tree now does**, per [ADR-0024](../11-adr/0024-key-hierarchy.md), with two arrows still missing. An account has an identity keypair (`identity` table, migration `0017`, both private halves AEAD-wrapped under the vault root); every DeviceCert is signed by `ID_S_priv` and verified against the identity; each Stream key is 32 random bytes per `(stream_id, epoch)`, wrapped under the vault root in `stream_keys` and distributed by HPKE `key_envelope` ops. The vault root no longer *implies* anything: it wraps keys at rest, and that is all it does.

The arrows not yet drawn in code are **sharing** — `share_grant` / `share_revoke` have no implementation, so a Stream reaches other devices of the same identity and nobody else — and **identity rotation**, which `identity_transition` would carry.

Of the six goals: **Confidentiality**, **Integrity** and **Authenticity** are delivered by the op envelope. **Revocability** is delivered for a device's *reads* and not for its writes. An informed replica seals a revoked device no envelope for any epoch minted after its cut, and the device holds no `ID_D_priv` to open the identity copy with. It goes on writing: the relay would have to be told out of band and cannot be, because it names devices by a ULID it minted and a vault knows no peer's ([#80](https://github.com/justin13888/Sunrise/issues/80)). Three further bounds: the account's *creator* keeps `ID_D_priv` until the recovery blob exists; a revoked device keeps `ID_S_priv` and can certify itself under a fresh id; and the register is per-replica, so a device that has not yet applied the revocation still seals to it. See [`key-rotation.md`](./key-rotation.md) §Revocation. **Recoverability** now has keys worth restoring, and its client route is the next slice. **Selective sharing** is unbuilt.

## Specs in this section

| Spec | Covers |
|---|---|
| [`primitives.md`](./primitives.md) | Algorithms, parameters, library choices, usage rules |
| [`identity-and-device-keys.md`](./identity-and-device-keys.md) | Identity / device keypairs, vault root, Stream keys, DeviceCert |
| [`data-encryption-format.md`](./data-encryption-format.md) | Op envelope, AEAD, AAD, signatures, blob chunks |
| [`pairing-and-onboarding.md`](./pairing-and-onboarding.md) | Noise XX pairing, account creation, SAS |
| [`recovery.md`](./recovery.md) | BIP-39 recovery code, Argon2id stretching, recovery blob |
| [`key-rotation.md`](./key-rotation.md) | Device, Stream, identity rotation; revocation |
| [`sharing-with-others.md`](./sharing-with-others.md) | HPKE share envelopes, egress scrubbing |
| [`encrypted-search.md`](./encrypted-search.md) | Client-side FTS only; rationale |
| [`audit-and-tamper-evidence.md`](./audit-and-tamper-evidence.md) | Per-Stream Merkle root, checkpoints, rollback detection |

## What lives where

| Material | Location | Confidentiality |
|---|---|---|
| Identity private keys (`ID_S_priv`, `ID_D_priv`) | OS keystore (Keychain / Android Keystore / Secure Enclave / StrongBox); fallback: file encrypted under passphrase-derived key | Hardware-backed where available; passphrase fallback otherwise |
| Device private keys (`D_S_priv`, `D_D_priv`) | Same as identity, separate keystore slot | Same |
| Vault root key | Process memory only; rebuilt at unlock; zeroized on lock/exit | OS memory protection |
| Stream keys (current + historical epochs) | SQLite `stream_keys` table, AEAD-wrapped under vault root | At-rest under vault root |
| Wrapped Stream keys for sibling devices | In Stream op log as `key_envelope` ops (HPKE to recipient device's `D_D_pub`) | HPKE |
| Wrapped Stream keys for shared peers | In Stream op log as `share_grant` ops (HPKE to recipient identity's `ID_D_pub`) | HPKE |
| Recovery blob (wraps `ID_S_priv`, `ID_D_priv`) | Server | AEAD under Argon2id-stretched recovery code |
| Recovery code | User-held (paper / password manager) | User's discretion; **not recoverable if lost** |

Where the tree differs from that table today: identity and device private keys live in the vault's own tables (`identity` and `local_identity`), AEAD-wrapped under the vault root, rather than in an OS keystore — the keystore slot is the target and the wrapping is the fallback the table already names; the **vault root is persisted**, not rebuilt — macOS keeps it in the login Keychain, the CLI in a mode-0600 keystore file — because there is no unlock secret to rebuild it from; and there are no `share_grant` rows, because sharing is unbuilt.
