---
status: accepted
---

# Cryptography — Overview

Sunrise is end-to-end encrypted: the server holds ciphertext and metadata; only paired user devices hold the keys to read content.

This document is the index. Each linked spec is normative for its area; **all crypto specs in this directory are accepted (frozen) for v1**.

**Accepted is not the same as implemented.** [ADR-0024](../11-adr/0024-key-hierarchy.md) records that the documented cryptosystem and the one in `crates/` are different systems sharing a vocabulary, and adopts the documented hierarchy — the one drawn below — as the target. Each spec in this directory now opens with an implementation-status section naming what the tree does today. The short version: the **op envelope, blob chunking, recovery-blob seal, Noise XX handshake and SAS are implemented and byte-exact**; the **key hierarchy above the Stream key is not**.

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

**That diagram is the target, per [ADR-0024](../11-adr/0024-key-hierarchy.md).** Today the tree has no identity keypair at all: the vault root is 32 random bytes **per account** (not derived per device), device certs are self-signed by the device's own key with no identity to anchor to, and each Stream key is *derived* from the root as `BLAKE3.derive_key("sunrise.stream_key.v1", vault_root || stream_id || u32_be(epoch))` at a pinned `EPOCH = 1` rather than generated and wrapped. So the arrows above run the other way in practice: the root is the top of the live hierarchy, and everything hangs off it. See [`identity-and-device-keys.md`](./identity-and-device-keys.md) §What is specified here vs. what is implemented.

Four of the six goals depend on that gap. **Revocability** and **Selective sharing** are not achievable while every device holds a root that *is* the key schedule; **Recoverability** restores identity keys that currently decrypt nothing. **Confidentiality**, **Integrity** and **Authenticity** are delivered today by the op envelope.

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

Where the tree differs from that table today: there are **no identity private keys** to store; the device signing secret lives in the vault's own `local_identity` row, AEAD-wrapped under the vault root; the **vault root is persisted**, not rebuilt — macOS keeps it in the login Keychain, the CLI in a mode-0600 keystore file — because there is no unlock secret to rebuild it from; and `stream_keys` is written but never read.
