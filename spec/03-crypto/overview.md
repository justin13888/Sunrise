---
status: draft
---

# Cryptography — Overview

Sunrise is end-to-end encrypted: the server holds ciphertext and metadata; only paired user devices hold the keys to read content.

This document is the index. Each linked spec is normative for its area.

## Goals

1. **Confidentiality.** Plaintext content is unreadable by the server, network attackers, or anyone without an authorized device.
2. **Integrity.** Tampering with stored or in-flight ops is detectable.
3. **Authenticity.** Each op is bound to the device that produced it.
4. **Forward secrecy of session traffic.** TLS-level (not within E2EE; we don't aim for op-level forward secrecy in v1).
5. **Recoverability.** A user with no surviving device can recover with a recovery code.
6. **Revocability.** A lost/stolen device can be revoked, after which it cannot read new content.
7. **Sharing.** Selective sharing of subgraphs to other identities, without leaking unrelated content.

## Non-goals

- **Plausible deniability** of identity ownership (deferred).
- **Post-quantum.** v1 uses classical primitives; PQ is tracked but not shipped (see [`primitives.md`](./primitives.md)).
- **Anonymity from the relay.** The server knows account email and device IDs.
- **Defense against compelled disclosure** of user secrets.

## High-level shape

```
                  ┌──────────────────────┐
                  │   Identity keypair   │  ← long-lived; restored from recovery
                  │   (Ed25519 + X25519) │
                  └──────────┬───────────┘
                             │ derives / signs
              ┌──────────────┼──────────────┐
              ▼              ▼              ▼
      ┌─────────────┐ ┌─────────────┐ ┌─────────────┐
      │ Device key  │ │ Device key  │ │ Device key  │  ← per device; signed by identity
      │  (Phone A)  │ │ (Laptop B)  │ │   (TUI C)   │
      └──────┬──────┘ └──────┬──────┘ └──────┬──────┘
             │               │               │
             ▼               ▼               ▼
       ┌──────────────────────────────────────────┐
       │         Vault root key (per-user)        │  ← wraps stream keys
       └──────────────────┬───────────────────────┘
                          │
            ┌─────────────┼──────────────┐
            ▼             ▼              ▼
       ┌─────────┐   ┌─────────┐    ┌──────────┐
       │ Stream A│   │ Stream B│    │ Inbox    │  ← per-stream content key
       │   key   │   │   key   │    │   key    │
       └────┬────┘   └────┬────┘    └────┬─────┘
            │             │              │
            ▼             ▼              ▼
        Encrypted    Encrypted      Encrypted ops
            ops          ops             …
```

| Spec | What it covers |
|---|---|
| [`primitives.md`](./primitives.md) | Algorithms, parameters, library choices |
| [`identity-and-device-keys.md`](./identity-and-device-keys.md) | How identity and device keys are generated, stored, derived |
| [`data-encryption-format.md`](./data-encryption-format.md) | Per-op envelope, AEAD, header fields |
| [`pairing-and-onboarding.md`](./pairing-and-onboarding.md) | Adding a new device |
| [`recovery.md`](./recovery.md) | Surviving total device loss |
| [`key-rotation.md`](./key-rotation.md) | Rotating identity, device, stream keys |
| [`sharing-with-others.md`](./sharing-with-others.md) | Granting access to another identity |
| [`encrypted-search.md`](./encrypted-search.md) | Searching without leaking to the server |
| [`audit-and-tamper-evidence.md`](./audit-and-tamper-evidence.md) | Detecting server-side tampering or rollback |

## What lives where

| Material | Location | Protected by |
|---|---|---|
| Identity private key | OS keystore (Keychain / Keystore / TPM / file w/ passphrase on Linux) | Hardware where available; passphrase fallback |
| Device private key | Same | Same |
| Vault root key | Derived at unlock; held in process memory only | Memory protection where OS permits |
| Stream keys | Encrypted in vault DB under vault root key | At-rest: vault root key |
| Recovery code | User-held (paper / password manager) | User's discretion |
