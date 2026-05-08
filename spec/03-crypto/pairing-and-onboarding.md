---
status: accepted
---

# Pairing and Onboarding

Three flows: account creation, adding a new device, recovering identity. Recovery is in [`recovery.md`](./recovery.md); this spec covers account creation and the pair-from-existing-device flow.

## Account creation

1. User opens Sunrise, picks "Create new identity."
2. Client generates `ID_S` and `ID_D` keypairs locally with the OS CSPRNG.
3. Client generates the first device's `D_S`, `D_D` and assembles a `DeviceCert`.
4. Client requires the user to set up an unlock method:
   - **Preferred:** OS keystore-backed passcode/biometric (iOS, macOS, modern Android, Windows Hello).
   - **Fallback:** passphrase. UI enforces minimum entropy via a zxcvbn-style estimator (≥ 60 bits).
5. Client requires the user to **save the recovery code** (see [`recovery.md`](./recovery.md)). Account creation is blocked until the user passes a paste-back challenge.
6. Client opens a TLS 1.3 connection to the server and sends a signed account-creation request:
   ```cddl
   AccountCreate = {
       email:        tstr,
       identity_id:  bstr .size 16,
       ID_S_pub:     bstr .size 32,
       ID_D_pub:     bstr .size 32,
       device_cert:  DeviceCert,
       recovery_blob: bstr,                ; HPKE single-shot encrypted to a key derived from the recovery code; see recovery.md
       sig:          bstr .size 64        ; Ed25519_sign(ID_S_priv,
                                          ;   "sunrise.account_create.v1" || BLAKE3(canonical_cbor(prev_fields)))
   }
   ```
7. Server validates the signature against `ID_S_pub`, persists the bundle, and returns an account ID. The server stores `recovery_blob` opaquely.

The server never sees `ID_S_priv`, `ID_D_priv`, the recovery code, the unlock secret, or any Stream key.

## Adding a new device — Mode A: pair from a trusted device

The new device (N) starts with no keys. An existing device (E) authorizes it via an out-of-band channel. This is the recommended path.

### Handshake

We use **Noise XX** with pattern `Noise_XX_25519_ChaChaPoly_SHA256` (Noise spec §7.5). XX gives mutual authentication with deferred identity transmission, suiting a flow where N's "identity" is its newly-generated key and E proves possession of `ID_S_priv` mid-flow.

The Noise static keys for the handshake are **not** the device or identity long-term keys. Each side generates an ephemeral X25519 static-for-this-handshake keypair (`s`) and the standard Noise ephemeral (`e`). Long-term keys are transmitted as Noise transport-layer messages after the handshake completes.

### Flow

1. **N → E**: presents a QR (and fallback 6-digit code).
   ```
   QR contents = base64url(canonical_cbor({
       1: "sunrise.pair.v1",
       2: N_static_pub,                     ; 32 B X25519
       3: account_email_hash,                ; BLAKE3-256(email)[0..8]; identifies which account
       4: relay_url                          ; URL E should reach to relay handshake messages
   }))
   ```
2. **User on E** scans the QR (or types the 6-digit fallback). E confirms a nickname for N.
3. **E ⇄ N**: Noise XX handshake runs over the relay (a TLS WebSocket whose payloads the server cannot interpret), or LAN mDNS-discovered direct WS, or USB.
4. After the handshake's third message:
   - Both sides have agreed transport keys.
   - Both sides compute the same handshake hash `h`.
5. **SAS confirmation.** Both devices display:
   ```
   SAS = decimal( BLAKE3-256("sunrise.pair_sas.v1" || h)[0..3] ) % 1_000_000   ; 6 digits
   ```
   The user reads SAS aloud or compares on-screen. Both devices show "Confirm" / "Reject." A user who reaches step 5 expecting to pair sees a code; a MITM cannot match it without a successful preimage attack on a 6-decimal-digit channel binding (~20 bits of authentication, *protected by the live interactive context* — one mismatched code aborts the entire pairing).
6. **On confirm, E sends transport messages to N** (each is a Noise transport message; the Noise tunnel is used as an authenticated, encrypted channel):
   ```cddl
   PairingPayload = {
       1: bstr .size 32,         ; ID_S_priv (32-byte seed)
       2: bstr .size 32,         ; ID_D_priv
       3: bstr .size 32,         ; ID_S_pub
       4: bstr .size 32,         ; ID_D_pub
       5: bstr .size 16,         ; identity_id_bytes
       6: { * uint => bstr },    ; stream_keys: { stream_id_first8 => epoch_table_cbor_bytes }
                                  ; epoch_table_cbor_bytes = canonical_cbor({ epoch => 32-byte stream key })
       7: tstr,                  ; nickname for N
       8: tstr                   ; platform
   }
   ```
7. **N receives the payload, validates** that `BLAKE3("sunrise.identity_id.v1" || ID_S_pub)[0..16] == identity_id_bytes`, generates its own `D_S` / `D_D` keypairs, and constructs a `DeviceCert` for itself signed by the just-received `ID_S_priv`.
8. **N publishes its `device_cert` op** (signed by its new `D_S_priv`, control envelope) into the vault-meta log, and stores keys per [`identity-and-device-keys.md`](./identity-and-device-keys.md).
9. **E displays** "Paired with N at `<time>`" in its devices list; N displays "Ready."

The relay sees only Noise traffic (opaque ciphertext) and the eventual `device_cert` envelope's metadata.

## Out-of-band channel security

- **QR path:** the QR carries `N_static_pub` (32 B), so the handshake's authentication is bound to a 256-bit value the user transferred out-of-band. MITM is computationally infeasible.
- **Numeric-only path:** the 6-digit code is the SAS computed from the handshake hash. Security relies on interactive context: the user aborts on mismatch. A MITM has a single online attempt at a 1-in-1,000,000 collision; we require the user to confirm explicitly on both sides, and we rate-limit pair attempts per account.
- **USB path** (Tauri desktop ↔ phone): the same Noise handshake runs over a USB transport; the SAS step is mandatory regardless of transport.

## Mode B: pair using only a recovery code

For users with no surviving trusted device. Documented in [`recovery.md`](./recovery.md).

## Onboarding UX requirements

Independent of cryptography:

- A new user reaches a usable Today screen in **≤ 90 s** from app launch on a 4G connection.
- The recovery code is shown **exactly once** and the user passes a paste-back confirmation.
- A device added via Mode A appears in every other paired device's "Devices" view within one sync round-trip, with the time and approximate location of the pairing.

## What the server sees during pairing

- The relay endpoint observes a stream of opaque Noise messages between two TLS-authenticated peers, plus one resulting `device_cert` op.
- The server learns: a new `device_id`, the new device's public keys, and the timestamp.
- The server does not learn: the SAS, the transferred private keys, or stream keys.
