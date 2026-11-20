---
status: accepted
---

# Recovery

If all of a user's devices are lost or wiped, the recovery code restores access. **If the user lacks both their devices and their recovery code, the data is unrecoverable.** This is the fundamental cost of E2EE; we do not pretend otherwise, and we tell the user so before account creation completes.

## Implementation status

Each line names what exists; the section it points to holds the rule, and the ADR holds the rationale.

**Built:**

- The blob's cryptography, exactly as §Server-side material specifies: `crates/sunrise-crypto/src/recovery.rs`.
- The 24-word encoding of §Recovery code, asserted against all 24 published BIP-39 English vectors: `crates/sunrise-crypto/tests/bip39_vectors.rs`.
- Sealing a blob at account creation on the CLI and on Apple ([#181](https://github.com/justin13888/Sunrise/issues/181)), and saying where none was sealed: §Before a blob is sealed, the vault is the only copy.
- The paste-back of §Test-recovery affordance, on Apple.
- Fetching the blob behind an OIDC step-up rather than an email OTP: §Recovery flow step 3, and [`../06-server/auth.md`](../06-server/auth.md) §Recovery.
- A write-once blob, replaceable only by a registered device, with a retired check in place of previous-blob retention: §Versioning and previous-blob retention ([ADR-0070](../11-adr/0070-a-recovery-code-survives-a-rotation-that-carries-it.md)).
- §Recovery flow, one implementation both clients call: `sunrise recover` (`crates/sunrise-cli/src/recover.rs`) and Apple's "Restore from recovery code" ([#349](https://github.com/justin13888/Sunrise/issues/349)).
- Restoring readable content through identity-sealed key envelopes and the derived vault-meta genesis key: §Recovery flow step 8 ([ADR-0024](../11-adr/0024-key-hierarchy.md)).
- Recovery that does not depend on op retention, through the account's key bundle ([ADR-0069](../11-adr/0069-the-accounts-keys-travel-as-an-identity-sealed-bundle.md), [ADR-0068](../11-adr/0068-a-streams-snapshot-travels-as-a-pinned-blob.md)): `crates/sunrise-e2e/tests/cli_recover_trimmed.rs`.
- Recovery after an identity rotation, with the carried code or the new one, and refusal of a retired one ([ADR-0070](../11-adr/0070-a-recovery-code-survives-a-rotation-that-carries-it.md)): `crates/sunrise-e2e/tests/cli_recover_rotated.rs`.
- §Recovery code rotation (`sunrise identity new-code`) and §Lockout vs. compromise (`sunrise identity rotate`).

**Not built:**

- The paste-back challenge of §Recovery code rotation step 4.
- The first-launch calibration and the out-of-memory message under §Argon2id parameters (frozen).
- The Mac half of §Device backups do not carry the vault root: until a signed Release ships, the Mac's login keychain carries the vault root into its backups.
- Everything under §Not built.

## Before a blob is sealed, the vault is the only copy

Every device admitted by pairing gets `dh_secret: None`: `PairingPayload` stopped carrying the identity's X25519 secret in [#76](https://github.com/justin13888/Sunrise/issues/76), so `ID_D_priv` lives only on the device that created the account and in the recovery blob. Where a blob has been sealed, that is the second copy, and losing the creating device is survivable. Where it has not, because the upload never completed or the device has no relay configured, **`ID_D_priv` is gone with that machine**: every `Recipient::Identity` copy in the op log becomes permanently unopenable, and no recovery feature added later can retrieve it, because sealing a blob needs the key it would carry.

So both clients seal at account creation, and both say so when they have not:

- **The ceremony.** `sunrise bootstrap` draws 32 bytes, seals the vault's identity keys under them, uploads the blob with `POST /api/v1/accounts`, and shows the 24-word code once, to stdout, never to a file or a log record. The Apple clients run the same ceremony behind one call, `SunriseCore::bootstrap_account`, which draws, seals, uploads and drops the seed on the Rust side, so the code that reaches Swift is always the code the stored blob opens; `RecoveryCodeModel` runs it the moment a vault is created ([#181](https://github.com/justin13888/Sunrise/issues/181)).
- **The disclosure.** `Keychain::holds_only_copy_of_identity_key` answers the question in the core API, and `Core::holds_identity_key` exports it across the UniFFI seam. `sunrise devices` prints the consequence under the listing, and the Apple client says it in the Devices section of Settings, on the devices it is true of and nowhere else ([#144](https://github.com/justin13888/Sunrise/issues/144)).

## Device backups do not carry the vault root

The blob does not carry the vault root, and neither does a device backup on iOS; this section is the part of that guarantee a user can actually meet. The root is stored under
`kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly` — `KeychainVaultRootStore.accessibility`
in `apps/apple/Sunrise/Identity/VaultRootStore.swift` — which iOS keeps wrapped under the
device's UID key, so an encrypted backup cannot re-key it for other hardware.

- **The vault is in the backup; its key is not.** The database sits in the app container and
  an encrypted device backup carries it ([`../07-clients/mobile-ios.md`](../07-clients/mobile-ios.md)
  §File system). Restore that backup onto a *new* device and Sunrise comes up holding a vault
  it cannot open — `SessionModel.LockReason.keyMissingForExistingVault`, which is a distinct
  state from first run and says so on screen: *"There is a vault on this device, but its key is
  not in this Keychain. Pair with a device that still has it, or restore from your recovery
  code. A new key would not open this data."*
- **Restoring the same device keeps the key.** The class excludes the item from moving to other
  hardware, not from a same-device restore.
- **Pairing is how the key comes back.** A device that still holds the root sends it over, and
  `SessionModel.adoptPairing` is the one path allowed to write a root over an existing vault.
- **With no surviving device, the recovery code is the only way back.** It restores the account
  from the relay into a new vault, and the unreadable one is moved aside rather than deleted.
  Without the code, this is the same total loss the opening of this document states, reached by a route
  that looks like it should have worked. Nothing about a completed backup implies the vault inside
  it is recoverable.
- **macOS does not have this guarantee yet.** The Mac app uses the file-based login keychain,
  which accepts `kSecAttrAccessible` and stores nothing (`SecItemAdd` with
  `kSecUseDataProtectionKeychain` returns `errSecMissingEntitlement` for an app with neither the
  App Sandbox nor an authorised keychain-access-group entitlement). A Mac moved by Migration
  Assistant or restored from Time Machine carries the login keychain and therefore the vault
  root. The Apple client declares the right class on both platforms; only iOS enforces it.
  Closing the gap is not a one-line entitlement: an ad-hoc-signed build carrying
  `keychain-access-groups` is killed at launch by AMFI, which is what makes the fix wait on a
  real signing identity — measured, with the other two configurations that were tried, in
  [`../07-clients/desktop.md`](../07-clients/desktop.md#the-data-protection-keychain-is-not-a-one-line-entitlement).
  `apps/apple/project.yml` therefore carries the entitlement on the **Release** configuration
  only, which is the one the team signs; Debug builds stay on the login keychain so that
  building the app needs no Apple account.

  The **move** an existing installation would need when that identity arrives is already built,
  and is the half that needed no signature: each store runs `KeychainMigration` on `load`, which
  writes the new item and verifies the read-back before it deletes the old one, is safe to
  interrupt at every step, and refuses rather than guesses when the destination already holds
  different bytes. `KeychainDomain.probe()` asks the platform which keychain this binary can
  actually reach. On every **Mac** build this repository can make the answer is the login
  keychain, so none of it changes what a Mac does today — **this bullet is still exactly true**.
  (On iOS it answers the data-protection keychain, because that is the only one iOS has; this
  bullet is about the Mac, which is where the gap is.) What is outstanding is the team that
  signs a Release and the Developer ID provisioning profile that grants the entitlement; the
  release pipeline already installs that profile and refuses a tag without it
  ([#389](https://github.com/justin13888/Sunrise/issues/389)). The first signed Release to
  launch on a Mac moves each item on its first `load`, and from then on the guarantee holds there.

## Recovery code

- **Format.** 24 words from the BIP-39 English wordlist (`crates/sunrise-crypto/src/bip39-english.txt`, 2048 entries).
- **Entropy.** 256 bits raw + 8-bit BIP-39 checksum = 264 bits encoded as 24 × 11-bit words. The 256-bit raw entropy is the working secret; the checksum is verified at code entry to detect typos.
- **Generation.** 32 bytes from the OS CSPRNG; encoded to BIP-39 per BIP-39 §3.

```
recovery_seed = os_csprng(32)            // 32 raw bytes
recovery_code = bip39_encode(recovery_seed)   // "abandon ability ... yellow"  (24 words)
```

## Server-side material

### Argon2id parameters (frozen)

- **Algorithm:** Argon2id, **version 0x13** (RFC 9106).
- **Memory:** `m = 65536` KiB (64 MiB).
- **Iterations:** `t = 3`.
- **Parallelism:** `p = 1`.
- **Output length:** 32 bytes.

These parameters are platform-uniform (chosen for the slower-bound mobile target). There is **no timeout**; the function runs to completion, and the UI shows a progress modal on mobile.

**Tests outside `sunrise-crypto`.** A debug build with `sunrise-crypto`'s `test-cheap-kdf` feature uses m = 8 KiB, t = 1, p = 1 instead, so tests that create or recover accounts without being about the KDF do not pay for it. A release build cannot: the cheap set is compiled only where `debug_assertions` is on, and a test fails if a shipped package enables the feature. `sunrise-crypto`'s own tests and its frozen vector stay at the parameters above. A blob sealed under one set opens only under that set. See `docs/10-cross-cutting/testing.md` §The recovery KDF in tests.

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

The server stores `upload` keyed by `account_id` (i.e. by the email/identity registered at account creation). The salt and nonce are stored in cleartext alongside the ciphertext; that is intended. The column is write-once through account creation: `POST /accounts` answers `409 RECOVERY_BLOB_EXISTS` to a differing blob rather than replacing it, which is what makes a displayed recovery code trustworthy. A replacement has its own route (§Recovery code rotation).

The server can serve `upload` to anyone who proves access to the account email (recovery flow, below). The server cannot decrypt the blob without `recovery_seed`, which it never sees.

### Versioning and previous-blob retention

`blob_v` is an unsigned integer. **The current implementation reads only `blob_v == 1`**; any other value yields `RECOVERY_VERSION_UNKNOWN` and the UI prompts an upgrade.

**A blob of a successor identity** carries two more plaintext fields within version 1 ([ADR-0070](../11-adr/0070-a-recovery-code-survives-a-rotation-that-carries-it.md) §2): **7**, the genesis `ID_S_pub`, and **8**, the vault-meta genesis keys. The AAD binds the identity the account record names, which is the genesis: `identity_id_from_pub(field 7)` when field 7 is present, field 5 otherwise. A reader that predates the two fields skips them and refuses the blob, because field 5 is then not the AAD's identity.

**No previous blob is retained.** An earlier revision of this section had the relay keep the replaced blob for 30 days and the client offer "restore from previous recovery key". That is withdrawn: a code is replaced because it should stop working, and a retired code that still opens a blob hands its holder the identity keys the replacement was meant to take away. What the relay keeps instead is the replaced blob's first 49 bytes, the newest eight per account, which tell the code that opened it from any other code (`sunrise_crypto::retired_check_matches`) and carry no plaintext.

## Recovery flow (Mode B from `pairing-and-onboarding.md`)

1. User installs Sunrise on a fresh device and picks "Restore from recovery code" (`sunrise recover` on the CLI). A recovery refuses a directory that already holds a vault, meaning `vault.db` or a non-empty `vault-id` marker; `check_recovery_directory` in `crates/sunrise-client-core/src/recovery.rs` is the one rule both clients run. The Apple client moves an unreadable vault aside rather than deleting it.
2. Client prompts for account email and the 24-word code. BIP-39 decoder validates the checksum (typos surface here), before the relay is asked anything.
3. Client requests `upload` from the server with `GET /api/v1/accounts/me/recovery_blob`. **The server requires an OIDC step-up** — an `auth_time` inside `recovery_max_auth_age_secs`, plus any `acr`/`amr` values the operator configured — before serving the blob, to slow down credential-stuffing-style brute-force attempts. Earlier revisions of this line specified a 6-digit email OTP, which [`../00-product/non-goals.md`](../00-product/non-goals.md) forbids the server from implementing; the step-up asks the IdP for the same assurance without Sunrise holding an OTP secret or sending mail. The single-tenant self-host verifier is exempt, since it has no IdP to ask; see [`../06-server/auth.md`](../06-server/auth.md) §Recovery. A stored token cannot satisfy the step-up, because its `auth_time` is whenever it happened, so the client signs in again with `max_age=0`. Before it fetches the blob, the client reads `ID_S_pub` off `GET /api/v1/accounts/me` (`identity_signing_pub`) and derives the `identity_id` the blob's AAD binds: a recovering client needs it before it can open the blob, and on a fresh device the account record is the only place to get it. `AccountInfo::identity_id` is the relay's own account id, not this value.
4. Client runs Argon2id with the stored salt to derive `recovery_key`.
5. Client AEAD-opens `recovery_blob_ct` to recover identity keys. A code that does not open it is compared against the retired checks the same response carries; a match fails here as a retired code. The identity chain record the response carries is verified from the genesis, and its carry shares are walked from the identity the blob opened: a rotation after that identity that carried no share also fails here as a retired code ([ADR-0070](../11-adr/0070-a-recovery-code-survives-a-rotation-that-carries-it.md) §4). Nothing has been written.
6. Client generates new `D_S`, `D_D` device keys, signs a fresh `DeviceCert` with `ID_S_priv`, and publishes it. It then stores the chain's links and walks the carry shares to the identity in force, certifies itself under it, and publishes that cert too; it registers with the relay under that one. The vault is seeded from the opened blob (`IdentitySeed::Recovered`), so it keeps `ID_D_priv`, and records itself in `identity.minted_by_device_id` so the key survives the next open. It mints its own vault root once the code has opened the blob, because the blob does not carry the root, and the Apple client stores that root only once the core reports the vault written. **The recovery reports success only once the replay has read the log:** reporting it earlier hands the user an empty vault and calls it a recovery.
7. Client SHOULD prompt the user to **revoke any other devices** and **rotate every Stream key** (see [`key-rotation.md`](./key-rotation.md)) — recovery implies an unknown-state environment.
8. Stream keys themselves are NOT in the recovery blob. *(`sunrise_client_core::recovery::recover_account` is what puts the pieces together, for `sunrise recover` and the Apple clients alike; the replay needs no walker of its own, because each identity-addressed `key_envelope` arrives as an ordinary op and every absorbed key drains the ops parked in `deferred_ops` waiting for it. What had to be added was a way in: the vault-meta stream's genesis key is derived, `BLAKE3.derive_key("sunrise.meta_genesis_key.v1", ID_D_priv ‖ identity_id)`, because every `key_envelope` op is sealed under the vault-meta stream's key and the identity's copy of that stream's genesis key would otherwise be sealed under itself. A recovering vault recomputes it; every other `(stream, epoch)` key is still 32 random bytes. [ADR-0024](../11-adr/0024-key-hierarchy.md) records why this adds no primitive, format or exposure.)* They do not need to be. Per [ADR-0024](../11-adr/0024-key-hierarchy.md) decision 4, every `(stream_id, epoch)` key is sealed by HPKE to **two** recipient classes: to each device's `D_D_pub`, and to the **identity**'s `ID_D_pub`. The identity-sealed `key_envelope` ops live in the op log, which the relay stores as ciphertext it cannot open, and they are opened by `ID_D_priv` — which this blob already carries. The recovered device replays them and reads its history. Where the relay has trimmed them, the account's key bundle carries the same keys to the same recipient, and the sync driver takes them from it when a snapshot record needs one ([ADR-0069](../11-adr/0069-the-accounts-keys-travel-as-an-identity-sealed-bundle.md)). A relay can serve an older bundle undetectably, which costs availability and, only if it also withholds every later op, writes under epochs a revoked device holds; ADR-0069 §Rollback states it in full. **A pure recovery, with no surviving device and no peer, restores readable content, however old the account is.**

### The cost of that, stated plainly

This is not free, and the tradeoff belongs here rather than in a footnote. `ID_D_priv` becomes a **long-lived unwrapping key**: its compromise reaches every epoch ever sealed to it, and those envelopes sit on the relay where an attacker who has the key can also fetch them. Epoch rotation bounds *forward* exposure — a revoked device reads nothing written after the new epoch — and that bound exists only because a paired device is no longer handed `ID_D_priv` and so cannot open the identity's copy of a new epoch. The exception is the device that created the account, which still holds the key and therefore still opens every identity copy. **Revoking that device now rotates the identity away from it** ([ADR-0037](../11-adr/0037-identity-transition.md)): the successor is minted, every surviving device is re-certified under it, and the departing device holds no share — so epochs minted afterwards are sealed under an identity it cannot reach.

The cost lands on the recovery code, and the user has to be told. A rotation normally carries the successor forward by sealing `ID_S_priv || ID_D_priv` to the **outgoing** `ID_D_pub`, so the existing BIP-39 code keeps working. When the device being revoked is the one holding `ID_D_priv`, that carry is refused — it would seal the successor to the very key the revocation exists to exclude — and the old code stops opening the account. `core.identity.recovery_code_invalidated` fires, `sunrise identity new-code` issues a working one, and `sunrise identity rotate` (without `--keep-code`) is the deliberate form of the same choice, which also issues the new code. What is *not* bounded, here or anywhere, is what the departing device already read: revocation is forward-only. Backward exposure was never bounded and nothing here pretends it was.

What gates that key is the recovery code itself: 256 bits of BIP-39 entropy through Argon2id at m = 65536, t = 3, p = 1. That is the same gate already protecting the blob, so identity-sealing the Stream keys adds no *new* single point of compromise — it widens the blast radius of the one that already existed.

The alternative this replaces was to store wrapped Stream keys server-side under recovery-stretched material. That is still rejected, for the reason it always was: it puts key material the server holds behind a human-memorable secret. Identity-sealed `key_envelope` ops avoid it — the server holds ciphertext addressed to a public key, and never holds anything wrapped under recovery-stretched material. The key bundle is the same construction: HPKE to `ID_D_pub`, never anything a guess at the code can open without first opening the blob. (See [`../11-adr/0004-crypto-primitives.md`](../11-adr/0004-crypto-primitives.md) and [ADR-0024](../11-adr/0024-key-hierarchy.md) §What this fixes in `recovery.md`.)

## Recovery code rotation

The user MAY rotate the recovery code at any time from a still-authorized device:

**Implemented** as `sunrise identity new-code` (alone) and inside `sunrise identity rotate` (with an identity rotation that retires the code), over `sunrise_client_core::account::reissue_recovery_code` ([ADR-0070](../11-adr/0070-a-recovery-code-survives-a-rotation-that-carries-it.md)). `POST /accounts` still refuses a second, differing blob; the replacement has its own route.

1. Generate a fresh 32-byte `recovery_seed'` and BIP-39 encoding.
2. Re-derive `recovery_key'` and re-seal the recovery blob with a fresh salt and nonce, under the identity in force (fields 7 and 8 when that is not the genesis).
3. Replace the server's stored blob with `PUT /api/v1/accounts/me/recovery_blob`, signed by this device's binding rather than by `ID_S_priv`: the relay holds the device key, and has nothing to check an identity signature against. The relay keeps a retired check of the old blob.
4. Display the new code once the relay holds the blob it opens. **Not built:** the paste-back challenge.

## Test-recovery affordance

Before account creation completes, the client offers (and on the strictest setting requires) a "verify recovery now" step: the user re-enters the freshly-displayed code; the client runs the same Argon2id derivation against the just-uploaded blob and confirms a clean decrypt. A user who fails this gate is forced back to "show me the code again" rather than being marooned on a code they wrote down wrong.

## What the user is told

The Recovery setup screen states verbatim:

> Sunrise is end-to-end encrypted. **We cannot reset your account if you lose this code.** This is by design — it means we can't read your data, but it also means we can't recover it for you. Save this code somewhere you'll find it years from now.

## Lockout vs. compromise

If the user thinks the recovery code has been compromised:

`sunrise identity rotate`, from a device that holds the recovery key (the one the account was created on or restored to), does all three, in the order that matters ([ADR-0070](../11-adr/0070-a-recovery-code-survives-a-rotation-that-carries-it.md) §2): a vault-meta epoch with no copy for the old identity, the identity rotation sealed under it, every Stream key rotated after it, and a new recovery code. Never `--keep-code` here: a carried rotation hands the successor to whoever holds the old code.

What it cannot undo is what the old code's holder already read: every epoch sealed to the old identity before the rotation. Revoke any device you do not trust as well (`sunrise device revoke`). This is a heavy operation and is not encouraged casually.

## Not built

None of these is ranked on the roadmap ([`../roadmap.md`](../roadmap.md)).

- **Social recovery / Shamir-split** of the recovery seed across trusted contacts. The recovery flow is designed to admit this: an alternate path can produce `recovery_seed` from M-of-N shares without changing the server-side blob format.
- **Hardware-key backup** (FIDO2 / YubiKey holding the wrapping key as a second path).
- **Device-to-device recovery without server.** A dusty laptop in a drawer is already a recovery path via Mode A pairing once it's powered on.
