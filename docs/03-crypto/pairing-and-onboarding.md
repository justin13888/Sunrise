---
status: accepted
---

# Pairing and Onboarding

Three flows: account creation, adding a new device, recovering identity. Recovery is in [`recovery.md`](./recovery.md); this spec covers account creation and the pair-from-existing-device flow.

## Implementation status

**The handshake is real; the transport under it is not, and what crosses is not yet the `PairingPayload` below.**

Implemented, in `crates/sunrise-pairing`:

* The full **Noise XX** transcript, `Noise_XX_25519_ChaChaPoly_SHA256` (`handshake.rs::NOISE_PARAMS`), with a throwaway X25519 static per handshake as §Handshake specifies.
* The **QR payload** codec — lex-ordered UTF-8 JSON, base64url no-pad, magic prefix (`qr.rs`).
* The **6-digit SAS**, `BLAKE3("sunrise.pair_sas.v1" || h, 3)` (`sas.rs`), with the SAS gate enforced by the session type rather than by a caller remembering to check it.

Not implemented:

* **The relay rendezvous does not exist.** There is no pairing route on `sunrise-server` — the router exposes `/accounts`, `/devices`, `/blobs`, `/meta`, `/health` and the `/sync` WebSocket, and nothing that routes by `pair_id`. §Relay framing for Noise, the three-message-per-role buffer and the 60 s window describe nothing. `crates/sunrise-core-bindings/src/pairing.rs` states the consequence outright: "the *transport* for them **is currently the user**" — the three handshake messages and the sealed root cross as base64url strings a person copies between the two machines by hand. The crypto is unaffected by that: the SAS binds the transcript either way.
* **The rate limits are constants, not a limiter.** `RATE_LIMIT_HOURLY = 10` and `RATE_LIMIT_DAILY = 30` are declared in `rate_limit.rs` and read by nothing. No counter, no `429`, no `Retry-After`.
* **The rendezvous, not the payload.** What crosses the channel *is* `PairingPayload` — `ID_S_priv`, the identity's public halves and id, the vault root, and every Stream key the sending device holds, as the integer-keyed canonical CBOR §Payload transfer specifies (`crates/sunrise-pairing/src/payload.rs`). **`ID_D_priv` does not travel**: sealing to the identity needs only the public half, and a paired device that held the private one could open the identity copy of every epoch, which is what made revocation unenforceable. The receiver checks `identity_id == identity_id_from_pub(ID_S_pub)` in constant time before writing anything, mints its own `D_S` / `D_D`, and self-issues a cert under the just-received `ID_S_priv`, which it publishes as a `device_cert` op. `Command::TrustDevice` is gone. One limit is surfaced rather than hidden: a payload over `MAX_PAIRING_PAYLOAD` is refused rather than truncated, because chunking it needs a framing contract that reaches the Swift seam, where a sealed payload is one base64 string.
* **Account creation (§Account creation) has no client.** `AccountCreateRequest` is consumed by the server and produced by nothing; no client mints an identity keypair, a recovery blob, or an unlock method.

## Account creation

