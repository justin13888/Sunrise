---
status: accepted
---

# Server Authentication

Sunrise piggybacks **OpenID Connect (OIDC)** for everything user-facing — account login, session management, account recovery, sign-up, MFA, password resets, email verification. We do not implement password storage, OTP delivery, magic links, JWT signing, refresh-token rotation, or hCaptcha integration ourselves. An OIDC issuer does all of that.

Cryptographic E2EE identity (identity + device keypairs) is **separate** from server auth. Device keys sign ops and pairing handshakes for end-to-end integrity; the device signing key is *additionally* used as a per-request binding (see "Device binding" below), which is not the same thing as being the credential: the bearer proves the account, the signature proves the device.

> **Implementation status.** Token verification, the account model, `allow_signup`,
> device binding and the sync session's expiry handling are built
> (`crates/sunrise-server/src/auth/`, `store.rs`, `ws.rs`). The **recovery** and
> **account-deletion** flows below are not: no route serves them, and the
> sections say so in place.
> [ADR-0022](../11-adr/0022-device-signature-canonical-json.md) replaces
> `header_sig_v1` with `header_sig_v2` (RFC 8785 canonical JSON over the request
> *value*), and [ADR-0023](../11-adr/0023-sse-sync-transport.md) replaces the
> `/sync` WebSocket described here with SSE plus typed `POST`; the credential
> rules are unchanged by either.

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

Every authenticated request — REST and the sync WebSocket — carries a standard OIDC **access token** in `Authorization: Bearer <jwt>`.

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

There are **no Sunrise-issued tokens and no refresh-token logic on the server side**. The OIDC client library on the device handles token refresh against the issuer. Token TTL is **1 hour**; clients renew at 75% of TTL pre-emptively without disconnecting (using the out-of-band token-refresh frame `0x12 RefreshToken { token: tstr }`, accepted at any time on the sync WebSocket).

The server's whole part in a refresh is to re-verify the token the device obtained and move the session's deadline out. It refuses in three distinguishable ways, and they are deliberately not alike:

| Case | Result | Why |
|---|---|---|
| Token does not verify | `Error AUTH_TOKEN_INVALID`, **session continues** | The credential it already holds is still valid and its own deadline still governs. Tearing down would turn a recoverable client bug into a dropped session. |
| Token verifies but names a different `(iss, sub)`, or a different `device_id` | `Error AUTH_TOKEN_INVALID`, **session ends** | The relay channel namespace was derived from the upgrade's token and is never re-derived. Continuing would relay one account's traffic under another's authority. |
| Session's current token has already expired | `Error` + `Close AUTH_TOKEN_EXPIRED` | The per-frame expiry check runs before dispatch, so this never reaches the refresh handler at all. A dead session is not resurrectable by presenting a live token; the client reconnects, which re-runs the whole upgrade pipeline including `allow_signup` and the account lookup. |

An accepted refresh **is acknowledged**. The server replies with
`0x13 RefreshTokenAck { expires_at_ms: uint }`, carrying the new deadline it
just installed, so the client learns the server's view of the expiry rather than
inferring success from the absence of a disconnect. The frame is gated on the
optional `SrvTokenRefresh` capability bit, which the server ORs into its
`HelloAck` set — a client only sends `0x12` after seeing that bit agreed, so a
refresh cannot be silently swallowed by a server that predates the frame.
Implemented in `ws.rs` (`handle_refresh`), specified in
[`../05-sync/wire-protocol.md`](../05-sync/wire-protocol.md), and covered by
`crates/sunrise-server/tests/ws_token_expiry.rs`.

A per-request Ed25519 device signature (`X-Sunrise-Device-Sig`) accompanies the bearer token. The mode is `header_sig_v2`, specified byte-for-byte under §Device binding below, and it is the only mode the server accepts. It is **optional by default** — `[auth] require_device_sig` is `false` — but a signature that is present is always verified.

For sync WebSocket connections: the client sends `Authorization: Bearer <token>` on the WebSocket upgrade request. `auth::extract_bearer` reads the `Authorization` header and nothing else, so **the `?access_token=…` query-string fallback earlier revisions promised browsers does not exist** — a browser that cannot set the header cannot authenticate. What *does* exist is the defence for it: `logging/mod.rs` assembles the trace layer by hand so the span records a templated path with the query dropped, precisely because `tower-http`'s stock `MakeSpan` records the full URI, and `crates/sunrise-server/tests/logging.rs` regression-tests that `access_token` never reaches the log. That is a mitigation standing guard over a feature that was never built; it MUST survive any port (ADR-0021 says the same), because the day the query fallback *is* added is the day it starts mattering. The upgrade is authenticated once, but the *session* carries the token's `exp` for its whole life, and expiry is enforced two ways:

- **On every inbound frame**, against the server clock. This is the cheap check and the one a busy session hits first.
- **On a deadline timer** in the session loop. An idle session sends nothing, so the per-frame check never runs; without the timer a client could connect, go quiet, and hold an authenticated socket open indefinitely on a dead credential.

On expiry the server sends `Error { code: "AUTH_TOKEN_EXPIRED" }` followed by `Close { code: "AUTH_TOKEN_EXPIRED", reason: … }`, and the client renews via OIDC and reconnects. Both codes are the canonical `ErrorCode`, not a bespoke string — a client has to be able to tell an aged-out credential (renew silently) from a withdrawn one (stop and ask the user), and an untyped close makes those indistinguishable.

A session closed by the server keeps reading its socket briefly before dropping it. This is not politeness: closing a socket that still holds unread inbound bytes makes the kernel send RST rather than FIN, and an RST discards whatever the peer had not yet read — which is precisely the `Close` frame just written to explain the disconnect.

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

That code is emitted **only** once `X-Sunrise-Device` has resolved to an
active row on the authenticated account. Every rejection upstream of that
lookup — no bearer, a bearer that did not verify, an account that did not
resolve, a device id that is not on this account — stays
`401 AUTH_TOKEN_INVALID`, because a finer answer there would let an
unauthenticated caller enumerate which devices an account has. The one
exception is an absent binding under `require_device_sig`, which names the
signature: `GET /meta`'s `device_binding_required` already tells every caller
the server demands one.

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
   as for REST.** For REST it already does: `bind_device` resolves the caller
   through `Store::active_device`, whose `WHERE` clause carries `revoked = 0`, so
   the next request after the revoking transaction commits fails with
   `403 AUTH_DEVICE_NOT_OWNER`. One connection behind a mutex makes that
   immediate — there is no propagation window and no cache to expire.

   For `/sync` it holds in two places. The upgrade runs `authenticate_sync`,
   which resolves the device from `X-Sunrise-Device` — falling back to the
   token's `device_id` claim, and requiring the two to agree when both are
   present — and refuses anything that is not an active row on that account. A
   session already open is re-checked on every inbound frame and on a timer
   bounded by `device_recheck_ms` (30 s by default), because the socket a
   revoked device is *already holding* is the case revocation exists for: a
   session that only receives never presents a frame to check against. It ends
   with `AUTH_DEVICE_REVOKED`, not `AUTH_TOKEN_EXPIRED` — the two ask the client
   for opposite behaviour.

   Until this landed, the upgrade resolved the account and stopped there, and
   the `devices` table was never read on the sync path at all, so a revoked
   device kept relaying until its bearer expired.

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
