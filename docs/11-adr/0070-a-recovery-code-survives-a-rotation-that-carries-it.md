# 0070 — A recovery code survives a rotation that carries it, and a rotation that does not issues the next one

**Status:** accepted

**Amends** [ADR-0024](./0024-key-hierarchy.md) decision 4 (what the identity
recipient class reaches after an identity rotation),
[ADR-0037](./0037-identity-transition.md) (what `keep_recovery_code` means),
[ADR-0069](./0069-the-accounts-keys-travel-as-an-identity-sealed-bundle.md)
§Consequences and §Revisit if, and
[`../03-crypto/recovery.md`](../03-crypto/recovery.md) §Recovery code rotation
and §Versioning and previous-blob retention.
**Built by** [#583](https://github.com/justin13888/Sunrise/issues/583).

## Context

What was true before this ADR, read off the code rather than the docs:

- **`keep_recovery_code: true`** sealed the successor's `ID_S_priv ‖ ID_D_priv`
  to the outgoing `ID_D_pub` (the carry share), inside the
  `identity_transition` op. Nothing else changed: no Stream rotated, and the
  recovery blob on the relay was untouched (it is write-once).
- **`keep_recovery_code: false`** omitted the carry share and did nothing
  else. The rotating device adopted the successor through its own device
  share, which carries `ID_S_priv` and never `ID_D_priv` (#76), so **the
  successor's `ID_D_priv` existed nowhere once the rotation committed**: every
  identity copy sealed from then on was unopenable, and no recovery blob could
  ever be sealed for the account again. The CLI told the user to run
  `sunrise bootstrap`, which cannot seal without that key.
- **Neither re-sealed the recovery blob.** There was no route to replace it.
- **A code carried across a rotation did not work either.** A vault restored
  from it opened the identity the code was made under. `recompute_identity_head`
  read only the head's transition and required this device to be in its
  roster; a restored device did not exist when the rotation ran, so it adopted
  nothing. Even with every op retained it stayed under a retired identity, and
  the key bundle (ADR-0069), sealed to the identity in force, did not open.
  Once the relay trimmed the transition op there was nothing left that could
  take it to the head at all.

## Decision

### 1. Two modes, and which one is the default

- **Carry (`keep_recovery_code: true`, `sunrise identity rotate --keep-code`).**
  The existing code keeps working, across any number of rotations that carried
  it. The carry share reaches **every** holder of the outgoing `ID_D_priv`: the
  account's creator, every device restored from the code, and whoever else holds
  the code. It is therefore never what to use when one of them is the reason for
  rotating.
- **Retire (`keep_recovery_code: false`, `sunrise identity rotate`).** The
  rotation retires the code and issues a new one. This is the CLI's default,
  because both reasons `key-rotation.md` gives for rotating — the code may be
  in someone else's hands, or `ID_S_priv` was extracted from the device that
  also holds `ID_D_priv` — are reasons the old code's holder must not follow the
  account.

Revocation-driven rotations are unchanged: they ask to carry, and
`rotate_identity` refuses the carry when the device being revoked is the one
recorded as holding `ID_D_priv`.

### 2. Retiring the code is three transactions and a new blob

`Engine::rotate_identity_retiring_code`:

1. A fresh vault-meta epoch, sealed to every current device and **not** to the
   identity. Its identity copy would be sealed to the outgoing `ID_D_pub`, which
   every holder of the old code holds.
2. The rotation, sealed under that epoch. A holder of the outgoing `ID_S_priv`
   can sign a competing transition from the same predecessor, but cannot seal it
   at an epoch it holds no key for, so its row sorts below this one however it
   dates its HLC (ADR-0037 §4, ADR-0040).
3. With the rotating device under the successor: the identity copy of the new
   vault-meta epoch, sealed to the successor, and a new epoch for every other
   Stream. The epochs the account writes under from now on are not ones the old
   identity holds a copy of.

**The rotating device keeps the successor's `ID_D_priv`**, in both modes: it
held the outgoing one, so it holds the incoming one, as the creator held the
genesis. Then it seals a new recovery blob and replaces the relay's
(`PUT /api/v1/accounts/me/recovery_blob`). `sunrise identity rotate` pushes the
rotation to the relay first and shows the new words only once the relay holds
the blob they open. `sunrise identity new-code` re-issues a code without
rotating, which is §Recovery code rotation of `recovery.md`, built at last.

A blob of a successor identity carries two more plaintext fields: **7**, the
genesis `ID_S_pub`, which the account record names and so what the AAD binds;
and **8**, the vault-meta genesis keys, which the successor's `ID_D_priv`
derives differently (`Keychain::meta_genesis_key`). A vault restored from it
records the genesis as its fold anchor and holds the account's vault-meta
genesis key, so its first write is sealed under a key every peer holds.

### 3. The relay keeps the identity chain beside the blob

A code made before a rotation needs the rotation's link before it can verify
anything else: every cert, snapshot record and key bundle it meets verifies
only under an identity on its chain, and the transition op is trimmed with the
rest of the vault-meta stream. So the chain travels as one **identity chain
record** (`sunrise_crypto::identity_chain`, magic kind `0x09`): the genesis
`ID_S_pub` and, per link, the transition's signed body and both signatures, the
fold's ranking inputs (meta epoch, HLC, emitter), and the carry share or `null`.
It carries no roster, so it names no device.

Any device that signs under the identity in force and is not bounded pins it
with `PUT /api/v1/accounts/me/identity_chain` (relay migration 0008), before its
session reports `Live` and on the resync tick, when the relay holds none, one of
another account, one that does not verify, or one that stops short of its own
fold. A replacement names the BLAKE3 digest of the record it read, as a key
bundle pin names its blob (`409 IDENTITY_CHAIN_SUPERSEDED`).

The stepped-up `GET /api/v1/accounts/me/recovery_blob`, the one route a
recovering device can call, serves the record beside the blob.

Rejected: putting the chain in the key bundle's header, which ADR-0069's
§Revisit if suggested. The bundle's decoder refuses unknown fields, so every
build before this one would read a new bundle as malformed and replace it with
its own on every tick. A separate record leaves the bundle as it is.

### 4. A recovered vault walks the chain, then takes its keys

`sunrise_client_core::recovery`:

1. Opens the blob. If the code does not open it, compares the code against the
   retired checks (§5). A match is `RestoreError::CodeRetired`.
2. Verifies the chain record from the genesis and walks its carry shares from
   the identity the blob carried. A link with no share it can open is
   `RestoreError::CodeRetired` too: the code still opens a blob, and a rotation
   after it retired it. Either way, nothing is written.
3. Opens the vault, then `Core::seed_identity_chain`: the links are stored as
   transition rows with no roster, and `Engine::recompute_identity_head` walks
   them **one link at a time**, opening each carry share with the `ID_D_priv`
   the previous one yielded. A device the roster does not name and that holds
   the successor's `ID_S_priv` certifies itself under it and publishes the cert
   (`core.identity.self_certified`), unless this replica holds it as revoked or
   read-bounded. The transition's own op, when it arrives, replaces the seeded
   row's missing roster and its rank.
4. Registers, and syncs. A vault restored in this process takes the account's
   key bundle before it applies the first frame: once it signs under the head,
   the identity copies sealed to the identity its code opened are sealed to a key
   it no longer holds, and it minted base epochs of its own when it opened, so an
   op sealed under one of those epochs with a key it lacks would be refused
   rather than parked. A caught-up session with ops still parked for a key does
   the same once.

### 5. A retired code fails with a reason, and opens nothing

When a blob is replaced the relay keeps the first 49 bytes of the old one: the
magic, the salt, the nonce, and four bytes of ciphertext. The plaintext under
those four bytes is the fixed CBOR prefix every payload starts with, so
`sunrise_crypto::retired_check_matches` re-derives the recovery key and tells
whether this code opened that blob. The relay keeps the newest eight. The error
names the date the relay replaced the blob.

Rejected: keeping the previous blob for 30 days so a user can "restore from
previous recovery key", which `recovery.md` §Versioning used to specify. A
retired code that still opens a blob hands its holder the identity keys the
rotation was done to take away.

## Security

**The relay never reads a key.** The chain record is signed bodies, public
keys, ranking inputs and carry shares HPKE-sealed to identity `ID_D_pub`s the
relay holds no private half of. A retired check is four bytes of keystream over
a known plaintext, with the salt and nonce it was already serving. The blob is
the same AEAD under the same Argon2id as before.

**A party holding only a retired code gains nothing.** The relay serves the new
blob, which the old code does not open; the retired check answers yes or no and
carries no plaintext; the chain's retiring link has no carry share; the
vault-meta epoch that link is sealed under, and every Stream epoch after it, has
no copy sealed to the old identity. The recovery flow refuses before it writes a
vault.

**A party who opened the old blob before the rotation keeps what it had, and
gains nothing after.** It holds the old identity's keys: it reads every epoch
sealed to that identity before the rotation (backward exposure was never
bounded), and it can sign as that identity. A competing transition it signs
sorts below the honest one by vault-meta epoch; a cert it signs under the old
identity is under a retired link and confers no membership; it cannot open the
key bundle, sealed to the successor. This holds from the moment the rotation is
on the relay, which is why the CLI pushes it before showing the new code.

**Substitution is refused.** A chain link verifies under its predecessor's key
and its own (`verify_identity_transition`), so the relay can drop links and
cannot forge one. A carry share is checked against the successor keys the
signed body names. The ranking inputs are not signed, and matter only between
two successors of one predecessor, which takes that predecessor's `ID_S_priv` to
produce: a relay that restates them while colluding with such a holder can
order its sibling first on a freshly restored vault until the honest
transition's op arrives and replaces the rank.

**Replacing a blob needs a device.** `PUT /accounts/me/recovery_blob` refuses a
bearer with no device binding, whatever `require_device_sig` says. A
replacement opens nothing, but it can make a written-down code stop working.

**Availability is the relay's to deny**, as for every pin: it can serve an old
chain record or none, which leaves a restored vault under the identity its code
opened and not current, which the client reports as such.

**What a carry does is unchanged and now stated.** A carried rotation hands the
successor to every holder of the outgoing `ID_D_priv`. A revocation of a device
restored from the code, run from the creator, still carries, because the
creator's replica records itself as the holder; such a device can open the carry
and follow the account. That gap predates this ADR and is not closed by it.

## Consequences

- **Relay migration 0008** (`identity_chains`, `retired_recovery_blobs`). No
  client migration: `STORAGE_V` stays 43, a seeded transition row is an ordinary
  row with an empty roster, and the vault-meta genesis key a re-issued blob
  carries is a new value of `stream_keys.source` (`recovery`) that no reader
  branches on.
- **Three routes**: `replaceRecoveryBlob` (`account` group), `pinIdentityChain`
  and `fetchIdentityChain` (`blob` group); `getRecoveryBlob` gains
  `identity_chain` and `retired`, present only on an account that rotated or
  replaced its blob. Two codes, `IDENTITY_CHAIN_SUPERSEDED` and
  `IDENTITY_CHAIN_NOT_FOUND`; two metrics, `sunrise_identity_chain_pin_total`
  and `sunrise_recovery_blob_replace_total`.
- **One magic kind**, `0x09`, and **two recovery blob fields**, 7 and 8. A build
  that predates them refuses a re-issued blob as a code that does not open it;
  its generated client refuses a recovery response carrying the new members.
  Neither restores a vault under the wrong anchor.
- **No UniFFI change.** `RestoreError::CodeRetired` crosses as the existing
  `BindingError::RecoveryCode`, with its message. Neither app offers a rotation
  or a new code yet.
- **The CLI's default flipped.** `sunrise identity rotate` retires the code and
  needs the relay; `--keep-code` carries it; `--new-recovery-code` is accepted
  as the default's old spelling. `sunrise identity new-code` is new.

## Revisit if

- A revocation can tell whether the device it cuts holds `ID_D_priv`: it should
  then refuse the carry for every such device, not only the one this replica
  recorded.
- The keychain keeps retired identities' `ID_D_priv`: a restored vault could
  then open the identity copies sealed before a rotation itself, and would not
  need the key bundle for them.