1. User opens Sunrise, picks "Create new identity."
2. Client generates `ID_S` and `ID_D` keypairs locally with the OS CSPRNG.
3. Client generates the first device's `D_S`, `D_D` and assembles a `DeviceCert`.
4. Client requires the user to set up an unlock method:
   - **Preferred:** OS keystore-backed passcode/biometric (iOS, macOS, modern Android, Windows Hello).
   - *Target state — fallback:* passphrase, with UI-enforced minimum entropy via a zxcvbn-style estimator (≥ 60 bits). Neither the passphrase path nor the estimator exists: `Unlock::Passphrase` has no caller, and every vault root is 32 random bytes held by the OS keystore ([`../04-storage/local-database.md`](../04-storage/local-database.md) §SQLCipher key).
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
                                          ;   "sunrise.account_create.v1" || BLAKE3(canonical_cbor(prev_fields), 32))
   }
   ```
7. Server validates the signature against `ID_S_pub`, persists the bundle, and returns an account ID. The server stores `recovery_blob` opaquely.

The server never sees `ID_S_priv`, `ID_D_priv`, the recovery code, the unlock secret, or any Stream key.

## Adding a new device — Mode A: pair from a trusted device

The new device (N) starts with no keys. An existing device (E) authorizes it via an out-of-band channel. This is the recommended path.

### Handshake

We use **Noise XX** with pattern `Noise_XX_25519_ChaChaPoly_SHA256` (Noise spec §7.5). XX gives mutual authentication with deferred identity transmission, suiting a flow where N's "identity" is its newly-generated key and E proves possession of `ID_S_priv` mid-flow.

The Noise static keys for the handshake are **not** the device or identity long-term keys. Each side generates an ephemeral X25519 static-for-this-handshake keypair (`s`) and the standard Noise ephemeral (`e`). Long-term keys are transmitted as Noise transport-layer messages after the handshake completes.

### QR / pairing-payload encoding

The QR payload begins with the unified 5-byte magic prefix from [`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md) §3 (`"SR" + kind=7 + version=0x0001`). The QR contents are JSON, UTF-8, with stable lex-ordered keys; see fixture `tests/fixtures/pair/qr.v1.json`. Inside the JSON object:

- `magic_v1` — the 5-byte magic prefix, encoded as 10 lowercase hex chars.
- `pair_id` — 16 random bytes generated by side N, base64url no-pad. This is the `pair_session_id` the relay uses to route the handshake (see "Relay framing" below).
- `n_static_pub` — N's 32-byte X25519 ephemeral static, base64url no-pad.
- `account_email_hash` — `BLAKE3(email_normalized, 4)` rendered as 8 lowercase hex chars. *Target state.* The relay does not route on it: pairing is routed on `pair_session_id`, and the account is resolved from the request's bearer ([`../06-server/api.md`](../06-server/api.md)`:85-89`). The field survives as an out-of-band check the user's own devices can make.
- `relay_url` — UTF-8 string, ≤ 256 bytes, MUST be `https://` or `wss://`. TLS validation uses the system trust store; v1 ships without certificate pinning (deferred to v2 per ADR).

`email_normalized` is UTF-8 NFC, ASCII-lowercased. The lowercase rule applies only to ASCII codepoints; non-ASCII characters are unchanged. All base64url crypto fields use **no padding**.

### Relay framing for Noise

Noise messages run over a TLS WebSocket. Each WebSocket frame is a single binary message with the layout:

```
0:1     msg_type: u8         ; 1 = noise_payload, 2 = pair_abort, 3 = sas_confirm, 4 = sas_reject
1:5     length: u32 big-endian
5:N     payload bytes
```

Maximum payload: 64 KiB. Routing: each side authenticates its WebSocket with `(account_email_hash, role)` where role ∈ `{N, E}`. The relay buffers up to 3 messages (≤ 64 KiB each) per `(pair_session_id, role)` for at most 60 s; beyond either limit the session is dropped.

### Flow

1. **N → E**: presents a QR (and fallback 6-digit code) with the contents above.
2. **User on E** scans the QR (or types the 6-digit fallback). E confirms a nickname for N.
3. **E ⇄ N**: Noise XX handshake runs over the relay — a TLS WebSocket whose payloads the server cannot interpret. Both devices connect to the same relay endpoint, identified by the `relay_url` carried in the QR.
4. After the handshake's third message:
   - Both sides have agreed transport keys.
   - Both sides compute the same handshake hash `h`.
5. **SAS confirmation.** Both devices display:
   ```
   SAS = decimal( BLAKE3("sunrise.pair_sas.v1" || h, 3) ) % 1_000_000   ; 6 digits
   ```
   The user reads SAS aloud or compares on-screen. Both devices show "Match" / "Don't match." **Both** must tap "Match" before the handshake completes. A "Don't match" tap on either side broadcasts `pair_abort` (msg_type=2) over the established Noise channel and both sides discard ephemeral keys. A timeout of 90 seconds at the SAS confirmation screen aborts the pair the same way. A MITM cannot match the SAS without a successful preimage attack on a 6-decimal-digit channel binding (~20 bits of authentication, *protected by the live interactive context* — one mismatched code aborts the entire pairing).
