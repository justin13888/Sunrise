# 0024 — Stream keys are random and wrapped, not derived from the vault root

**Status:** accepted

**Amends** [`docs/03-crypto/identity-and-device-keys.md`](../03-crypto/identity-and-device-keys.md),
[`key-rotation.md`](../03-crypto/key-rotation.md) and
[`recovery.md`](../03-crypto/recovery.md), which specify the target hierarchy this
ADR adopts. **Bumps** `CRYPTO_SUITE_V` and `DOC_SCHEMA_V`.

## Context

The implemented cryptosystem and the documented one are different systems that
share a vocabulary. The documents specify an identity-anchored hierarchy with
independently-generated per-Stream keys and HPKE envelopes. The code implements
one account-wide random vault root from which everything else is *derived*:

```rust
// crates/sunrise-core/src/keychain.rs
fn derive_stream_key(vault_root, stream_id, epoch) -> StreamKey {
    derive_key_32("sunrise.stream_key.v1", vault_root ‖ stream_id ‖ epoch)
}
pub const EPOCH: u32 = 1;   // "Rotation (new epochs) is a later slice"
```

Everything else follows from that one line:

* **Rotating a Stream key requires rotating the vault root**, which rotates every
  Stream key simultaneously. There is no per-Stream rotation to implement.
* **Revoking a device is impossible.** A paired device holds the vault root, and
  the root *is* the entire key schedule. `key-rotation.md` specifies device,
  Stream and identity rotation in full; none of it can be built on this.
* **Sharing is impossible** for the same reason: there is no unit smaller than
  "everything" to grant.
* **Recovery restores nothing readable.** `recovery.rs` is implemented and
  correct — Argon2id v0x13, m=65536, t=3, p=1 — and the blob it seals carries the
  *identity* keys. In the implemented system identity keys decrypt nothing;
  decryption depends solely on the vault root, which is in one machine's Keychain
  and in no backup. Losing the last paired device is total, unrecoverable loss.

Three related divergences compound it. `DeviceSigningKeyPair` is a **type alias**
of `IdentitySigningKeyPair`, so `primitives.md`'s claim that one cannot be passed
where the other is expected is false in code. Device certificates are **self-signed**
by the device's own key and `identity_id` is derived from that same key, so there
is no identity anchor — trust rests entirely on whatever out-of-band channel
delivered the cert. And `hpke = "0.13"` has sat in the workspace manifest since the
crypto epic with **zero crates depending on it**.

What makes this tractable is that the seam is already built and deliberately
left open. `wrap_stream_key` exists in `sunrise-crypto/src/stream_key.rs`.
`Keychain::persist_stream_key` already writes wrapped keys into a `stream_keys`
table, and says why it is not read yet: *"Reads still derive; this table write
only lets a later slice make rotation table-driven."* This ADR is that slice.

## Decision

**Stream keys become independently random per `(stream_id, epoch)`, wrapped under
the vault root, and read from the `stream_keys` table.** The derivation is deleted,
not deprecated.

Alongside it, the hierarchy the documents already specify is made real:

1. **The identity keypair is independent of device keys.** `ID_S` (Ed25519) and
   `ID_D` (X25519) are generated once per account, persisted, and are not derived
   from each other or from any device key. `DeviceSigningKeyPair` and
   `DeviceDhKeyPair` become distinct types rather than aliases, so the compiler
   enforces what `primitives.md` claims.
2. **`identity_id = BLAKE3("sunrise.identity_id.v1" ‖ ID_S_pub, 16)`** — anchored
   to the identity key, not to whichever device happened to create the vault.
3. **DeviceCerts are signed by `ID_S_priv`** and verified against the identity,
   replacing self-signature. A device is trusted because the identity vouched for
   it, which is what makes revocation meaningful.
4. **`key_envelope` ops distribute Stream keys** by HPKE, sealing each
   `(stream_id, epoch)` key to a recipient's X25519 public key. Two recipient
   classes, and the distinction is what makes both revocation and recovery work:
   * to each **device**'s `D_D_pub`, so a device learns the epochs it is entitled to;
   * to the **identity**'s `ID_D`, so the recovery path can reach them.
