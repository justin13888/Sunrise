---
status: accepted
---

# Server Authentication

Sunrise piggybacks **OpenID Connect (OIDC)** for everything user-facing — account login, session management, account recovery, sign-up, MFA, password resets, email verification. We do not implement password storage, OTP delivery, magic links, JWT signing, refresh-token rotation, or hCaptcha integration ourselves. An OIDC issuer does all of that.

Cryptographic E2EE identity (identity + device keypairs) is **separate** from server auth. Device keys sign ops and pairing handshakes for end-to-end integrity; the device signing key is *additionally* used as a per-request binding (see "Device binding" below), which is not the same thing as being the credential: the bearer proves the account, the signature proves the device.

> **Implementation status.** Token verification, the account model, `allow_signup`,
> device binding and the sync session's expiry handling are built
> (`crates/sunrise-server/src/auth/`, `store.rs`, `api/sync.rs`,
> `sync_session.rs`). The **recovery** and **account-deletion** flows below are
> not: no route serves them, and the sections say so in place.
> [ADR-0022](../11-adr/0022-device-signature-canonical-json.md) replaces
> `header_sig_v1` with `header_sig_v2` (RFC 8785 canonical JSON over the request
> *value*), and [ADR-0023](../11-adr/0023-sse-sync-transport.md) replaced the
> `/sync` WebSocket with SSE plus typed `POST`. Both have landed; the credential
> rules were unchanged by either, which is why this page still holds.

## Components

- **Managed cloud:** Sunrise operates a single hosted OIDC issuer (e.g. Keycloak, Authelia, or a managed provider like Auth0/WorkOS — choice tracked in [`../11-adr/`](../11-adr/)). Users sign up, log in, and recover via that issuer's standard browser flow. The Sunrise server is an OIDC relying party (RP).
- **Self-host:** the operator brings any OIDC issuer. The Sunrise server's `auth.oidc_issuer` config points at the issuer's discovery URL. Operators who don't want a separate IdP run a single-tenant Keycloak/Authelia container alongside the relay.

The Sunrise server holds no passwords, no OTP secrets, no recovery emails-in-transit, and no MFA factors.

## Account model

| Field | Source | Notes |
|---|---|---|
| `account_id` | Sunrise-internal random ID | Stable per OIDC `iss + sub` pair |
| `oidc_sub` | OIDC `sub` claim | The IdP's stable user identifier |
| `oidc_iss` | OIDC `iss` claim | Distinguishes IdPs in self-host with multiple issuers (rare) |
| `email` | OIDC `email` claim | Used for sharing/discovery only; auth doesn't depend on it |
| `identity_pub_s`, `identity_pub_d` | First device on first login | Registered after OIDC sign-in via authenticated API call |
| `recovery_blob` | Same | Opaque ciphertext; see "Recovery" below |

