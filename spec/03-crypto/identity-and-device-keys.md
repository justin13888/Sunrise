---
status: accepted
---

# Identity and Device Keys

## Identity

A user has **exactly one** Identity. The identity is the cryptographic anchor: long-lived, restored from recovery if all devices are lost.

Two keypairs:

- **Identity signing key** (Ed25519) — `ID_S_pub` (32 B), `ID_S_priv` (32 B seed).
- **Identity DH key** (X25519) — `ID_D_pub` (32 B), `ID_D_priv` (32 B).

The two private halves are independent (NOT derived from each other or a shared seed), to keep the algorithms separable for any future rotation.

### Identity ID

```
identity_id_bytes = BLAKE3( "sunrise.identity_id.v1" || ID_S_pub, 16 )
identity_id_str   = "idn_" || crockford_base32( identity_id_bytes )
```

`crockford_base32` uses Crockford's alphabet (`0123456789ABCDEFGHJKMNPQRSTVWXYZ`), uppercase. 16 bytes (128 bits) encode to 26 characters with no padding character used; the encoder handles the partial last group.

The identity ID is stable forever; it does NOT change on identity rotation (a rotation publishes a transition certificate that maps the new keys to the same identity ID; see [`key-rotation.md`](./key-rotation.md)).

## Devices

Each device has its own keypairs:

- **Device signing key** (Ed25519) — `D_S_pub`, `D_S_priv`. Used to sign every op the device emits.
- **Device DH key** (X25519) — `D_D_pub`, `D_D_priv`. Recipient key for HPKE key envelopes targeting this device.

### Device ID

```
device_id_bytes = ULID()  // 128-bit, generated client-side at provisioning
device_id_str   = "dev_" || crockford_base32( device_id_bytes )
```

Device IDs are 16 bytes encoded as 26-char Crockford base32 (no padding character used); the ULID timestamp prefix is convenience for sorting, not load-bearing.

### DeviceCert

Binds a device's public keys to the identity. Format:

```cddl
DeviceCertBody = {
    1: uint,                     ; v (= 1)
    2: bstr .size 16,            ; device_id (raw bytes)
    3: bstr .size 32,            ; D_S_pub
    4: bstr .size 32,            ; D_D_pub
    5: bstr .size 16,            ; identity_id_bytes
    6: uint,                     ; created_at (ms since epoch)
    7: tstr .size (1..64),       ; nickname (utf-8)
    8: tstr,                     ; platform ("ios" / "android" / "macos" / "windows" / "linux" / "web" / "tui")
}

DeviceCert = {
    body: DeviceCertBody,
    sig:  bstr .size 64,         ; Ed25519_sign(ID_S_priv,
                                  ;   "sunrise.device_cert.v1" || BLAKE3(canonical_cbor(body), 32))
}
```

Verification of a device's authority to act as part of an identity requires:

1. The DeviceCert's signature verifies under the identity's current `ID_S_pub`.
2. The DeviceCert appears in the user's vault-meta op log and has not been superseded by a `device_revoke` op.

The DeviceCert is published as a `device_cert` op in the user's vault-meta log (a per-identity log distinct from any Stream).

## Vault root key (per device)

Used to wrap at-rest material on a single device. Computed at unlock and held in process memory only.

```
unlock_secret = (
    OS_KEYSTORE_RELEASE  // 32 B random, kept in keystore, released after biometric/passcode
    OR
    Argon2id(            // passphrase fallback (Linux without TPM, kiosk web, etc.)
        password = utf8_nfc(passphrase),
        salt     = device_salt,
        version  = 0x13,                 // RFC 9106
        m        = 65536,                // KiB (64 MiB)
        t        = 3,
        p        = 1,
        out_len  = 32
    )
)

vault_root = BLAKE3.derive_key(
    context      = "sunrise.vault_root.v1",
    key_material = unlock_secret || device_salt,
    out_len      = 32
)
```

`vault_root` MUST be zeroized on lock, app exit, and after a configurable idle timeout (default 15 minutes; user-configurable from 1 minute to "until quit").

### `device_salt` persistence

`device_salt` is 32 random bytes generated at first run. Storage is per-platform:

