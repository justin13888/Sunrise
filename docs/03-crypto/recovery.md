---
status: accepted
---

# Recovery

If all of a user's devices are lost or wiped, the recovery code restores access. **If the user lacks both their devices and their recovery code, the data is unrecoverable.** This is the fundamental cost of E2EE; we do not pretend otherwise, and we tell the user so before account creation completes.

## Implementation status

The **cryptography below is implemented and correct.** `crates/sunrise-crypto/src/recovery.rs` seals and unseals the blob exactly as specified: Argon2id version 0x13, m = 65536 KiB, t = 3, p = 1, 32-byte output; the blob is `magic(5) || salt(16) || nonce(24) || ct`; the AAD is `"sunrise.recovery_blob.v1" || identity_id_bytes`, so a blob cannot be replayed across accounts. `crates/sunrise-onboarding/src/recovery.rs` wraps the unseal as `recover_identity`.

**The flow around it is now built on the CLI path, and partly built elsewhere.** Specifically:

* **BIP-39 is implemented** in `crates/sunrise-crypto/src/bip39.rs`, against the wordlist in `bip39-english.txt` — the file §Recovery code names, pinned to the SHA-256 digest BIP-39 publishes for it. Both directions are asserted against all twenty-four published English test vectors (`crates/sunrise-crypto/tests/bip39_vectors.rs`), not only against a round trip, which is what makes a code produced here readable by any other BIP-39 implementation. `sunrise_onboarding::recover_identity_from_code` is steps 2–5 of §Recovery flow from what a user types.
* **`sunrise bootstrap` calls `seal_recovery_blob`.** It draws 32 bytes, seals the vault's identity keys under them, uploads the blob with `POST /api/v1/accounts`, and shows the 24-word code once — to stdout, never to a file and never to a log record. **The Apple clients do not yet**, so a vault created there still has no blob.
* **`ID_D_priv` has a second copy on the CLI path and nowhere else.** Since `PairingPayload` stopped carrying the identity's X25519 secret, `Keychain::create` keeps it only on the device that *created* the account; every device admitted by pairing gets `dh_secret: None`. Where the blob has been sealed, that is the second copy and losing the creating device is survivable. Where it has not — every vault created by an Apple client today — `ID_D_priv` is gone with that machine: every `Recipient::Identity` copy in the op log becomes permanently unopenable, and **no recovery feature added later can retrieve it**, because sealing a blob needs the key it would carry. `Keychain::holds_only_copy_of_identity_key` answers the question in the core API and `Core::holds_identity_key` passes it through; there is still no binding, so the Apple app cannot ask.
* **The blob can be fetched back.** `GET /api/v1/accounts/me/recovery_blob` serves it, behind an **OIDC step-up** rather than the email OTP §Recovery flow step 3 below used to specify — see [`../06-server/auth.md`](../06-server/auth.md) §Recovery for why, and `crates/sunrise-server/src/auth/step_up.rs` for what an ordinary bearer is refused for. The column is write-once: `POST /accounts` answers `409 RECOVERY_BLOB_EXISTS` to a differing blob rather than discarding it, which is what makes a displayed recovery code trustworthy.
* **Previous-blob retention is not implemented.** There is one `recovery_blob` column and no history table; the 30-day window in §Versioning describes nothing. Neither is the `PUT`, so **§Recovery code rotation has no route.**
* **Recovery restores readable content only where `key_envelope` ops exist for it.** The blob carries *identity* keys, and [ADR-0024](../11-adr/0024-key-hierarchy.md) seals every `(stream_id, epoch)` key to `ID_D_pub` as well as to each device — so a recovered identity opens the history. What the blob does not carry is the **vault root**, which keys the local database: a recovering device mints its own. See §Recovery flow step 8, and [`identity-and-device-keys.md`](./identity-and-device-keys.md) §What is specified here vs. what is implemented.

## Device backups do not carry the vault root

This is the guarantee §Implementation status leans on when it says the vault root is in no
backup, and it is the part of that guarantee a user can actually meet. The root is stored under
`kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly` — `KeychainVaultRootStore.accessibility`
in `apps/apple/Sunrise/Identity/VaultRootStore.swift` — which iOS keeps wrapped under the
device's UID key, so an encrypted backup cannot re-key it for other hardware.

- **The vault is in the backup; its key is not.** The database sits in the app container and
  an encrypted device backup carries it ([`../07-clients/mobile-ios.md`](../07-clients/mobile-ios.md)
  §File system). Restore that backup onto a *new* device and Sunrise comes up holding a vault
  it cannot open — `SessionModel.LockReason.keyMissingForExistingVault`, which is a distinct
  state from first run and says so on screen: *"There is a vault on this device, but its key is
  not in this Keychain. Pair with a device that still has it — creating a new key would leave
  the existing data unreadable."*
- **Restoring the same device keeps the key.** The class excludes the item from moving to other
  hardware, not from a same-device restore.
