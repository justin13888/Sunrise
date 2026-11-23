# 0074 — A viewer reads a shared Stream through a grant the relay routes, and the relay is never the confidentiality boundary

**Status:** accepted

**Extends** [ADR-0020](./0020-v1-must-demotions.md) §(a) items 4 and 5 and its
addendum (a read-only grant per Stream, with expiry, revocable by key
rotation): this is the written threat model §"What would force revisiting this"
item 1 asks for before sharing is built, and the relay half of it.
**Amends** [`../01-architecture/trust-and-server-role.md`](../01-architecture/trust-and-server-role.md)
§What the server explicitly does *not* do (the relay now evaluates one grant:
who may *read* a channel) and
[`../03-crypto/sharing-with-others.md`](../03-crypto/sharing-with-others.md)
§Revoking access step 3 and §Share expiration (the relay stops serving a
withdrawn or expired grant, as hygiene rather than as the guarantee).
**Built by** [#1319](https://github.com/justin13888/Sunrise/issues/1319).

## Context

The viewer grant's cryptography is built (`crates/sunrise-crypto/src/share_grant.rs`):
a Stream key HPKE-sealed to a second identity's `ID_D_pub`, signed by the
granting identity, with an expiry the recipient checks, and a re-seal that a
rotation sends to every recipient but the revoked and the expired. Nothing on
the relay could carry it. The relay's channel key is
`(account_key(account_id), stream_id)`, and the account half is derived from
the verified caller, never taken from the request, so a Stream belongs to one
account by construction and a second account has no way to name it.

ADR-0020 gave the reason sharing waited: in an end-to-end-encrypted product a
sharing model that is almost right is a vulnerability that fails no test. So
the relay half is decided against a threat model written first, below, and the
model's first conclusion shapes everything after it: **the relay cannot be the
boundary that keeps a Stream's contents from someone, because the relay is one
of the parties the boundary is drawn against.** What the relay can do is decide
whom it hands ciphertext and metadata to, and that is what this ADR builds.

## Threat model

### What is protected

| Asset | Where it lives | What protects it |
|---|---|---|
| A shared Stream's contents | op envelopes, sealed under the Stream key of an epoch | XChaCha20-Poly1305 under a key the relay never holds |
| The Stream key, handed to a recipient | `ShareGrantPayload.hpke_ct` | HPKE Base to the recipient identity's `ID_D_pub`, granter in the AAD |
| The grant's authenticity | `ShareGrantPayload.identity_sig` | Ed25519 under the granting identity's `ID_S_pub`, checked against an OOB fingerprint |
| Contents after a revocation | ops under the next epoch | the rotation seals the next epoch to everyone but the revoked |
| Contents after an expiry | ops the recipient would apply | the recipient's `expires_at` check, before it applies anything |
| The shared Stream's ciphertext and its metadata (op counts, sizes, timing, device ids, `seq`) | the owner's relay channel | **the relay grant this ADR adds** |
| The owner's other Streams | their own channels | the channel key: no grant names them |

Only the last two rows depend on the relay behaving. Every row above them holds
against a relay that does whatever it likes.

### Who attacks it

1. **A hostile relay, or its operator.** Holds every frame, every row and the
   routing; can serve any channel to any session, ignore a withdrawal, ignore an
   expiry, withhold frames, and replay old ones. Holds no `ID_D_priv`, no
   `ID_S_priv`, no Stream key: `sunrise-server` does not link `sunrise-crypto`.
2. **A third account on the same relay**, authenticated, with no grant.
3. **Someone who intercepts the claim token** on its way from the owner to the
   recipient, before the recipient claims it.
4. **A revoked recipient**, and **an expired one**, each still holding the
   epochs it was sealed.
5. **A viewer that tries to write.**
6. **A malicious owner** probing the recipient, and **a malicious recipient**
   probing the owner.

### What each one gets

| Attacker | Can | Cannot |
|---|---|---|
| Hostile relay | Serve the shared channel's ciphertext and metadata to anyone; keep serving a withdrawn or expired recipient; withhold or replay frames; see the sharing graph | Read a byte of content; open a grant; forge one (`identity_sig`); re-sign the owner's ciphertext as its own (granter in the AAD); extend a revoked recipient past the rotation; make an expired recipient apply an op |
| Third account, honest relay | Nothing on the owner's channel | Subscribe to it: a subscription names a grant, and a grant id it does not hold, or one bound to another account, is refused. Publish into it: no route writes another account's channel |
| Token thief | Claim the grant first, and so pull the shared channel's ciphertext and metadata until the owner withdraws it | Open the grant, which is sealed to the intended recipient's identity, or read anything under it. The theft is visible: the intended recipient's claim then fails, and the owner's list shows the grant claimed |
| Revoked recipient | Keep every epoch it was sealed; under a hostile relay, keep receiving ciphertext | Read anything under the epoch the revocation rotated to |
| Expired recipient | Same as revoked, until the rotation | Apply an op at or after `expires_at`, or open a grant then |
| Viewer writing | Publish to its own account's channel, which nobody else reads | Reach the owner's channel, or any other recipient's |
| Malicious owner | Name a `stream_id` that collides with one of the recipient's own; withdraw at any time; register grants nobody claims | Make the recipient's session confuse the two (a set that names a shared `stream_id` twice is refused); grow the relay's rows without bound (a cap per owner) |
| Malicious recipient | Renounce the grant | Escalate to writing; widen the grant to another Stream; hand it on (a claimed grant is bound to one account) |

The relay grant is therefore a **routing and metadata** control. Its failure,
by a bug or by a hostile operator, costs the owner the privacy of the shared
Stream's metadata and its ciphertext's availability to others. It never costs
content, and nothing in this ADR may be read as making the relay responsible
for content.

## Decision

### 1. A relay viewer grant: registered by the owner, claimed by the recipient

`POST /api/v1/sync/grants` records a grant on the owner's account: one of its
Stream ids, a **claim token**, and an optional expiry. The relay draws the
grant's id (16 random bytes) and stores the token only as its BLAKE3 hash under
the domain `sunrise.relay.viewer_grant.claim.v1`. The token is 32 random bytes
the owner's device draws; it travels to the recipient beside the sealed
`ShareGrantPayload`, which is the deep link's job
([#1320](https://github.com/justin13888/Sunrise/issues/1320)), not this one.

`POST /api/v1/sync/grants/claim` presents the token. The first account to
present it, other than the owner, is bound as the recipient, and its later
claims are answered again; any other account's claim, a token nobody
registered, and an expired grant are all the same `404`, so a claim is no
oracle for which tokens exist. The recipient learns the grant id, the Stream id
and the expiry.

`GET /api/v1/sync/grants` lists the grants the caller owns and those it holds;
`DELETE /api/v1/sync/grants/{grant_id}` withdraws one, by its owner or by its
recipient (a decline is a withdrawal by the recipient).

Why a token rather than naming the recipient account: the owner knows the
recipient by identity, and the relay offers no lookup from an identity to an
account. It keeps the identity keys an account was created with
(`accounts.identity_pub_s`, write-once), but a lookup over them would make
every account on the relay enumerable by its identity key, and would bind the
grant to a key an identity rotation replaces (ADR-0037) while the column keeps
the old one. Account ids never travel to another account (`account_ns.rs`). A
claim token binds the grant to whichever account the recipient signs in with,
and moves nothing about any account to anyone.

### 2. A subscription names the grant, and the relay routes it to the owner's channel

`StreamSubscription` gains an optional `grant`. With it, the relay reads the
grant, requires it claimed by the caller's account, naming that Stream, and not
expired, and subscribes the session to `(account_key(owner), stream_id)`
instead of its own channel. Replay, cursors, resume ids, gap reports and live
fan-out are the machinery every channel already has; the owner's channel is
simply one more key the session reads. A set that names a shared `stream_id`
twice is refused, so one id in the event stream always means one channel.

A grant that is not in force refuses the whole subscribe with
`403 RELAY_GRANT_REVOKED`, a code the catalogue already holds. The relay does
not say which of withdrawn, expired, unclaimed or never-existed it was.

The cursors a viewer declares for a shared Stream are not recorded: the cursor
table feeds the owner's blob-tombstone quorum, and a viewer's acknowledgement
gates nothing of the owner's.

### 3. A viewer cannot write, by construction

`POST /api/v1/sync/ops` is unchanged: it appends to the caller's own channel,
and no request field names another. So there is no path by which a viewer's op
reaches the owner's channel, and nothing to authorize per op. That is why this
ADR builds `viewer` only: an `editor` needs the relay to accept writes into
another account's channel, and with them a per-op check of who signed; that is
[#1321](https://github.com/justin13888/Sunrise/issues/1321), a later decision.

### 4. Withdrawal and expiry stop the relay serving; rotation is what stops the reading

An open stream re-checks each grant it reads on the tick it already re-checks
its device on (`device_recheck_ms`), and ends with `RELAY_GRANT_REVOKED` when
the grant is withdrawn, expired, or its recipient is no longer the session's
account. A new subscribe is refused the same way.

That is hygiene, not the guarantee, and both specs keep saying so: a
withdrawal saves the owner's metadata and the relay's bandwidth; the rotation
(#1318) is what puts the next epoch out of the revoked recipient's reach, and
the recipient's own `expires_at` check is what stops an expired one applying
ops. The relay's expiry is the one the owner stated when registering, because
the relay cannot read `ShareGrantPayload` field 5; if the two disagree, the
cost is bandwidth one way or the other, never content.

### 5. Bounds and erasure

An owner holds at most 256 grants; registering one first deletes the owner's
expired grants, so the cap counts live ones. Grants cascade from both account
rows, so erasing either account erases every grant it is party to, and a
stream reading the owner's channel ends on its next re-check.

## Alternatives considered

- **No relay grant: anyone who knows the owner's account key and the Stream id
  may subscribe, and confidentiality rests on the seal alone.** Rejected: it
  hands every account on the relay the ciphertext, op counts and timing of
  every shared Stream whose id leaks, and it makes the channel key, which was
  never a secret, into a capability.
- **The relay verifies the `ShareGrantPayload` itself.** Rejected: it would
  need the granting identity in force, where the relay keeps only the one the
  account was created with, and a binding from recipient identity to account,
  which it does not hold; and checking the signature would still decide
  nothing about content. It would also put a parser of sealed grant bytes into
  a binary that deliberately has no `sunrise-crypto` dependency.
- **The owner names the recipient account.** Rejected in §1: it needs an
  account directory the relay does not and should not have.
- **A stateless, relay-signed capability with an expiry.** Rejected: it cannot
  be withdrawn before it expires, a grant with no expiry would be a permanent
  bearer credential to the channel, and a relay key shared across nodes is a
  new secret to manage.
- **Skip the relay-side re-check and expiry.** Rejected: the recipient's
  client would discard what it is sent, but the relay would go on disclosing
  the owner's metadata to an account the owner removed.

## Consequences

- **Relay migration 0010** (`viewer_grants`) and its Postgres counterpart, 0004.
- **Four routes** in the `account` rate-limit group: `createViewerGrant`,
  `claimViewerGrant`, `listViewerGrants`, `withdrawViewerGrant`. One request
  field, `StreamSubscription.grant`, optional, so a client that predates it
  subscribes exactly as before. No new error code.
- **The relay learns the sharing graph at account granularity**: which account
  reads which Stream of which other account, since when, and until when.
  [`sharing-with-others.md`](../03-crypto/sharing-with-others.md) §Privacy
  implications always said it would.
- **Same relay only.** Both accounts are on the owner's relay. Cross-server
  delivery stays unbuilt (`trust-and-server-role.md` §Cross-server delivery).

## Not built here

- The `share_grant`, `share_revoke` and `share_decline` ops and the rotation
  that calls the re-seal: [#1318](https://github.com/justin13888/Sunrise/issues/1318).
- The deep link that carries the claim token and the sealed grant, and every
  client surface: [#1320](https://github.com/justin13888/Sunrise/issues/1320).
- A viewer's reads beyond the op log: the owner's pinned snapshot (ADR-0068),
  its blobs, and a push wake-up when the owner publishes. A viewer whose cursor
  falls behind retention gets `SYNC_CURSOR_GAP` and nothing to resolve it with.

## Revisit if

1. **An `editor` role is decided** (#1321): §3's "by construction" ends, and
   the relay needs per-op authorization of writes into another account's
   channel.
2. **Cross-relay sharing is designed**: the claim token is a credential on the
   owner's relay only.
3. **The relay gains an identity directory**: naming the recipient directly
   becomes possible, and §1's reason for a token is worth re-reading.