| Platform | Path | Mode |
|---|---|---|
| Linux | `$XDG_STATE_HOME/sunrise/device.toml` | 0600 |
| macOS | `~/Library/Application Support/Sunrise/device.toml` | 0600 |
| Windows | `%LOCALAPPDATA%\Sunrise\device.toml` | (ACL: current user only) |
| iOS | Keychain item `service=sunrise.device_salt, accessGroup=<bundle>.sunrise` | (Keychain) |
| Android | `EncryptedSharedPreferences` keyed `device_salt` | (system) |

The file format on desktop is TOML with `device_salt = "<base64url>"` and a sibling field `unlock_path = "keystore" | "passphrase"`.

On launch the client reads the salt; if missing, this device is treated as **uninitialized** and a fresh salt is generated. **Loss of `device_salt` is loss of the device's identity** — the device must be re-paired (the user is prompted).

### Path switch (keystore ⇄ passphrase)

Switching unlock paths requires re-wrapping every wrapped Stream key:

1. User authenticates under the **current** path.
2. Core derives `vault_root_old` from the current path.
3. Core generates `vault_root_new` from the new path (new passphrase or new keystore key).
4. All wrapped Stream keys are decrypted with `vault_root_old`, re-wrapped under `vault_root_new`, and committed in a single SQLite transaction.
5. On commit, `unlock_path` and `device_salt` (regenerated as part of the switch) are updated atomically.
6. On crash mid-switch: a `path_switch_in_progress` marker is detected at next open; the user is prompted to re-authenticate under the **new** path; if they cannot, they fall back to the old path and the new wrapping is rolled back.

The OS-keystore release path is preferred wherever available.

## Stream keys

A Stream's content is encrypted under a 32-byte symmetric key. Each Stream key has an integer **epoch** starting at 1; rotation produces epoch 2, 3, … (see [`key-rotation.md`](./key-rotation.md)).

```
stream_key_<epoch>  // 32 B, generated by os.csprng() at create or rotate
```

Each device that has access stores all current and past epochs locally, AEAD-wrapped under `vault_root`:

```
wrapped_stream_key = XChaCha20-Poly1305_seal(
    key       = vault_root,
    nonce     = random 24 B,
    plaintext = stream_key,
    aad       = "sunrise.wrap.stream_key.v1" || stream_id || epoch
)
```

Past epochs are retained because old ops remain encrypted under the epoch under which they were created (see "Why we keep epochs" in [`key-rotation.md`](./key-rotation.md)).

### Why per-Stream, not per-Task or per-vault

The Stream is the unit of sharing in the domain model. Matching the crypto unit to the sharing unit avoids:

- Per-task keys: key-envelope explosion at sync time.
- Per-vault key: cannot share a subset selectively.

## Storage at rest — summary

| Material | Storage | Wrapped by |
|---|---|---|
| `ID_S_priv`, `ID_D_priv` | OS keystore (preferred) or AEAD-encrypted file under `vault_root` (fallback) | OS keystore / vault root |
| `D_S_priv`, `D_D_priv` | Same | Same |
| `vault_root` | RAM only | (n/a; zeroized on lock) |
| Stream keys (all epochs) | SQLite `stream_keys` table | `vault_root` (XChaCha20-Poly1305) |
| Wrapped Stream keys for siblings / peers | Stream op log (`key_envelope`, `share_grant` ops) | HPKE to recipient `D_D_pub` / `ID_D_pub` |
| `device_salt` (32 B) | Per-platform path (see "device_salt persistence") | (non-secret; integrity by FS perms / Keychain / EncryptedSharedPreferences) |

## Public-key publication

At account creation, the client uploads `{identity_id, ID_S_pub, ID_D_pub}` to the server, signed by `ID_S_priv`. The server stores them as the canonical record for `idn_…` lookups. The signature lets any device detect server substitution: a sharer's client refuses to use a peer's key bundle whose self-signature does not verify.

Out-of-band fingerprint verification (QR or numeric SAS) is offered for users who want to confirm a peer's keys haven't been MITM'd by a hostile server. The displayed fingerprint is:

```
fingerprint = base32_groups_of_5(
    BLAKE3("sunrise.identity_fingerprint.v1" || ID_S_pub || ID_D_pub, 15)
)   // 24 chars in 5 groups of 4 + final group of 4 with separator dashes; 120 bits
```

120 bits is more than sufficient against any practical MITM search; the format is chosen for human readability over a phone call.
