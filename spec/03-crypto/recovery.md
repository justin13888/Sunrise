---
status: draft
---

# Recovery

If all of a user's devices are lost or wiped, recovery restores access. If the user lacks both their devices and their recovery code, **the data is unrecoverable**. This is the cost of E2EE; it is non-negotiable.

## Recovery code

- **Format.** 24 words from the BIP-39 word list. Equivalent to 256 bits of entropy.
- **Derivation.** A fresh 256-bit secret is generated client-side at account creation. Encoded to BIP-39.
- **Storage on user side.** User-displayed *once* on creation; UI strongly nudges saving to a password manager and writing on paper.
- **Storage on server side.** A blob containing `ID_S_priv` and `ID_D_priv` AEAD-encrypted under a key derived from the recovery secret via Argon2id (m=64MiB, t=4, p=1, salt=user-specific).

## Recovery flow

1. User enters recovery code on a fresh install.
2. Client fetches the encrypted recovery blob from the server.
3. Client runs Argon2id to derive the recovery key.
4. Client decrypts identity keys, generates new device keys, publishes a new `DeviceCert`.
5. Client SHOULD prompt the user to revoke other devices and rotate stream keys (see [`key-rotation.md`](./key-rotation.md)) — recovery implies an unknown-state environment.

## Recovery code rotation

The user can rotate the recovery code at any time. Old code is invalidated.

## Why not "social recovery"?

We considered M-of-N split among trusted contacts. Rejected for v1:

- High UX cost (recipients need accounts, must be reachable).
- Coordination complexity vs. the audience we serve.

Tracked as future work; the recovery primitive is designed to be extended (the recovery blob can be split with Shamir's secret sharing without changing the unlock protocol).

## What the user is told

The Recovery setup screen explicitly states:

> Sunrise is end-to-end encrypted. **We cannot reset your account if you lose this code.** This is by design — it means we can't read your data, but it also means we can't recover it for you. Save this code somewhere you'll find it years from now.

A "test recovery now" affordance is offered: enter your code; we verify it can decrypt your blob. This is mandatory before account creation completes.

## Alternative paths

- **Backup unlock to a hardware key.** v2 candidate. WebAuthn-style FIDO2 or YubiKey can hold a wrapping key.
- **Device-to-device recovery without server.** If the user's old laptop is dusty in a drawer but powers on, that laptop alone is a recovery path even without internet (LAN pair).

## Lockout vs. compromise

If the user thinks the recovery code might be compromised:

- Rotate it immediately.
- Rotate identity keys (see [`key-rotation.md`](./key-rotation.md)).
- Re-share any shared streams (recipients must accept new keys).

This is a heavyweight operation (re-encrypts share-grants, may invalidate other devices). Documented; not encouraged casually.
