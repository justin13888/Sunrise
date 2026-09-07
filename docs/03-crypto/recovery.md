---
status: accepted
---

# Recovery

If all of a user's devices are lost or wiped, the recovery code restores access. **If the user lacks both their devices and their recovery code, the data is unrecoverable.** This is the fundamental cost of E2EE; we do not pretend otherwise, and we tell the user so before account creation completes.

## Implementation status

The **cryptography below is implemented and correct.** `crates/sunrise-crypto/src/recovery.rs` seals and unseals the blob exactly as specified: Argon2id version 0x13, m = 65536 KiB, t = 3, p = 1, 32-byte output; the blob is `magic(5) || salt(16) || nonce(24) || ct`; the AAD is `"sunrise.recovery_blob.v1" || identity_id_bytes`, so a blob cannot be replayed across accounts. `crates/sunrise-onboarding/src/recovery.rs` wraps the unseal as `recover_identity`.

**The flow around it is not.** Specifically:

* **BIP-39 is not implemented anywhere.** No wordlist, no encoder, no checksum validator. `sunrise-onboarding` says so in its own header: the crate "accepts the already-derived 32-byte seed". Steps 2 and 3 of §Recovery code have no code behind them.
* **No client ever calls `seal_recovery_blob`** outside tests, so no blob is ever produced or uploaded. `AccountCreateRequest.recovery_blob` has no producer in the tree.
* **`ID_D_priv` now exists in exactly one vault, and losing that vault destroys it permanently.** Since `PairingPayload` stopped carrying the identity's X25519 secret, `Keychain::create` keeps it only on the device that *created* the account; every device admitted by pairing gets `dh_secret: None`. The blob above is meant to be the second copy and is not built, so there is no second copy. If the creator's device is lost before the blob ships, `ID_D_priv` is gone: every `Recipient::Identity` copy in the op log becomes permanently unopenable, and **no recovery feature added later can retrieve it**, because sealing a blob needs the key it would carry. Before this change every paired device held `ID_D_priv`, so any survivor could have produced the blob afterwards; none can now. This is disclosed rather than gated — the read bound the change buys is worth more than the window costs — and it applies to every vault created from now on, not only to upgraded ones. `Keychain::holds_only_copy_of_identity_key` answers the question in the core API. **No client surfaces it yet** — there is no `Core` pass-through and no binding, so the Apple app cannot currently ask — which means a user with one device has one copy of the key their account's history is sealed to and is not told so.
* **The blob cannot be fetched back.** `recovery_blob` is a write-only column: `POST /api/v1/accounts` stores it (`crates/sunrise-server/src/store.rs`), and the only read route, `GET /api/v1/accounts/me`, returns `AccountInfo`, which has no blob field. **There is no `GET /accounts/me/recovery_blob` route**, and no email-OTP gate in front of one.
* **Previous-blob retention is not implemented.** There is one `recovery_blob` column and no history table; the 30-day window in §Versioning describes nothing.
* **Recovery today restores nothing readable.** The blob carries *identity* keys. In the tree's hierarchy, decryption depends solely on the vault root, which is a per-account random value in one machine's Keychain and in no backup — see [`identity-and-device-keys.md`](./identity-and-device-keys.md) §What is specified here vs. what is implemented. **Losing the last paired device is total, unrecoverable data loss.** [ADR-0024](../11-adr/0024-key-hierarchy.md) is the decision that fixes this; §Recovery flow step 8 below states what it changes.

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
8. Stream keys themselves are NOT in the recovery blob. *(The `key_envelope` ops this step depends on exist as of [ADR-0024](../11-adr/0024-key-hierarchy.md); the client route that fetches the blob and replays them does not yet, and neither does the BIP-39 codec — see §Implementation status.)* They do not need to be. Per [ADR-0024](../11-adr/0024-key-hierarchy.md) decision 4, every `(stream_id, epoch)` key is sealed by HPKE to **two** recipient classes: to each device's `D_D_pub`, and to the **identity**'s `ID_D_pub`. The identity-sealed `key_envelope` ops live in the op log, which the relay stores as ciphertext it cannot open, and they are opened by `ID_D_priv` — which this blob already carries. The recovered device replays them and reads its history. **A pure recovery, with no surviving device and no peer, restores readable content.**

### The cost of that, stated plainly

This is not free, and the tradeoff belongs here rather than in a footnote. `ID_D_priv` becomes a **long-lived unwrapping key**: its compromise reaches every epoch ever sealed to it, and those envelopes sit on the relay where an attacker who has the key can also fetch them. Epoch rotation bounds *forward* exposure — a revoked device reads nothing written after the new epoch — and that bound exists only because a paired device is no longer handed `ID_D_priv` and so cannot open the identity's copy of a new epoch. The exception is the device that created the account, which still holds the key and therefore still opens every identity copy; revoking that one device does not bound its reads. Closing the exception needs identity rotation on revocation; see [`key-rotation.md`](./key-rotation.md) §Revocation. Backward exposure was never bounded and nothing here pretends it was.

What gates that key is the recovery code itself: 256 bits of BIP-39 entropy through Argon2id at m = 65536, t = 3, p = 1. That is the same gate already protecting the blob, so identity-sealing the Stream keys adds no *new* single point of compromise — it widens the blast radius of the one that already existed.

The alternative this replaces was to store wrapped Stream keys server-side under recovery-stretched material. That is still rejected, for the reason it always was: it puts key material the server holds behind a human-memorable secret. Identity-sealed `key_envelope` ops avoid it — the server holds ciphertext addressed to a public key, and never holds anything wrapped under recovery-stretched material. (See [`../11-adr/0004-crypto-primitives.md`](../11-adr/0004-crypto-primitives.md) and [ADR-0024](../11-adr/0024-key-hierarchy.md) §What this fixes in `recovery.md`.)

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
