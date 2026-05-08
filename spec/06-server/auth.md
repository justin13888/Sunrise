---
status: accepted
---

# Server Authentication

Sunrise piggybacks **OpenID Connect (OIDC)** for everything user-facing — account login, session management, account recovery, sign-up, MFA, password resets, email verification. We do not implement password storage, OTP delivery, magic links, JWT signing, refresh-token rotation, or hCaptcha integration ourselves. An OIDC issuer does all of that.

Cryptographic E2EE identity (identity + device keypairs) is **separate** from server auth. Device keys sign CRDT ops and pairing handshakes for end-to-end integrity; they are not used as a per-request server credential.

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
4. Optionally verifying a `device_id` claim (set by the client on token request via OIDC `acr_values` / a custom claim — see "Device binding" below).

There are **no Sunrise-issued tokens and no refresh-token logic on the server side**. The OIDC client library on the device handles token refresh against the issuer. Token TTL is **1 hour**; clients renew at 75% of TTL pre-emptively without disconnecting (using the out-of-band token-refresh frame `0x12 RefreshToken { token: tstr }`, accepted at any time on the sync WebSocket).

A per-request Ed25519 device signature (`X-Sunrise-Device-Sig` over the canonical request line + `Date` + body hash) accompanies the bearer token; this is the `header_sig_v1` device-binding mode and is the only mode v1 supports. See [`api.md`](./api.md) for the exact mechanics.

For sync WebSocket connections: the client sends `Authorization: Bearer <token>` on the WebSocket upgrade request (browsers without header support use the `?access_token=…` query param, scrubbed from logs). The server checks `exp` on every inbound message; on expiry it sends `Error { code: "AUTH_TOKEN_EXPIRED" }` followed by `Close { code: "auth_expired", reason: "renew and reconnect" }`, and the client renews via OIDC and reconnects.

## Device binding

A device must be associated with an account so the server can route ops correctly and so revocation works.

1. After OIDC login, the client calls `POST /api/v1/devices` with `{ device_pub_s, device_pub_d, device_cert, nickname, platform }`. The server records the device under the account.
2. The client's OIDC token implicitly identifies the *account*. The `device_id` is conveyed in two places (defense in depth): a custom URI-namespaced claim `https://sunrise.app/device_id` on the OIDC token (each device runs its own OIDC client and requests this claim via the authorization-request `claims` parameter — RFC 7519 §4.2; if the IdP refuses, it returns `invalid_claims`) and the `X-Sunrise-Device` header. The server requires both to match.
3. Revocation: `DELETE /api/v1/devices/<dev_id>` from any other paired device removes the device row in the request's transaction. Subsequent requests carrying a token with that `device_id` are rejected at the server even if the OIDC token is still valid; in-flight requests may still succeed for up to 5 s as connection-affinity caches expire. Token revocation at the IdP is also supported but not required.

## Recovery

Recovery is a *cryptographic* operation, not an auth operation. Losing all devices means losing access to the local `recovery_blob` ciphertext, which the server holds opaquely.

Flow:

1. User logs in via OIDC on a fresh device (the IdP handles email verification, MFA, etc. — none of our problem).
2. Authenticated client calls `GET /api/v1/accounts/me/recovery_blob` and receives the ciphertext.
3. User enters their **recovery code** locally; client runs Argon2id and decrypts the blob.

OIDC alone cannot recover the vault — the recovery code is required to decrypt. This is the double-gate property: even if the IdP is fully compromised, an attacker still cannot read the user's data without the offline recovery code.

The server stores recovery blobs **opaquely** — it does not validate internal format or version. It only enforces the size cap (10 MiB; see [`api.md`](./api.md)) and that the upload is signed by an active device. Account deletion uses a single-use confirmation token (32 bytes Crockford base32, 52 chars, TTL 15 minutes) issued by `POST /api/v1/accounts/me/delete/initiate`; the OIDC issuer relays the token to the user's verified email, after which `DELETE /api/v1/accounts/me { confirm_phrase: "<token>" }` consumes it. A wrong or expired phrase returns `403 ACCOUNT_DELETE_PHRASE_INVALID`.

The user's only "Sunrise password" is the recovery code, and the server never sees it. Everything else (login factors, MFA, account deletion confirmations) is the IdP's responsibility.

## Compromise response

| Compromise | Action |
|---|---|
| Device key stolen | Revoke device from another device → server stops accepting that `device_id`. The thief still cannot decrypt anything without the device's vault unlock. |
| Recovery code stolen | Rotate recovery code (re-encrypt blob with new code) and re-upload. |
| Identity key stolen | Initiate identity rotation (heavyweight). |
| OIDC account compromised | User resolves at the IdP (password reset, MFA reset). E2EE data still requires recovery code or a surviving device. |
| Sunrise server credentials leaked | Operator rotates the OIDC client secret. Client-side keys unaffected. |

## Anti-abuse

Delegated to the IdP (rate limits, brute-force lockout, captcha, IP throttling). The Sunrise server keeps only standard rate limits on its REST/sync endpoints (per [`../05-sync/backpressure-and-quotas.md`](../05-sync/backpressure-and-quotas.md)).

## What we explicitly do not build

- Password storage, password hashing parameters, password rotation policy.
- Email-OTP / magic-link delivery, SMTP integration.
- MFA enrollment UX, TOTP secrets, WebAuthn ceremonies.
- Refresh-token rotation, opaque-token introspection endpoints, custom token-revocation lists.
- hCaptcha, Cloudflare Turnstile, or in-house bot mitigation.
- Per-request HMAC/signature schemes.
- A custom session cookie format.

Each item above is a standard OIDC issuer feature; reimplementing it would add weeks of build time and a permanent surface for subtle security bugs.
