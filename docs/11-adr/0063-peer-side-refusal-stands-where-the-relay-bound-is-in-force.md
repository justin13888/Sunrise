# 0063 — Device signatures default on with an issuer, and peer-side revocation enforcement stays: the only line where the relay bound is off, defence in depth where it is on

**Status:** accepted

**Amends** [ADR-0041](./0041-peer-side-revocation-is-a-fold.md) §Context
("And it is conditional"), §Alternatives (a) and (c), and the "Why not the
relay" paragraph of §Consequences. Each reasons from `require_device_sig`
defaulting to false, which it no longer does on a relay with an OIDC issuer.
The fold, the gate, the discount, the read bound and the scope of what peers
refuse are unchanged.

**Answers** ADR-0041 §"What would force revisiting this" item 1, which has
fired.

**Restates a premise in** [ADR-0024](./0024-key-hierarchy.md),
[ADR-0032](./0032-revocation-cannot-bound-cert-issuance.md),
[ADR-0034](./0034-revocation-bounds-reads-not-writes.md) and
[ADR-0037](./0037-identity-transition.md), each of which says
`require_device_sig` is false "at its default". None of their decisions rests on
that, as §Decision 3 shows.

**Depends on** [#363](https://github.com/justin13888/Sunrise/issues/363), which
is closed, and whose state is read from the tree below rather than taken from
the issue.

## Context

ADR-0041's revisit trigger 1 reads: "**`require_device_sig` ceasing to default
to false**, or single-tenant self-host gaining device rows. Both change which
line is load-bearing, and the argument in §Alternatives (a) turns on it."

The first half has happened. [#363](https://github.com/justin13888/Sunrise/issues/363)
made the flag an `Option<bool>`, and `ServerConfig::device_sig_required` resolves
an unset one to whether an OIDC issuer is configured
(`crates/sunrise-server/src/config/model.rs:512#device_sig_required`). The
default is therefore no longer one value. It depends on the deployment:

- **Single-tenant self-host**, which has no issuer. The flag resolves off, and
  `ServerConfig::validate` refuses to start with it on
  (`crates/sunrise-server/src/config/model.rs:385#validate`), because every
  caller maps to one account and a device signature would bind nothing. A
  request with no binding comes back from the device check as "no device"
  rather than a refusal (`crates/sunrise-server/src/api/signed.rs:207#verify_binding`).
  This is the shape [ADR-0027](./0027-v1-self-host-first.md) makes primary.
- **A multi-tenant relay**, which has an issuer, with the flag left unset. The
  flag resolves on. A request without a complete binding is refused before any
  lookup, and a signed one resolves its device through
  `active_device`, whose SQL ends `AND revoked = 0`
  (`crates/sunrise-server/src/store/devices.rs:190#active_device`). So a device
  the relay has been told about is refused on every signed route, however it
  sends the request.
- **A multi-tenant relay whose operator set `require_device_sig = false`.** The
  override still works. The relay logs `srv.start.device_sig_optional` when it
  starts (`crates/sunrise-server/src/state.rs:145#assemble`), and a revoked
  device that stops signing keeps uploading.

The second half of the trigger has not happened. Single-tenant self-host still
cannot tell devices apart. Whether or not a client registers a row there, the
verifier maps every caller to one account, `validate` still refuses the
binding, and an unsigned request is still accepted. So the relay bound is no
more in force there than it was when ADR-0041 was written.

The normative documents already state the new default:
[`key-rotation.md`](../03-crypto/key-rotation.md) §Implementation status,
[`overview.md`](../03-crypto/overview.md),
[`auth.md`](../06-server/auth.md) and the threat model's A3. They also already
draw the conclusion this record reaches for the flag-off deployments: "there the
control-op refusal is the only line there is rather than defence in depth". The
decision records were left as they were, which is what this record settles.

## Decision

**ADR-0041's decision stands unchanged. The trigger changes which line is
load-bearing in one deployment, and in none of them does it change what peers
refuse.**

### 1. Which line is load-bearing, deployment by deployment

| Deployment | Relay bound on a revoked device's uploads | Peer-side refusal of its two control ops |
|---|---|---|
| Single-tenant self-host (no issuer) | Absent. `validate` refuses the binding, so an unsigned upload is accepted | **The only line** |
| Multi-tenant, `require_device_sig` unset (on) | In force for a device the relay has been told about | **Defence in depth** |
| Multi-tenant, `require_device_sig = false` | Absent for a device that stops signing | **The only line** |

ADR-0041 §Context's "in the default deployment the relay bound is not in force"
now reads: in single-tenant self-host, and on a multi-tenant relay whose
operator turned the binding off, the relay bound is not in force. On a
multi-tenant relay with the default config, it is.

### 2. §Alternatives (a) still fails

Alternative (a) says the relay is the only place revocation belongs. It has to
hold in every deployment Sunrise ships, and it fails in the first row of the
table above, which is the primary one. There an unsigned request is bound to no
device, the relay enforces nothing, and under (a) a revoked device could still
revoke the rest of the account on every replica. That was the hole ADR-0041
closed, and (a) would leave it open in exactly the deployment it was open in
before.

On a multi-tenant relay with the default config, (a) is no longer a hole, but
it is still not enough on its own. What the relay bound does not reach is the
reason peer-side refusal is kept there as defence in depth rather than dropped:

- **The relay learns late.** `Command::RevokeDevice` queues an intent that the
  sync driver drains on the revoking device's next session
  ([ADR-0041 §#80 landed, read out of the tree](./0041-peer-side-revocation-is-a-fold.md#80-landed-read-out-of-the-tree)).
  Until the drain lands, the revoked device's uploads are accepted and fanned
  out, and a `device_revoke` it sends in that window reaches every replica.
- **The relay forgets nothing it already accepted.** It refuses forward from the
  moment it is told. The fold judges a sender against the whole ledger whatever
  date its op carries, and is deliberately retroactive for this op family
  ([ADR-0041 §What that gives up](./0041-peer-side-revocation-is-a-fold.md#what-that-gives-up-revocation-here-is-retroactive)).
- **Some devices have nothing to match.** A device that registered before
  clients sent `vault_device_id` gets a `404` from the revocation route, logged
  as `sync.device.revoke_unknown_to_relay`, and goes on being accepted
  ([`key-rotation.md`](../03-crypto/key-rotation.md) §Implementation status).
- **The relay bounds only itself.** A device on another transport, or on a relay
  this account does not use, is not bounded by it.
- **The relay is an adversary in the threat model.** A2 is a hostile server.
  Under (a), which devices count as members of an account would be whatever the
  relay chose to forward. The fold reaches its answer from signed ops alone,
  which is the posture A2's mitigations take: a hostile server can withhold an
  op, but it cannot forge one.

That is what [ADR-0034](./0034-revocation-bounds-reads-not-writes.md) corollary 3
predicted peer-side enforcement would be: "defence in depth, not the only line".
The prediction now holds in one of the three deployments and still fails in the
other two.

### 3. ADR-0041's other lines, and the premise in ADR-0024, 0032, 0034 and 0037

- **ADR-0041 §Alternatives (c).** It notes that #80 took most of (c)'s premise
  away "where the bound is in force", and that the premise "remains true in the
  default deployment". Read "the default deployment" as the two flag-off rows.
  The rejection of (c) never rested on the relay, so it stands.
- **ADR-0041 §Consequences, "Why not the relay"** (on a per-sender upload quota
  for [#315](https://github.com/justin13888/Sunrise/issues/315)). One of its
  three reasons changes. On a multi-tenant relay with the default config, the
  relay does know the sending device, so a per-sender quota there would be
  per-device rather than per-account. The other two reasons still reject the
  quota everywhere: envelopes are opaque, so a quota counts uploads of every
  kind and not revocations, and a rate is not a bound. The flag-off rows still
  have no device to count against. The decision is unchanged.
- **ADR-0024 (`:23`), ADR-0032 (`:173`), ADR-0034 (`:181`, `:276`, `:323`,
  `:351`, `:483`, `:495`) and ADR-0037 (`:282`).** Each says that with
  `require_device_sig` "at its default `false`", or "where it is true, which is
  not the default", a revoked device that stops signing keeps uploading. Read
  that as the flag-off rows of the table above. Each sentence explains why some
  other mechanism is still needed. In ADR-0024 that is the revocation guarantee's
  wording, in ADR-0032 it is the disclosure, in ADR-0034 it is the peer-side
  decision about entity writes, and in ADR-0037 it is the identity rotation.
  Every one of them is still needed, because the flag-off rows include the
  primary deployment. None of those decisions changes. This record restates the
  premise, and the index row of each points here. Their bodies are not edited,
  for the reason ADR-0034's own revisit trigger 1 gives: a decision record is
  amended by the record that re-takes it, not edited into it.

## Alternatives considered

**(a) Edit the "defaults to false" sentences in place.** That would make each
record read as if it had been written against the new default. Rejected,
because it hides when the premise changed and what was re-examined when it did.
That is the history the amendment convention keeps. ADR-0041 and ADR-0034
already carry amendments in this form.

**(b) Add an "Amended by" header to ADR-0024, 0032, 0034 and 0037 as well as
0041.** Rejected for now. Only ADR-0041 has an argument that turns on the
default, which is why only its trigger names it. In the other four the premise
is an aside to a decision that stands on other grounds, and the index row is
enough to point a reader here. A header is a one-line addition if a later
reader finds the index row too easy to miss.

**(c) Drop peer-side refusal on multi-tenant relays now that the bound is in
force there.** Rejected, for the five reasons in §Decision 2. It also could
not be done per deployment. A replica does not know which relay configuration
the other devices of its account use, and the register has to be the same pure
function of the op set on every replica, or the divergence ADR-0034 corollary 3
forbids comes back.

## Consequences

- **No code, storage or protocol change.** `STORAGE_V`, `DOC_SCHEMA_V`,
  `CRYPTO_SUITE_V` and `ENVELOPE_FORMAT_V` are untouched.
- **ADR-0041's index status records the trigger as fired**, and points here.
- **The single-tenant self-host deployment is where peer-side refusal carries
  the whole weight.** Any change that weakens the control-op gate is a change to
  the only revocation bound that deployment has. Any change that weakens the
  relay bound on a multi-tenant relay leaves the gate as the only line there
  too, which is the third row of the table.

## What would force revisiting this

1. **Single-tenant self-host being able to tell devices apart**, so that
   `validate` could stop refusing the binding there. The relay bound would then
   be in force by default everywhere, and the first row of the table would join
   the second. Peer-side refusal would still not become removable, for the
   reasons in §Decision 2 that do not depend on the deployment.
2. **The explicit `require_device_sig = false` override being removed from
   multi-tenant relays.** The third row would disappear. That changes nothing
   here, but this record's table would need restating.