6. **On confirm, E sends transport messages to N** (each is a Noise transport message; the Noise tunnel is used as an authenticated, encrypted channel):
   ```cddl
   PairingPayload = {
       1: bstr .size 32,         ; ID_S_priv (32-byte seed)
       ; 2 was ID_D_priv. Burned, never reused: a payload still carrying it
       ; is refused, because a device that holds it can open the identity copy
       ; of every epoch and no revocation can bound its reads.
       3: bstr .size 32,         ; ID_S_pub
       4: bstr .size 32,         ; ID_D_pub
       5: bstr .size 16,         ; identity_id_bytes
       6: { * bstr .size 16 => { * uint => bstr .size 32 } },
                                 ; stream_keys: stream_id => epoch => key
       7: tstr,                  ; nickname for N
       8: tstr,                  ; platform
       9: bstr .size 32          ; vault_root
   }
   ```
7. **N receives the payload, validates** that `BLAKE3("sunrise.identity_id.v1" || ID_S_pub, 16) == identity_id_bytes` (a constant-time compare via `subtle::ConstantTimeEq`), generates its own `D_S` / `D_D` keypairs, and constructs a `DeviceCert` for itself signed by the just-received `ID_S_priv`. `ID_D_pub` is **not** checked, and cannot be: with no private half in the payload there is nothing to recompute it from, and a receiver holding only public material cannot tell the account's real `ID_D_pub` from any other valid X25519 point. What stands behind it is the same SAS-confirmed Noise channel that stands behind `ID_S_priv` and the vault root in the same message. A wrong value here does not disclose anything — it seals epochs the recovery blob cannot open, so the failure is an unrecoverable account rather than a readable one, which is why `identity_id`, the field that decides *whose* account this device joins, keeps its check.
8. **N publishes its `device_cert` op** (signed by its new `D_S_priv`, control envelope) into the vault-meta log, and stores keys per [`identity-and-device-keys.md`](./identity-and-device-keys.md).
9. **E displays** "Paired with N at `<time>`" in its devices list; N displays "Ready."

The relay sees only Noise traffic (opaque ciphertext) and the eventual `device_cert` envelope's metadata.

### Failure paths

| Event | Both sides see | Server-side effect |
|---|---|---|
| Either party closes WebSocket pre-SAS | UI: "Pairing interrupted, try again." | Session discarded; counts against rate limit. |
| One confirms, one rejects | UI on confirmer: "Other device rejected the pairing." UI on rejector: "Pairing cancelled." | Session discarded. |
| Relay disconnect mid-handshake | UI: "Pairing interrupted." Both retry from QR. | Session expires after 60 s. |
| SAS confirmation timeout (90 s) | UI: "Pairing timed out." | Session discarded. |
| Inactivity timeout (300 s from QR generation) | UI: "Pair window expired, generate a new code." | Session discarded; QR no longer valid. |

### Rate limits

- **Per account_email_hash:** at most **10 pair attempts per rolling hour**, **30 per rolling day**.
- **Per relay-IP:** at most 60/hour (defense in depth against shared-NAT users).
- Excess returns `srv.pair.rate_limited` HTTP 429 with `Retry-After` set to the seconds until the next slot opens. The Noise transport is never opened.
- A pair attempt counts against quota the moment the relay accepts the first Noise message; aborted-before-first-message attempts do not count. SAS aborts (whether by tap or timeout) DO count.

## Out-of-band channel security

- **QR path:** the QR carries `n_static_pub` (32 B), so the handshake's authentication is bound to a 256-bit value the user transferred out-of-band. MITM is computationally infeasible.
- **Numeric-only path:** the 6-digit code is the SAS computed from the handshake hash. Security relies on interactive context: the user aborts on mismatch. A MITM has a single online attempt at a 1-in-1,000,000 collision; we require the user to confirm explicitly on both sides, and we rate-limit pair attempts per account.

> v1 does not ship LAN/mDNS, BLE, or USB pairing transports. All pairing handshakes relay through the server (which sees only opaque Noise ciphertext).

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
