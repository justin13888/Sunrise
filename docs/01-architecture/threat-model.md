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
| Account email (login) | Low | Server, in plaintext (`accounts.email`) |
| Push tokens (APNs/FCM) | Low | Server, in plaintext |
| Encrypted blobs | Low (ciphertext) | Server + devices |

## Adversaries

### A1: Network attacker (active MitM)

**Capabilities.** Can intercept, modify, drop, replay traffic between client and server.

**Goal denied.** Reading or modifying user data; impersonating a user or device.

**Mitigations.**
- TLS 1.3. **Certificate pinning is not implemented** and is deferred past v1 — [`../03-crypto/pairing-and-onboarding.md`](../03-crypto/pairing-and-onboarding.md) says so for the pairing relay, and the same holds for sync: `rustls` validates against the webpki/Mozilla root bundle, with no pinned key anywhere in the tree.
- All sync ops are E2E-encrypted *under TLS*; TLS compromise alone yields ciphertext.
- Replay defense via per-device monotonic op counters.

### A2: Compromised Sunrise server (operator turns hostile, or storage leaked)

**Capabilities.** Read/write all stored data; serve malicious clients.

**Goal denied.** Reading user content. Forging ops attributed to a user.

**Mitigations.**
- Content stored only as ciphertext under per-record keys derived from per-stream keys held by paired devices.
- Op log entries are signed by the originating device key; server cannot forge.
- Server cannot decrypt without device or recovery material.
- Tampering with stored ciphertext is detected at decryption (AEAD) — implemented, and the mechanism that actually holds today. The Merkle-style hashing of the op log (see [`audit-and-tamper-evidence.md`](../03-crypto/audit-and-tamper-evidence.md)) exists only as two hash functions with no caller outside their frozen-vector tests; it detects nothing yet, and neither does the rollback high-water-mark named under Residual risk.

**Residual risk.** A hostile server can:
- Withhold ops (deny service). Detectable via gaps in op counters.
- Observe metadata (op counts, timestamps, device IDs, sync IPs).
- Serve a known-old snapshot to a specific device (rollback). Mitigation: clients track high-water-mark of op log root in tamper-evident structure.

### A3: Compromised user device

**Capabilities.** Full plaintext access to whatever was on that device.

**Goal denied.** Persistent access *after* the user has noticed and revoked.

**Mitigations.**
- Device revocation: any other paired device can revoke; identity key rotates; new per-Stream keys are issued; the revoked device's ops are no longer accepted by other devices. **Two of those three are implemented** ([ADR-0024](../11-adr/0024-key-hierarchy.md)): any paired device can revoke another, every stream the revoked device could read gets a fresh epoch sealed only to the devices that remain, and every replica refuses ops the revoked device signed at or after the cut. **The identity key does not rotate**, and until it does A3's denied goal is only half denied. Pairing hands each device `ID_D_priv`, and every epoch is sealed to the identity as well as to each device so that recovery can reach it, so a revoked device opens the identity's copy of every new epoch and keeps reading. It also keeps `ID_S_priv`, so it can issue itself a valid cert under a fresh device id. See [`../03-crypto/key-rotation.md`](../03-crypto/key-rotation.md) §Revocation. What is denied today is *writing*; what remains is *reading*.
**Residual: the cut is a convergence boundary, not a cryptographic one.** The cut is the HLC of the `device_revoke` op, chosen by the device that *revokes*; the reading it is compared against is stamped by the device being cut off. So a revoked device that can still reach a peer can stamp an op below the cut and have every replica apply it. Only the direction is constrained: the clock gate refuses an HLC reading in the receiver's future, so the lie can only ever run backwards.

Closing that is a transport question rather than a comparison. `DELETE /api/v1/devices/{id}` makes the relay refuse the device's signed uploads and its sync sessions outright, after which nothing it signs reaches anyone. `Command::RevokeDevice` does not call it — the `device_revoke` op is vault state and the relay does not read it, which [`../03-crypto/key-rotation.md`](../03-crypto/key-rotation.md) §Revocation records as step 3 not built.

The alternative was considered and rejected: treating revocation as retroactive — refusing *every* op from a revoked device regardless of its HLC — throws away honest work the user did yesterday on the laptop they revoked today, which is both worse and far more likely than a backdating attacker who already had to be reachable and already holds every key the account issued before the cut.

**Residual: any member can revoke any other device, and that is by construction.** There is no authority check on a `device_revoke` op beyond "the sender is a member of this account", and there cannot usefully be one: every paired device holds `ID_S_priv` and the vault root, so a device that wanted to lock a sibling out could equally well issue itself a fresh cert, read everything, or rotate every Stream key on its own. Revocation is therefore a **coordination mechanism among devices that already trust each other** — the way an account agrees that a laptop is gone — and not a defence against one of them. It is the same assumption [#76](https://github.com/justin13888/Sunrise/issues/76) turns on: give devices distinguishable authority and revocation becomes enforceable; until then it is not.

What the implementation does guarantee is that the *revocation register* is not order-dependent. The cut is the HLC of the `device_revoke` op itself — there is no emitter-chosen field — and concurrent revocations resolve as an LWW register on that op's own `(hlc, device_id)`, in a table keyed on the revoked device rather than on `devices`, so a revocation naming a device this replica has never seen is durable without inventing one. Every replica reaches the same cut whatever order it saw them in, a cut that landed wrong is correctable by revoking again, and a revocation is never gated on its own sender being revoked — otherwise two devices revoking each other would leave each replica holding whichever one it happened to see first.

**Residual: revocation converges the cut, not the effect.** An op already applied when the revocation arrives is never re-examined. Two replicas holding identical op sets and an identical revocation register can therefore hold different task tables — the one that saw the task op first applied it and kept it, the one that saw the revocation first refused it — and which happened is a function of delivery order alone. A refusal is not itself permanent (nothing is recorded, and the op applies if the relay offers it again under a corrected cut), but nothing re-offers an op a replica has already applied, so the divergence persists. Closing that would mean retracting materialized state when a revocation lands, which nothing else in this engine does to anything. Until it is closed, "the revoked device's writes are bounded" is true of every replica individually and not of the account as a whole.

**So: until identity rotation lands ([#76](https://github.com/justin13888/Sunrise/issues/76)), revocation is a convergence mechanism and not a security boundary.** It is how the account's replicas agree to stop applying a departed device's writes. It is not a barrier that device cannot cross, in either direction — it reads through the rotation by way of the identity-sealed envelopes, and it can write below the cut for as long as it can reach the relay.

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
- Push tokens stored on the server are held in **plaintext**. The `push_tokens` table in `crates/sunrise-server/src/store.rs` stores `(device_id, platform, token, updated_at_ms)` with no wrapping, and the relay's own SQLite database is not SQLCipher-encrypted (see [`trust-and-server-role.md`](./trust-and-server-role.md)). Encrypting them at rest under a key the operator does not back up is the target, not the state; until it lands, an operator or a storage leak sees every device's APNs/FCM token. The A2 residual-risk list above already assumes the server can read them.
- Account deletion removes all blob storage and metadata within 30 days.