5. **Epochs are real.** `EPOCH` stops being a constant, so revoking a device can
   mint a new epoch for every Stream it could read. The revoked device keeps
   what it already had — unavoidable, and stated — and, as built, reads what
   comes afterwards too: see the scope note below.

   **Scope, as implemented: the machinery exists and enforces nothing.** A
   `device_revoke` op is recorded and converged as an LWW register on the op's
   own HLC, and no code consults it. Two enforcement claims were built here and
   both removed. *Withholding new epoch keys* withholds nothing: every epoch is
   also sealed to the identity so recovery can reach it, and pairing hands every
   device `ID_D_priv`, so a revoked device opens the identity copy —
   [#76](https://github.com/justin13888/Sunrise/issues/76). *Refusing a revoked
   device's ops on a peer* freezes that peer's sync cursor for it while the
   relay, which knows nothing of the revocation, goes on accepting its uploads;
   within retention that latches a permanent data-loss warning on every device
   in the account. Bounding writes needs the relay
   ([#82](https://github.com/justin13888/Sunrise/issues/82) behind
   [#80](https://github.com/justin13888/Sunrise/issues/80)); converging the
   *effect* rather than the record is
   [#78](https://github.com/justin13888/Sunrise/issues/78).

   What this ADR delivers is the hierarchy that makes revocation **expressible**
   — random per-`(stream, epoch)` keys, wrapped rather than derived — which is
   the thing that was structurally impossible before. It is not revocation.
6. **`stream_keys` becomes the read path.** `EPOCH` stops being a constant.

### What this fixes in `recovery.md`

`recovery.md` currently concludes that after a pure recovery "the user has
identity but no Stream content keys… MUST contact any peer… or accept that
historical content is lost", and calls that non-negotiable because storing
wrapped Stream keys server-side under recovery-stretched material would
reintroduce a single point of compromise.

Decision 4 removes the dilemma the caveat was reasoning about. Identity-sealed
`key_envelope` ops live in the **op log**, which the relay stores as ciphertext
it cannot open, and they are opened by `ID_D_priv` — which the recovery blob
already carries. Recovery therefore restores readable content without the server
ever holding anything wrapped under recovery-stretched material.

The cost is real and is not hidden: `ID_D_priv` becomes a long-lived unwrapping
key, so its compromise reaches every epoch ever sealed to it, and the envelopes
are on the relay. What gates that is the recovery code itself — 256 bits of
BIP-39 through Argon2id — which is the same gate already protecting the blob.
Epoch rotation bounds forward exposure; it does not bound backward exposure, and
`recovery.md` must say so rather than imply recovery is free.

## Alternatives considered

| Option | Why not |
|---|---|
| **Random per-Stream keys wrapped under the vault root, no HPKE** | Delivers rotation with a much smaller change, since `wrap_stream_key` and the table already exist. But every device still holds the root, so a revoked device can unwrap every new epoch it can fetch — revocation stays theatre, and calendar secrets can only be sealed to "the vault", not to a device set. It buys the least valuable third of the problem. |
| **Correct the docs to the derived-key model** | Honest and cheap, and it permanently forecloses rotation, revocation, sharing and recovery — including accepting that losing one device destroys the account's data. Not a v1 posture for a product whose pitch is that your data is yours. |
| **Per-device vault roots, as `identity-and-device-keys.md` §vault root literally specifies** | The doc derives the root per-device from a keystore secret or Argon2id passphrase plus a `device_salt`. Nothing implements it, and it is orthogonal: it changes how a device unlocks, not what a Stream key is. Deferred, and the `Unlock::{Passphrase, DevicePaired, RecoveryCode}` seam already anticipates it. |
| **Keep deriving, add an epoch input** | `stream_key = KDF(root ‖ stream ‖ epoch)` already takes an epoch. Bumping it rotates the ciphertext without rotating the *secret*, since anyone with the root computes any epoch. It looks like rotation and is not. |

## Consequences

* **New op families.** `key_envelope` and `device_revoke` are new `InnerOp`
  variants. A build that does not know an op family refuses it — a new variant is
  not a new field — so this is a breaking change to the op vocabulary. Acceptable
  under [ADR-0018](./0018-storage-baseline-reset.md), where no older build exists,
  and recorded rather than discovered.
* `CRYPTO_SUITE_V` and `DOC_SCHEMA_V` both bump. `ENVELOPE_FORMAT_V` does not:
  the container is unchanged, which is exactly the split
  [ADR-0015](./0015-envelope-doc-schema-split.md) exists to allow.
* `hpke` gains its first dependent, six months after entering the manifest.
* Pairing changes shape. Today it transfers the vault root, and the receiving
  device derives everything; afterwards it transfers the root *and* the identity
  private keys, and the new device receives `key_envelope` ops through normal
  sync. `export_vault_root_for_pairing` keeps its deliberately conspicuous name.
* The relay is unaffected. It has no `sunrise-crypto` dependency and can only
  reach `EnvelopeHeader`, a type with no payload field; `key_envelope` ops are
  ciphertext like every other op. This ADR adds no server-visible metadata.
* `sunrise-crypto-test-vectors` needs frozen vectors for the envelope seal, and
  the existing byte-exact vectors must keep passing — the container is unchanged,
  so a regression there means this ADR touched something it should not have.
