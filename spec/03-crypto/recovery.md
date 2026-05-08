---
status: accepted
---

# Recovery

If all of a user's devices are lost or wiped, the recovery code restores access. **If the user lacks both their devices and their recovery code, the data is unrecoverable.** This is the fundamental cost of E2EE; we do not pretend otherwise, and we tell the user so before account creation completes.

## Recovery code

- **Format.** 24 words from the BIP-39 English wordlist (`bip39-english.txt`, 2048 entries).
- **Entropy.** 256 bits raw + 8-bit BIP-39 checksum = 264 bits encoded as 24 × 11-bit words. The 256-bit raw entropy is the working secret; the checksum is verified at code entry to detect typos.
- **Generation.** 32 bytes from the OS CSPRNG; encoded to BIP-39 per BIP-39 §3.

```
recovery_seed = os_csprng(32)            // 32 raw bytes
recovery_code = bip39_encode(recovery_seed)   // "abandon ability ... yellow"  (24 words)
```

## Server-side material

### Argon2id parameters (frozen for v1)

- **Algorithm:** Argon2id, **version 0x13** (RFC 9106).
- **Memory:** `m = 65536` KiB (64 MiB).
- **Iterations:** `t = 3`.
- **Parallelism:** `p = 1`.
- **Output length:** 32 bytes.

These parameters are platform-uniform (chosen for the slower-bound mobile target). There is **no timeout**; the function runs to completion, and the UI shows a progress modal on mobile.

**First-launch calibration.** The app runs Argon2id once on a synthetic password and records the wall-clock time. If > 5 s, the UI shows: *"Recovery on this device is slow (≈ Ns). You can still use it, but consider enabling biometric/keystore unlock for daily use."* This is informational; recovery is never blocked.

**OOM handling.** Catch `argon2::Error::MemoryAllocation` and present: *"Not enough memory to derive recovery key. Close other apps and try again."* This does not consume a rate-limit slot.

### Recovery blob construction

At account creation the client constructs and uploads a single `recovery_blob`:

```
recovery_salt   = os_csprng(16)
recovery_key    = Argon2id(
                      password = recovery_seed,         // 32 bytes
                      salt     = recovery_salt,
                      version  = 0x13,
                      m = 65536 (64 MiB), t = 3, p = 1,
                      out_len  = 32
                  )

recovery_plaintext = canonical_cbor({
    1: ID_S_priv (bstr .size 32),
    2: ID_D_priv (bstr .size 32),
    3: ID_S_pub  (bstr .size 32),
    4: ID_D_pub  (bstr .size 32),
    5: identity_id_bytes (bstr .size 16),
    6: created_at (uint)
})

recovery_nonce  = os_csprng(24)
recovery_blob_ct = XChaCha20-Poly1305_seal(
    key       = recovery_key,
    nonce     = recovery_nonce,
    plaintext = recovery_plaintext,
    aad       = "sunrise.recovery_blob.v1" || identity_id_bytes
)

upload = {
    salt:   recovery_salt,
    nonce:  recovery_nonce,
    ct:     recovery_blob_ct,
    blob_v: 1
}
```

The on-disk and on-wire bytes of `upload` begin with the unified 5-byte magic prefix from [`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md) §3 (`"SR" + kind=3 + version=0x0001`); the recovery-blob version is the same value as the `blob_v` field.

The server stores `upload` keyed by `account_id` (i.e. by the email/identity registered at account creation). The salt and nonce are stored in cleartext alongside the ciphertext; that is intended.

The server can serve `upload` to anyone who proves access to the account email (recovery flow, below). The server cannot decrypt the blob without `recovery_seed`, which it never sees.

### Versioning and previous-blob retention

`blob_v` is an unsigned integer. **v1 reads only `blob_v == 1`**; any other value yields `RECOVERY_VERSION_UNKNOWN` and the UI prompts an upgrade.

When a client uploads a new blob (e.g. on passphrase or recovery-code rotation), the server retains the **previous** blob for **30 days** under `recovery/<account_id>/<uploaded_at>.blob`. The client UI offers "restore from previous recovery key" within that window. After 30 days the previous blob is hard-deleted.

## Recovery flow (Mode B from `pairing-and-onboarding.md`)

1. User installs Sunrise on a fresh device, picks "Recover existing identity."
2. Client prompts for account email and the 24-word code. BIP-39 decoder validates the checksum (typos surface here).
3. Client requests `upload` from the server. The server requires email-OTP verification (a 6-digit code sent to the registered address) before serving the blob, to slow down credential-stuffing-style brute-force attempts. (For self-hosted servers operators may disable this step; see [`../06-server/auth.md`](../06-server/auth.md).)
4. Client runs Argon2id with the stored salt to derive `recovery_key`.
5. Client AEAD-opens `recovery_blob_ct` to recover identity keys.
6. Client generates new `D_S`, `D_D` device keys, signs a fresh `DeviceCert` with `ID_S_priv`, and publishes it.
7. Client SHOULD prompt the user to **revoke any other devices** and **rotate every Stream key** (see [`key-rotation.md`](./key-rotation.md)) — recovery implies an unknown-state environment.
8. Stream keys themselves are NOT in the recovery blob. The recovered device joins as a fresh device of the identity; it receives current Stream keys via the normal sibling-device key-envelope flow once another paired device of this identity comes online — but in the pure recovery scenario, no such device exists. In that case the user has identity but no Stream content keys; they MUST contact any peer with whom they had shared a Stream and request a re-share, or accept that historical content is lost.

This last point is non-negotiable in v1: storing wrapped Stream keys server-side under recovery-stretched material would re-introduce a single point of compromise; we considered it and rejected it. (See [`../11-adr/0004-crypto-primitives.md`](../11-adr/0004-crypto-primitives.md) for the rationale.)

## Recovery code rotation

The user MAY rotate the recovery code at any time from a still-authorized device:

1. Generate a fresh 32-byte `recovery_seed'` and BIP-39 encoding.
2. Re-derive `recovery_key'` and re-seal the recovery blob with a fresh salt and nonce.
3. Sign an upload request with `ID_S_priv` and replace the server's stored blob.
4. Display the new code with a paste-back challenge; old code is invalidated server-side.

## Test-recovery affordance

Before account creation completes, the client offers (and on the strictest setting requires) a "verify recovery now" step: the user re-enters the freshly-displayed code; the client runs the same Argon2id derivation against the just-uploaded blob and confirms a clean decrypt. A user who fails this gate is forced back to "show me the code again" rather than being marooned on a code they wrote down wrong.

## What the user is told

The Recovery setup screen states verbatim:

> Sunrise is end-to-end encrypted. **We cannot reset your account if you lose this code.** This is by design — it means we can't read your data, but it also means we can't recover it for you. Save this code somewhere you'll find it years from now.

## Lockout vs. compromise

If the user thinks the recovery code has been compromised:

1. From a still-authorized device, rotate the recovery code (above).
2. Rotate identity keys (see [`key-rotation.md`](./key-rotation.md)).
3. Rotate every Stream key.

This is a heavy operation (re-signs DeviceCerts, re-issues every share grant, invalidates previous shares pending peer re-acceptance). It is documented and not encouraged casually.

## Deferred (not v1)

- **Social recovery / Shamir-split** of the recovery seed across trusted contacts. The recovery flow is designed to admit this: an alternate path can produce `recovery_seed` from M-of-N shares without changing the server-side blob format.
- **Hardware-key backup** (FIDO2 / YubiKey holding the wrapping key as a second path).
- **Device-to-device recovery without server.** A dusty laptop in a drawer is already a recovery path via Mode A pairing once it's powered on.
