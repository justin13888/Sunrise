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
* **The rendezvous, not the messages.** What crosses the channel is the three-message exchange §Flow specifies — offer, request, grant — encoded as integer-keyed canonical CBOR (`crates/sunrise-pairing/src/protocol.rs`). **Neither identity private key travels.** `ID_D_priv` stopped in [#76](https://github.com/justin13888/Sunrise/issues/76): sealing to the identity needs only the public half, and a paired device that held the private one could open the identity copy of every epoch. `ID_S_priv` stopped in [#105](https://github.com/justin13888/Sunrise/issues/105): it signs every `DeviceCert`, so a device holding it could mint one for any device id it invented. The joining device mints its own `D_S` / `D_D` and the sponsor issues its cert; `Command::TrustDevice` is gone.
* **Account creation (§Account creation) has a client on the CLI only.** `sunrise bootstrap` mints nothing new — the identity keypair is `Keychain::create`'s, minted at first vault open — but it is what publishes `ID_S_pub`/`ID_D_pub` and a sealed recovery blob to the relay, and it shows the 24-word recovery code once. **No Apple client does**, so a vault created there still uploads no blob and its `ID_D_priv` has no second copy; see [`recovery.md`](./recovery.md) §Implementation status. The unlock method is still not chosen by any client: every `Unlock` variant carries an already-materialized vault root.

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

The QR payload begins with the unified 5-byte magic prefix from [`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md) §3 (`"SR" + kind=7 + version=0x0001`). The QR contents are JSON, UTF-8, with stable lex-ordered keys; the encoder and decoder are `crates/sunrise-pairing/src/qr.rs`, which is the authority on the field encodings below. There is no checked-in fixture: the `tests/fixtures/pair/` tree this section used to name was never created. Inside the JSON object:

- `magic_v1` — the 5-byte magic prefix, encoded as 10 lowercase hex chars.
- `pair_id` — 16 random bytes generated by side N, base64url no-pad. This is the `pair_session_id` the relay uses to route the handshake (see "Relay framing" below).
- `n_static_pub` — N's 32-byte X25519 ephemeral static, base64url no-pad.
- `account_email_hash` — `BLAKE3(email_normalized, 4)` rendered as 8 lowercase hex chars. *Target state.* The relay does not route on it: pairing is routed on `pair_session_id`, and the account is resolved from the request's bearer ([`../06-server/api.md`](../06-server/api.md)`:85-89`). The field survives as an out-of-band check the user's own devices can make.
- `relay_url` — UTF-8 string, ≤ 256 bytes, MUST be `https://` or `wss://`. TLS validation uses the system trust store; certificate pinning is not built and not ranked on the roadmap ([`../roadmap.md`](../roadmap.md)).

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
6. **On confirm, three messages cross the channel**, alternating. Each is a Noise transport message; the Noise tunnel is an authenticated, encrypted channel and it is bidirectional, which the old one-shot payload never used.

   **Why three.** `ID_S_priv` does not travel. A `DeviceCert` names only its *subject* and carries one signature — the identity's — so a device holding `ID_S_priv` can mint a genuinely valid cert for any device id it invents, which is how a revoked device rejoined ([#105](https://github.com/justin13888/Sunrise/issues/105)). E issues N's cert instead, and E cannot sign a cert for keys N has not minted yet. There is no one-shot shape that works; the two that were priced are in §Rejected one-shot alternatives below.

   **Message 1 — E → N, the offer.** Public material only. An abandoned pairing costs nothing, and an attacker who intercepts this alone has learned what the account already publishes.
   ```cddl
   PairingOffer = {
       1: bstr .size 32,         ; ID_S_pub
       2: bstr .size 32,         ; ID_D_pub
       3: bstr .size 16,         ; identity_id_bytes, in force
       4: bstr .size 16,         ; genesis_identity_id
       5: bstr .size 32,         ; genesis ID_S_pub
       6: tstr,                  ; E's nickname
       7: tstr                   ; E's platform
   }
   ```

   **Message 2 — N → E, the cert request.** N mints `D_S` / `D_D` on receiving the offer and sends the **public halves**. The private halves never leave N, which is what separates this from the rejected shape where E mints N's keypair.
   ```cddl
   PairingRequest = {
       1: bstr .size 16,         ; device_id = BLAKE3("sunrise.device_id.v1", D_S_pub, 16)
       2: bstr .size 32,         ; D_S_pub
       3: bstr .size 32,         ; D_D_pub
       4: bstr .size 16,         ; identity_id the offer named, echoed back
       5: tstr,                  ; nickname N wants
       6: tstr                   ; N's platform
   }
   ```

   **Message 3 — E → N, the grant.** The only message worth stealing.
   ```cddl
   PairingGrant = {
       1: bstr,                  ; DeviceCert for N, signed by ID_S_priv
       2: bstr .size 32,         ; vault_root
       3: { * bstr .size 16 => { * uint => bstr .size 32 } }
                                 ; stream_keys: stream_id => epoch => key
   }
   ```

7. **What each side checks**, and every field is a value the *other* side chose, so each is recomputed rather than believed. All comparisons are constant-time via `subtle::ConstantTimeEq`.

   - **N, on the offer**: `BLAKE3("sunrise.identity_id.v1" || ID_S_pub, 16) == identity_id_bytes`, and the same on the genesis pair. `ID_D_pub` is **not** checked, and cannot be: with no private half anywhere in the exchange there is nothing to recompute it from, and a receiver holding only public material cannot tell the account's real `ID_D_pub` from any other valid X25519 point. What stands behind it is the SAS-confirmed channel. A wrong value does not disclose anything — it seals epochs the recovery blob cannot open, so the failure is an unrecoverable account rather than a readable one, which is why `identity_id`, the field that decides *whose* account this device joins, keeps its check.
   - **E, on the request**: `device_id == BLAKE3("sunrise.device_id.v1" || D_S_pub, 16)`, and `identity_id` is this account's. A joiner that could name its own id would choose one the revocation register already excludes, or one that collides with a sibling's row.
   - **N, on the grant**: the cert parses, verifies under the `ID_S_pub` **the offer named** — not under whatever identity the cert claims, so a cert that is internally consistent under some other well-formed identity is refused — and names N's own `device_id`, `d_s_pub` and `d_d_pub`. This is the check that makes a captured request useless at a second sponsor: that sponsor will happily issue, and N refuses what comes back.

8. **N assembles a `PairingPayload`** from the three — the account's public identity, its own device keys, its cert, the vault root and the Stream keys — and opens its vault with it. That type is no longer a wire message; it exists because two seams have to carry the assembled result across a process or language boundary (UniFFI's `paired_bundle`, the CLI's pending-pairing file). Fields 1, 2, 7 and 8 of its encoding are **burned**: 1 was `ID_S_priv` and 2 was `ID_D_priv`, and a payload still carrying either is refused rather than silently stripped, because tolerating it would leave the operator believing a revocation binds when it does not.
9. **N publishes its `device_cert` op** (signed by its new `D_S_priv`, control envelope) into the vault-meta log, and stores keys per [`identity-and-device-keys.md`](./identity-and-device-keys.md).
10. **E displays** "Paired with N at `<time>`" in its devices list; N displays "Ready."

One limit is surfaced rather than hidden: a grant over `MAX_PAIRING_PAYLOAD` is refused rather than truncated, because chunking it needs a framing contract that reaches the Swift seam, where a sealed message is one base64 string.

### What this costs, and why it is the right trade

**A device added by pairing cannot add another**, because it holds no `ID_S_priv` to issue a cert with. Nor can it rotate the account identity or seal a recovery blob. Only the device the account was created on — or one restored from the recovery code — can do any of those.

That is the same absence that stops a device you *revoked* from certifying itself back in, and it is why the fix is complete rather than a bound: nothing about a revocation is what prevents the attack, so it keeps preventing it on the second device removed and the tenth. Clients say so up front — `sunrise identity status` prints a `pairing` line, the Apple clients disable "Add a device…" with an explanation — rather than letting a user discover it at the last leg.

### Rejected one-shot alternatives

Both would have avoided the round trip. Both are worse.

1. **E mints N's keypair.** One message, and `ID_S_priv` still never travels. It also hands E permanent impersonation of every device it ever paired — and unlike `ID_S_priv`, no rotation touches `D_S`, so revoking E does not take the capability away. It is #105 in a new shape.
2. **N self-issues, then is re-certified.** N holds no valid cert during the window, so it cannot publish anything — including the request for the cert that would end the window.

A third shape, *sponsor-countersigned* certs, is [ADR-0032](../11-adr/0032-revocation-cannot-bound-cert-issuance.md)'s rejected alternative 2 and is **not** what this is. A countersignature binds a device's membership to its sponsor forever, so revoking one device silently invalidates every device it ever paired. What E produces here is signed by `ID_S_priv` and by nothing else: the same `DeviceCert` shape, one signature, no issuer field, no sponsor binding. Revoking a sponsor locks nobody out.

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

- **QR path:** the QR carries `n_static_pub` (32 B), so the handshake's authentication is bound to a 256-bit value the user transferred out-of-band. MITM is computationally infeasible. The binding is a *comparison*, and E is what makes it: Noise XX defers identity, so E learns N's static only from the third message and has no opinion about which key it should have been. E holds the scanned `n_static_pub` and refuses the transcript — before the SAS screen — if the two differ. Without that comparison the QR path degrades silently to the numeric path's 20 bits.
- **Numeric-only path:** the 6-digit code is the SAS computed from the handshake hash. Security relies on interactive context: the user aborts on mismatch. A MITM has a single online attempt at a 1-in-1,000,000 collision; we require the user to confirm explicitly on both sides, and we rate-limit pair attempts per account.

> There are no LAN/mDNS, BLE, or USB pairing transports. All pairing handshakes relay through the server (which sees only opaque Noise ciphertext).

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
