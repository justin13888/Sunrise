# 0076 — A shared Stream carries its writers' certs and its re-sealed grants, and a recipient reads it in a replica of its own

**Status:** accepted

**Extends** [ADR-0074](./0074-a-viewer-reads-a-shared-stream-through-a-relay-grant.md)
(a viewer reads the owner's channel through a relay grant) with what the
viewer needs on that channel to read it, and builds item 1 of
[ADR-0075](./0075-an-editor-writes-a-shared-streams-contents-and-every-replica-is-the-boundary.md)
§8 for the owner's own devices.
**Amends** [`../03-crypto/sharing-with-others.md`](../03-crypto/sharing-with-others.md)
§Granting access step 3 ("The recipient never reads this op": it still never
reads the vault-meta op, and now reads a copy of each re-seal in the Stream)
and [`../03-crypto/data-encryption-format.md`](../03-crypto/data-encryption-format.md)
§Signature key resolution (a recipient resolves the owner's devices from the
shared Stream, not from a vault-meta log it cannot read).
**Answers** [#1336](https://github.com/justin13888/Sunrise/issues/1336).
**Built:** §1 and §2, and the recipient's verification and keying of what it
reads, by #1336. §3's replica and §4's withdrawal handling are
[#1337](https://github.com/justin13888/Sunrise/issues/1337).

## Context

ADR-0074 lets a recipient's account subscribe to the owner's channel for one
Stream. Two things it reads there are not readable with what it holds.

1. **Who signed each op.** Every replica verifies an envelope under a
   `DeviceCert` from its own account's vault-meta log
   ([`data-encryption-format.md`](../03-crypto/data-encryption-format.md)
   §Signature key resolution). The owner's certs travel in the owner's
   vault-meta stream, sealed under a key only the owner's devices hold
   (`Engine::self_authenticating_signer`). A recipient holds the granter's
   `ID_S_pub`, from the link, and nothing else of the owner's.
2. **Every epoch after the first.** A rotation re-seals its new epoch to each
   share as a `ShareGrant` op on the owner's vault-meta stream
   (`engine/share_grant.rs`). The recipient never reads that stream, so it
   would stop reading at the owner's first rotation.

And one thing about where it reads: the owner picks its Stream ids, entity
ids and device ids, so a malicious owner can name ids that collide with the
recipient's own (ADR-0074, threat model: "Malicious owner").

## Decision

### 1. Writer certs travel in the shared Stream, one per device per epoch

A `StreamWriterCert` control op in the shared Stream's own log carries one
device's identity-signed `DeviceCert` and the owner account's identity chain
record (`data-encryption-format.md` §Identity chain record), genesis to the
identity in force where it was written:

```cddl
StreamWriterCertPayload = {
    "cert"  => bstr,    ; canonical-CBOR DeviceCert of a device that writes into this Stream
    "chain" => bstr,    ; the account's identity chain record, genesis first
    * tstr => any,
}
```

It is written per `(Stream, epoch, device)`, because a recipient holds only
the epochs it was sealed, and must read a writer's cert under an epoch it
holds:

- Before a device logs anything into a Stream that has a share in force,
  under epoch `e`, it logs its own cert into that Stream under `e`, unless its
  cert is already in the Stream's log under `e`.
- `Engine::share_stream` logs, under the live epoch, the cert of every device
  with an op in the Stream's log whose cert this replica holds, beside its
  own. That covers a sibling that wrote under the live epoch and will not
  write again.

A cert can be logged by any device of the account: it is the identity's
signature that authenticates it, not the envelope's.

**The recipient believes a cert** when the chain verifies from its own
genesis, the granter's identity (`identity_id_from_pub` of the
`granter_id_s_pub` the link carried) is on it, and the cert verifies under an
identity on it. Any identity on the chain, not only the head, as the account's
own replicas resolve a signer (`self_authenticating_signer`): which key signed
an envelope is a fact about the device, not a judgement of its standing. The
envelope that carries the cert verifies under that cert where the cert names
the envelope's device, and otherwise under a cert the recipient already
believes. An op from a device no believed cert names is not applied, and is
retried when a cert lands.

The owner's own replicas apply the op and store nothing from it: their own
vault-meta log already says who their devices are.

### 2. A re-sealed grant travels in the shared Stream, under the epoch it replaces

Every re-seal `ShareGrant` a rotation or a backfill logs on the vault-meta
stream is also logged in the shared Stream as a `StreamShareGrant`:

```cddl
StreamShareGrantPayload = {
    "grant" => bstr,    ; canonical ShareGrantPayload, the re-seal's own bytes, at most 8 KiB
    "chain" => bstr,    ; the account's identity chain record, genesis first
    * tstr => any,
}
```

It is sealed under the epoch of the share's previous grant: the one its
recipient holds. Where two keys were minted at that epoch, one op is logged
under each. The emitting device logs its own writer cert under that epoch
first (§1), so the recipient can verify the op that gives it the next epoch
with what it already holds.

A device that re-seals while holding no key at that epoch logs no copy
(`core.share.stream_copy_skipped`, `no_previous_key`), and nothing logs one
later: the vault-meta re-seal already moved the share to the new epoch, so no
backfill re-seals it, and the next rotation's copy is sealed under the epoch
the recipient never received. That recipient stays at the epoch it holds
until the owner shares the Stream with it again (Revisit item 3).

The recipient checks it exactly as it checks the grant its link carried
(`sharing-with-others.md` §Granting access step 5), against the identity that
signed it rather than the link's: that identity must be on a chain that
verifies and holds the granter's identity, so the share survives an identity
rotation of the owner's. It then holds the new epoch's key beside the old,
under the granter it accepted (`held_share_keys`). A grant to another
recipient is not its to open and is skipped. A grant that has expired is
refused, as the link's is.

What it costs: every holder of the outgoing epoch reads the grant's clear
fields. That is the owner's devices, every recipient of the Stream, and a
recipient this very rotation revoked. So the Stream's recipients learn each
other's identity ids, their expiries and the new epoch number. They learn no
key, no name and no content: each grant's `hpke_ct` opens only under its own
recipient's `ID_D_priv`.

### 3. A recipient reads a shared Stream in a replica of its own

What a recipient applies from a shared Stream lives apart from its account's
own vault: a replica per held share, keyed by `(granter_identity_id,
stream_id)`, whose op log, materialized rows and cursors share no row with
the account's. Its trust roots are the granter's chain and the writer certs
in the Stream (§1), and its keys are the held share keys, never
`stream_keys`. Nothing it applies reaches the account's own op log, outbox,
key set, digests or key bundle. So an owner that names one of the recipient's
own Stream, entity or device ids writes into nothing of the recipient's.

The replica is read-only: no command writes to it, and nothing in it is
published (ADR-0074 §3 already keeps a viewer off the owner's channel).

### 4. Expiry and withdrawal

The recipient refuses to apply an op of a share at or after the share's
`expires_at`, on its own clock, whatever the relay sends
([`sharing-with-others.md`](../03-crypto/sharing-with-others.md) §Share
expiration). A subscription the relay refuses or ends with
`RELAY_GRANT_REVOKED` stops reading that share; what was applied stays, as it
does after a revocation (§Revoking access), until the person removes the
share.

## Alternatives considered

- **A relay-side mailbox on the grant for re-sealed grants.** Rejected: it is
  a new relay route and new relay state for one op family, the relay would
  hold the owner's re-seals outside the channel that already carries the
  Stream, and a revoked recipient's mailbox would need its own withdrawal.
  The Stream's own channel already reaches exactly the accounts that should
  read it.
- **A fresh link for every rotation.** Rejected: every rotation would need a
  person to carry a link out of band, and a rotation by a device without
  `ID_S_priv` would wait on both a holder and a person.
- **Seal each in-Stream grant once more, to its recipient alone**, hiding the
  co-recipients. Rejected for now: it is a second HPKE format and test vector
  to protect 16-byte identity ids, which are hashes of keys and name nobody
  until someone compares fingerprints. Revisit item 1.
- **Writer certs once per device per Stream.** Rejected: a recipient sealed
  epoch 3 cannot read a cert logged under epoch 1, and the device's ops under
  epoch 3 would verify under nothing.
- **A writer cert carrying only the cert, verified under the granter's key
  alone.** Rejected: a device certified by an identity before or after the
  granter's, across an identity rotation, would verify under nothing.
- **Apply the owner's ops into the recipient's own vault, under a Stream id
  rewritten on arrival.** Rejected: entity ids and device ids collide as
  Stream ids do, and every query, digest and rebuild would need to know the
  rewrite.

## Consequences

- Two control families, `StreamWriterCert` and `StreamShareGrant`, at
  `DOC_SCHEMA_V = 34`, behind no feature: they write no entity, and a build
  that cannot read them parks them, which costs the owner's replicas nothing.
- A shared Stream's log gains one op per writing device per epoch, and one
  per re-sealed grant.
- ADR-0075 §8 item 1 is built for the owner's devices. An editor's devices
  publish theirs the same way once an editor exists.

## Revisit if

1. **The co-recipients of a Stream must not learn each other's identity
   ids**: wrap each in-Stream grant to its recipient (§Alternatives, third
   item).
2. **The relay gains per-recipient delivery**: §2's grants could travel
   there, off the Stream's log.
3. **A device of the account can re-seal an epoch without holding the one
   before it**: a skipped copy then strands its recipient (§2). Have a
   holder of the earlier key log the missing copy when it learns the
   re-seal, or have such a device leave the re-seal to a holder.
