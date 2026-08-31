---
status: accepted
---

# Threat Model

## Assets

| Asset | Sensitivity | Where it lives |
|---|---|---|
| Plaintext task/note/stream content | High | User devices only |
| User identity private key | Critical | User devices only (OS keystore) |
| Per-device key | High | User devices only (OS keystore) |
| Sync metadata (op count, timestamps) | Low–Medium | Server + devices |
| Account email (login) | Low | Server (hashed where possible) |
| Push tokens (APNs/FCM) | Low | Server |
| Encrypted blobs | Low (ciphertext) | Server + devices |

## Adversaries

### A1: Network attacker (active MitM)

**Capabilities.** Can intercept, modify, drop, replay traffic between client and server.

**Goal denied.** Reading or modifying user data; impersonating a user or device.

**Mitigations.**
- TLS 1.3 with cert pinning on managed clients.
- All sync ops are E2E-encrypted *under TLS*; TLS compromise alone yields ciphertext.
- Replay defense via per-device monotonic op counters.

### A2: Compromised Sunrise server (operator turns hostile, or storage leaked)

**Capabilities.** Read/write all stored data; serve malicious clients.

**Goal denied.** Reading user content. Forging ops attributed to a user.

**Mitigations.**
- Content stored only as ciphertext under per-record keys derived from per-stream keys held by paired devices.
- Op log entries are signed by the originating device key; server cannot forge.
- Server cannot decrypt without device or recovery material.
- Tampering with stored ciphertext is detected at decryption (AEAD) and via Merkle-style hashing of op log (see [`audit-and-tamper-evidence.md`](../03-crypto/audit-and-tamper-evidence.md)).

**Residual risk.** A hostile server can:
- Withhold ops (deny service). Detectable via gaps in op counters.
- Observe metadata (op counts, timestamps, device IDs, sync IPs).
- Serve a known-old snapshot to a specific device (rollback). Mitigation: clients track high-water-mark of op log root in tamper-evident structure.

### A3: Compromised user device

**Capabilities.** Full plaintext access to whatever was on that device.

**Goal denied.** Persistent access *after* the user has noticed and revoked.

**Mitigations.**
- Device revocation: any other paired device can revoke; identity key rotates; new per-stream keys are derived; the revoked device's ops are no longer accepted by other devices.
- Optional periodic re-auth (passphrase / biometric) before plaintext is decrypted on disk.
- OS keystore (Keychain / Keystore / TPM) for at-rest protection of device keys; unlocked only with user presence on platforms that support it.

### A4: Coerced user (rubber-hose / lawful order)

**Capabilities.** Compels user to unlock a device or hand over recovery code.

**Goal denied.** *Not within scope.* If the user unlocks, content is exposed.

**Mitigation (partial).** "Plausible deniability" subkey vault is a possible future feature but not scoped for v1.

### A5: Malicious client peer (sharing scenario)

**Capabilities.** A user we shared a Stream with tries to access *other* Streams.

**Goal denied.** Access beyond the shared subgraph.

**Mitigations.**
- Per-Stream content keys; the shared identity receives only the keys for shared Streams.
- Cross-stream references in shared content (e.g. `@waiting-on:alice` task linking to a private Stream) are scrubbed at share-egress; the recipient sees the title or a redacted reference, never the underlying ciphertext for non-shared Streams.

**Reference scrubbing semantics.** Cross-stream reference scrubbing happens at **op-emit time** by the owner's device:

- Cross-stream references can only be **created** by the owner. The UI ensures this — non-owner editors do not have ids for entities outside the shared Stream, so they cannot author such a reference in the first place.
- The owner's device knows which Streams each recipient cohort can read (from `share_grant` records). Any reference to an entity in a Stream the cohort cannot read becomes `{kind: "redacted", reason: "private_ref"}` in the encrypted-for-cohort envelope. The owner retains the original (unscrubbed) form locally.
- Scrubbing is **per-recipient-cohort**, not retroactive. If a cohort gains access to a previously-private Stream, prior ops they received remain redacted; new ops include the previously-private references unredacted.

### A6: Malicious dependency / supply chain

**Capabilities.** A crate or npm package introduces an exfiltration backdoor.

**Mitigations.**
- Pinned, vendored deps for crypto-touching code.
- `cargo-deny` policy: deny unmaintained, unsigned, or recently-changed-ownership crates in the crypto allowlist.
- Reproducible builds for releases; checksummed binaries published.
- No telemetry SDK in the core; minimal third-party JS in the web app.

### A7: Lost / stolen device with weak unlock

**Capabilities.** Physical possession of a powered-off / locked device.

**Mitigations.**
- Device key wrapped at rest by an OS-secret-store-protected wrapping key.
- On platforms with secure enclave (iOS, modern Android, macOS), wrapping key is non-extractable.
- If the user has set a Sunrise passphrase, plaintext requires both OS unlock *and* passphrase entry.

## Out of scope

- Side-channel attacks on devices (cold-boot, EM, acoustic).
- Endpoint protection (we are not an EDR).
- Targeted nation-state adversaries with active device implants.
- Defending the user against themselves once authenticated (we don't prompt-confirm every delete).

## Privacy commitments (operational)

- The Sunrise-managed cloud server logs request metadata for ≤14 days, no payloads.
- Push tokens stored on the server are encrypted at rest with a key the operator does not back up.
- Account deletion removes all blob storage and metadata within 30 days.
