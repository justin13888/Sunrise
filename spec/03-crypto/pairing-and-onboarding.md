---
status: draft
---

# Pairing and Onboarding

Three flows: account creation, adding a new device, recovering identity.

## Account creation

1. User opens Sunrise, picks "Create new identity."
2. Client generates `ID_S` and `ID_D` keypairs locally.
3. Client generates a device key (`D_S`, `D_D`) and the first `DeviceCert`.
4. Client requires the user to set up an unlock method:
   - Strong preference: OS keystore-backed passcode/biometric.
   - Fallback: passphrase (Argon2id).
5. Client requires the user to **save a recovery code** (see [`recovery.md`](./recovery.md)). Account creation is *blocked* until the user confirms they've stored it (paste-back challenge).
6. Client connects to the server, registers the account by:
   - Sending `email`, `ID_S_pub`, `ID_D_pub`, signed account-creation request.
   - The server stores the public keys; never sees private material.

## Adding a new device

The new device starts with no keys. It must obtain:

- The user's `ID_S` and `ID_D` (private halves) — *or* be authorized as a new device under the existing identity.

We support both.

### Mode A — Pair from a trusted device (recommended)

Out-of-band channel between an existing device (E) and a new device (N).

1. **N → E**: shows a 6-digit short code + QR.
   - Code is a short auth code derived from N's ephemeral public X25519 key + a fresh nonce.
2. **E** scans QR (or user types code) and confirms the new device's nickname.
3. **E ⇄ N**: a Noise XX handshake using ephemeral keys, authenticated by the user-displayed short code (SAS).
4. After the handshake:
   - **E sends** `ID_S_priv`, `ID_D_priv`, the current set of stream keys, and an updated `DeviceCert` for N.
   - **N stores** keys in its OS keystore, generates its own per-device keys, and publishes its own DeviceCert (signed by `ID_S_priv` it just received) into the op log.
5. The server sees only the encrypted exchange and the resulting DeviceCert metadata.

Notes:

- LAN: handshake can run over mDNS-discovered direct WS.
- WAN: relayed through the server as an opaque tunneled exchange.
- USB: an alternative transport for paranoid users (Tauri desktop ↔ phone via USB tether).

### Mode B — Pair from a recovery code

For users with no existing trusted device.

1. N enters the recovery code.
2. Argon2id stretches it to a recovery key.
3. N fetches the account's recovery blob from the server (containing `ID_S`, `ID_D` wrapped under the recovery key).
4. N decrypts, generates its own device keys, publishes a new `DeviceCert`.
5. User SHOULD revoke any other devices of unknown trust.

## Onboarding UX requirements

Independent of the cryptographic flow:

- A new user reaches a usable Today screen in **≤90 seconds** from app launch on a 4G connection.
- Recovery code is shown **only once** and the user is required to confirm storage.
- Any device added in Mode A surfaces in the existing devices' "Devices" view immediately, with the time and approximate location of the pairing.

## Out-of-band channel security

- The 6-digit short code is a Short Authenticated String (SAS) over the Noise handshake transcript. 6 decimal digits = ~20 bits, mitigated by interactive use (attacker has one shot).
- QR encodes a longer (≥128-bit) channel binding to make MITM exponentially harder when the user scans.

## What the server sees

- Account email and `ID_S_pub`.
- Per-device public keys and `DeviceCert`s appearing in the op log.
- Pairing happens via opaque control envelopes the server cannot decrypt.