A first OIDC login with no matching `account_id` either auto-provisions the account (if the server's `auth.allow_signup` is true) or returns `403 AUTH_SIGNUP_DISABLED`. `allow_signup` is purely a server-side guard: when `false`, the server rejects unknown-account tokens even if the IdP issued them. Sign-up is otherwise an OIDC concern (the IdP gates it with whatever its admin configured: email verification, captcha, invite codes, allow-list, etc.).

## Per-request auth

Every authenticated request — REST and every sync operation — carries a standard OIDC **access token** in `Authorization: Bearer <jwt>`.

The Sunrise server validates the token by:

1. Fetching the issuer's JWKS (cached per the discovery doc's `Cache-Control`).
2. Verifying signature, `iss`, `aud` (must equal the server's configured client ID), `exp`, `nbf`.
3. Looking up the account row by `(iss, sub)`.
4. Optionally verifying a `device_id` claim — the URI-namespaced
   `https://sunrise.app/device_id` (`auth::oidc::DEVICE_ID_CLAIM`), requested by
   the client through the authorization request's `claims` parameter. When the
   token carries it, it MUST equal `X-Sunrise-Device`; when it does not, the
   header stands alone. See "Device binding" below.

Configuration gates which verifier runs. `[auth] oidc_issuer` **and**
`oidc_client_id` together install `OidcVerifier`; with either missing the server
stays in single-tenant mode behind `NullVerifier`, where every caller maps to
one account — which is why `ServerConfig::validate` refuses to bind that mode to
anything but loopback. `allow_signup` defaults to `true`.

There are **no Sunrise-issued tokens and no refresh-token logic on the server side**. The OIDC client library on the device handles token refresh against the issuer. Token TTL is **1 hour**; clients renew at 75% of TTL pre-emptively without dropping the event stream, through `POST /api/v1/sync/session/refresh` carrying the replacement bearer. The sync driver still emits a `0x12 RefreshToken { token: tstr }` frame for this; `SseTransport` turns it into that request and turns the reply back into a `0x13 RefreshTokenAck` frame, so the driver's contract is the one the socket had and only the adapter under it changed.

The server's whole part in a refresh is to re-verify the token the device obtained and move the session's deadline out. It refuses in three distinguishable ways, and they are deliberately not alike:

| Case | Result | Why |
|---|---|---|
| Token does not verify | `Error AUTH_TOKEN_INVALID`, **session continues** | The credential it already holds is still valid and its own deadline still governs. Tearing down would turn a recoverable client bug into a dropped session. |
| Token verifies but names a different `(iss, sub)`, or a different `device_id` | `Error AUTH_TOKEN_INVALID`, **session ends** | The relay channel namespace was derived from the establishing token and is never re-derived. Continuing would relay one account's traffic under another's authority. |
| Session's current token has already expired | `401 AUTH_TOKEN_INVALID`, and the open stream is closed with `AUTH_TOKEN_EXPIRED` | `resolve` looks the session up before the handler body runs, and `SessionStore::get` collects an expired row rather than returning it, so this never reaches the refresh handler at all. A dead session is not resurrectable by presenting a live token; the client re-establishes, which re-runs the whole `POST /sync/session` pipeline including `allow_signup` and the account lookup. |

An accepted refresh **is acknowledged**. The server replies with
`{ expires_at_ms }`, carrying the new deadline it just installed, so the client
learns the server's view of the expiry rather than inferring success from the
absence of a disconnect. The operation is gated on the optional
`SrvTokenRefresh` capability bit, which the server ORs into its negotiated set
at `POST /sync/session` — a client only refreshes after seeing that bit agreed,
so a refresh cannot be silently swallowed by a server that predates it.
Implemented as `refresh` in `crates/sunrise-server/src/api/sync.rs`, specified in
[`../05-sync/wire-protocol.md`](../05-sync/wire-protocol.md), and covered by that
module's own tests — `a_refresh_extends_the_session`,
`an_unverifiable_refresh_is_refused_but_keeps_the_session`,
`a_refresh_naming_another_principal_ends_the_session` and
`a_refresh_for_a_token_with_no_expiry_reports_zero` are the four cases the table
above describes.

A per-request Ed25519 device signature (`X-Sunrise-Device-Sig`) accompanies the bearer token. The mode is `header_sig_v2`, specified byte-for-byte under §Device binding below, and it is the only mode the server accepts. It is **optional by default** — `[auth] require_device_sig` is `false` — but a signature that is present is always verified.

For sync: every operation carries `Authorization: Bearer <token>`, and kynos's
`BearerToken` carrier reads the `Authorization` header and nothing else
(`api/auth.rs`), so **the `?access_token=…` query-string fallback earlier
revisions promised browsers does not exist** — a browser that cannot set the
header cannot authenticate. The defence for it survived the port, as ADR-0021
required, and it changed shape in the process. The request log
(`crates/sunrise-server/src/api/observe.rs`) is handed the *matched route*,
whose `path()` is the description's own `paths` key rather than the request's
target, and it never consults the URI — so there is no query string in reach of
it to redact. The hazard `tower-http`'s stock `MakeSpan` created is gone by
construction rather than mitigated by a hand-assembled span, which is the
outcome to preserve on the day the query fallback *is* added.

`templatize_path` in `crates/sunrise-log/src/field.rs` is the old mitigation and
it is still exported and still tested (`templatize_drops_query_string`), but
**nothing calls it any more** — the span it fed is gone. Read it as a tool kept
for the next caller that logs a concrete target, not as a guard currently
standing. The one that is standing is the observer's contract.

Establishing a session authenticates once, but the *session* carries the token's
`exp` for its whole life (`Session::deadline_ms` in `sync_session.rs`), and
expiry is enforced two ways:

- **On every operation.** `resolve` looks the session up first, and
  `SessionStore::get` collects an expired row instead of returning it, so an
  aged-out session answers `401` to anything it is asked to do.
- **On a timer inside the open event stream.** A subscriber that only reads
  issues no further operations, so the check above never runs for it; without
  the timer a client could establish a session, go quiet, and hold a fan-out
  open indefinitely on a dead credential. The same tick re-reads the device row,
  so a revocation also reaches a stream already open.

On expiry the stream emits a typed `Closed` event carrying
`AUTH_TOKEN_EXPIRED` and then ends, and the client renews via OIDC and
re-establishes. A revoked device gets `AUTH_DEVICE_REVOKED` on the same shape.
Both are the canonical `ErrorCode`, not a bespoke string — a client has to be
able to tell an aged-out credential (renew silently) from a withdrawn one (stop
and ask the user), and an untyped close makes those indistinguishable.

The close reaches the client because it is an event on the body, not a control
frame racing a socket teardown: the stream sends `Closed` and *then* ends, so
there is no unread-bytes hazard for the reason to be lost to.

## Device binding

A device must be associated with an account so the server can route ops correctly and so revocation works.

### `header_sig_v2`: the canonical string

Specified here rather than left to an implementation, which is the gap
[ADR-0022](../11-adr/0022-device-signature-canonical-json.md) records against v1
— whose byte layout lived only in the module that produced it, a fact that
module noted about itself.

A device signs the UTF-8 bytes of

```text
sunrise-device-sig-v2\n<METHOD>\n<path?query>\n<Date>\n<blake3-hex(canonical-body)>
```

with **no trailing newline**, and sends the detached Ed25519 signature as
`X-Sunrise-Device-Sig`, base64url no-pad.

| Field | Value |
|---|---|
| `<METHOD>` | The HTTP method, uppercase: `GET`, `POST`, `PUT`, `DELETE`. |
| `<path?query>` | The **concrete** target as sent, including any query string. Never a route template: a signature over `/api/v1/devices/{device_id}` would verify for every device id, which is the whole thing this binding exists to prevent. |
| `<Date>` | The `Date` header verbatim, RFC 2822. It is inside the signature, so it cannot be adjusted in flight. |
| `<canonical-body>` | The body's **canonical form** — see below. |
| `blake3-hex(...)` | BLAKE3 of those bytes, 64 lowercase hex characters. |

**The canonical form of a body.**

- **JSON** — the RFC 8785 (JCS) encoding of the parsed *value*, not of the
  octets that carried it. Both sides compute it from the typed value: the client
  from what it is sending, the server from what it parsed. Transport-level
  variation in key order, whitespace and escaping stops mattering, which is what
  JCS is for.
- **Binary** — the bytes as received. A chunk upload is raw ciphertext with no
  key order, whitespace or escaping to normalise away, so the bytes already
  *are* the canonical form. This is the same rule rather than a second scheme,
  and it is why a chunk's signature covers exactly the bytes the blob store then
  content-addresses.
- **No body** — the empty string. Stripping a body is therefore not a way to
  produce a signature that verifies.

**`Date` is checked against the server's injected clock inside ±300 s**
(`MAX_CLOCK_SKEW_SECS`), which is the replay window. A `Date` outside it is
answered `401 AUTH_DEVICE_SIG_INVALID` — the same code an unverifiable
signature gets, and deliberately *not* `AUTH_TOKEN_INVALID`: the bearer is
fine, and a client told otherwise refreshes it into the identical refusal
forever. The two ways a client measures the skew it must correct are
`POST /sync/session`'s `server_time_ms` field and the `Date` header on any
response.

That code is emitted once `X-Sunrise-Device` has resolved to an active row on
the authenticated account. Every rejection upstream of that lookup — no bearer,
a bearer that did not verify, an account that did not resolve, a device id that
is not on this account — stays `401 AUTH_TOKEN_INVALID`, because a finer answer
there would let an unauthenticated caller enumerate which devices an account
has.

The one pre-lookup exception is an **incomplete binding** under
`require_device_sig`, which names the signature. `verify_bytes` reads
`X-Sunrise-Device` and `X-Sunrise-Device-Sig` together, so a request missing
either one takes this path as surely as one missing both. It discloses nothing
about the account. What it does disclose is that the *bearer* is valid — an
invalid one is refused before reaching this code, so the two refusals are told
apart by a caller who already holds the token.

**That disclosure is accepted, not pending**
([ADR-0035](../11-adr/0035-bearer-validity-oracle-accepted.md)). The reason is
not `GET /meta`'s `device_binding_required`, which answers a different question
— it says the server *demands* a binding, not whether any particular bearer is
good. The reason is that the same disclosure is already unavoidable, and
stronger, two routes away: `POST /accounts` and `POST /devices` take the
**bootstrap exemption** and accept a bearer with no binding at all, on every
configuration, because a device cannot sign before it exists. Against either,
an invalid bearer is a `401` from `resolve_bearer`, a valid one reaches the
handler's own validation (`400`) or succeeds (`201`) — so gating the code above
behind a setting would quieten a weaker duplicate of a disclosure that stays
open regardless. `AUTH_SIGNUP_DISABLED`'s `403` is a third instance and is
chosen on exactly the same reasoning.

So, stated as a property: **this surface does not hide whether a bearer is valid
from the party presenting it, and does hide everything about the account behind
it.** Which accounts exist, which devices are on one, and whether a named device
id is one of them all stay `AUTH_TOKEN_INVALID`.

**Request bodies reject unknown fields** (`serde(deny_unknown_fields)`). This is
the load-bearing half: a signature over a re-serialisation verifies only if the
parse was lossless, and a silently-dropped field is exactly a lossy parse.
Rejecting the field turns a would-be signature mismatch into a typed `400` that
names the offending member. Floats stay forbidden, matching
`sunrise_cbor::CborValue`'s existing refusal, so JCS's number rules are never
exercised on a value the codebase treats as inadmissible anyway.

**Verification happens after parsing**, necessarily — there is no canonical form
before there is a value. So a malformed body fails as a `400` before it can fail
as a `401`, and that ordering is observable.

Implemented once, in `sunrise-http-sig`, so the relay and the generated client
cannot disagree about it. `sign` is the client half and `verify` the server's.

1. After OIDC login, the client calls `POST /api/v1/devices` with `{ device_pub_s, device_pub_d, device_cert, nickname, platform }`. The server records the device under the account.
2. The client's OIDC token implicitly identifies the *account*. The `device_id` is conveyed in two places (defense in depth): a custom URI-namespaced claim `https://sunrise.app/device_id` on the OIDC token (each device runs its own OIDC client and requests this claim via the authorization-request `claims` parameter — RFC 7519 §4.2; if the IdP refuses, it returns `invalid_claims`) and the `X-Sunrise-Device` header. The server requires both to match.
3. Revocation: `DELETE /api/v1/devices/<dev_id>` from **another** paired device
   marks the row `revoked = 1, revoked_at_ms = <now>` and deletes that device's
   push tokens, in one SQLite transaction. It is a soft delete — the row stays
   so `DeviceMeta.revoked` can be reported — and a device attempting to revoke
   *itself* is refused with `403 AUTH_DEVICE_NOT_OWNER`. Token revocation at the
   IdP is also supported but not required.

   **The revoked device's requests MUST be rejected at the server even while its
   OIDC token is still valid, and that MUST hold for the `/sync` session as well
   as for REST.** Every route on the surface, sync included, resolves a binding
   through one path — `api::signed::verify_bytes`, which looks the device up
   with `Store::active_device`, whose `WHERE` clause carries `revoked = 0`. Where
   a binding is presented, the next request after the revoking transaction
   commits therefore fails. One connection behind a mutex makes that immediate:
   there is no propagation window and no cache to expire.

   The case that needs more than a per-request check is the **event stream a
   revoked device is already holding**, because a subscriber that only reads
   issues no further requests to be checked. `live_loop` re-reads the device row
   on a timer bounded by `device_recheck_ms` (30 s by default) and ends the
   stream with `AUTH_DEVICE_REVOKED`, not `AUTH_TOKEN_EXPIRED` — the two ask the
   client for opposite behaviour.

   Until this landed, the sync path resolved the account and stopped there and
   the `devices` table was never read on it at all, so a revoked device kept
   relaying until its bearer expired.

## Recovery — NOT IMPLEMENTED

Recovery is a *cryptographic* operation, not an auth operation. Losing all devices means losing access to the local `recovery_blob` ciphertext, which the server holds opaquely.

**The flow below cannot be executed.** `accounts.recovery_blob` is written once
by `POST /api/v1/accounts` and read back by nothing: no `SELECT` in `store.rs`
names the column, `row_to_account` does not project it, and neither
`GET /api/v1/accounts/me/recovery_blob` nor its `PUT` is mounted. Step 2 has no
route to call. Fixing it is a precondition for
[ADR-0024](../11-adr/0024-key-hierarchy.md), whose recovery path unseals
identity-addressed `key_envelope` ops with the `ID_D_priv` this blob carries.

Flow (target):

1. User logs in via OIDC on a fresh device (the IdP handles email verification, MFA, etc. — none of our problem).
2. Authenticated client calls `GET /api/v1/accounts/me/recovery_blob` and receives the ciphertext.
3. User enters their **recovery code** locally; client runs Argon2id and decrypts the blob.

OIDC alone cannot recover the vault — the recovery code is required to decrypt. This is the double-gate property: even if the IdP is fully compromised, an attacker still cannot read the user's data without the offline recovery code.

The server stores recovery blobs **opaquely** — it does not validate internal format or version. The 10 MiB cap and the active-device signature belong to the unbuilt `PUT` route; the live `POST /accounts` path takes the blob under the bootstrap exemption, bounded only by `[server] max_body_bytes`.

**Account deletion is also not implemented**: neither
`POST /api/v1/accounts/me/delete/initiate` nor `DELETE /api/v1/accounts/me` is
mounted, no confirmation token is minted or stored, and
`ACCOUNT_DELETE_PHRASE_INVALID` is not among `error.rs`'s codes. The target
flow: a single-use confirmation token (32 bytes Crockford base32, 52 chars, TTL
15 minutes) issued by `POST /api/v1/accounts/me/delete/initiate`; the OIDC
issuer relays the token to the user's verified email, after which
`DELETE /api/v1/accounts/me { confirm_phrase: "<token>" }` consumes it. A wrong
or expired phrase returns `403 ACCOUNT_DELETE_PHRASE_INVALID`. Note that
`devices.account_id` carries `ON DELETE CASCADE` with `PRAGMA foreign_keys = ON`,
so the row-level machinery for an account delete exists even though no route
triggers it.

The user's only "Sunrise password" is the recovery code, and the server never sees it. Everything else (login factors, MFA, account deletion confirmations) is the IdP's responsibility.

## Compromise response

| Compromise | Action |
|---|---|
| Device key stolen | Revoke device from another device → server stops accepting that `device_id` on REST at once, and ends its live `/sync` session within `device_recheck_ms`. The thief still cannot decrypt anything without the device's vault unlock. Note that revocation is a *server-side* stop only: under the implemented derived-key model a paired device holds the vault root, so it can still read anything it already has. [ADR-0024](../11-adr/0024-key-hierarchy.md) is what makes revocation cryptographically meaningful, by minting a new epoch and re-sealing only to the surviving devices. |
| Recovery code stolen | Rotate recovery code (re-encrypt blob with new code) and re-upload. |
| Identity key stolen | Initiate identity rotation (heavyweight). |
| OIDC account compromised | User resolves at the IdP (password reset, MFA reset). E2EE data still requires recovery code or a surviving device. |
| Sunrise server credentials leaked | Operator rotates the OIDC client secret. Client-side keys unaffected. |

## Anti-abuse

Delegated to the IdP (rate limits, brute-force lockout, captcha, IP throttling). The Sunrise server is *specified* to keep standard rate limits on its REST/sync endpoints (per [`../05-sync/backpressure-and-quotas.md`](../05-sync/backpressure-and-quotas.md)); **none are implemented** — there is no rate-limiting middleware in the relay at all. See [`api.md`](./api.md) §rate-limits.

## What we explicitly do not build

- Password storage, password hashing parameters, password rotation policy.
- Email-OTP / magic-link delivery, SMTP integration.
- MFA enrollment UX, TOTP secrets, WebAuthn ceremonies.
- Refresh-token rotation, opaque-token introspection endpoints, custom token-revocation lists.
- hCaptcha, Cloudflare Turnstile, or in-house bot mitigation.
- Per-request HMAC schemes over a shared secret. (`header_sig_v2` is a *device* signature under a public key the account registered — asymmetric, no shared secret, and it exists to bind a request to a device rather than to authenticate the account.)
- A custom session cookie format.

Each item above is a standard OIDC issuer feature; reimplementing it would add weeks of build time and a permanent surface for subtle security bugs.
