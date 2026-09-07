# 0032 — Revocation cannot bound certificate issuance; disclose it rather than half-enforce it

**Status:** accepted

**Amends** [`docs/03-crypto/key-rotation.md`](../03-crypto/key-rotation.md)
§Revocation and §Identity rotation, and
[`docs/01-architecture/threat-model.md`](../01-architecture/threat-model.md) §A3. Changes no
wire format, adds no op family, bumps no version constant.

## Context

[ADR-0024](./0024-key-hierarchy.md) and the pull request that closed
[#76](https://github.com/justin13888/Sunrise/issues/76) established the read
half of revocation: `Engine::emit_key_envelopes` anti-joins
`device_revocations`, and `PairingPayload` no longer carries `ID_D_priv`, so a
revoked device is sealed no envelope for any epoch minted after its cut and has
no identity-sealed copy to open instead.

That bound names a **device id**. The capability it would have to remove is
`ID_S_priv`, the account identity's signing key, which
[`pairing-and-onboarding.md`](../03-crypto/pairing-and-onboarding.md) hands to
every paired device because a device that cannot issue a certificate can never
admit the next one. A revoked device therefore mints a fresh keypair, signs a
`DeviceCert` for it under the identity key it kept, publishes it, and every
replica admits the new id — `Engine::backfill_key_envelopes` then seals it every
key the revocation had just rotated away. This is
[#105](https://github.com/justin13888/Sunrise/issues/105), and
`engine::tests::a_revoked_device_rejoins_under_a_fresh_device_id` asserts the
round trip rather than describing it.

Three documents and one module comment named a narrower fix as available and
merely unbuilt: *"refuse to backfill a device id first seen in a cert whose
signer is already revoked."* Attempting it is what produced this ADR. **There is
no signer to key that refusal on**, and the same objection kills every variant
short of identity rotation.

## Decision

**Do not enforce. Disclose, and record why the enforcement everyone reaches for
first does not exist.** Concretely:

1. Applying a `device_cert` for a device id this vault has never seen, in an
   account that has at least one recorded revocation, emits
   `core.device.admitted_after_revocation` (`sender_h`, `subject_h`). The cert
   is applied either way.
2. The prose that named an unsound narrow fix is corrected, in
   `key-rotation.md` and in `sunrise_pairing::payload`'s module docs.
3. Closing #105 is identity rotation, and it stays filed as such.

The same posture the pull request behind #76 took for the sole-copy condition
(`Keychain::holds_only_copy_of_identity_key`): surface the fact, state the
consequence, do not build a gate that would be wrong.

## Alternatives considered

**1. An issuer field on the certificate, refused when the issuer is revoked.**
This is what #105 names as the direct fix and what the tree claimed was merely
unbuilt. It cannot work. A `DeviceCert` is signed by `ID_S_priv` alone, and the
`device_cert` op is signed by the *subject's* own `D_S_priv` — a check the
`DeviceCertPublish` arm already makes. A revoked device controls both halves of
the fresh identity it mints, so it writes whatever issuer id it likes and signs
for it: any issuer field is attacker-chosen and unverifiable. Worse, in the
honest flow there is no issuer to name — `Keychain::create` has the *joining*
device self-issue its own cert from the `ID_S_priv` the payload carried, so
issuer and subject are the same device on every certificate this codebase has
ever produced. The field would be a constant.

**2. Sponsor-signed certificates: the certifying device countersigns with its
own `D_S_priv`, and a cert whose sponsor is revoked is not admitted.** This is
sound — a revoked device holds no other device's `D_S_priv`, so it can only
sponsor as itself and the register catches it — and it is convergent, because
"is the sponsor revoked" is a pure function of the cert (an op fact) and the
register (an LWW register). It is rejected on two counts. It restructures
pairing: the sponsoring device would have to receive the joining device's public
keys and return a countersigned cert, which is a second Noise frame, a payload
format change and a change at the Swift seam. And it **over-blocks
permanently**: revoking the laptop you paired your phone from would silently
lock the phone out, because a revocation cannot be made non-retroactive without
an unforgeable timestamp, and this system has none — `Hlc::receive` bounds a
stamp from the future by `MAX_DRIFT_MS` and a stamp from the *past* not at all
("a laptop opened after a week offline emits ops with old timestamps, and those
are accepted without comment"), so a revoked device backdates its cert op below
its own cut and any time-bounded variant lets it through. Healing the over-block
needs a re-sponsorship op family and a user decision about which of a revoked
device's sponsees to keep — which is a larger deliverable than identity
rotation, not a smaller one.

**3. Refuse to seal keys to a device whose cert arrived under a superseded
epoch.** #105 lists this as the weak version that needs no format change; it is
sound in one respect — a revoked device demonstrably cannot seal an op under the
epoch its own revocation minted — and it fails on the other two. It does not
close the round trip: the fresh id still has a `devices` row, so
`emit_key_envelopes` seals it every *subsequent* epoch and the bypass survives
one rotation later. And it introduces a permanent lockout for an honest device
whose certificate races a rotation, which is
[#107](https://github.com/justin13888/Sunrise/issues/107)'s failure mode
deliberately widened, in the same pull request that closes #107.

**4. Refuse the cert at apply time when this replica already knows the signer's
device is revoked.** Rejected outright: it is the non-convergent refusal
`key-rotation.md` §Revocation already documents as forbidden. Two replicas with
the same op set would permanently disagree about whether the new device is a
member — one sealing it every future epoch and the other none — which is worse
than the hole.

**5. Identity rotation** ([#76](https://github.com/justin13888/Sunrise/issues/76)
option (a), `key-rotation.md` §Identity rotation). The only one that removes the
capability rather than refusing its output: a fresh `ID_S`/`ID_D`, every
surviving device re-certified under it, and the revoked device's `ID_S_priv`
signing nothing the account accepts. Not rejected — **required**, and out of
scope here. It is a new op family (`identity_transition`, specified and
unbuilt), it needs the recovery blob re-sealed, and it needs a path back for a
device that was offline across the rotation.

## Consequences

- **The bypass is open and is now asserted in a test**, not only described.
  `a_revoked_device_rejoins_under_a_fresh_device_id` is the test that must flip
  when identity rotation lands; until then it documents the guarantee's real
  edge.
- **Revocation's contract is: a revoked device stops reading new content under
  the id it was revoked as.** Not "a revoked device stops reading". Any client
  copy that says otherwise is wrong, and `vision.md`'s framing of "revoke this
  device" as a security action is still ahead of what the system does.
- **The new log event fires on ordinary pairings too**, in any account that has
  ever revoked a device. That is not noise to be filtered: the two are
  indistinguishable inside the vault, and an event that fired only on the
  hostile case would be the enforcement this ADR says cannot be built.
- **`ID_S_priv` on every device is now a recorded cost of pairing**, not an
  incidental. Any future change that would let a device admit another without
  holding it — alternative 2's shape, most likely — removes the precondition
  this whole ADR rests on, and should reopen it.

## What would force revisiting this

1. **Identity rotation shipping.** It closes #105 and supersedes this ADR's
   decision, not its analysis.
2. **A relay-side write bound** ([#80](https://github.com/justin13888/Sunrise/issues/80)).
   A revoked device that cannot upload cannot publish a cert either, which
   bounds the bypass without any vault-side check — but only for the relay's
   own accounts, and only while the device stays off every other transport.
3. **A user-visible device list.** Alternative 2's over-block and this ADR's
   disclosure are both decisions about who tells the user what; a surface that
   shows "these devices joined after you revoked one" changes the balance.