- **Pairing is how the key comes back.** A device that still holds the root sends it over, and
  `SessionModel.adoptPairing` is the one path allowed to write a root over an existing vault.
- **With no surviving device there is no way back** — the same total loss §Implementation status
  states, reached by a route that looks like it should have worked. Nothing about a completed
  backup implies the vault inside it is recoverable.
- **macOS does not have this guarantee yet.** The Mac app uses the file-based login keychain,
  which accepts `kSecAttrAccessible` and stores nothing (`SecItemAdd` with
  `kSecUseDataProtectionKeychain` returns `errSecMissingEntitlement` for an app with neither the
  App Sandbox nor a keychain-access-group entitlement, both deferred to release work in
  `apps/apple/project.yml`). A Mac moved by Migration Assistant or restored from Time Machine
  carries the login keychain and therefore the vault root. The Apple client declares the right
  class on both platforms; only iOS enforces it.

## Recovery code

- **Format.** 24 words from the BIP-39 English wordlist (`crates/sunrise-crypto/src/bip39-english.txt`, 2048 entries).
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
3. Client requests `upload` from the server with `GET /api/v1/accounts/me/recovery_blob`. **The server requires an OIDC step-up** — an `auth_time` inside `recovery_max_auth_age_secs`, plus any `acr`/`amr` values the operator configured — before serving the blob, to slow down credential-stuffing-style brute-force attempts. Earlier revisions of this line specified a 6-digit email OTP, which [`../00-product/non-goals.md`](../00-product/non-goals.md) forbids the server from implementing; the step-up asks the IdP for the same assurance without Sunrise holding an OTP secret or sending mail. The single-tenant self-host verifier is exempt, since it has no IdP to ask; see [`../06-server/auth.md`](../06-server/auth.md) §Recovery.
4. Client runs Argon2id with the stored salt to derive `recovery_key`.
5. Client AEAD-opens `recovery_blob_ct` to recover identity keys.
6. Client generates new `D_S`, `D_D` device keys, signs a fresh `DeviceCert` with `ID_S_priv`, and publishes it.
7. Client SHOULD prompt the user to **revoke any other devices** and **rotate every Stream key** (see [`key-rotation.md`](./key-rotation.md)) — recovery implies an unknown-state environment.
8. Stream keys themselves are NOT in the recovery blob. *(The `key_envelope` ops this step depends on exist as of [ADR-0024](../11-adr/0024-key-hierarchy.md), as do the BIP-39 codec and the route that fetches the blob; the client that puts the three together and replays the ops does not — see §Implementation status.)* They do not need to be. Per [ADR-0024](../11-adr/0024-key-hierarchy.md) decision 4, every `(stream_id, epoch)` key is sealed by HPKE to **two** recipient classes: to each device's `D_D_pub`, and to the **identity**'s `ID_D_pub`. The identity-sealed `key_envelope` ops live in the op log, which the relay stores as ciphertext it cannot open, and they are opened by `ID_D_priv` — which this blob already carries. The recovered device replays them and reads its history. **A pure recovery, with no surviving device and no peer, restores readable content.**

### The cost of that, stated plainly

This is not free, and the tradeoff belongs here rather than in a footnote. `ID_D_priv` becomes a **long-lived unwrapping key**: its compromise reaches every epoch ever sealed to it, and those envelopes sit on the relay where an attacker who has the key can also fetch them. Epoch rotation bounds *forward* exposure — a revoked device reads nothing written after the new epoch — and that bound exists only because a paired device is no longer handed `ID_D_priv` and so cannot open the identity's copy of a new epoch. The exception is the device that created the account, which still holds the key and therefore still opens every identity copy; revoking that one device does not bound its reads. Closing the exception needs identity rotation on revocation; see [`key-rotation.md`](./key-rotation.md) §Revocation. Backward exposure was never bounded and nothing here pretends it was.

What gates that key is the recovery code itself: 256 bits of BIP-39 entropy through Argon2id at m = 65536, t = 3, p = 1. That is the same gate already protecting the blob, so identity-sealing the Stream keys adds no *new* single point of compromise — it widens the blast radius of the one that already existed.

The alternative this replaces was to store wrapped Stream keys server-side under recovery-stretched material. That is still rejected, for the reason it always was: it puts key material the server holds behind a human-memorable secret. Identity-sealed `key_envelope` ops avoid it — the server holds ciphertext addressed to a public key, and never holds anything wrapped under recovery-stretched material. (See [`../11-adr/0004-crypto-primitives.md`](../11-adr/0004-crypto-primitives.md) and [ADR-0024](../11-adr/0024-key-hierarchy.md) §What this fixes in `recovery.md`.)

## Recovery code rotation

The user MAY rotate the recovery code at any time from a still-authorized device:

**Not implemented: there is no `PUT /api/v1/accounts/me/recovery_blob`, and `POST /accounts` refuses a second, differing blob.** The steps below are the target.

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
