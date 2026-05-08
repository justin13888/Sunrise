---
status: draft
---

# Server Authentication

Three auth contexts:

1. **Account creation** — proving control of an email and accepting terms.
2. **Sync connection** — proving device-key possession on every connect.
3. **Recovery** — proving knowledge of the recovery code.

## Account creation

Goal: prevent automated account spam, prove email control, register identity public keys.

Flow:

1. Client `POST /api/v1/accounts/init` with `{ email }`.
2. Server emails a one-time code (or magic link).
3. Client `POST /api/v1/accounts` with `{ email, otp, identity_pub_s, identity_pub_d, recovery_blob, … }`.
4. Server verifies OTP and stores public keys.

No password is ever stored. The recovery code is the user's only "password" and the server never sees it.

## Per-connection device auth

Every sync WebSocket connect:

1. Client connects, sends `ClientHello` with `account_id`, `device_id`.
2. Server replies `ServerHello` with a fresh `server_nonce` (32 random bytes).
3. Client sends `Auth` with `Ed25519_sign(D_S_priv, BLAKE3(account_id || device_id || server_nonce || timestamp))`.
4. Server verifies the signature against the stored `D_S_pub` for that device, ensures the device isn't revoked, and replies `AuthOk`.

Per-REST-request auth uses the same primitive: a signed challenge over the request method + path + body hash + nonce, carried in the `Authorization` header.

Why not OAuth / JWT? We have a public-key identity for every device already; using it directly is simpler and avoids token-rotation complexity. Tokens *are* short-lived signed challenges, computed per request.

## Recovery auth

Goal: allow a user with no surviving device to fetch their `recovery_blob` and decrypt it locally.

Flow:

1. Client `POST /api/v1/accounts/recovery/init` with `{ email }`.
2. Server emails a recovery start link (out-of-band confirmation).
3. Client follows link, server returns `{ challenge, salt, argon2_params }`.
4. Client runs Argon2id over (recovery_code, salt, params) to derive a short-term key.
5. Client signs the challenge with that key and `POST /api/v1/accounts/recovery/blob`.
6. Server verifies and returns the encrypted recovery blob.

This double gate (email + recovery code) prevents a stolen recovery code alone from being used without email access. **Open**: should there be a "no email" mode for self-host? Default proposal: yes, configurable.

## OIDC / SSO (self-host only)

Self-hosted operators can configure an OIDC provider to gate account creation. The cryptographic identity model is unchanged; OIDC simply gates "can this user create an account on this server."

## Token refresh

Per-request signing means there are no refresh tokens. The device key is the credential.

## Compromise response

| Compromise | Action |
|---|---|
| Device key stolen | Revoke device from another device → server stops accepting that key |
| Recovery code stolen | Rotate recovery code (re-encrypt blob with new code) |
| Identity key stolen | Initiate identity rotation (heavyweight) |
| Server-side credentials leaked | Rotate per-server signing keys; doesn't affect client identity keys |

## Anti-abuse

- Email throttling for OTP and recovery flows.
- Per-IP exponential backoff on auth failures.
- Account creation requires solving a lightweight PoW or hCaptcha for managed cloud (configurable; off for self-host).
