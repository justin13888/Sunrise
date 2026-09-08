# 0035 — The bearer-validity disclosure on `AUTH_DEVICE_SIG_INVALID` is accepted

**Status:** accepted

**Amends:** [`../06-server/auth.md`](../06-server/auth.md) (§Device binding: the
disclosure stops being "tracked separately" and is recorded as accepted, with the
argument that actually carries it) and
[`../01-architecture/threat-model.md`](../01-architecture/threat-model.md)
(A2 residual risk gains the status-code disclosures the surface deliberately
carries).

## Context

### The question

[#79](https://github.com/justin13888/Sunrise/issues/79) asks whether the distinct
`AUTH_DEVICE_SIG_INVALID` (id 204) should keep its pre-lookup case, or whether
that case should go behind an opt-in setting defaulting to the older
`AUTH_TOKEN_INVALID`. It is a security trade with a real cost on both sides: the
distinct code is better diagnostics — a device with a skewed clock is told to fix
its clock instead of refreshing a bearer that was never wrong — and a worse
oracle.

### What the oracle is, precisely

With `require_device_sig = true`, `verify_bytes`
(`crates/sunrise-server/src/api/signed.rs:152`) destructures
`(Some(device_id), Some(signature))` and, failing that, returns
`ApiError::device_sig_invalid()` **before any device or account lookup**. So a
caller presenting an incomplete binding — either header missing, not only both —
gets:

- `401 AUTH_DEVICE_SIG_INVALID` when its bearer is valid, and
- `401 AUTH_TOKEN_INVALID` when its bearer is not.

Both were `AUTH_TOKEN_INVALID` before. The branch is pre-lookup, so it discloses
nothing about the account. What it separates is **"this bearer is currently
valid"** from "it is not", for a caller who already holds the bearer.

The audience is therefore narrow and specific: whoever holds a token and wants
to know whether it is still live. Not a network attacker — A1 is under TLS and
never sees the bearer. Not an enumerating stranger — nothing about which accounts
or devices exist is reachable through this branch, and a device id that is *not*
on the authenticated account deliberately stays `AUTH_TOKEN_INVALID`
(`crates/sunrise-server/src/api/signed.rs:176-185`), guarded by
`an_unknown_device_is_still_indistinguishable_from_a_bad_bearer`.

### The argument currently on the record is the wrong argument

Both the doc comment (`crates/sunrise-server/src/api/error.rs:244`) and
`auth.md` justify the pre-lookup case by pointing at `GET /meta`'s
`device_binding_required` (`crates/sunrise-server/src/api/meta.rs:59`). That
answers a different question. `device_binding_required` tells every caller that
the server *demands* a binding; it says nothing about whether any particular
bearer is valid, which is the whole of what this branch discloses. The citation
is true and does not carry the weight put on it. #79 is right to ask.

### The argument that does carry it: the bootstrap exemption

Two routes accept a bearer with **no binding at all**, by construction, on every
configuration including `require_device_sig = true`:

- `POST /api/v1/accounts` (`crates/sunrise-server/src/api/accounts.rs:84`)
- `POST /api/v1/devices` (`crates/sunrise-server/src/api/devices.rs:156`)

Both take `SignedBootstrap`, and `signed_of` skips verification entirely when the
binding is absent (`crates/sunrise-server/src/api/signed.rs:372-376`) — *a device
cannot sign before it exists*, so this exemption is not an oversight and cannot
be configured away without making device registration impossible.

`caller_of` resolves the bearer **first**, before the binding headers and before
the body (`crates/sunrise-server/src/api/signed.rs:327-338`, then
`:365-370`). So against either route, with no device key and no binding:

| bearer | outcome |
|---|---|
| invalid | `401` from `resolve_bearer` (`crates/sunrise-server/src/api/auth.rs:117`) |
| valid, body deliberately invalid | `400` from the handler's own validation |
| valid, body well-formed | `201`, carrying account or device data |

That is the same oracle, **strictly stronger**: it is reachable on every
configuration rather than only where `require_device_sig` is on, it can be probed
without side effects via the `400` row, and its `201` row returns real data and
mutates state. Putting case 1 behind a config would close a weaker duplicate of a
disclosure that stays wide open two routes away, and would have to be explained
to an operator as protection it does not get.

A third instance is already on the record as deliberate. `AUTH_SIGNUP_DISABLED`
is a `403` chosen *precisely* because the token is valid and the caller is who
they say they are (`crates/sunrise-server/src/api/auth.rs:62-73`): answering
`401` "tells a legitimate user to go and fix a credential that was never wrong",
which is the identical argument #70 made for the 204. This surface has already
decided, once, that distinguishing a valid bearer from an invalid one in a status
code is acceptable; #79 is the second instance of the same trade, and answering
it differently would leave the two in contradiction.

### What the threat model says

[`../01-architecture/threat-model.md`](../01-architecture/threat-model.md) has no
adversary whose capability is "holds a bearer and wants to know if it is live".
A1 is a network attacker under TLS; A2 is a hostile server, which holds the
session rows outright and needs no oracle; A3 is a compromised device, which
holds the live token. The A2 residual-risk list already concedes that the server
observes device ids, op counts, timestamps and sync IPs, and the operational
commitments already concede that push tokens are stored in plaintext. A code that
tells a token's own holder whether their token still works is well inside that
line, and it is not currently written down — which is the part worth fixing.

## Decision

**The pre-lookup `AUTH_DEVICE_SIG_INVALID` stays, unconditionally, with no new
configuration. The bearer-validity disclosure is accepted and recorded in
`auth.md` and the threat model, and the justification is corrected to the
bootstrap exemption rather than `device_binding_required`.**

Stated for a reader of `auth.md`:

> Sunrise's authenticated surface does not hide whether a bearer is valid from
> the party presenting it. Three places say so — the pre-lookup
> `AUTH_DEVICE_SIG_INVALID`, `AUTH_SIGNUP_DISABLED`'s `403`, and the two
> bootstrap routes, which accept a bearer with no binding because a device
> cannot sign before it exists. What the surface *does* hide is everything about
> the account behind the bearer: which accounts exist, which devices are on one,
> and whether a named device id is one of them. Those stay
> `AUTH_TOKEN_INVALID`, and that is the property the codes are shaped around.

## Alternatives considered

**Put case 1 behind its own opt-in setting, defaulting to the pre-#70
behaviour.** Rejected — this is #79's own proposal. It closes a strictly weaker
duplicate of an oracle the bootstrap routes carry unconditionally, so an operator
who enables the silent mode is no better hidden and now believes they are. It
also costs a permanent branch on the one code path that most wants to be
explicable, and it prices in a config key whose honest description is "makes one
of three disclosures quieter". Growing a config surface for that is how config
surfaces stop making sense, which is the argument #70 used to defer the decision
rather than take it in a batch-dedup change.

**Revert to `AUTH_TOKEN_INVALID` for case 1 with no setting.** Rejected. It gives
back the diagnostic the code exists for and buys nothing, for the same reason:
the disclosure survives at the bootstrap routes. It would also reintroduce
exactly the failure #70 fixed for the *partial*-binding caller — a client sending
`X-Sunrise-Device` without `X-Sunrise-Device-Sig` is a client bug, and telling it
"your token is bad" sends it into a refresh loop.

**Close the bootstrap oracle instead, and then reconsider case 1.** Rejected as
impossible without breaking registration. `Binding::Bootstrap` exists because a
device has no key until `POST /api/v1/devices` returns
(`crates/sunrise-server/src/api/signed.rs:223-227`). Any scheme that refuses a
binding-less bearer there refuses first-device onboarding.

**Accept it, and leave the record on the doc comment.** Rejected, and this is the
status quo #79 exists to end. A disclosure recorded only where the code emitting
it lives is a disclosure nobody auditing the threat model will find, and the note
there currently reasons from `device_binding_required`, which does not support
the conclusion. A reader who checks the argument finds a hole and reopens the
question — which is what happened.

## Consequences

- **`auth.md` §Device binding states the disclosure as accepted**, with the
  bootstrap exemption as the reason and a pointer here.
- **The threat model's A2 residual risk names the status-code disclosures.**
  That is the page an auditor reads, and "the surface does not hide bearer
  validity from the bearer's holder, and does hide everything about the account"
  is a one-line property worth being able to cite.
- **`ApiError::device_sig_invalid`'s doc comment is corrected.** Its "What case 1
  discloses" section keeps the disclosure and replaces "recorded here and the
  behaviour is left alone" with this ADR and the argument that holds. Comment
  only; no behaviour changes.
- **`codes.toml` is not touched.** Its `docs` string for 204 is accurate about
  what it claims — that `GET /meta` advertises the binding requirement — and it
  makes no claim about the bearer disclosure. Editing a generator input to
  restate an ADR is not worth the regeneration.
- **The wording defect #79 names is already fixed.** `signed.rs:110-116`,
  `error.rs:221` and `codes.toml`'s 204 entry all describe the pre-lookup case as
  an **incomplete** binding — "either header missing, not only both" — matching
  the `let else` at `signed.rs:152`. Commit `d52999d` landed that before this
  record; nothing further is owed.
- **No new configuration key.** `require_device_sig` keeps its single meaning and
  its derivation from `oidc_issuer`
  (`crates/sunrise-server/src/config.rs:486`).

## What would force revisiting this

1. **A binding-less caller becoming able to reach a `200` on a bound route.**
   The disclosure is bounded by the fact that no route returns success to an
   unbound caller except the two bootstrap ones. A route that changes that turns
   a validity oracle into an access path, and the reasoning here does not cover
   it.
2. **The bootstrap exemption going away.** If device registration ever gains a
   pre-registration proof — an enrolment token, an attested key — then the
   `SignedBootstrap` oracle closes, and the case-1 disclosure is no longer a
   duplicate. At that point the opt-in setting rejected above becomes a real
   option and should be re-taken on its own merits.
3. **A threat model gaining an adversary who holds a bearer but not the device.**
   The disclosure is priced against the current A1–A7 list, in which no adversary
   holds a live token without also holding the device it was issued to. Token
   exfiltration through a proxy, a shared-workstation model, or bearer-in-URL
   logging would each add one, and each makes "is this token still live?" worth
   money to somebody.
4. **Bearer validity becoming inferable at a lower cost than a request.** The
   accepted disclosure costs one authenticated request per probe and is
   rate-limited with everything else. A cache, an error page, or a response
   header that leaks it without a round trip is a different disclosure wearing
   this one's name.
