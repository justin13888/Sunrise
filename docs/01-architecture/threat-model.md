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
- Device revocation: any other paired device can revoke; new per-Stream keys are issued; the revoked device stops being able to *read* what is written afterwards. **That much is enforced**, on every replica that has applied the revocation: such a replica seals the device no `key_envelope` for any epoch minted at or after its cut, and — since `PairingPayload` stopped carrying `ID_D_priv` — there is no identity-sealed copy to open instead. What is **not** enforced: its writes, at all. The relay would have to be told out of band and cannot be ([#80](https://github.com/justin13888/Sunrise/issues/80)), and peers do not refuse its ops deliberately. Nor does the identity key rotate, so a revoked device keeps `ID_S_priv` and can issue itself a valid cert under a fresh device id; and the account's *creator* keeps `ID_D_priv` until the recovery blob exists, so revoking that particular device does not bound even its reads. See [`../03-crypto/key-rotation.md`](../03-crypto/key-rotation.md) §Revocation.

**Residual: the cut is a time, and the register is per-replica.** `emit_key_envelopes` compares `cut_ms` against the HLC reading the mint emits under — the same physical half the `key_envelope` ops it emits will carry — rather than against a wall clock, so both sides of the comparison sit on one timeline and a peer whose clock runs ahead no longer reads as "not yet revoked". The revoking transaction does not rely on that comparison at all for the device it is revoking: it passes that device id down as an unconditional exclusion, because the transaction knows which device it is removing and no clock reading should be able to put it back. What is left is propagation, not skew — a replica that has not yet applied the `device_revoke` op has no cut to compare and will seal a new epoch to the revoked device — so the boundary is eventual consistency, and it is the one no comparison can close. A convergent peer-side check on *writes* is [#82](https://github.com/justin13888/Sunrise/issues/82).

**Residual: any member can revoke any other device, and that is by construction.** There is no authority check on a `device_revoke` op beyond "the sender is a member of this account", and there cannot usefully be one: every paired device holds `ID_S_priv` and the vault root, so a device that wanted to lock a sibling out could equally well issue itself a fresh cert, read everything, or rotate every Stream key on its own. Revocation is therefore a **coordination mechanism among devices that already trust each other** — the way an account agrees that a laptop is gone — and not a defence against one of them. This is why revocation being enforceable against a device's *reads* does not make it a defence against a hostile member: such a device can mint itself a new identity-signed cert and rejoin under a fresh id. Giving devices distinguishable authority is what would change that, and it is identity rotation's ground rather than revocation's.

What the implementation does guarantee is that the *revocation register* is not order-dependent. The cut is the HLC of the `device_revoke` op itself — there is no emitter-chosen field — and concurrent revocations resolve as an LWW register on that op's own `(hlc, device_id)`, in a table keyed on the revoked device rather than on `devices`, so a revocation naming a device this replica has never seen is durable without inventing one. Every replica reaches the same cut whatever order it saw them in, a cut that landed wrong is correctable by revoking again, and a revocation is never gated on its own sender being revoked — otherwise two devices revoking each other would leave each replica holding whichever one it happened to see first.

**Residual: revocation is forward-only, and two paths route around it.** It cannot take back what the revoked device already had — no rotation can, and the device keeps every key it held. Beyond that: the account's **creator** still holds `ID_D_priv`, because until the recovery blob is built there is nowhere else for it to live, so revoking that one device does not bound its reads. And any revoked device still holds `ID_S_priv` and can certify itself under a fresh device id; what refuses that today is the relay declining its uploads, which is an availability control rather than a cryptographic one. Both close with identity rotation. Peer-side refusal of a revoked device's ops is *not* the missing piece and is deliberately absent: refusing at apply time is not convergent without a projection rebuild, so a replica that applied an op before the revocation arrived would disagree with one that did not, permanently ([#82](https://github.com/justin13888/Sunrise/issues/82), [#78](https://github.com/justin13888/Sunrise/issues/78)).

What this slice delivers is the **key hierarchy that makes any of it expressible**: per-`(stream, epoch)` keys that are random and wrapped rather than derived from the vault root, so a key can be rotated at all. Before it, `stream_key = BLAKE3(root‖stream‖epoch)` with `EPOCH = 1` meant every device that ever knew the root could derive every key forever, and revocation was not weak — it was inexpressible.

**Residual: even once something enforces the cut, revocation will converge the cut and not the effect.** An op already applied when the revocation arrives is never re-examined, so two replicas holding identical op sets and an identical register can still hold different tables, decided by delivery order alone. Closing that means retracting materialized state when a revocation lands, which nothing else in this engine does to anything — it is [#78](https://github.com/justin13888/Sunrise/issues/78), and it is a decision rather than an omission.

**So: revocation is a read boundary and not a write one.** It stops a device that pairing admitted from reading anything an informed replica writes after its cut. It does not stop it writing, it does not undo the past, it does not bind the account's creator, and it does not survive that device re-certifying itself under a fresh id. "Revoke this device" is worth doing and is worth describing accurately in the UI: it cuts the device off from new content, and it does not eject it from the account.

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
- The owner's device knows which Streams each recipient cohort can read (from `share_grant` records). Any reference to an entity in a Stream the cohort cannot read becomes `{kind: "redacted", reason: "private_ref", placeholder_text: "—"}` in the encrypted-for-cohort envelope. The owner retains the original (unscrubbed) form locally.
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
