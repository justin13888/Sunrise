# 0069 — The account's Stream keys travel as one identity-sealed bundle the relay pins, so a recovery does not depend on op retention

**Status:** accepted

**Amends** [ADR-0024](./0024-key-hierarchy.md) decision 4 (the identity
recipient class is how the recovery path reaches the keys) and
[`../03-crypto/recovery.md`](../03-crypto/recovery.md) §Recovery flow step 8.
**Depends on** [ADR-0068](./0068-a-streams-snapshot-travels-as-a-pinned-blob.md),
whose pin machinery it reuses, and on ADR-0041 §4 (a key a read-bounded device
delivered is never written under).
**Built by** [#580](https://github.com/justin13888/Sunrise/issues/580).

## Context

A recovery from the 24-word code restores `ID_S` and `ID_D` and nothing else.
Every Stream key reached the recovering device as an identity-addressed
`key_envelope` op in the vault-meta stream (ADR-0024 decision 4), opened under
the restored `ID_D_priv`. The vault-meta stream's genesis key is derived from
the identity, so the first epoch was always reachable; every later one was
reachable only through those ops.

The relay trims a channel's frames past 30 days or 256 MiB
(`crates/sunrise-server/src/relay_log.rs`). Once it has trimmed the vault-meta
frames that carried a rotated key, a recovery holds the identity and no key
past the genesis. ADR-0068's snapshot records do not help: the vault-meta
record is sealed under the current vault-meta key, which is the key the
recovering device lacks, and every other record is sealed under a key it can
only learn from that one. Paired devices are unaffected, because pairing hands
the keys over, which is why nothing had surfaced it.

## Decision

### 1. A key bundle: every key a device may write under, sealed to the identity

The bundle (`crates/sunrise-crypto/src/key_bundle.rs`, magic kind `0x08`;
[`../03-crypto/data-encryption-format.md`](../03-crypto/data-encryption-format.md)
§Key bundle) is a list of `(stream_id, epoch, key)` sealed by HPKE to the
identity in force's `ID_D_pub`, under a header naming that identity, the
publishing device's cert, and a cleartext manifest of `(stream_id, epoch,
n_keys)`. The header is the HPKE AAD and the whole is signed by the
publisher's `D_S`.

It carries the keys `Keychain::writable_stream_keys_tx` returns: every epoch of
every stream, except a key some read-bounded device delivered.

Rejected: sealing the bundle to the recovery blob's identity rather than the
identity in force. After a rotation that did not carry the recovery code, the
blob's identity is held by exactly the device the rotation excluded.

Rejected: a bundle per stream. One account-wide structure is one pin, one
fetch, and one thing to keep current; the whole is a few dozen bytes a key.

### 2. The relay pins one per account, and a replacement names what it replaces

The bundle is uploaded through the blob routes and pinned with
`PUT /api/v1/accounts/me/key_bundle { blob_id, manifest_digest, replaces }`
(relay migration 0007, `key_bundles`). As with a snapshot pin, the relay reads
only the magic prefix's kind, keeps the pin outside op retention, never
collects the blob it names, and deletes a replaced one.

Unlike a snapshot pin, a replacement is conditional: `replaces` must name the
pin in place, or the relay answers `409 KEY_BUNDLE_SUPERSEDED` and writes
nothing. The `manifest_digest` is stored beside the pin and handed back, so a
device can tell an unchanged bundle without fetching it; the relay cannot check
it, and nothing trusts it beyond deciding to do nothing.

Rejected: last pin wins, as ADR-0068 has it. Applying an older snapshot record
is a join and loses nothing; pinning an older bundle loses every key minted
since, and two devices racing past each other would do exactly that.

### 3. A device publishes when it holds more than the pin, before it is `Live`

Before a session reports `Live`, and on the resync tick, the sync driver asks
whether this device should publish (`crates/sunrise-core/src/key_bundle_sync.rs`):

- It does not, unless it signs under the identity in force and this replica
  holds it as neither revoked nor read-bounded.
- If the pin's digest is its own, or the pinned bundle's manifest covers its
  own, there is nothing to do.
- If its own manifest covers the pinned one and is larger, it publishes,
  naming the pin it read.
- If each holds a key the other lacks (two devices rotated concurrently), it
  waits until it has absorbed the other's keys.
- If the pinned bundle is not one to keep (malformed, another identity's, a
  signature that does not verify, or published by a device this replica has
  bounded), it replaces it whatever it carries.

A rotation, a new stream and a revocation each add keys, so each is published
by the first device to hold the new set, which is normally the one that
performed it, in the session that sent it. Any device can: sealing needs
`ID_D_pub` only.

Rejected: publishing from `rotate_stream_key`, `revoke_device` and stream
creation directly. Three call sites inside the engine's transactions, each
needing a transport they do not have, for the same effect the manifest
comparison gets from one place.

### 4. A recovering device takes its keys from the bundle when a record needs them

When a snapshot record cannot be applied because it is sealed under a key the
vault does not hold (`SnapshotApplied::NoKey` or `Pending`), and the vault
holds `ID_D_priv`, the driver fetches the pinned bundle once per session,
checks it is the blob the pin names, and calls `Core::apply_key_bundle`. That
verifies it under the identity in force, opens it, and stores every key as
delivered by the publisher (`KeySource::Bundle`), so the read bound treats it
exactly as an envelope from that device. The deferred records are then retried.

`sunrise_client_core::recovery::rejoin_account` changes nothing: it already
waits for the driver to report `Live`, and that is now reached through the
bundle where the envelopes are gone.

Rejected: fetching the bundle in the recovery flow, before the driver starts.
The recovery's REST client carries no device binding, so on a relay with
`require_device_sig` on it could not read the pin; the driver's transport
signs. Fetching on demand also covers any vault that holds the identity key
and meets the same gap, not only one in the middle of a recovery.

## Security

**The relay never reads a key.** The keys are HPKE-sealed to `ID_D_pub`; the
relay holds no `ID_D_priv`. What it reads in clear is the manifest, which names
streams and epochs it already reads off every op envelope's header (fields 2
and 8), plus how many keys an epoch holds, which is one except after a
concurrent mint.

**Nothing new reaches a revoked device or the relay.** The bundle's recipient
is the identity in force, which is the recipient class every `key_envelope` has
carried since ADR-0024. Whoever can open the bundle can already open the
identity copy of every key in it. No device admitted by pairing holds
`ID_D_priv` (#76). The devices that do, the account's creator and a device
restored from the code, are excluded from future keys by a revocation run from
the creator, which rotates the identity away from them (ADR-0037), and then new
bundles are sealed to a successor they cannot open. Revoking them from a paired
device cannot rotate the identity, and they go on opening identity envelopes
and bundles alike: the bound ADR-0024 and ADR-0037 already record, and not one
this changes. The bundle never carries a key a read-bounded device delivered,
so it cannot launder one into a key its reader writes under.

**Substitution is refused.** The identity is in the HPKE `info` and in the
header, which a reader compares to its own before anything else. The
publisher's cert must verify under the reader's `ID_S_pub` and name its
identity, and the signature must verify under that cert. A relay holds no
device key, so it can neither write a bundle nor move one between accounts,
and a ciphertext moved under another header fails the AEAD even when the
signature has been made to match.

### Rollback

A reader with nothing but the identity cannot tell the newest bundle from an
older one its account also signed. Both verify. So the relay can serve any
bundle it ever held, or none, and so can a revoked device that can still reach
the pin route (the relay bounds its writes only with `require_device_sig` on).

What that costs:

- **Availability, visibly.** A key minted after the served bundle is missing;
  the record sealed under it stays `NoKey` and the session stays `Degraded`,
  which the clients show as changes missing. Nothing reports success over it.
- **Stale write epochs, under collusion.** If the relay serves a bundle from
  before a revocation *and* withholds every later op and record, the recovered
  device does not learn of the revocation and writes under epochs the revoked
  device holds, so a revoked device working with the relay reads what it writes
  next. The same attack is available against envelope-based recovery today:
  withholding the revocation and rotation ops is the same fork. It ends at the
  first honest op or record the device sees, and the recovery's step 7 (revoke
  the lost devices, rotate every Stream key) is the user's way out of it.
- **Nothing else.** An old bundle carries keys the identity already held; a
  bundle from a revoked device has its keys credited to that device, which this
  replica stops writing under once it learns of the revocation.

Among honest devices there is no rollback: a device publishes only a superset
of what it read, and only over the pin it read. The one exception is a pin a
replica refuses because it holds the publisher as read-bounded, which it
replaces whatever it carries; read bounds are per replica (#411), so two
honest replicas can disagree about that publisher, and the cost is the
availability one above until the next publish. A pin left by a revoked device
is replaced by the next honest device to reach `Live`.

## Consequences

- **Relay migration 0007** (`key_bundles`). No client migration: `STORAGE_V`
  stays 43, and the bundle source is a new value of the existing
  `stream_keys.source` column, which no reader branches on.
- **Two routes**, `pinKeyBundle` and `fetchKeyBundle`, in the `blob` rate-limit
  group; one error code, `KEY_BUNDLE_SUPERSEDED`; one metric,
  `sunrise_key_bundle_pin_total`. No UniFFI change.
- **One request per process** for a device whose keys have not changed: the
  pin's digest answers it. A device whose keys changed fetches and judges the
  pinned bundle once.
- **A recovery after an identity rotation is covered by
  [ADR-0070](./0070-a-recovery-code-survives-a-rotation-that-carries-it.md).** The bundle is
  sealed to the identity in force; a code carried across a rotation reaches it
  by walking the carry shares of an identity chain record the relay keeps
  beside the blob, and a rotation that did not carry the code issues a new one
  sealed to the successor.

## Revisit if

- ~~A recovery after an identity rotation is built: the bundle header gains the
  carry chain from the recovery identity to the identity in force.~~ Built by
  ADR-0070 as a separate record rather than in the header: this bundle's
  decoder refuses unknown fields, so every earlier build would have replaced a
  bundle that carried it.
- A shared blob backend lands (ADR-0062): as for ADR-0068's pins, deleting a
  replaced blob becomes a cross-service write.
